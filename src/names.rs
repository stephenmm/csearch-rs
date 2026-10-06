//! Every name csearch-rs puts on a machine, in one place.
//!
//! None of them may be a name the original codesearch uses, because the two
//! are meant to be installed side by side. Google's tools keep `cindex`,
//! `csearch`, `$CSEARCHINDEX` and `~/.csearchindex`; this project uses the
//! names below. The two index formats are not interchangeable, so a shared
//! file name would leave each tool reporting the other's index as corrupt.
//!
//! `tests/names.rs` holds this module to that, and `tests/coexist.rs` runs
//! the real original next to these binaries when it is installed.

/// The indexer binary.
pub const CINDEX: &str = "cindex-rs";

/// The search binary.
pub const CSEARCH: &str = "csearch-rs";

/// The index file's name, both in the home directory and at a project root.
pub const INDEX_FILE_NAME: &str = ".csearch-rs-index";

/// Environment variable naming the index file to use.
pub const INDEX_ENV: &str = "CSEARCH_RS_INDEX";

/// What the original codesearch calls its things -- and what csearch-rs
/// called its own before 0.3. These are never written, and never used as an
/// index or as configuration. They are named here only so that a file left
/// behind by csearch-rs 0.2 can be pointed out (see
/// [`crate::paths::find_legacy_index`]) and so the tests can check that
/// nothing above collides with them.
pub mod original {
    /// `cgrep` is the third tool upstream ships; csearch-rs has no equivalent,
    /// which is all the more reason not to take the name.
    pub const BINARIES: &[&str] = &["cindex", "csearch", "cgrep"];
    pub const INDEX_FILE_NAME: &str = ".csearchindex";
    pub const INDEX_ENV: &str = "CSEARCHINDEX";
}
