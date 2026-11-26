//! A compact file index: a tree of names, as Everything keeps it.
//!
//! Each node stores its parent, its name, and its metadata in flat arrays.
//! Names live in two shared text buffers (original and lowercase), so a node
//! costs about 45 bytes plus its name twice, instead of a full path per file.
//! Full paths are built on demand.
//!
//! Node ids stay stable while the index changes: removed nodes are marked dead
//! and skipped. When the index is built, ids follow the path order (see
//! [`crate::search::compare_paths`]), which makes sorting by path free until the
//! index changes.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rayon::prelude::*;

use crate::model::Entry;
use crate::search::compare_paths;

/// Parent values with this bit name a root path instead of a node.
const ROOT: u32 = 1 << 31;
const NONE: u32 = u32::MAX;

const IS_DIR: u8 = 1;
const HAS_SIZE: u8 = 2;
const HAS_MODIFIED: u8 = 4;
const HAS_CREATED: u8 = 8;
const HAS_ATTRIBUTES: u8 = 16;
const DEAD: u8 = 32;

/// The folder a node lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Parent {
    /// A root path, such as the scanned folder `C:\`.
    Root(u32),
    /// A folder node.
    Node(u32),
}

impl Parent {
    fn encode(self) -> u32 {
        match self {
            Self::Root(root) => ROOT | root,
            Self::Node(node) => node,
        }
    }

    fn decode(value: u32) -> Self {
        if value & ROOT != 0 {
            Self::Root(value & !ROOT)
        } else {
            Self::Node(value)
        }
    }
}

/// Metadata of one file or folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Meta {
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub created: Option<u64>,
    pub attributes: Option<u32>,
}

impl Meta {
    fn flags(&self) -> u8 {
        let mut flags = 0;
        flags |= if self.is_dir { IS_DIR } else { 0 };
        flags |= if self.size.is_some() { HAS_SIZE } else { 0 };
        flags |= if self.modified.is_some() { HAS_MODIFIED } else { 0 };
        flags |= if self.created.is_some() { HAS_CREATED } else { 0 };
        flags |= if self.attributes.is_some() { HAS_ATTRIBUTES } else { 0 };
        flags
    }
}

impl Entry {
    pub fn meta(&self) -> Meta {
        Meta {
            is_dir: self.is_dir,
            size: self.size,
            modified: self.modified,
            created: self.created,
            attributes: self.attributes,
        }
    }
}

/// Collects nodes for a [`SearchIndex`].
#[derive(Default)]
pub struct Nodes {
    parent: Vec<u32>,
    name_at: Vec<u32>,
    name_len: Vec<u16>,
    size: Vec<u64>,
    modified: Vec<u64>,
    created: Vec<u64>,
    attributes: Vec<u32>,
    flags: Vec<u8>,
    names: String,
    /// Exact names that are not valid Unicode, by node.
    exact: HashMap<u32, OsString>,
    roots: Vec<OsString>,
    /// MFT record number of each node, or empty when unknown.
    records: Vec<u32>,
    /// The record of the volume root folder and its root number.
    root_record: Option<(u32, u32)>,
}

