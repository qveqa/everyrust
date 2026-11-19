//! EFU file list (EFU) reading and writing.
//!
//! EFU is a UTF-8 CSV file whose first line defines the columns; `Filename` is
//! required and `Size`, `Date Modified`, `Date Created`, `Attributes` are
//! optional. Dates are either Windows FILETIMEs or ISO 8601; attributes are
//! decimal or `0x`-prefixed hexadecimal.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::model::Entry;
use crate::timeutil;

const HEADER: [&str; 5] = [
    "Filename",
    "Size",
    "Date Modified",
    "Date Created",
    "Attributes",
];

/// Reads an EFU file list.
///
/// Accepts `.efu` / `.csv` (CSV with a `Filename` column) and `.txt` (one path
/// per line). Relative paths are resolved against the list's own directory.
pub fn read_file_list(path: &Path) -> Result<Vec<Entry>> {
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let is_plain_text = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("txt"))
        .unwrap_or(false);
    if is_plain_text {
        return read_plain_text(path, base);
    }

    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read file list {}", path.display()))?;
    let text = std::str::from_utf8(strip_bom(&bytes))
        .with_context(|| format!("file list {} is not valid UTF-8", path.display()))?;
    parse_csv(text, base).with_context(|| format!("cannot parse file list {}", path.display()))
}

/// Writes entries as an EFU file with the canonical five columns.
pub fn write_file_list(path: &Path, entries: &[Entry]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
    }
    let file = File::create(path)
        .with_context(|| format!("cannot create file list {}", path.display()))?;
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .terminator(csv::Terminator::CRLF)
        .from_writer(BufWriter::new(file));

    writer.write_record(HEADER).context("cannot write EFU header")?;
    for entry in entries {
        let filename = if entry.is_dir {
            dir_with_separator(&entry.path)
        } else {
            entry.path.display().to_string()
        };
        writer
            .write_record([
                filename,
                number(entry.size),
                number(entry.modified),
                number(entry.created),
                entry.attributes.map(|a| a.to_string()).unwrap_or_default(),
            ])
            .context("cannot write EFU record")?;
    }
    writer.flush().context("cannot flush EFU writer")?;
    Ok(())
}

fn read_plain_text(path: &Path, base: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read file list {}", path.display()))?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (path, trailing_dir) = resolve(base, line);
        entries.push(Entry {
            path,
            is_dir: trailing_dir,
            size: None,
            modified: None,
            created: None,
            attributes: None,
        });
    }
    Ok(entries)
}

fn parse_csv(text: &str, base: &Path) -> Result<Vec<Entry>> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(text.as_bytes());
    let headers = reader.headers().context("missing CSV header")?.clone();

    let column = |name: &str| {
        headers
            .iter()
            .position(|header| header.trim().eq_ignore_ascii_case(name))
    };
    let filename_idx = match column("Filename") {
        Some(idx) => idx,
        None => bail!("EFU header has no Filename column"),
    };
    let size_idx = column("Size");
    let modified_idx = column("Date Modified");
    let created_idx = column("Date Created");
    let attributes_idx = column("Attributes");

    // A later entry replaces an earlier entry for the same path.
    let mut by_path: HashMap<String, Entry> = HashMap::new();
    let mut record = csv::StringRecord::new();
    while reader.read_record(&mut record).context("invalid CSV record")? {
        let raw = record.get(filename_idx).unwrap_or("").trim();
        if raw.is_empty() {
            continue;
        }
        let (path, trailing_dir) = resolve(base, raw);
        let attributes = cell(&record, attributes_idx).and_then(parse_attributes);
        let is_dir = trailing_dir || attributes.map(|a| a & 0x10 != 0).unwrap_or(false);
        let entry = Entry {
            path,
            is_dir,
            size: cell(&record, size_idx).and_then(|s| s.trim().parse().ok()),
            modified: cell(&record, modified_idx).and_then(parse_datetime),
            created: cell(&record, created_idx).and_then(parse_datetime),
            attributes,
        };
        by_path.insert(dedup_key(&entry.path), entry);
    }
    Ok(by_path.into_values().collect())
}

fn cell<'a>(record: &'a csv::StringRecord, idx: Option<usize>) -> Option<&'a str> {
    idx.and_then(|i| record.get(i)).map(str::trim).filter(|s| !s.is_empty())
}

fn parse_datetime(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        text.parse().ok()
    } else {
        timeutil::parse_iso8601(text)
    }
}

fn parse_attributes(text: &str) -> Option<u32> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

