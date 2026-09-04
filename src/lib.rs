//! csearch-rs: a Rust port of Google Code Search (Russ Cox's `codesearch`).
//!
//! * `cindex` walks directory trees, extracts the set of distinct 3-byte
//!   trigrams from every text file (AVX2-accelerated, files processed in
//!   parallel with rayon), and writes a compact posting-list index.
//! * `csearch` turns a regular expression into a boolean trigram query
//!   (the same analysis as Cox's `regexp.go`), evaluates it against the
//!   index to get a small candidate set, then greps those files in parallel
//!   with the SIMD-accelerated `regex` crate.

pub mod gitstate;
pub mod paths;
pub mod query;
pub mod read;
pub mod regexp;
pub mod trigram;
pub mod varint;
pub mod write;
