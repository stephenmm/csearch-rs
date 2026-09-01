//! Index-file location and path normalisation helpers.

use std::env;
use std::path::{Path, PathBuf};

/// `$CSEARCHINDEX`, else `~/.csearchindex` (`%USERPROFILE%` on Windows).
pub fn default_index_path() -> PathBuf {
    if let Some(p) = env::var_os("CSEARCHINDEX") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".csearchindex")
}

/// Absolute, canonical path as a display string, without the Windows
/// `\\?\` verbatim prefix that `canonicalize` adds.
pub fn canonical_string(p: &Path) -> std::io::Result<String> {
    let c = p.canonicalize()?;
    let s = c.to_string_lossy().into_owned();
    Ok(strip_verbatim(&s))
}

pub fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    s.to_string()
}
