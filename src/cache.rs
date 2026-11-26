//! Saves an index to a file and loads it back, so the app can show the last
//! index at once while a fresh scan runs.
//!
//! Entries are stored in chunks. Inside a chunk, each path stores only the
//! bytes that differ from the previous path (front coding), which keeps the
//! file small for path-sorted indexes. Chunks are encoded and decoded in
//! parallel.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;

use crate::model::Entry;

const MAGIC: &[u8; 8] = b"EVRIDX\0\x01";
const CHUNK_ENTRIES: usize = 32 * 1024;

const HAS_SIZE: u8 = 1;
const HAS_MODIFIED: u8 = 2;
const HAS_CREATED: u8 = 4;
const HAS_ATTRIBUTES: u8 = 8;
const IS_DIR: u8 = 16;
/// The path is not valid Unicode and is stored in the platform's own form.
const NATIVE_PATH: u8 = 32;

/// Encodes `entries` of the index for `root`.
pub fn encode(root: &Path, entries: &[Entry]) -> Vec<u8> {
    let chunks: Vec<Vec<u8>> = entries.par_chunks(CHUNK_ENTRIES).map(encode_chunk).collect();
    let mut out = Vec::with_capacity(64 + chunks.iter().map(|chunk| chunk.len() + 16).sum::<usize>());
    out.extend_from_slice(MAGIC);
    write_bytes(&mut out, &native_bytes(root.as_os_str()));
    write_varint(&mut out, entries.len() as u64);
    write_varint(&mut out, chunks.len() as u64);
    for (index, chunk) in chunks.iter().enumerate() {
        let count = entries.len().saturating_sub(index * CHUNK_ENTRIES).min(CHUNK_ENTRIES);
        write_varint(&mut out, count as u64);
        write_varint(&mut out, chunk.len() as u64);
    }
    for chunk in &chunks {
        out.extend_from_slice(chunk);
    }
    out
}

/// Decodes a file made by [`encode`]. Fails when the file is damaged or
/// belongs to another root.
pub fn decode(root: &Path, bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut reader = Reader { bytes, position: 0 };
    ensure!(reader.take(MAGIC.len())? == MAGIC, "not an index cache");
    let stored_root = reader.bytes_field()?;
    ensure!(stored_root == native_bytes(root.as_os_str()).as_slice(), "the cache is for another folder");
    let total = reader.varint()? as usize;
    let chunk_count = reader.varint()? as usize;
    ensure!(chunk_count <= bytes.len(), "damaged index cache");
    let mut table = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        table.push((reader.varint()? as usize, reader.varint()? as usize));
    }
    let mut slices = Vec::with_capacity(chunk_count);
    let mut sum = 0usize;
    for &(count, length) in &table {
        // Every entry takes at least three bytes.
        ensure!(count <= length, "damaged index cache");
        slices.push((count, reader.take(length)?));
        sum = sum.checked_add(count).context("damaged index cache")?;
    }
    ensure!(sum == total && reader.position == bytes.len(), "damaged index cache");
    let chunks: Vec<Vec<Entry>> = slices
        .into_par_iter()
        .map(|(count, chunk)| decode_chunk(count, chunk))
        .collect::<Result<_>>()?;
    let mut entries = Vec::with_capacity(total);
    for chunk in chunks {
        entries.extend(chunk);
    }
    Ok(entries)
}

/// Writes the cache file through a temporary file, so a crash never leaves a
/// half-written cache behind.
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

/// Reads and decodes a cache file for `root`.
pub fn load(file: &Path, root: &Path) -> Result<Vec<Entry>> {
    let bytes = std::fs::read(file)?;
    decode(root, &bytes)
}

fn encode_chunk(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 40);
    let mut previous: &[u8] = &[];
    for entry in entries {
        let text = entry.path.to_str().map(str::as_bytes);
        let mut flags = 0;
        flags |= if text.is_none() { NATIVE_PATH } else { 0 };
        flags |= if entry.is_dir { IS_DIR } else { 0 };
        flags |= if entry.size.is_some() { HAS_SIZE } else { 0 };
        flags |= if entry.modified.is_some() { HAS_MODIFIED } else { 0 };
        flags |= if entry.created.is_some() { HAS_CREATED } else { 0 };
        flags |= if entry.attributes.is_some() { HAS_ATTRIBUTES } else { 0 };
        out.push(flags);
        match text {
            Some(path) => {
                let shared = previous.iter().zip(path).take_while(|(a, b)| a == b).count();
                write_varint(&mut out, shared as u64);
                write_bytes(&mut out, &path[shared..]);
                previous = path;
            }
            None => {
                write_varint(&mut out, 0);
                write_bytes(&mut out, &native_bytes(entry.path.as_os_str()));
                previous = &[];
            }
        }
        for value in [entry.size, entry.modified, entry.created].into_iter().flatten() {
            write_varint(&mut out, value);
        }
        if let Some(attributes) = entry.attributes {
            write_varint(&mut out, u64::from(attributes));
        }
    }
    out
}