/// Resolves an EFU filename against the list's directory, returning whether the
/// name was written as a folder (trailing separator).
fn resolve(base: &Path, raw: &str) -> (PathBuf, bool) {
    let trailing_dir = raw.ends_with('\\') || raw.ends_with('/');
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if raw.starts_with('\\') || raw.starts_with('/') {
        // A leading separator means "relative to the list's root".
        let root = base
            .ancestors()
            .last()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("\\"));
        root.join(raw.trim_start_matches(['\\', '/']))
    } else {
        base.join(raw)
    };
    (joined, trailing_dir)
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn dedup_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn dir_with_separator(path: &Path) -> String {
    let text = path.display().to_string();
    if text.ends_with('\\') || text.ends_with('/') {
        text
    } else {
        format!("{text}\\")
    }
}

fn number(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_csv() -> String {
        let mut text = String::from("Filename,Size,Date Modified,Date Created,Attributes\r\n");
        text.push_str("\"C:\\data\\a.txt\",12,134346726600000000,,32\r\n");
        text.push_str("\"C:\\data\\with,comma.txt\",5,,,\r\n");
        text.push_str("\"C:\\data\\say \"\"hi\"\".txt\",3,,,0x20\r\n");
        text.push_str("\"C:\\data\\подпапка\\\",,,,16\r\n");
        text.push_str("\"C:\\data\\iso.md\",7,2026-09-23T21:31:00,2026-09-23,32\r\n");
        text
    }

    #[test]
    fn parses_all_columns() {
        let entries = parse_csv(&sample_csv(), Path::new("C:\\lists")).unwrap();
        let by_name: HashMap<String, Entry> = entries
            .into_iter()
            .map(|e| (e.name_string(), e))
            .collect();

        assert_eq!(by_name.len(), 5);
        let a = &by_name["a.txt"];
        assert_eq!(a.size, Some(12));
        assert_eq!(a.attributes, Some(32));
        assert_eq!(a.modified, Some(134_346_726_600_000_000));

        let comma = &by_name["with,comma.txt"];
        assert_eq!(comma.size, Some(5));

        let quoted = &by_name["say \"hi\".txt"];
        assert_eq!(quoted.attributes, Some(0x20));

        let folder = &by_name["подпапка"];
        assert!(folder.is_dir);

        let iso = &by_name["iso.md"];
        assert_eq!(iso.modified, Some(134_346_726_600_000_000));
        assert_eq!(
            iso.created,
            Some(134_346_726_600_000_000 - 77_460 * 10_000_000)
        );
    }

    #[test]
    fn relative_filenames_resolve_against_list_directory() {
        let text = "Filename\nsub\\file.txt\n";
        let entries = parse_csv(text, Path::new("C:\\lists")).unwrap();
        assert_eq!(entries[0].path, PathBuf::from("C:\\lists\\sub\\file.txt"));
    }

    #[test]
    fn duplicate_filenames_keep_the_last_entry() {
        let text = "Filename,Size\na.txt,1\na.txt,2\n";
        let entries = parse_csv(text, Path::new("C:\\lists")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, Some(2));
    }

    #[test]
    fn header_is_required() {
        assert!(parse_csv("Size\n12\n", Path::new("C:\\lists")).is_err());
    }

    #[test]
    fn bom_is_supported() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"Filename\nx.txt\n");
        let text = std::str::from_utf8(strip_bom(&bytes)).unwrap();
        let entries = parse_csv(text, Path::new("C:\\lists")).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn writes_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let efu = dir.path().join("list.efu");
        let entries = vec![
            Entry {
                path: PathBuf::from("C:\\data\\a.txt"),
                is_dir: false,
                size: Some(12),
                modified: Some(134_346_726_600_000_000),
                created: None,
                attributes: Some(32),
            },
            Entry {
                path: PathBuf::from("C:\\data\\folder"),
                is_dir: true,
                size: None,
                modified: None,
                created: None,
                attributes: Some(16),
            },
        ];
        write_file_list(&efu, &entries).unwrap();
        let text = std::fs::read_to_string(&efu).unwrap();
        assert!(text.starts_with("Filename,Size,Date Modified,Date Created,Attributes"));
        assert!(text.contains("C:\\data\\folder\\"));

        let mut reloaded = read_file_list(&efu).unwrap();
        reloaded.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded[0].path, PathBuf::from("C:\\data\\a.txt"));
        assert!(reloaded[1].is_dir);
        assert_eq!(reloaded[1].attributes, Some(16));
    }

    #[test]
    fn plain_text_lists_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("files.txt");
        std::fs::write(&list, "one.txt\nsub\\two.txt\n").unwrap();
        let entries = read_file_list(&list).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, dir.path().join("one.txt"));
    }
}
