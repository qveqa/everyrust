//! The NTFS change journal (USN journal).
//!
//! NTFS writes one journal record for every change to a file: created,
//! deleted, renamed, written. Everything uses it to keep its index current
//! without scanning. EveryRust does the same: after an MFT scan it remembers
//! the journal position, and later it reads only the records after that
//! position and applies them to the index (see [`apply`]).
//!
//! Records name files by file reference number (FRN). The low 48 bits are the
//! MFT record number, which is how index nodes from an MFT scan are found.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

use crate::index::{Meta, Parent, SearchIndex};

pub const REASON_FILE_DELETE: u32 = 0x200;
pub const REASON_RENAME_OLD_NAME: u32 = 0x1000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// A position in a volume's journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalState {
    /// Changes when the journal is deleted and created again.
    pub id: u64,
    /// The first record not read yet.
    pub next_usn: i64,
}

/// One journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub frn: u64,
    pub parent_frn: u64,
    pub reason: u32,
    pub attributes: u32,
    pub name: OsString,
}

/// The state of a file after a run of journal records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The file no longer exists.
    Gone,
    /// The file exists; `links` is its number of hard links.
    Present { meta: Meta, links: u32 },
    /// The file exists, but its metadata could not be read.
    Unknown,
}

/// What happened to one file, from all its records in a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub frn: u64,
    /// Parent folder and name after the changes.
    pub parent_frn: u64,
    pub name: OsString,
    pub attributes: u32,
    pub state: State,
}

/// Reads USN_RECORD_V2 records from the output of `FSCTL_READ_USN_JOURNAL`,
/// after its leading 8-byte next-USN value.
pub fn parse_records(bytes: &[u8]) -> Vec<Record> {
    let mut records = Vec::new();
    let mut offset = 0;
    while let Some(length) = le_u32(bytes, offset) {
        let length = length as usize;
        if length < 60 || offset + length > bytes.len() {
            break;
        }
        let record = &bytes[offset..offset + length];
        offset += length;
        if le_u16(record, 4) != Some(2) {
            continue;
        }
        let (Some(frn), Some(parent_frn), Some(reason), Some(attributes), Some(name_length), Some(name_offset)) = (
            le_u64(record, 8),
            le_u64(record, 16),
            le_u32(record, 40),
            le_u32(record, 52),
            le_u16(record, 56),
            le_u16(record, 58),
        ) else {
            continue;
        };
        let (start, length) = (usize::from(name_offset), usize::from(name_length));
        let Some(name) = record.get(start..start + length) else { continue };
        let wide: Vec<u16> = name.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        records.push(Record {
            frn,
            parent_frn,
            reason,
            attributes,
            name: wide_to_os(&wide),
        });
    }
    records
}

/// Merges the records of each file, in the order files first appear. The
/// state is [`State::Gone`] when the last record deletes the file, else
/// [`State::Unknown`] until the caller reads the metadata.
pub fn collapse(records: Vec<Record>) -> Vec<Change> {
    let mut changes: Vec<Change> = Vec::new();
    let mut positions: HashMap<u64, usize> = HashMap::new();
    for record in records {
        let position = *positions.entry(record.frn).or_insert_with(|| {
            changes.push(Change {
                frn: record.frn,
                parent_frn: record.parent_frn,
                name: record.name.clone(),
                attributes: record.attributes,
                state: State::Unknown,
            });
            changes.len() - 1
        });
        let change = &mut changes[position];
        // The old name of a rename is not where the file is now.
        if record.reason & REASON_RENAME_OLD_NAME == 0 {
            change.parent_frn = record.parent_frn;
            change.name = record.name;
            change.attributes = record.attributes;
        }
        change.state = if record.reason & REASON_FILE_DELETE != 0 { State::Gone } else { State::Unknown };
    }
    changes
}

