use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use everyrust::model::Entry;
use everyrust::scan;
use everyrust::search::{self, SearchIndex, SortKey};

const BATCH_WINDOW: Duration = Duration::from_millis(220);

pub enum Update {
    Paths(HashMap<PathBuf, Option<Entry>>),
    Rescan,
    Warning(String),
}

pub struct Watch {
    _watcher: RecommendedWatcher,
    updates: Receiver<Update>,
}

impl Watch {
    pub fn start(roots: &[PathBuf], ctx: egui::Context) -> (Option<Self>, Option<String>) {
        let (events, incoming) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(events) {
            Ok(watcher) => watcher,
            Err(error) => return (None, Some(format!("Live updates unavailable: {error}"))),
        };
        let mut watched = Vec::new();
        let mut failures = 0;
        for root in roots {
            match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(()) => watched.push(root.clone()),
                Err(_) => failures += 1,
            }
        }
        let warning = (failures > 0).then(|| format!("Live updates unavailable for {failures} source(s)"));
        if watched.is_empty() {
            return (None, warning);
        }
        let (outgoing, updates) = mpsc::sync_channel(2);
        std::thread::spawn(move || {
            while let Ok(first) = incoming.recv() {
                let mut paths = HashMap::new();
                let mut rescan = false;
                let mut warning = None;
                let deadline = Instant::now() + BATCH_WINDOW;
                let mut next = first;
                loop {
                    match next {
                        Ok(event) => rescan |= collect_paths(event, &watched, &mut paths),
                        Err(error) => warning = Some(format!("Live update error: {error}")),
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match incoming.recv_timeout(remaining) {
                        Ok(event) => next = event,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                let update = if rescan {
                    Some(Update::Rescan)
                } else if !paths.is_empty() {
                    Some(Update::Paths(resolve(paths)))
                } else {
                    warning.map(Update::Warning)
                };
                if let Some(update) = update {
                    if outgoing.send(update).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        });
        (Some(Self { _watcher: watcher, updates }), warning)
    }

    pub fn poll(&self) -> Result<Update, TryRecvError> {
        self.updates.try_recv()
    }
}

fn collect_paths(event: Event, roots: &[PathBuf], paths: &mut HashMap<PathBuf, bool>) -> bool {
    if event.need_rescan() {
        return true;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let structural = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    for path in event.paths {
        if roots.iter().any(|root| path != *root && path.starts_with(root)) {
            *paths.entry(path).or_default() |= structural;
        }
    }
    false
}

fn resolve(paths: HashMap<PathBuf, bool>) -> HashMap<PathBuf, Option<Entry>> {
    let mut resolved = HashMap::with_capacity(paths.len());
    let mut subtrees = Vec::new();
    for (path, structural) in paths {
        match scan::read_entry(path.clone()) {
            Ok(entry) => {
                if entry.is_dir {
                    if !structural {
                        continue;
                    }
                    if entry.attributes.unwrap_or(0) & 0x400 == 0 {
                        subtrees.push(path.clone());
                    }
                }
                resolved.insert(path, Some(entry));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                resolved.insert(path, None);
            }
            Err(_) => {}
        }
    }
    subtrees.sort_unstable();
    let mut last_root: Option<PathBuf> = None;
    for root in subtrees {
        if last_root.as_ref().is_some_and(|parent| root.starts_with(parent)) {
            continue;
        }
        if let Ok(entries) = scan::scan(&root) {
            for entry in entries {
                resolved.entry(entry.path.clone()).or_insert(Some(entry));
            }
        }
        last_root = Some(root);
    }
    resolved
}

pub fn find(index: &SearchIndex, order: &[usize], path: &Path) -> Option<usize> {
    order
        .binary_search_by(|&id| search::compare_paths(&index.entry(id).path, path))
        .ok()
        .map(|row| order[row])
}

/// Applies a batch using the path order for lookups, then reinserts just the changed slots.
pub fn apply(
    index: &mut SearchIndex,
    path_order: &mut Vec<usize>,
    result_order: &mut Vec<usize>,
    sort: (SortKey, bool),
    changes: HashMap<PathBuf, Option<Entry>>,
) -> bool {
    let mut replacements = HashMap::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();
    for (path, entry) in changes {
        match entry {
            Some(entry) => {
                if let Some(id) = find(index, path_order, &path) {
                    if *index.entry(id) != entry {
                        replacements.insert(id, entry);
                    }
                } else {
                    added.push(entry);
                }
            }
            None => {
                let start = path_order.partition_point(|&id| {
                    search::compare_paths(&index.entry(id).path, &path) == std::cmp::Ordering::Less
                });
                removed.extend(
                    path_order[start..].iter().copied()
                        .take_while(|&id| index.entry(id).path.starts_with(&path)),
                );
            }
        }
    }
    if replacements.is_empty() && added.is_empty() && removed.is_empty() {
        return false;
    }

    let mut invalid = vec![false; index.len()];
    let mut updated = Vec::with_capacity(replacements.len() + added.len() + removed.len());
    for (id, entry) in replacements {
        index.replace_entry(id, entry);
        invalid[id] = true;
        updated.push(id);
    }
    removed.sort_unstable();
    removed.dedup();
    // Removing in descending slot order keeps the original deletion slots valid.
    // Record which original entry moved into any surviving hole.
    let mut moved = HashMap::new();
    for id in removed.into_iter().rev() {
        invalid[id] = true;
        let tail = index.len() - 1;
        if id != tail {
            let original = moved.remove(&tail).unwrap_or(tail);
            invalid[original] = true;
            moved.insert(id, original);
        }
        index.swap_remove_entry(id);
    }
    updated.retain(|&id| id < index.len());
    updated.extend(moved.into_keys());
    updated.extend(index.len()..index.len() + added.len());
    for entry in added {
        index.push_entry(entry);
    }
    updated.sort_unstable();
    updated.dedup();
    path_order.retain(|&id| !invalid[id]);
    result_order.retain(|&id| !invalid[id]);
    search::insert_sorted_indices(index, path_order, &updated, SortKey::Path, false);
    search::insert_sorted_indices(index, result_order, &updated, sort.0, sort.1);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use everyrust::search::{Query, SearchOptions};

    fn file(path: &str, size: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            is_dir: false,
            size: Some(size),
            modified: None,
            created: None,
            attributes: None,
        }
    }

    #[test]
    fn batch_preserves_order_for_files_of_all_sizes() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let mut index = SearchIndex::new(
            vec![
                file("sub/a.txt", 2),
                file("sub/b.txt", 3),
                file("outside.txt", GIB),
                file("sub/large.txt", GIB),
                file("sub/c.txt", 5),
                file("keep.txt", 9),
            ],
            true,
        );
        let mut paths: Vec<usize> = (0..index.len()).collect();
        search::sort_hits(&index, &mut paths, SortKey::Path, false);
        let mut order: Vec<usize> = (0..index.len()).collect();
        search::sort_hits(&index, &mut order, SortKey::Size, true);
        let options = SearchOptions { match_path: true, ..Default::default() };
        let query = Query::parse("new", options).unwrap();
        assert!(search::filter_ordered_indices(&index, &query, &order).is_empty());

        let changes = HashMap::from([
            (PathBuf::from("sub"), None),
            (PathBuf::from("outside.txt"), Some(file("outside.txt", 2 * GIB))),
            (PathBuf::from("new.txt"), Some(file("new.txt", 17))),
            (PathBuf::from("new-large.txt"), Some(file("new-large.txt", GIB))),
        ]);
        assert!(apply(&mut index, &mut paths, &mut order, (SortKey::Size, true), changes));
        assert_eq!(index.len(), 4);
        assert_eq!(paths.iter().map(|&id| index.entry(id).name_string()).collect::<Vec<_>>(),
            ["keep.txt", "new-large.txt", "new.txt", "outside.txt"]);
        assert_eq!(order.iter().map(|&id| index.entry(id).name_string()).collect::<Vec<_>>(),
            ["outside.txt", "new-large.txt", "new.txt", "keep.txt"]);
        assert_eq!(index.entry(order[0]).size, Some(2 * GIB));
        let hits = search::filter_ordered_indices(&index, &query, &order);
        assert_eq!(hits.iter().map(|&id| index.entry(id).name_string()).collect::<Vec<_>>(),
            ["new-large.txt", "new.txt"]);
    }
}
