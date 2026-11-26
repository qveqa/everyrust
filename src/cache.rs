//! Saves an index to a file and loads it back, so the app can show the last
//! index at once and then bring it up to date from the change journal.
//!
//! The file holds a short header (the indexed root and the journal position)
//! followed by the index nodes as flat arrays (see [`SearchIndex::encode`]).

use std::io::Write;
use std::path::Path;

use anyhow::{ensure, Context, Result};

use crate::index::{Nodes, SearchIndex};
use crate::usn::JournalState;

const MAGIC: &[u8; 8] = b"EVRIDX\0\x02";

/// A loaded index file.
pub struct Saved {
    pub index: SearchIndex,
    /// The journal position the index matches, for indexes read from the MFT.
    pub journal: Option<JournalState>,
}

/// Encodes the index of `root`.
pub fn encode(root: &Path, index: &SearchIndex, journal: Option<JournalState>) -> Vec<u8> {
    let mut out = Vec::with_capacity(index.live_len() * 64);
    out.extend_from_slice(MAGIC);
    let root = root.as_os_str().as_encoded_bytes();
    out.extend_from_slice(&(root.len() as u32).to_le_bytes());
    out.extend_from_slice(root);
    match journal {
        Some(journal) => {
            out.push(1);
            out.extend_from_slice(&journal.id.to_le_bytes());
            out.extend_from_slice(&journal.next_usn.to_le_bytes());
        }
        None => out.push(0),
    }
    index.encode(&mut out);
    out
}

/// Decodes a file made by [`encode`]. Fails when the file is damaged or
/// belongs to another root.
pub fn decode(root: &Path, bytes: &[u8]) -> Result<Saved> {
    ensure!(bytes.get(..MAGIC.len()) == Some(MAGIC.as_slice()), "not an index file");
    let mut position = MAGIC.len();
    let mut take = |length: usize| -> Result<&[u8]> {
        let slice = bytes.get(position..position + length).context("damaged index file")?;
        position += length;
        Ok(slice)
    };
    let root_length = u32::from_le_bytes(take(4)?.try_into()?) as usize;
    ensure!(take(root_length)? == root.as_os_str().as_encoded_bytes(), "the index file is for another folder");
    let journal = if take(1)?[0] != 0 {
        Some(JournalState {
            id: u64::from_le_bytes(take(8)?.try_into()?),
            next_usn: i64::from_le_bytes(take(8)?.try_into()?),
        })
    } else {
        None
    };
    let (nodes, used) = Nodes::decode(&bytes[position..])?;
    ensure!(position + used == bytes.len(), "damaged index file");
    Ok(Saved {
        index: SearchIndex::from_nodes(nodes, true),
        journal,
    })
}

/// Writes the file through a temporary file, so a crash never leaves a
/// half-written file behind.
pub fn save(file: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let temporary = file.with_extension("tmp");
    let mut out = std::fs::File::create(&temporary)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&temporary, file)?;
    Ok(())
}

/// Reads and decodes the index file of `root`.
pub fn load(file: &Path, root: &Path) -> Result<Saved> {
    let bytes = std::fs::read(file)?;
    decode(root, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Meta, Parent};
    use crate::model::Entry;
    use std::path::PathBuf;

    fn entry(path: &str, is_dir: bool) -> Entry {
        Entry {
            path: PathBuf::from(path),
            is_dir,
            size: (!is_dir).then_some(u64::MAX - 3),
            modified: Some(133_000_000_000_000_000),
            created: None,
            attributes: Some(0x20),
        }
    }

    fn listing(index: &SearchIndex) -> Vec<(PathBuf, Meta, Option<u32>)> {
        let mut list: Vec<_> = index.ids().iter().map(|&id| (index.path(id), index.meta(id), index.record(id))).collect();
        list.sort();
        list
    }

    #[test]
    fn round_trip_keeps_every_field() {
        let mut entries = vec![
            entry("/c/Windows", true),
            entry("/c/Windows/notepad.exe", false),
            entry("/c/Windows/отчёт 😀.txt", false),
            entry("/d", true),
        ];
        entries[3].attributes = None;
        for index in 0..1000 {
            entries.push(entry(&format!("/c/many/{index:06}"), false));
        }
        let mut index = SearchIndex::new(entries, false);
        // Changes leave removed slots; the file holds only live nodes.
        let removed = index.find(Path::new("/c/Windows/notepad.exe")).unwrap();
        index.remove(&[removed]);
        let parent = index.parent_for(Path::new("/c/Windows/new.txt"));
        index.insert(parent, "new.txt".as_ref(), entry("/x", false).meta(), None);
        let journal = Some(JournalState { id: 7, next_usn: -2 });
        let bytes = encode(Path::new("/c"), &index, journal);
        let saved = decode(Path::new("/c"), &bytes).unwrap();
        assert_eq!(saved.journal, journal);
        assert_eq!(listing(&saved.index), listing(&index));
        assert_eq!(saved.index.len(), index.live_len());
        assert!(saved.index.is_path_sorted());
        assert!(decode(Path::new("/other"), &bytes).is_err());
    }

    #[test]
    fn records_survive_and_damaged_files_are_rejected() {
        let mut nodes = Nodes::default();
        let root = nodes.add_root("/v");
        nodes.set_root_record(5, root);
        let folder = nodes.push_str(Parent::Root(root), "d", Meta { is_dir: true, ..Meta::default() }, Some(40));
        nodes.push_str(Parent::Node(folder), "a", Meta::default(), Some(41));
        let index = SearchIndex::from_nodes(nodes, true);
        let bytes = encode(Path::new("/v"), &index, None);
        let saved = decode(Path::new("/v"), &bytes).unwrap();
        assert_eq!(saved.index.nodes_of_record(41).len(), 1);
        assert_eq!(saved.index.folder_of_record(5), Some(Parent::Root(0)));
        for length in 0..bytes.len() {
            assert!(decode(Path::new("/v"), &bytes[..length]).is_err(), "length {length}");
        }
        for position in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[position] ^= 0xFF;
            let _ = decode(Path::new("/v"), &damaged);
        }
    }
}