/// Index ids that a run of changes touched.
#[derive(Debug, Default)]
pub struct Applied {
    /// Added nodes and nodes whose name, folder, or metadata changed.
    pub updated: Vec<usize>,
    pub removed: Vec<usize>,
    /// Whether a folder moved or got a new name, which changes the paths below it.
    pub folders_moved: bool,
}

impl Applied {
    pub fn is_empty(&self) -> bool {
        self.updated.is_empty() && self.removed.is_empty()
    }
}

/// Applies changes to an index built from the MFT. `exists` tells whether a
/// path is on the disk; it decides which hard links of a file still exist.
pub fn apply(index: &mut SearchIndex, changes: Vec<Change>, exists: impl Fn(&Path) -> bool) -> Applied {
    let mut applied = Applied::default();
    index.start_batch();
    let mut pending = changes;
    // A file can appear before the folder it moved into; retry those.
    loop {
        let before = pending.len();
        let mut deferred = Vec::new();
        for change in pending {
            if !apply_one(index, &change, &exists, &mut applied) {
                deferred.push(change);
            }
        }
        if deferred.is_empty() {
            break;
        }
        if deferred.len() == before {
            // The folder is outside the index, for example in `$Extend`.
            for change in deferred {
                let nodes = index.nodes_of_record(record_of(change.frn));
                applied.removed.extend(index.remove(&nodes));
            }
            break;
        }
        pending = deferred;
    }
    applied.removed.extend(index.finish_batch());
    applied.updated.sort_unstable();
    applied.updated.dedup();
    applied.updated.retain(|&id| index.is_live(id));
    applied.removed.sort_unstable();
    applied.removed.dedup();
    applied
}

/// Applies one change. False when its folder is not in the index yet.
fn apply_one(index: &mut SearchIndex, change: &Change, exists: &impl Fn(&Path) -> bool, applied: &mut Applied) -> bool {
    let record = record_of(change.frn);
    // Changes to the volume root itself (its attributes, for example) do not
    // add a node.
    if index.folder_of_record(record).is_some_and(|parent| matches!(parent, Parent::Root(_))) {
        return true;
    }
    let existing = index.nodes_of_record(record);
    let (meta, links) = match &change.state {
        State::Gone => {
            applied.removed.extend(index.remove(&existing));
            return true;
        }
        State::Present { meta, links } => (Some(*meta), *links),
        State::Unknown => (None, 1),
    };
    let Some(parent) = index.folder_of_record(record_of(change.parent_frn)) else {
        return false;
    };
    let is_dir = meta.map_or(change.attributes & FILE_ATTRIBUTE_DIRECTORY != 0, |meta| meta.is_dir);
    let new_meta = meta.unwrap_or(Meta {
        is_dir,
        attributes: Some(change.attributes),
        ..Meta::default()
    });
    let name = change.name.as_os_str();

    if !is_dir && (links > 1 || existing.len() > 1) {
        // Hard links. A record can name a link that was just removed, so
        // keep only the names that are still on the disk. A journal run
        // names one link per file; a second link added in the same run
        // appears with the next rescan.
        let mut have_name = false;
        for &id in &existing {
            if !exists(&index.path(id)) {
                applied.removed.extend(index.remove(&[id]));
                continue;
            }
            have_name |= index.parent(id) == parent && index.name_os(id) == name;
            if meta.is_some() && index.set_meta(id, new_meta) {
                applied.updated.push(id);
            }
        }
        if !have_name && exists(&index.parent_path(parent).join(name)) {
            applied.updated.push(index.insert(parent, name, new_meta, Some(record)));
        }
        return true;
    }

    match existing.split_first() {
        Some((&id, extra)) if index.is_dir(id) == is_dir => {
            // Moving a folder into a folder that is still inside it would
            // make a loop; a later change moves that folder out first.
            if is_dir && index.is_inside(parent, id) {
                return false;
            }
            if index.rename(id, parent, name) {
                applied.updated.push(id);
                applied.folders_moved |= is_dir;
            }
            if meta.is_some() && index.set_meta(id, new_meta) {
                applied.updated.push(id);
            }
            applied.removed.extend(index.remove(extra));
        }
        _ => {
            applied.removed.extend(index.remove(&existing));
            applied.updated.push(index.insert(parent, name, new_meta, Some(record)));
        }
    }
    true
}

