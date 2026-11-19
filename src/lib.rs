//! EveryRust indexes file names, paths, and metadata for local search.
//!
//! - [`efu`] reads and writes EFU, CSV, and plain-text file lists.
//! - [`model`] defines indexed metadata.
//! - [`scan`] walks directories.
//! - [`search`] builds indexes, filters queries, and sorts results.
//! - [`timeutil`] converts file timestamps.

pub mod efu;
pub mod model;
pub mod scan;
pub mod search;
pub mod timeutil;
