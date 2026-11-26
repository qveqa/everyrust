use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use everyrust::index::SearchIndex;
use everyrust::model::Entry;
use everyrust::scan;
use everyrust::search::{self, SortKey};
use everyrust::usn::{Applied, Change, JournalState};

const BATCH_WINDOW: Duration = Duration::from_millis(220);
/// How often the change journal is read.
#[cfg(windows)]
const JOURNAL_INTERVAL: Duration = Duration::from_millis(500);

pub enum Update {
    Paths(HashMap<PathBuf, Option<Entry>>),
    /// Journal changes and the journal position after them.
    Journal(Vec<Change>, JournalState),
    Rescan,
    Warning(String),
}

/// Live updates for one index: file system notifications for folders, or
/// the change journal for NTFS drives indexed from the MFT.
pub struct Watch {
    _watcher: Option<RecommendedWatcher>,
    updates: Receiver<Update>,
    stop: Arc<AtomicBool>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Watch {
    /// Reads the change journal of a drive every half second.
    #[cfg(windows)]
    pub fn journal(journal: everyrust::usn::Journal, mut state: JournalState, ctx: egui::Context) -> Self {
        let (outgoing, updates) = mpsc::sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                std::thread::sleep(JOURNAL_INTERVAL);
                match journal.read(state) {
                    Ok((changes, next)) => {
                        state = next;
                        if changes.is_empty() {
                            continue;
                        }
                        if outgoing.send(Update::Journal(changes, next)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = outgoing.send(Update::Rescan);
                        ctx.request_repaint();
                        return;
                    }
                }
                ctx.request_repaint();
            }
        });
        Self {
            _watcher: None,
            updates,
            stop,
        }
    }

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
        let stop = Arc::new(AtomicBool::new(false));
        (Some(Self { _watcher: Some(watcher), updates, stop }), warning)
    }

    /// Whether updates come from the change journal.
    pub fn is_journal(&self) -> bool {
        self._watcher.is_none()
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

/// Applies resolved path changes from file system notifications.
pub fn apply_paths(index: &mut SearchIndex, changes: HashMap<PathBuf, Option<Entry>>) -> Applied {
    let mut applied = Applied::default();
    let mut changes: Vec<(PathBuf, Option<Entry>)> = changes.into_iter().collect();
    // Removals first, then parents before their children.
    changes.sort_by_key(|(path, entry)| (entry.is_some(), path.components().count()));
    for (path, entry) in changes {
        match entry {
            None => applied.removed.extend(index.remove_path(&path)),
            Some(entry) => match index.find(&path) {
                Some(id) if index.is_dir(id) == entry.is_dir => {
                    if index.set_meta(id, entry.meta()) {
                        applied.updated.push(id);
                    }
                }
                found => {
                    if let Some(id) = found {
                        applied.removed.extend(index.remove(&[id]));
                    }
                    let Some(name) = path.file_name() else { continue };
                    let parent = index.parent_for(&path);
                    applied.updated.push(index.insert(parent, name, entry.meta(), None));
                }
            },
        }
    }
    applied.updated.retain(|&id| index.is_live(id));
    applied
}

/// Brings a sorted result order up to date after changes.
pub fn update_order(index: &SearchIndex, order: &mut Vec<usize>, sort: (SortKey, bool), applied: &Applied) {
    if sort.0 == SortKey::Path && applied.folders_moved {
        // Paths below a moved folder changed; sort again.
        *order = index.ids();
        search::sort_hits(index, order, sort.0, sort.1);
        return;
    }
    let mut touched = vec![false; index.len()];
    for &id in applied.removed.iter().chain(&applied.updated) {
        touched[id] = true;
    }
    order.retain(|&id| !touched[id]);
    search::insert_sorted_indices(index, order, &applied.updated, sort.0, sort.1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
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
        let sort = (SortKey::Size, true);
        let mut order = index.ids();
        search::sort_hits(&index, &mut order, sort.0, sort.1);
        let options = SearchOptions { match_path: true, ..Default::default() };
        let query = Query::parse("new", options).unwrap();
        assert!(search::filter_ordered_indices(&index, &query, &order).is_empty());

        let changes = HashMap::from([
            (PathBuf::from("sub"), None),
            (PathBuf::from("outside.txt"), Some(file("outside.txt", 2 * GIB))),
            (PathBuf::from("new.txt"), Some(file("new.txt", 17))),
            (PathBuf::from("new-large.txt"), Some(file("new-large.txt", GIB))),
        ]);
        let applied = apply_paths(&mut index, changes);
        assert!(!applied.is_empty());
        update_order(&index, &mut order, sort, &applied);
        assert_eq!(index.live_len(), 4);
        let outside = index.find(Path::new("outside.txt")).unwrap();
        let names = |ids: &[usize]| ids.iter().map(|&id| index.name(id).to_owned()).collect::<Vec<_>>();
        assert_eq!(names(&order), ["outside.txt", "new-large.txt", "new.txt", "keep.txt"]);
        assert_eq!(index.size(outside), Some(2 * GIB));
        let hits = search::filter_ordered_indices(&index, &query, &order);
        assert_eq!(names(&hits), ["new-large.txt", "new.txt"]);

        let mut paths = index.ids();
        search::sort_hits(&index, &mut paths, SortKey::Path, false);
        assert_eq!(names(&paths), ["keep.txt", "new-large.txt", "new.txt", "outside.txt"]);
    }
}