pub fn record_of(frn: u64) -> u32 {
    (frn & RECORD_MASK) as u32
}

fn wide_to_os(name: &[u16]) -> OsString {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(name)
    }
    #[cfg(not(windows))]
    OsString::from(String::from_utf16_lossy(name))
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

fn le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}

#[cfg(windows)]
pub use windows::Journal;

#[cfg(windows)]
mod windows {
    use std::fs::File;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use anyhow::{bail, Context, Result};
    use rayon::prelude::*;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, OpenFileById, BY_HANDLE_FILE_INFORMATION, FILE_ID_DESCRIPTOR,
        FILE_ID_DESCRIPTOR_0, FileIdType,
    };
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::*;

    const SHARE_ALL: u32 = 0x7;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;

    /// The journal of one NTFS volume.
    pub struct Journal {
        volume: File,
        /// Any handle on the volume, for `OpenFileById`.
        root: File,
    }

    impl Journal {
        /// Opens the journal of drive `letter`. Needs administrator privileges.
        pub fn open(letter: char) -> Result<Self> {
            let volume = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(SHARE_ALL)
                .open(format!(r"\\.\{letter}:"))
                .context("cannot open the volume")?;
            let root = std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(SHARE_ALL)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(format!(r"{letter}:\"))
                .context("cannot open the drive root")?;
            Ok(Self { volume, root })
        }

        /// The current end of the journal.
        pub fn state(&self) -> Result<JournalState> {
            let data = self.query()?;
            Ok(JournalState {
                id: data.UsnJournalID,
                next_usn: data.NextUsn,
            })
        }

        fn query(&self) -> Result<USN_JOURNAL_DATA_V0> {
            let mut data = USN_JOURNAL_DATA_V0::default();
            let mut returned = 0;
            let ok = unsafe {
                DeviceIoControl(
                    self.volume.as_raw_handle() as _,
                    FSCTL_QUERY_USN_JOURNAL,
                    std::ptr::null(),
                    0,
                    (&raw mut data).cast(),
                    std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error()).context("the change journal is not available");
            }
            Ok(data)
        }

        /// Reads the changes after `from` and the metadata of every changed
        /// file. Fails when the journal no longer holds them; then the
        /// volume must be scanned again.
        pub fn read(&self, from: JournalState) -> Result<(Vec<Change>, JournalState)> {
            let data = self.query()?;
            if data.UsnJournalID != from.id {
                bail!("the change journal was replaced");
            }
            if from.next_usn < data.FirstUsn {
                bail!("the change journal no longer holds the changes");
            }
            let mut records = Vec::new();
            let mut next = from.next_usn;
            let mut buffer = vec![0u8; 1 << 20];
            while next < data.NextUsn {
                let input = READ_USN_JOURNAL_DATA_V0 {
                    StartUsn: next,
                    ReasonMask: u32::MAX,
                    ReturnOnlyOnClose: 0,
                    Timeout: 0,
                    BytesToWaitFor: 0,
                    UsnJournalID: from.id,
                };
                let mut returned = 0;
                let ok = unsafe {
                    DeviceIoControl(
                        self.volume.as_raw_handle() as _,
                        FSCTL_READ_USN_JOURNAL,
                        (&raw const input).cast(),
                        std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut returned,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(std::io::Error::last_os_error()).context("cannot read the change journal");
                }
                let returned = returned as usize;
                if returned < 8 {
                    break;
                }
                let following = i64::from_le_bytes(buffer[..8].try_into().expect("8 bytes"));
                records.extend(parse_records(&buffer[8..returned]));
                if returned == 8 || following <= next {
                    break;
                }
                next = following;
            }
            let mut changes = collapse(records);
            changes.par_iter_mut().for_each(|change| {
                if change.state != State::Gone {
                    change.state = self.metadata(change.frn);
                }
            });
            Ok((changes, JournalState { id: from.id, next_usn: next }))
        }

        fn metadata(&self, frn: u64) -> State {
            let descriptor = FILE_ID_DESCRIPTOR {
                dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
                Type: FileIdType,
                Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
            };
            let handle = unsafe {
                OpenFileById(
                    self.root.as_raw_handle() as _,
                    &descriptor,
                    FILE_READ_ATTRIBUTES,
                    SHARE_ALL,
                    std::ptr::null(),
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                )
            };
            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                let error = std::io::Error::last_os_error();
                // Not found or a stale sequence number: the file is gone.
                return match error.raw_os_error() {
                    Some(2 | 3 | 87 | 1168) => State::Gone,
                    _ => State::Unknown,
                };
            }
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
            unsafe { CloseHandle(handle) };
            if ok == 0 {
                return State::Unknown;
            }
            let filetime = |time: windows_sys::Win32::Foundation::FILETIME| {
                u64::from(time.dwHighDateTime) << 32 | u64::from(time.dwLowDateTime)
            };
            let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            State::Present {
                meta: Meta {
                    is_dir,
                    size: (!is_dir).then(|| u64::from(info.nFileSizeHigh) << 32 | u64::from(info.nFileSizeLow)),
                    modified: Some(filetime(info.ftLastWriteTime)),
                    created: Some(filetime(info.ftCreationTime)),
                    attributes: Some(info.dwFileAttributes),
                },
                links: info.nNumberOfLinks,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Nodes;

    fn record(frn: u64, parent_frn: u64, reason: u32, name: &str) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let length = (60 + wide.len()).div_ceil(8) * 8;
        let mut bytes = vec![0u8; length];
        bytes[0..4].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        bytes[8..16].copy_from_slice(&frn.to_le_bytes());
        bytes[16..24].copy_from_slice(&parent_frn.to_le_bytes());
        bytes[40..44].copy_from_slice(&reason.to_le_bytes());
        bytes[52..56].copy_from_slice(&0x20u32.to_le_bytes());
        bytes[56..58].copy_from_slice(&(wide.len() as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&60u16.to_le_bytes());
        bytes[60..60 + wide.len()].copy_from_slice(&wide);
        bytes
    }

    #[test]
    fn records_parse_and_collapse_per_file() {
        let mut bytes = Vec::new();
        bytes.extend(record(100, 5, 0x100, "new.txt"));
        bytes.extend(record(100, 5, REASON_RENAME_OLD_NAME, "new.txt"));
        bytes.extend(record(100, 7, 0x2000, "отчёт.txt"));
        bytes.extend(record(200, 5, 0x2, "data.bin"));
        bytes.extend(record(300, 5, REASON_FILE_DELETE | 0x8000_0000, "gone.txt"));
        bytes.extend([0u8; 3]);
        let records = parse_records(&bytes);
        assert_eq!(records.len(), 5);
        assert_eq!(records[2].name, OsString::from("отчёт.txt"));
        let changes = collapse(records);
        assert_eq!(changes.len(), 3);
        assert_eq!((changes[0].parent_frn, changes[0].name.to_str().unwrap()), (7, "отчёт.txt"));
        assert_eq!(changes[0].state, State::Unknown);
        assert_eq!(changes[2].state, State::Gone);
        assert!(parse_records(&[1, 2, 3]).is_empty());
    }

    fn file(size: u64) -> Meta {
        Meta {
            is_dir: false,
            size: Some(size),
            modified: Some(size),
            created: None,
            attributes: Some(0x20),
        }
    }

    fn folder() -> Meta {
        Meta {
            is_dir: true,
            attributes: Some(0x10),
            ..Meta::default()
        }
    }

    #[test]
    fn changes_follow_moves_and_deletes() {
        let mut nodes = Nodes::default();
        let root = nodes.add_root("/v");
        nodes.set_root_record(5, root);
        let a = nodes.push_str(Parent::Root(root), "a", folder(), Some(20));
        nodes.push_str(Parent::Node(a), "x.txt", file(1), Some(21));
        nodes.push_str(Parent::Root(root), "b.txt", file(2), Some(22));
        let mut index = SearchIndex::from_nodes(nodes, true);
        let present = |meta: Meta| State::Present { meta, links: 1 };
        let changes = vec![
            // x.txt moves into a folder that is created later in the run.
            Change { frn: 21, parent_frn: 30, name: "x.txt".into(), attributes: 0x20, state: present(file(5)) },
            Change { frn: 30, parent_frn: 20, name: "new".into(), attributes: 0x10, state: present(folder()) },
            // Folder a gets a new name.
            Change { frn: 20, parent_frn: 5, name: "renamed".into(), attributes: 0x10, state: present(folder()) },
            Change { frn: 22, parent_frn: 5, name: "b.txt".into(), attributes: 0x20, state: State::Gone },
            // A file in a folder outside the index is skipped.
            Change { frn: 40, parent_frn: 11, name: "$Secure".into(), attributes: 0x20, state: present(file(1)) },
        ];
        let applied = apply(&mut index, changes, |_| true);
        assert!(applied.folders_moved);
        let mut paths: Vec<String> = index.ids().iter().map(|&id| index.path(id).to_string_lossy().into_owned()).collect();
        paths.sort();
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            paths,
            [
                format!("/v{separator}renamed"),
                format!("/v{separator}renamed{separator}new"),
                format!("/v{separator}renamed{separator}new{separator}x.txt"),
            ]
        );
        let x = index.nodes_of_record(21)[0];
        assert_eq!(index.size(x), Some(5));
    }

    fn volume(build: impl FnOnce(&mut Nodes, u32)) -> SearchIndex {
        let mut nodes = Nodes::default();
        let root = nodes.add_root("/v");
        nodes.set_root_record(5, root);
        build(&mut nodes, root);
        SearchIndex::from_nodes(nodes, true)
    }

    fn listing(index: &SearchIndex) -> Vec<std::path::PathBuf> {
        let mut paths: Vec<_> = index.ids().iter().map(|&id| index.path(id)).collect();
        paths.sort();
        paths
    }

    fn change(frn: u64, parent_frn: u64, name: &str, state: State) -> Change {
        Change { frn, parent_frn, name: name.into(), attributes: 0, state }
    }

    #[test]
    fn crossing_folder_moves_do_not_make_a_loop() {
        // /v/a/b/c, then: a -> b, b -> root (applied in the other order).
        let mut index = volume(|nodes, root| {
            let a = nodes.push_str(Parent::Root(root), "a", folder(), Some(20));
            let b = nodes.push_str(Parent::Node(a), "b", folder(), Some(21));
            nodes.push_str(Parent::Node(b), "c", folder(), Some(22));
            nodes.push_str(Parent::Root(root), "gone", folder(), Some(23));
        });
        let present = State::Present { meta: folder(), links: 1 };
        let changes = vec![
            change(20, 21, "a", present.clone()),
            change(23, 5, "gone", State::Gone),
            change(21, 5, "b", present),
        ];
        let applied = apply(&mut index, changes, |_| true);
        assert!(applied.folders_moved);
        let p = |text: &str| Path::new("/v").join(text);
        assert_eq!(listing(&index), [p("b"), p("b/a"), p("b/c")]);
    }

    #[test]
    fn removed_hard_links_disappear() {
        let mut index = volume(|nodes, root| {
            nodes.push_str(Parent::Root(root), "keep.txt", file(1), Some(30));
            nodes.push_str(Parent::Root(root), "gone.txt", file(1), Some(30));
            nodes.push_str(Parent::Root(root), "x1", file(2), Some(31));
            nodes.push_str(Parent::Root(root), "x2", file(2), Some(31));
            nodes.push_str(Parent::Root(root), "x3", file(2), Some(31));
        });
        let on_disk = [Path::new("/v").join("keep.txt"), Path::new("/v").join("x1"), Path::new("/v").join("x3")];
        // The journal names the removed link; the file itself still exists.
        let changes = vec![
            change(30, 5, "gone.txt", State::Present { meta: file(1), links: 1 }),
            change(31, 5, "x2", State::Present { meta: file(2), links: 2 }),
            // The volume root changed; it is not a node.
            change(5, 5, ".", State::Present { meta: folder(), links: 1 }),
        ];
        apply(&mut index, changes, |path| on_disk.iter().any(|known| known == path));
        let mut expected = on_disk.to_vec();
        expected.sort();
        assert_eq!(listing(&index), expected);
    }

    /// Applies the difference between two MFT snapshots as journal changes
    /// and checks that the result equals the second snapshot.
    ///
    /// Set `EVERYRUST_USN_BEFORE` and `EVERYRUST_USN_AFTER` to two NTFS images
    /// of the same volume, then run with `--ignored`.
    #[test]
    #[ignore = "needs two NTFS images"]
    fn snapshot_difference_applies_cleanly() {
        use std::collections::HashSet;
        use std::path::PathBuf;

        let load = |name: &str| {
            let image = PathBuf::from(std::env::var_os(name).unwrap());
            let (nodes, _) = crate::mft::read_volume(|| std::fs::File::open(&image), 4, 1 << 20, Path::new("/v")).unwrap();
            SearchIndex::from_nodes(nodes, true)
        };
        let mut before = load("EVERYRUST_USN_BEFORE");
        let after = load("EVERYRUST_USN_AFTER");
        let parent_record = |index: &SearchIndex, id: usize| match index.parent(id) {
            Parent::Root(_) => 5,
            Parent::Node(parent) => u64::from(index.record(parent as usize).unwrap()),
        };
        let mut records: Vec<u32> = Vec::new();
        records.extend(before.ids().iter().map(|&id| before.record(id).unwrap()));
        records.extend(after.ids().iter().map(|&id| after.record(id).unwrap()));
        records.sort_unstable();
        records.dedup();
        let mut changes = Vec::new();
        for record in records {
            let old = before.nodes_of_record(record);
            let new = after.nodes_of_record(record);
            if new.is_empty() {
                changes.push(Change { frn: record.into(), parent_frn: 5, name: "x".into(), attributes: 0, state: State::Gone });
                continue;
            }
            let describe = |index: &SearchIndex, ids: &[usize]| {
                let mut list: Vec<_> = ids.iter().map(|&id| (parent_record(index, id), index.name_os(id).to_owned(), index.meta(id))).collect();
                list.sort();
                list
            };
            if describe(&before, &old) == describe(&after, &new) {
                continue;
            }
            for &id in &new {
                changes.push(Change {
                    frn: record.into(),
                    parent_frn: parent_record(&after, id),
                    name: after.name_os(id).to_owned(),
                    attributes: after.attributes(id).unwrap_or(0),
                    state: State::Present { meta: after.meta(id), links: new.len() as u32 },
                });
            }
        }
        // Shuffle so files often come before the folders they moved into.
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        for i in (1..changes.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            changes.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        let count = changes.len();
        let paths: HashSet<PathBuf> = after.ids().iter().map(|&id| after.path(id)).collect();
        let applied = apply(&mut before, changes, |path| paths.contains(path));
        eprintln!("{count} changes, {} updated, {} removed", applied.updated.len(), applied.removed.len());

        let list = |index: &SearchIndex| -> HashSet<(PathBuf, Meta)> {
            index.ids().iter().map(|&id| (index.path(id), index.meta(id))).collect()
        };
        let (got, want) = (list(&before), list(&after));
        let only_got: Vec<_> = got.difference(&want).take(10).collect();
        let only_want: Vec<_> = want.difference(&got).take(10).collect();
        assert!(only_got.is_empty() && only_want.is_empty(), "only applied: {only_got:#?}\nonly expected: {only_want:#?}");
        assert_eq!(before.live_len(), after.live_len());
    }
}
