//! EveryRust indexes file names, paths, and metadata for local search.
//!
//! - [`efu`] reads and writes EFU, CSV, and plain-text file lists.
//! - [`model`] defines indexed metadata.
//! - [`usn`] reads the NTFS change journal and applies it to the index.
//! - [`index`] keeps the compact tree of names that searches run on.
//! - [`cache`] saves an index to disk for a fast start.
//! - [`mft`] reads the NTFS Master File Table for fast drive indexing.
//! - [`scan`] walks directories.
//! - [`search`] builds indexes, filters queries, and sorts results.
//! - [`timeutil`] converts file timestamps.

pub mod cache;
pub mod efu;
pub mod index;
pub mod mft;
pub mod model;
pub mod scan;
pub mod search;
pub mod timeutil;
pub mod usn;