impl Nodes {
    pub fn with_capacity(nodes: usize, name_bytes: usize) -> Self {
        Self {
            parent: Vec::with_capacity(nodes),
            name_at: Vec::with_capacity(nodes),
            name_len: Vec::with_capacity(nodes),
            size: Vec::with_capacity(nodes),
            modified: Vec::with_capacity(nodes),
            created: Vec::with_capacity(nodes),
            attributes: Vec::with_capacity(nodes),
            flags: Vec::with_capacity(nodes),
            names: String::with_capacity(name_bytes),
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Tells that MFT record `record` is root number `root`.
    pub fn set_root_record(&mut self, record: u32, root: u32) {
        self.root_record = Some((record, root));
    }

    /// Adds a root path and returns its number for [`Parent::Root`].
    pub fn add_root(&mut self, path: impl Into<OsString>) -> u32 {
        self.roots.push(path.into());
        (self.roots.len() - 1) as u32
    }

    /// Adds a node and returns its id.
    pub fn push(&mut self, parent: Parent, name: &OsStr, meta: Meta, record: Option<u32>) -> u32 {
        let id = self.parent.len() as u32;
        match name.to_str() {
            Some(text) => self.push_name(text),
            None => {
                self.push_name(&name.to_string_lossy());
                self.exact.insert(id, name.to_owned());
            }
        }
        self.push_rest(parent, meta, record);
        id
    }

    /// Adds a node whose name is valid Unicode.
    pub fn push_str(&mut self, parent: Parent, name: &str, meta: Meta, record: Option<u32>) -> u32 {
        let id = self.parent.len() as u32;
        self.push_name(name);
        self.push_rest(parent, meta, record);
        id
    }

    fn push_name(&mut self, name: &str) {
        let name = truncate(name, u16::MAX as usize);
        self.name_at.push(self.names.len() as u32);
        self.name_len.push(name.len() as u16);
        self.names.push_str(name);
    }

    fn push_rest(&mut self, parent: Parent, meta: Meta, record: Option<u32>) {
        self.parent.push(parent.encode());
        self.size.push(meta.size.unwrap_or(0));
        self.modified.push(meta.modified.unwrap_or(0));
        self.created.push(meta.created.unwrap_or(0));
        self.attributes.push(meta.attributes.unwrap_or(0));
        self.flags.push(meta.flags());
        if let Some(record) = record {
            self.records.resize(self.parent.len() - 1, NONE);
            self.records.push(record);
        }
    }

    /// Builds nodes from full paths. Entries whose parent folder is not in
    /// the list hang below a root path. Ids follow the path order.
    pub fn from_entries(entries: Vec<Entry>) -> Self {
        let count = entries.len();
        let mut folders: HashMap<&OsStr, u32> = HashMap::new();
        for (id, entry) in entries.iter().enumerate() {
            if entry.is_dir {
                folders.entry(entry.path.as_os_str()).or_insert(id as u32);
            }
        }
        let mut nodes = Self::with_capacity(count, count * 24);
        let mut root_ids: HashMap<OsString, u32> = HashMap::new();
        // Split paths and find parent folders in parallel; parents that are
        // not in the list become root paths afterwards.
        let split: Vec<(Option<u32>, &OsStr, &OsStr)> = entries
            .par_iter()
            .map(|entry| {
                let (parent, name) = match (entry.path.parent(), entry.path.file_name()) {
                    (Some(parent), Some(name)) => (parent.as_os_str(), name),
                    _ => (OsStr::new(""), entry.path.as_os_str()),
                };
                (folders.get(parent).copied(), parent, name)
            })
            .collect();
        let mut parents = Vec::with_capacity(count);
        let mut names: Vec<&OsStr> = Vec::with_capacity(count);
        for &(folder, parent, name) in &split {
            let parent = match folder {
                Some(folder) => folder,
                None => {
                    let next = root_ids.len() as u32;
                    ROOT | *root_ids.entry(parent.to_owned()).or_insert(next)
                }
            };
            parents.push(parent);
            names.push(name);
        }
        drop(split);
        let mut roots: Vec<(OsString, u32)> = root_ids.into_iter().collect();
        roots.sort_by_key(|(_, id)| *id);
        let mut root_order: Vec<u32> = (0..roots.len() as u32).collect();
        root_order.sort_by(|&a, &b| compare_paths(Path::new(&roots[a as usize].0), Path::new(&roots[b as usize].0)));
        for (path, _) in &roots {
            nodes.add_root(path.clone());
        }

        // Group by parent, sorted by name, then walk depth-first.
        let mut sorted: Vec<u32> = (0..count as u32).collect();
        sorted.par_sort_unstable_by_key(|&id| (parents[id as usize], id));
        sorted.par_chunk_by_mut(|&a, &b| parents[a as usize] == parents[b as usize]).for_each(|group| {
            group.sort_unstable_by(|&a, &b| {
                names[a as usize].as_encoded_bytes().cmp(names[b as usize].as_encoded_bytes()).then_with(|| a.cmp(&b))
            })
        });
        let range = |parent: u32| {
            let start = sorted.partition_point(|&id| parents[id as usize] < parent);
            let end = sorted.partition_point(|&id| parents[id as usize] <= parent);
            (start, end)
        };
        let mut new_id = vec![NONE; count];
        for root in root_order {
            let mut stack = vec![range(ROOT | root)];
            while let Some((next, end)) = stack.last_mut() {
                if next == end {
                    stack.pop();
                    continue;
                }
                let old = sorted[*next] as usize;
                *next += 1;
                let parent = match parents[old] {
                    parent if parent & ROOT != 0 => Parent::Root(parent & !ROOT),
                    parent => Parent::Node(new_id[parent as usize]),
                };
                new_id[old] = nodes.push(parent, names[old], entries[old].meta(), None);
                if entries[old].is_dir && folders.get(entries[old].path.as_os_str()) == Some(&(old as u32)) {
                    stack.push(range(old as u32));
                }
            }
        }
        drop((folders, names, sorted));
        // Freeing the paths takes a while; do it on another thread.
        std::thread::spawn(move || drop(entries));
        nodes
    }
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The searchable index.
pub struct SearchIndex {
    nodes: Nodes,
    lower_at: Vec<u32>,
    lower_len: Vec<u16>,
    lower: String,
    root_text: Vec<String>,
    root_lower: Vec<String>,
    dead_roots: Vec<bool>,
    /// Record number to first node, when records are known.
    by_record: Vec<u32>,
    /// More nodes of the same record (hard links), by record.
    more_links: HashMap<u32, Vec<u32>>,
    /// The record of the volume root folder and its root number.
    root_record: Option<(u32, u32)>,
    /// (parent, name hash) to node, built on the first path lookup.
    children: Option<HashMap<(u32, u64), u32>>,
    dead: usize,
    /// Whether ids no longer follow the path order.
    mutated: bool,
    ranks: OnceLock<Vec<u32>>,
}

impl SearchIndex {
    /// Builds an index from full paths. `_need_paths` is kept for
    /// compatibility; full paths are built on demand.
    pub fn new(entries: Vec<Entry>, _need_paths: bool) -> Self {
        Self::from_nodes(Nodes::from_entries(entries), true)
    }

    /// Builds an index. `path_sorted` tells whether node ids follow the path order.
    pub fn from_nodes(mut nodes: Nodes, path_sorted: bool) -> Self {
        let count = nodes.len();
        if !nodes.records.is_empty() {
            nodes.records.resize(count, NONE);
        }
        // Lowercase names in parallel chunks, then join the chunks.
        const CHUNK: usize = 1 << 15;
        let chunks: Vec<(String, Vec<u32>, Vec<u16>)> = (0..count.div_ceil(CHUNK))
            .into_par_iter()
            .map(|chunk| {
                let range = chunk * CHUNK..((chunk + 1) * CHUNK).min(count);
                let mut text = String::with_capacity(range.len() * 20);
                let mut at = Vec::with_capacity(range.len());
                let mut len = Vec::with_capacity(range.len());
                for id in range {
                    let start = text.len();
                    push_lowercase(&mut text, nodes_name(&nodes, id));
                    let name = truncate(&text[start..], u16::MAX as usize).len();
                    text.truncate(start + name);
                    at.push(start as u32);
                    len.push(name as u16);
                }
                (text, at, len)
            })
            .collect();
        let total: usize = chunks.iter().map(|(text, _, _)| text.len()).sum();
        let mut lower = String::with_capacity(total);
        let mut lower_at = Vec::with_capacity(count);
        let mut lower_len = Vec::with_capacity(count);
        for (text, at, len) in chunks {
            let base = lower.len() as u32;
            lower.push_str(&text);
            lower_at.extend(at.into_iter().map(|offset| offset + base));
            lower_len.extend(len);
        }
        let root_text: Vec<String> = nodes.roots.iter().map(|root| root.to_string_lossy().into_owned()).collect();
        let root_lower = root_text.iter().map(|root| root.to_lowercase()).collect();
        let dead_roots = vec![false; nodes.roots.len()];
        let dead = nodes.flags.iter().filter(|&&flags| flags & DEAD != 0).count();
        let mut index = Self {
            nodes,
            lower_at,
            lower_len,
            lower,
            root_text,
            root_lower,
            dead_roots,
            by_record: Vec::new(),
            more_links: HashMap::new(),
            root_record: None,
            children: None,
            dead,
            mutated: !path_sorted,
            ranks: OnceLock::new(),
        };
        index.root_record = index.nodes.root_record;
        index.build_records();
        index
    }

    fn build_records(&mut self) {
        self.by_record.clear();
        self.more_links.clear();
        let Some(&max) = self.nodes.records.iter().filter(|&&record| record != NONE).max() else {
            return;
        };
        self.by_record = vec![NONE; max as usize + 1];
        for id in 0..self.nodes.len() {
            let record = self.nodes.records[id];
            if record != NONE && self.is_live(id) {
                self.link_record(record, id as u32);
            }
        }
    }

    fn link_record(&mut self, record: u32, id: u32) {
        let index = record as usize;
        if self.by_record.len() <= index {
            self.by_record.resize(index + 1, NONE);
        }
        if self.by_record[index] == NONE {
            self.by_record[index] = id;
        } else {
            self.more_links.entry(record).or_default().push(id);
        }
    }

    fn unlink_record(&mut self, id: u32) {
        let Some(&record) = self.nodes.records.get(id as usize).filter(|&&record| record != NONE) else {
            return;
        };
        let index = record as usize;
        if self.by_record.get(index) == Some(&id) {
            let next = self.more_links.get_mut(&record).and_then(|more| more.pop());
            self.by_record[index] = next.unwrap_or(NONE);
        } else if let Some(more) = self.more_links.get_mut(&record) {
            more.retain(|&other| other != id);
        }
        if self.more_links.get(&record).is_some_and(Vec::is_empty) {
            self.more_links.remove(&record);
        }
    }

    /// Tells the index that MFT record `record` is root number `root`.
    pub fn set_root_record(&mut self, record: u32, root: u32) {
        self.root_record = Some((record, root));
    }

    /// Number of node slots, including removed ones.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live_len() == 0
    }

    /// Number of files and folders in the index.
    pub fn live_len(&self) -> usize {
        self.nodes.len() - self.dead
    }

    pub fn is_live(&self, id: usize) -> bool {
        self.nodes.flags[id] & DEAD == 0
    }

    /// The ids of all files and folders, in id order.
    pub fn ids(&self) -> Vec<usize> {
        if self.dead == 0 {
            return (0..self.len()).collect();
        }
        (0..self.len()).filter(|&id| self.is_live(id)).collect()
    }

    pub fn has_records(&self) -> bool {
        !self.nodes.records.is_empty()
    }

    pub fn roots(&self) -> &[OsString] {
        &self.nodes.roots
    }

    pub fn name(&self, id: usize) -> &str {
        nodes_name(&self.nodes, id)
    }

    pub fn name_lower(&self, id: usize) -> &str {
        let start = self.lower_at[id] as usize;
        &self.lower[start..start + self.lower_len[id] as usize]
    }

    /// The exact name, which can differ from [`Self::name`] for names that
    /// are not valid Unicode.
    pub fn name_os(&self, id: usize) -> &OsStr {
        match self.nodes.exact.get(&(id as u32)) {
            Some(exact) => exact,
            None => OsStr::new(self.name(id)),
        }
    }

    pub fn is_dir(&self, id: usize) -> bool {
        self.nodes.flags[id] & IS_DIR != 0
    }

    pub fn size(&self, id: usize) -> Option<u64> {
        (self.nodes.flags[id] & HAS_SIZE != 0).then(|| self.nodes.size[id])
    }

    pub fn modified(&self, id: usize) -> Option<u64> {
        (self.nodes.flags[id] & HAS_MODIFIED != 0).then(|| self.nodes.modified[id])
    }

    pub fn created(&self, id: usize) -> Option<u64> {
        (self.nodes.flags[id] & HAS_CREATED != 0).then(|| self.nodes.created[id])
    }

    pub fn attributes(&self, id: usize) -> Option<u32> {
        (self.nodes.flags[id] & HAS_ATTRIBUTES != 0).then(|| self.nodes.attributes[id])
    }

    pub fn meta(&self, id: usize) -> Meta {
        Meta {
            is_dir: self.is_dir(id),
            size: self.size(id),
            modified: self.modified(id),
            created: self.created(id),
            attributes: self.attributes(id),
        }
    }

    pub fn parent(&self, id: usize) -> Parent {
        Parent::decode(self.nodes.parent[id])
    }

    pub fn record(&self, id: usize) -> Option<u32> {
        self.nodes.records.get(id).copied().filter(|&record| record != NONE)
    }

    /// Ancestors of a node from the top: (root, nodes from the top down).
    fn chain(&self, id: usize, chain: &mut Vec<u32>) -> u32 {
        chain.clear();
        let mut current = id as u32;
        loop {
            chain.push(current);
            match Parent::decode(self.nodes.parent[current as usize]) {
                Parent::Root(root) => {
                    chain.reverse();
                    return root;
                }
                Parent::Node(parent) => current = parent,
            }
        }
    }

    pub fn path(&self, id: usize) -> PathBuf {
        let mut chain = Vec::new();
        let root = self.chain(id, &mut chain);
        let mut path = self.nodes.roots[root as usize].clone();
        for &node in &chain {
            push_component(&mut path, self.name_os(node as usize));
        }
        PathBuf::from(path)
    }

    /// Writes the full path as text, original or lowercase, into `text`.
    pub fn write_path(&self, id: usize, lowercase: bool, chain: &mut Vec<u32>, text: &mut String) {
        let root = self.chain(id, chain) as usize;
        text.clear();
        text.push_str(if lowercase { &self.root_lower[root] } else { &self.root_text[root] });
        for &node in chain.iter() {
            if !text.is_empty() && !text.ends_with(['\\', '/']) {
                text.push(std::path::MAIN_SEPARATOR);
            }
            text.push_str(if lowercase { self.name_lower(node as usize) } else { self.name(node as usize) });
        }
    }

    pub fn entry(&self, id: usize) -> Entry {
        let meta = self.meta(id);
        Entry {
            path: self.path(id),
            is_dir: meta.is_dir,
            size: meta.size,
            modified: meta.modified,
            created: meta.created,
            attributes: meta.attributes,
        }
    }

    /// All files and folders as entries with full paths.
    pub fn entries(&self) -> Vec<Entry> {
        self.ids().into_par_iter().map(|id| self.entry(id)).collect()
    }

    /// Whether node ids follow the path order.
    pub fn is_path_sorted(&self) -> bool {
        !self.mutated
    }

    /// Position of each node in path order, or `None` when it equals the id.
    pub fn path_ranks(&self) -> Option<&[u32]> {
        if !self.mutated {
            return None;
        }
        Some(self.ranks.get_or_init(|| self.compute_ranks()))
    }

    fn compute_ranks(&self) -> Vec<u32> {
        let ids: Vec<u32> = self.ids().into_iter().map(|id| id as u32).collect();
        let mut sorted = ids;
        let parent_of = |id: u32| self.nodes.parent[id as usize];
        sorted.par_sort_unstable_by_key(|&id| (parent_of(id), id));
        sorted.par_chunk_by_mut(|&a, &b| parent_of(a) == parent_of(b)).for_each(|group| {
            group.sort_unstable_by(|&a, &b| {
                let name = |id: u32| self.name_os(id as usize).as_encoded_bytes();
                name(a).cmp(name(b)).then_with(|| a.cmp(&b))
            })
        });
        let parents = &self.nodes.parent;
        let range = |parent: u32| {
            let start = sorted.partition_point(|&id| parents[id as usize] < parent);
            let end = sorted.partition_point(|&id| parents[id as usize] <= parent);
            (start, end)
        };
        let mut roots: Vec<u32> = (0..self.nodes.roots.len() as u32).collect();
        roots.sort_by(|&a, &b| {
            compare_paths(Path::new(&self.nodes.roots[a as usize]), Path::new(&self.nodes.roots[b as usize]))
        });
        let mut ranks = vec![u32::MAX; self.len()];
        let mut next_rank = 0;
        for root in roots {
            let mut stack = vec![range(ROOT | root)];
            while let Some((next, end)) = stack.last_mut() {
                if next == end {
                    stack.pop();
                    continue;
                }
                let id = sorted[*next];
                *next += 1;
                ranks[id as usize] = next_rank;
                next_rank += 1;
                if self.is_dir(id as usize) {
                    stack.push(range(id));
                }
            }
        }
        ranks
    }

    /// Compares two nodes by full path.
    pub fn compare_by_path(&self, a: usize, b: usize) -> Ordering {
        if !self.mutated {
            return a.cmp(&b);
        }
        if let Some(ranks) = self.ranks.get() {
            return ranks[a].cmp(&ranks[b]);
        }
        compare_paths(&self.path(a), &self.path(b))
    }

    fn changed(&mut self) {
        self.mutated = true;
        self.ranks = OnceLock::new();
    }

    // ----- Changes -----

    fn children_map(&mut self) -> &mut HashMap<(u32, u64), u32> {
        if self.children.is_none() {
            let map = (0..self.len())
                .filter(|&id| self.is_live(id))
                .map(|id| ((self.nodes.parent[id], name_hash(self.name_os(id))), id as u32))
                .collect();
            self.children = Some(map);
        }
        self.children.as_mut().expect("built above")
    }

    /// The child of `parent` named `name`.
    pub fn child(&mut self, parent: Parent, name: &OsStr) -> Option<usize> {
        let key = (parent.encode(), name_hash(name));
        let id = *self.children_map().get(&key)? as usize;
        (self.is_live(id) && self.name_os(id) == name).then_some(id)
    }

    /// The node at `path`, if it is in the index.
    pub fn find(&mut self, path: &Path) -> Option<usize> {
        let (parent, rest) = self.locate_root(path)?;
        let mut current = parent;
        let mut found = None;
        for component in rest.components() {
            let id = self.child(current, component.as_os_str())?;
            found = Some(id);
            current = Parent::Node(id as u32);
        }
        found
    }

    /// The deepest live root that contains `path`, and the rest of the path.
    fn locate_root<'p>(&self, path: &'p Path) -> Option<(Parent, &'p Path)> {
        let mut best: Option<(usize, &Path)> = None;
        for (root, root_path) in self.nodes.roots.iter().enumerate() {
            if self.dead_roots[root] {
                continue;
            }
            if let Ok(rest) = path.strip_prefix(root_path) {
                if best.is_none_or(|(best, _)| self.nodes.roots[best].len() < root_path.len()) {
                    best = Some((root, rest));
                }
            }
        }
        best.map(|(root, rest)| (Parent::Root(root as u32), rest))
    }

    /// The folder that holds `path`: an indexed folder or a root path.
    /// Adds a root path when the folder is not in the index.
    pub fn parent_for(&mut self, path: &Path) -> Parent {
        let folder = path.parent().unwrap_or(Path::new(""));
        if let Some(id) = self.find(folder).filter(|&id| self.is_dir(id)) {
            return Parent::Node(id as u32);
        }
        if let Some(root) = self.nodes.roots.iter().position(|root| Path::new(root) == folder) {
            if !self.dead_roots[root] {
                return Parent::Root(root as u32);
            }
        }
        let root = self.nodes.add_root(folder.as_os_str().to_owned());
        let text = folder.to_string_lossy().into_owned();
        self.root_lower.push(text.to_lowercase());
        self.root_text.push(text);
        self.dead_roots.push(false);
        self.changed();
        Parent::Root(root)
    }

    /// Adds a node and returns its id.
    pub fn insert(&mut self, parent: Parent, name: &OsStr, meta: Meta, record: Option<u32>) -> usize {
        if record.is_none() && !self.nodes.records.is_empty() {
            self.nodes.records.push(NONE);
        }
        if record.is_some() && self.nodes.records.len() < self.len() {
            self.nodes.records.resize(self.len(), NONE);
        }
        let id = self.nodes.push(parent, name, meta, record) as usize;
        self.push_lower(id);
        if let Some(record) = record {
            self.link_record(record, id as u32);
        }
        let key = (parent.encode(), name_hash(name));
        if let Some(children) = self.children.as_mut() {
            children.insert(key, id as u32);
        }
        self.changed();
        id
    }

    fn push_lower(&mut self, id: usize) {
        let start = self.lower.len();
        let name = nodes_name(&self.nodes, id).to_owned();
        push_lowercase(&mut self.lower, &name);
        let length = truncate(&self.lower[start..], u16::MAX as usize).len();
        self.lower.truncate(start + length);
        if id == self.lower_at.len() {
            self.lower_at.push(start as u32);
            self.lower_len.push(length as u16);
        } else {
            self.lower_at[id] = start as u32;
            self.lower_len[id] = length as u16;
        }
    }

    /// Replaces the metadata of a node. Returns whether it changed.
    pub fn set_meta(&mut self, id: usize, meta: Meta) -> bool {
        if self.meta(id) == meta {
            return false;
        }
        let nodes = &mut self.nodes;
        let keep_dead = nodes.flags[id] & DEAD;
        nodes.flags[id] = meta.flags() | keep_dead;
        nodes.size[id] = meta.size.unwrap_or(0);
        nodes.modified[id] = meta.modified.unwrap_or(0);
        nodes.created[id] = meta.created.unwrap_or(0);
        nodes.attributes[id] = meta.attributes.unwrap_or(0);
        true
    }

    /// Moves or renames a node. Its children move with it.
    pub fn rename(&mut self, id: usize, parent: Parent, name: &OsStr) -> bool {
        if self.parent(id) == parent && self.name_os(id) == name {
            return false;
        }
        let old_key = (self.nodes.parent[id], name_hash(self.name_os(id)));
        if let Some(children) = self.children.as_mut() {
            if children.get(&old_key) == Some(&(id as u32)) {
                children.remove(&old_key);
            }
        }
        self.nodes.exact.remove(&(id as u32));
        let text = match name.to_str() {
            Some(text) => Cow::Borrowed(text),
            None => {
                self.nodes.exact.insert(id as u32, name.to_owned());
                name.to_string_lossy()
            }
        };
        let text = truncate(&text, u16::MAX as usize);
        self.nodes.name_at[id] = self.nodes.names.len() as u32;
        self.nodes.name_len[id] = text.len() as u16;
        self.nodes.names.push_str(text);
        self.nodes.parent[id] = parent.encode();
        self.push_lower(id);
        if let Some(children) = self.children.as_mut() {
            children.insert((parent.encode(), name_hash(name)), id as u32);
        }
        self.changed();
        true
    }

    /// Removes nodes and everything below them. Returns all removed ids.
    pub fn remove(&mut self, ids: &[usize]) -> Vec<usize> {
        let mut removed = Vec::new();
        let mut folders = false;
        for &id in ids {
            if self.is_live(id) {
                folders |= self.is_dir(id);
                self.kill(id);
                removed.push(id);
            }
        }
        if folders {
            removed.extend(self.remove_orphans());
        }
        removed
    }

    /// Removes the node at `path`, everything below it, and every root path
    /// inside it. Returns all removed ids.
    pub fn remove_path(&mut self, path: &Path) -> Vec<usize> {
        let mut removed = match self.find(path) {
            Some(id) => self.remove(&[id]),
            None => Vec::new(),
        };
        let mut roots = false;
        for root in 0..self.nodes.roots.len() {
            if !self.dead_roots[root] && Path::new(&self.nodes.roots[root]).starts_with(path) {
                self.dead_roots[root] = true;
                roots = true;
            }
        }
        if roots {
            removed.extend(self.remove_orphans());
        }
        removed
    }

    fn kill(&mut self, id: usize) {
        self.nodes.flags[id] |= DEAD;
        self.dead += 1;
        self.unlink_record(id as u32);
        let key = (self.nodes.parent[id], name_hash(self.name_os(id)));
        if let Some(children) = self.children.as_mut() {
            if children.get(&key) == Some(&(id as u32)) {
                children.remove(&key);
            }
        }
        self.changed();
    }

    /// Removes live nodes below removed folders or roots.
    fn remove_orphans(&mut self) -> Vec<usize> {
        const UNKNOWN: u8 = 0;
        const GONE: u8 = 1;
        const ALIVE: u8 = 2;
        let mut state: Vec<u8> = self.nodes.flags.iter().map(|&flags| if flags & DEAD != 0 { GONE } else { UNKNOWN }).collect();
        let mut chain = Vec::new();
        let mut removed = Vec::new();
        for id in 0..self.len() {
            if state[id] != UNKNOWN {
                continue;
            }
            chain.clear();
            let mut current = id;
            let result = loop {
                chain.push(current);
                match Parent::decode(self.nodes.parent[current]) {
                    Parent::Root(root) => break if self.dead_roots[root as usize] { GONE } else { ALIVE },
                    Parent::Node(parent) => {
                        let parent = parent as usize;
                        if state[parent] != UNKNOWN {
                            break state[parent];
                        }
                        current = parent;
                    }
                }
            };
            for &node in &chain {
                state[node] = result;
                if result == GONE {
                    removed.push(node);
                }
            }
        }
        for &id in &removed {
            self.kill(id);
        }
        removed
    }

    /// Node of a folder MFT record, or the root path it stands for.
    pub fn folder_of_record(&self, record: u32) -> Option<Parent> {
        if let Some((root_record, root)) = self.root_record {
            if record == root_record {
                return Some(Parent::Root(root));
            }
        }
        let id = *self.by_record.get(record as usize)?;
        (id != NONE && self.is_dir(id as usize) && self.is_live(id as usize)).then_some(Parent::Node(id))
    }

    /// Live nodes of an MFT record (more than one for hard links).
    pub fn nodes_of_record(&self, record: u32) -> Vec<usize> {
        let mut nodes = Vec::new();
        if let Some(&first) = self.by_record.get(record as usize) {
            if first != NONE {
                nodes.push(first as usize);
            }
        }
        if let Some(more) = self.more_links.get(&record) {
            nodes.extend(more.iter().map(|&id| id as usize));
        }
        nodes.retain(|&id| self.is_live(id));
        nodes
    }

    /// Copies the live nodes into new [`Nodes`] in path order, so the
    /// result has no removed slots. Returns the nodes and the root record.
    pub fn compact(&self) -> Nodes {
        let mut order = self.ids();
        if let Some(ranks) = self.path_ranks() {
            order.par_sort_unstable_by_key(|&id| ranks[id]);
        }
        let mut new_id = vec![NONE; self.len()];
        let mut nodes = Nodes::with_capacity(order.len(), self.nodes.names.len());
        for root in &self.nodes.roots {
            nodes.add_root(root.clone());
        }
        for &id in &order {
            let parent = match self.parent(id) {
                Parent::Root(root) => Parent::Root(root),
                Parent::Node(parent) => Parent::Node(new_id[parent as usize]),
            };
            new_id[id] = nodes.push(parent, self.name_os(id), self.meta(id), self.record(id));
        }
        nodes.root_record = self.root_record;
        nodes
    }
}

/// Serialization for the saved index (see [`crate::cache`]).
impl SearchIndex {
    /// Writes the live nodes in path order, so a loaded index has no
    /// removed slots and its ids follow the path order again.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut order = self.ids();
        if let Some(ranks) = self.path_ranks() {
            order.par_sort_unstable_by_key(|&id| ranks[id]);
        }
        let mut new_id = vec![NONE; self.len()];
        for (position, &id) in order.iter().enumerate() {
            new_id[id] = position as u32;
        }
        let records = self.has_records();
        put_u32(out, order.len() as u32);
        put_u32(out, self.nodes.roots.len() as u32);
        for root in &self.nodes.roots {
            put_bytes(out, &native_bytes(root));
        }
        out.push(u8::from(records));
        match self.root_record {
            Some((record, root)) => {
                out.push(1);
                put_u32(out, record);
                put_u32(out, root);
            }
            None => out.push(0),
        }
        let name_bytes: usize = order.iter().map(|&id| self.nodes.name_len[id] as usize).sum();
        put_u32(out, name_bytes as u32);
        for &id in &order {
            out.extend_from_slice(self.name(id).as_bytes());
        }
        let parent = |id: usize| match self.parent(id) {
            Parent::Root(root) => ROOT | root,
            Parent::Node(node) => new_id[node as usize],
        };
        out.reserve(order.len() * 40);
        for &id in &order {
            put_u32(out, parent(id));
        }
        for &id in &order {
            out.extend_from_slice(&self.nodes.name_len[id].to_le_bytes());
        }
        for array in [&self.nodes.size, &self.nodes.modified, &self.nodes.created] {
            for &id in &order {
                out.extend_from_slice(&array[id].to_le_bytes());
            }
        }
        for &id in &order {
            put_u32(out, self.nodes.attributes[id]);
        }
        for &id in &order {
            out.push(self.nodes.flags[id] & !DEAD);
        }
        if records {
            for &id in &order {
                put_u32(out, self.nodes.records[id]);
            }
        }
        let exact: Vec<(u32, &OsString)> = self
            .nodes
            .exact
            .iter()
            .filter(|(&id, _)| self.is_live(id as usize))
            .map(|(&id, name)| (new_id[id as usize], name))
            .collect();
        put_u32(out, exact.len() as u32);
        for (id, name) in exact {
            put_u32(out, id);
            put_bytes(out, &native_bytes(name));
        }
    }
}

