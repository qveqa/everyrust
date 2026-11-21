//! Parallel directory scanner.
//!
//! The scanner walks a root folder and produces one [`Entry`] per file and
//! directory below it (the root itself is not included). Directories are
//! scheduled as they are found. Reparse points (junctions, symlinks) are
//! recorded but not descended into, which keeps the walk cycle-free on Windows.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::model::Entry;
use crate::timeutil;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Number of read errors encountered during the directory scan.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub unreadable: usize,
}

/// Scans `root` recursively.
pub fn scan(root: &Path) -> Result<Vec<Entry>> {
    scan_with_stats(root).map(|(entries, _stats)| entries)
}

/// Reads one path's metadata without following a symlink or junction.
pub fn read_entry(path: PathBuf) -> std::io::Result<Entry> {
    let metadata = fs::symlink_metadata(&path)?;
    let is_dir = metadata.is_dir() || metadata_info(&metadata) & FILE_ATTRIBUTE_DIRECTORY != 0;
    Ok(entry_with_metadata(path, Some(metadata), is_dir))
}

/// Scans `root` recursively, also reporting unreadable entries.
pub fn scan_with_stats(root: &Path) -> Result<(Vec<Entry>, ScanStats)> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("cannot read {}: no such file or directory", root.display()))?;

    if !metadata.is_dir() {
        return Ok((
            vec![entry_with_metadata(root.to_path_buf(), Some(metadata), false)],
            ScanStats::default(),
        ));
    }

    // Walk through an extended-length (`\\?\`) root so files deeper than
    // MAX_PATH are still reachable without a longPathAware manifest.
    #[cfg(windows)]
    let directory = extended_path(&std::path::absolute(root)?);
    #[cfg(not(windows))]
    let directory = root.to_path_buf();

    Ok(scan_directories(vec![directory]))
}

fn scan_directories(directories: Vec<PathBuf>) -> (Vec<Entry>, ScanStats) {
    if directories.is_empty() {
        return (Vec::new(), ScanStats::default());
    }
    // Each task writes to its own worker's buffer. This avoids copying
    // intermediate results while the directory tree is being walked.
    let workers: Vec<Mutex<ScanBatch>> = (0..rayon::current_num_threads())
        .map(|_| Mutex::new(ScanBatch::default()))
        .collect();
    let debug_scan = std::env::var_os("EVERYRUST_DEBUG_SCAN").is_some();
    rayon::scope_fifo(|scope| {
        for directory in directories {
            schedule_directory(scope, directory, &workers, debug_scan);
        }
    });

    let batches: Vec<ScanBatch> = workers
        .into_iter()
        .map(|batch| batch.into_inner().unwrap())
        .collect();
    let count = batches.iter().map(|batch| batch.entries.len()).sum();
    let mut entries = Vec::with_capacity(count);
    let mut stats = ScanStats::default();
    for mut batch in batches {
        entries.append(&mut batch.entries);
        stats.unreadable += batch.unreadable;
    }
    (entries, stats)
}

fn schedule_directory<'scope>(
    scope: &rayon::ScopeFifo<'scope>,
    directory: PathBuf,
    workers: &'scope [Mutex<ScanBatch>],
    debug_scan: bool,
) {
    scope.spawn_fifo(move |scope| {
        let worker = rayon::current_thread_index().expect("directory scan ran outside Rayon");
        let mut batch = workers[worker].lock().unwrap();
        batch.read_dir(&directory, debug_scan, |child| {
            schedule_directory(scope, child, workers, debug_scan);
        });
    });
}

#[derive(Default)]
struct ScanBatch {
    entries: Vec<Entry>,
    unreadable: usize,
}