fn decode_chunk(count: usize, bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut reader = Reader { bytes, position: 0 };
    let mut entries = Vec::with_capacity(count);
    let mut previous = String::new();
    for _ in 0..count {
        let flags = reader.take(1)?[0];
        let shared = reader.varint()? as usize;
        let suffix = reader.bytes_field()?;
        let path = if flags & NATIVE_PATH != 0 {
            ensure!(shared == 0, "damaged index cache");
            previous.clear();
            from_native_bytes(suffix)?
        } else {
            ensure!(shared <= previous.len(), "damaged index cache");
            let mut text = Vec::with_capacity(shared + suffix.len());
            text.extend_from_slice(&previous.as_bytes()[..shared]);
            text.extend_from_slice(suffix);
            previous = String::from_utf8(text).context("damaged index cache")?;
            PathBuf::from(previous.clone())
        };
        let mut optional = |flag: u8| -> Result<Option<u64>> {
            if flags & flag != 0 {
                reader.varint().map(Some)
            } else {
                Ok(None)
            }
        };
        let size = optional(HAS_SIZE)?;
        let modified = optional(HAS_MODIFIED)?;
        let created = optional(HAS_CREATED)?;
        let attributes = optional(HAS_ATTRIBUTES)?
            .map(|value| u32::try_from(value).context("damaged index cache"))
            .transpose()?;
        entries.push(Entry {
            path,
            is_dir: flags & IS_DIR != 0,
            size,
            modified,
            created,
            attributes,
        });
    }
    ensure!(reader.position == bytes.len(), "damaged index cache");
    Ok(entries)
}

/// The path in UTF-16 (Windows) or raw bytes (other systems).
fn native_bytes(text: &std::ffi::OsStr) -> Vec<u8> {
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

fn from_native_bytes(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        ensure!(bytes.len() % 2 == 0, "damaged index cache");
        let wide: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(length).context("damaged index cache")?;
        let Some(slice) = self.bytes.get(self.position..end) else {
            bail!("damaged index cache");
        };
        self.position = end;
        Ok(slice)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)?[0];
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("damaged index cache")
    }

    fn bytes_field(&mut self) -> Result<&'a [u8]> {
        let length = usize::try_from(self.varint()?).context("damaged index cache")?;
        self.take(length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn round_trip_keeps_every_field() {
        let mut entries = vec![
            entry("/c/Windows", true),
            entry("/c/Windows/notepad.exe", false),
            entry("/c/Windows/отчёт 😀.txt", false),
            entry("/c/Windows/отчёт.txt", false),
            entry("/d", true),
        ];
        entries[4].attributes = None;
        entries[4].modified = None;
        for index in 0..(CHUNK_ENTRIES + 10) {
            entries.push(entry(&format!("/c/many/{index:06}"), false));
        }
        let bytes = encode(Path::new("/c"), &entries);
        let decoded = decode(Path::new("/c"), &bytes).unwrap();
        let key = |entry: &Entry| {
            (entry.path.clone(), entry.is_dir, entry.size, entry.modified, entry.created, entry.attributes)
        };
        assert_eq!(decoded.iter().map(key).collect::<Vec<_>>(), entries.iter().map(key).collect::<Vec<_>>());
        assert!(decode(Path::new("/other"), &bytes).is_err());
    }

    #[test]
    fn damaged_files_are_rejected_without_panics() {
        let entries = vec![entry("/c/a", false), entry("/c/ab", false)];
        let bytes = encode(Path::new("/c"), &entries);
        for length in 0..bytes.len() {
            assert!(decode(Path::new("/c"), &bytes[..length]).is_err(), "length {length}");
        }
        for position in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[position] ^= 0xFF;
            let _ = decode(Path::new("/c"), &damaged);
        }
    }
}