impl Nodes {
    /// Reads nodes written by [`SearchIndex::encode`]. Returns them and the
    /// number of bytes read. Nodes come out in path order.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<(Self, usize)> {
        use anyhow::{ensure, Context};
        let mut reader = Reader { bytes, position: 0 };
        let count = reader.u32()? as usize;
        ensure!(count <= bytes.len() / 20, "damaged index");
        let root_count = reader.u32()? as usize;
        ensure!(root_count <= bytes.len(), "damaged index");
        let mut nodes = Nodes::default();
        for _ in 0..root_count {
            nodes.roots.push(from_native_bytes(reader.bytes()?)?);
        }
        let records = reader.take(1)?[0] != 0;
        if reader.take(1)?[0] != 0 {
            let record = reader.u32()?;
            let root = reader.u32()?;
            ensure!((root as usize) < root_count, "damaged index");
            nodes.root_record = Some((record, root));
        }
        let name_bytes = reader.u32()? as usize;
        nodes.names = String::from_utf8(reader.take(name_bytes)?.to_vec()).context("damaged index")?;
        let u32s = |reader: &mut Reader| -> anyhow::Result<Vec<u32>> {
            Ok(reader.take(count * 4)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
        };
        let u64s = |reader: &mut Reader| -> anyhow::Result<Vec<u64>> {
            Ok(reader.take(count * 8)?.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect())
        };
        nodes.parent = u32s(&mut reader)?;
        nodes.name_len = reader.take(count * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        nodes.size = u64s(&mut reader)?;
        nodes.modified = u64s(&mut reader)?;
        nodes.created = u64s(&mut reader)?;
        nodes.attributes = u32s(&mut reader)?;
        nodes.flags = reader.take(count)?.to_vec();
        if records {
            nodes.records = u32s(&mut reader)?;
        }
        let exact_count = reader.u32()? as usize;
        ensure!(exact_count <= count, "damaged index");
        for _ in 0..exact_count {
            let id = reader.u32()?;
            ensure!((id as usize) < count, "damaged index");
            nodes.exact.insert(id, from_native_bytes(reader.bytes()?)?);
        }
        // Check every reference, so a damaged file cannot cause a panic later.
        let mut at = 0u32;
        nodes.name_at = Vec::with_capacity(count);
        for (id, &length) in nodes.name_len.iter().enumerate() {
            let end = at as usize + length as usize;
            ensure!(
                end <= nodes.names.len() && nodes.names.is_char_boundary(end),
                "damaged index"
            );
            nodes.name_at.push(at);
            at = end as u32;
            let parent = nodes.parent[id];
            ensure!(
                if parent & ROOT != 0 { ((parent & !ROOT) as usize) < root_count } else { (parent as usize) < id },
                "damaged index"
            );
            ensure!(nodes.flags[id] & DEAD == 0, "damaged index");
        }
        ensure!(at as usize == nodes.names.len(), "damaged index");
        // Record numbers size a lookup table; keep them near the node count.
        let limit = (count as u64 * 64 + (1 << 20)).min(u64::from(NONE)) as u32;
        ensure!(
            nodes.records.iter().all(|&record| record == NONE || record < limit)
                && nodes.root_record.is_none_or(|(record, _)| record < limit),
            "damaged index"
        );
        Ok((nodes, reader.position))
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> anyhow::Result<&'a [u8]> {
        let end = self.position.checked_add(length).filter(|&end| end <= self.bytes.len());
        let end = end.ok_or_else(|| anyhow::anyhow!("damaged index"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> anyhow::Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

/// The path in UTF-16 (Windows) or raw bytes (other systems).
fn native_bytes(text: &OsStr) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        text.encode_wide().flat_map(u16::to_le_bytes).collect()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        text.as_bytes().to_vec()
    }
}

fn from_native_bytes(bytes: &[u8]) -> anyhow::Result<OsString> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        anyhow::ensure!(bytes.len() % 2 == 0, "damaged index");
        let wide: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        Ok(OsString::from_wide(&wide))
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(OsStr::from_bytes(bytes).to_owned())
    }
}

fn nodes_name(nodes: &Nodes, id: usize) -> &str {
    let start = nodes.name_at[id] as usize;
    &nodes.names[start..start + nodes.name_len[id] as usize]
}

fn push_lowercase(text: &mut String, name: &str) {
    if name.is_ascii() {
        text.extend(name.bytes().map(|byte| char::from(byte.to_ascii_lowercase())));
    } else {
        for ch in name.chars() {
            text.extend(ch.to_lowercase());
        }
    }
}

fn push_component(path: &mut OsString, name: &OsStr) {
    let bytes = path.as_encoded_bytes();
    if !bytes.is_empty() && !matches!(bytes.last(), Some(b'\\' | b'/')) {
        path.push(std::path::MAIN_SEPARATOR_STR);
    }
    path.push(name);
}

fn name_hash(name: &OsStr) -> u64 {
    name.as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3))
}