impl ScanBatch {
    fn read_dir(
        &mut self,
        directory: &Path,
        debug_scan: bool,
        mut visit_directory: impl FnMut(PathBuf),
    ) {
        let children = match fs::read_dir(directory) {
            Ok(children) => children,
            Err(error) => {
                self.record_error(directory, &error, debug_scan);
                return;
            }
        };
        let display_directory = strip_extended(directory);
        for child in children {
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    self.record_error(directory, &error, debug_scan);
                    continue;
                }
            };
            let name = child.file_name();
            let path = display_directory.join(&name);
            // On Windows DirEntry can supply metadata from the directory
            // enumeration itself, so no second pass reopens each path.
            let metadata = match child.metadata() {
                Ok(metadata) => Some(metadata),
                Err(error) => {
                    self.record_error(&path, &error, debug_scan);
                    None
                }
            };
            let attributes = metadata.as_ref().map(metadata_info);
            let is_dir = match &metadata {
                Some(metadata) => {
                    metadata.is_dir() || attributes.unwrap_or(0) & FILE_ATTRIBUTE_DIRECTORY != 0
                }
                None => child.file_type().is_ok_and(|kind| kind.is_dir()),
            };
            if is_dir && attributes.unwrap_or(0) & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                visit_directory(directory.join(&name));
            }
            self.entries.push(entry_with_metadata(path, metadata, is_dir));
        }
    }

    fn record_error(&mut self, path: &Path, error: &std::io::Error, debug_scan: bool) {
        self.unreadable += 1;
        if debug_scan {
            eprintln!("unreadable: {}: {error}", path.display());
        }
    }
}

/// Scans every fixed local drive (e.g. `C:\`, `D:\`) and merges the results.
pub fn scan_local_drives() -> Result<(Vec<Entry>, ScanStats)> {
    let directories = local_fixed_drives()
        .into_iter()
        .map(|drive| extended_path(&drive))
        .collect();
    Ok(scan_directories(directories))
}

/// The fixed local drives as root paths (`C:\`, `D:\`, ...).
#[cfg(windows)]
pub fn local_fixed_drives() -> Vec<PathBuf> {
    const DRIVE_FIXED: u32 = 3;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root_path_name: *const u16) -> u32;
    }

    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();
    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + index as u8) as char);
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        if unsafe { GetDriveTypeW(wide.as_ptr()) } == DRIVE_FIXED {
            drives.push(PathBuf::from(root));
        }
    }
    drives
}

#[cfg(not(windows))]
pub fn local_fixed_drives() -> Vec<PathBuf> {
    Vec::new()
}

/// `C:\dir` -> `\\?\C:\dir` so the Windows API accepts paths over MAX_PATH.
fn extended_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if let Some(unc) = text.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{unc}"));
    }
    PathBuf::from(format!(r"\\?\{text}"))
}

/// `\\?\C:\dir` -> `C:\dir` for display and for shell actions.
#[cfg(windows)]
fn strip_extended(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

#[cfg(not(windows))]
fn strip_extended(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn entry_with_metadata(path: PathBuf, metadata: Option<fs::Metadata>, is_dir: bool) -> Entry {
    let metadata = metadata.as_ref();
    let size = if is_dir {
        None
    } else {
        metadata.map(fs::Metadata::len)
    };
    let modified = metadata
        .and_then(|m| m.modified().ok())
        .and_then(timeutil::system_time_to_filetime);
    let created = metadata
        .and_then(|m| m.created().ok())
        .and_then(timeutil::system_time_to_filetime);
    let attributes = metadata.map(metadata_info);
    Entry {
        path,
        is_dir,
        size,
        modified,
        created,
        attributes,
    }
}

fn metadata_info(metadata: &fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_a_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("sub").join("b.md"), "bb").unwrap();
        std::fs::write(root.join("sub").join("deep").join("c.txt"), "ccc").unwrap();

        let entries = scan(root).unwrap();
        let names: Vec<String> = entries.iter().map(|e| e.name_string()).collect();
        assert_eq!(entries.len(), 5, "entries: {names:?}");
        assert!(names.contains(&"sub".to_string()));
        assert!(names.contains(&"deep".to_string()));
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.md".to_string()));
        assert!(names.contains(&"c.txt".to_string()));

        let a = entries.iter().find(|e| e.name_string() == "a.txt").unwrap();
        assert_eq!(a.size, Some(1));
        assert!(!a.is_dir);
        assert!(a.modified.is_some());

        let deep = entries.iter().find(|e| e.name_string() == "deep").unwrap();
        assert!(deep.is_dir);
        assert_eq!(deep.size, None);
    }

    #[test]
    fn scans_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("only.txt");
        std::fs::write(&file, "x").unwrap();
        let entries = scan(&file).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, file);
    }

    #[test]
    fn missing_root_is_an_error() {
        let err = scan(Path::new("C:\\definitely\\not\\here\\12345")).unwrap_err();
        assert!(err.to_string().contains("no such file"));
    }
}
