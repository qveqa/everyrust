use std::path::{Path, PathBuf};

/// One indexed filesystem item.
///
/// Timestamps are Windows FILETIMEs: 100-nanosecond intervals since
/// 1601-01-01 UTC. Keeping the wire representation through the whole pipeline
/// makes EFU import/export lossless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub created: Option<u64>,
    pub attributes: Option<u32>,
}

impl Entry {
    /// The display name: the last path component (or the whole path for a
    /// volume root such as `C:\`).
    pub fn name_string(&self) -> String {
        match self.path.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => self.path.display().to_string(),
        }
    }

    /// Path joined with the given base when the stored path is relative.
    pub fn path_with_base(&self, base: &Path) -> PathBuf {
        if self.path.is_absolute() {
            self.path.clone()
        } else {
            base.join(&self.path)
        }
    }

    pub fn is_directory_attr(&self) -> bool {
        self.attributes.map(|a| a & 0x10 != 0).unwrap_or(false)
    }
}
