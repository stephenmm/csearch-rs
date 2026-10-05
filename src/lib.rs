//! csearch-rs: a Rust port of Google Code Search (Russ Cox's `codesearch`).
//!
//! * `cindex-rs` walks directory trees, extracts the set of distinct 3-byte
//!   trigrams from every text file (AVX2-accelerated, files processed in
//!   parallel with rayon), and writes a compact posting-list index.
//! * `csearch-rs` turns a regular expression into a boolean trigram query
//!   (the same analysis as Cox's `regexp.go`), evaluates it against the
//!   index to get a small candidate set, then greps those files in parallel
//!   with the SIMD-accelerated `regex` crate.
//!
//! The binaries, the index file and the environment variable all carry names
//! of their own (see [`names`]), so this can be installed next to the
//! original without either disturbing the other.

pub mod githead;
pub mod listing;
pub mod lock;
pub mod names;
pub mod paths;
pub mod query;
pub mod read;
pub mod regexp;
pub mod stamp;
pub mod trigram;
pub mod varint;
pub mod write;