impl Nodes {
    /// Root paths, by number.
    pub fn roots(&self) -> &[OsString] {
        &self.roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            is_dir,
            size: (!is_dir).then_some(size),
            modified: Some(size * 10),
            created: None,
            attributes: Some(if is_dir { 0x10 } else { 0x20 }),
        }
    }

    fn paths(index: &SearchIndex) -> Vec<PathBuf> {
        let mut ids = index.ids();
        if let Some(ranks) = index.path_ranks() {
            ids.sort_by_key(|&id| ranks[id]);
        }
        ids.iter().map(|&id| index.path(id)).collect()
    }

    fn path_list(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn entries_round_trip_in_path_order() {
        let entries = vec![
            entry("/r/b", true, 0),
            entry("/r/a b.txt", false, 3),
            entry("/r/b/c.txt", false, 5),
            entry("/r/a", true, 0),
            entry("/r/a/z", false, 1),
            entry("/other/x", false, 2),
            entry("/r/отчёт.txt", false, 7),
        ];
        let index = SearchIndex::new(entries.clone(), false);
        assert!(index.is_path_sorted());
        let mut expected: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
        expected.sort_by(|a, b| compare_paths(a, b));
        assert_eq!(paths(&index), expected);
        for original in &entries {
            let id = (0..index.len()).find(|&id| index.path(id) == original.path).unwrap();
            assert_eq!(&index.entry(id), original);
        }
        let id = (0..index.len()).find(|&id| index.name(id) == "отчёт.txt").unwrap();
        assert_eq!(index.name_lower(id), "отчёт.txt");
        let mut chain = Vec::new();
        let mut text = String::new();
        index.write_path(id, true, &mut chain, &mut text);
        assert_eq!(text, format!("/r{}отчёт.txt", std::path::MAIN_SEPARATOR));
    }

    #[test]
    fn changes_keep_children_and_paths_consistent() {
        let mut index = SearchIndex::new(
            vec![
                entry("/r/a", true, 0),
                entry("/r/a/x.txt", false, 1),
                entry("/r/a/sub", true, 0),
                entry("/r/a/sub/y.txt", false, 2),
                entry("/r/b.txt", false, 3),
            ],
            false,
        );
        let a = index.find(Path::new("/r/a")).unwrap();
        let root = index.parent(a);
        // Rename a folder: its children follow.
        index.rename(a, root, OsStr::new("renamed"));
        assert!(index.find(Path::new("/r/a/sub/y.txt")).is_none());
        let y = index.find(Path::new("/r/renamed/sub/y.txt")).unwrap();
        assert_eq!(index.size(y), Some(2));
        // Add a file in a new folder.
        let parent = index.parent_for(Path::new("/r/renamed/sub/new.txt"));
        let new = index.insert(parent, OsStr::new("new.txt"), entry("/x", false, 9).meta(), None);
        assert_eq!(index.path(new), PathBuf::from("/r/renamed/sub/new.txt"));
        // Remove a folder: everything below goes too.
        let removed = index.remove_path(Path::new("/r/renamed/sub"));
        assert_eq!(removed.len(), 3);
        assert_eq!(paths(&index), path_list(&["/r/b.txt", "/r/renamed", "/r/renamed/x.txt"]));
        assert_eq!(index.live_len(), 3);
        // Compacting keeps the same files in path order.
        let nodes = index.compact();
        let compact = SearchIndex::from_nodes(nodes, true);
        assert_eq!(paths(&compact), path_list(&["/r/b.txt", "/r/renamed", "/r/renamed/x.txt"]));
        assert_eq!(compact.len(), 3);
    }

    #[test]
    fn records_track_hard_links() {
        let mut nodes = Nodes::default();
        let root = nodes.add_root("/v");
        let folder = nodes.push_str(Parent::Root(root), "d", Meta { is_dir: true, ..Meta::default() }, Some(20));
        nodes.push_str(Parent::Node(folder), "one", Meta::default(), Some(30));
        nodes.push_str(Parent::Node(folder), "two", Meta::default(), Some(30));
        let mut index = SearchIndex::from_nodes(nodes, true);
        index.set_root_record(5, root);
        assert_eq!(index.nodes_of_record(30).len(), 2);
        assert_eq!(index.folder_of_record(20), Some(Parent::Node(folder)));
        assert_eq!(index.folder_of_record(5), Some(Parent::Root(root)));
        index.remove(&[1]);
        assert_eq!(index.nodes_of_record(30), [2]);
        index.remove(&[0]);
        assert!(index.nodes_of_record(30).is_empty());
        assert!(index.folder_of_record(20).is_none());
        assert_eq!(index.live_len(), 0);
    }
}
