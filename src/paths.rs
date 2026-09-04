//! Index-file location and path normalisation helpers.

use std::env;
use std::path::{Path, PathBuf};

/// The index file's name, both in the home directory and at a project root.
pub const INDEX_FILE_NAME: &str = ".csearchindex";

/// Where the index is, in order of precedence:
///
/// 1. `$CSEARCHINDEX`, if set and non-empty;
/// 2. the nearest `.csearchindex` **file** at or above the working directory
///    -- a per-project index created by `cindex --local`;
/// 3. `~/.csearchindex` (`%USERPROFILE%\.csearchindex` on Windows).
///
/// Only step 2 is new relative to the original csearch, and it only ever
/// finds a file someone deliberately created, so an existing setup resolves
/// exactly as it always did. `--indexpath` overrides all of this.
pub fn default_index_path() -> PathBuf {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let over = env::var_os("CSEARCHINDEX")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let cwd = env::current_dir().ok();
    resolve_index_path(over, cwd.as_deref(), home.as_deref())
}

/// The resolution rule with its inputs made explicit, so it can be tested
/// without touching process-global state. See [`default_index_path`].
pub fn resolve_index_path(
    env_override: Option<PathBuf>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(p) = env_override {
        return p;
    }
    if let Some(local) = cwd.and_then(find_local_index) {
        return local;
    }
    home.map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(INDEX_FILE_NAME)
}

/// The nearest `.csearchindex` file at or above `start`. A directory of that
/// name does not count.
pub fn find_local_index(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join(INDEX_FILE_NAME))
        .find(|p| p.is_file())
}

/// The enclosing repository root: the nearest directory at or above `start`
/// containing a `.git` entry -- a directory normally, a file for a worktree.
/// `None` outside any repository.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolution_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let proj = home.join("work").join("proj");
        let deep = proj.join("src").join("nested");
        fs::create_dir_all(&deep).unwrap();

        // Nothing local: the home default, whether or not the file exists.
        assert_eq!(
            resolve_index_path(None, Some(&deep), Some(&home)),
            home.join(INDEX_FILE_NAME)
        );

        // A project index above the working directory wins over home.
        let local = proj.join(INDEX_FILE_NAME);
        fs::write(&local, b"x").unwrap();
        assert_eq!(resolve_index_path(None, Some(&deep), Some(&home)), local);

        // When two are stacked, the nearest wins.
        let nearer = proj.join("src").join(INDEX_FILE_NAME);
        fs::write(&nearer, b"x").unwrap();
        assert_eq!(resolve_index_path(None, Some(&deep), Some(&home)), nearer);

        // The environment overrides everything, even a local index.
        let over = dir.path().join("elsewhere.idx");
        assert_eq!(
            resolve_index_path(Some(over.clone()), Some(&deep), Some(&home)),
            over
        );

        // A directory of that name is not an index.
        let other = dir.path().join("other").join("sub");
        fs::create_dir_all(other.join(INDEX_FILE_NAME)).unwrap();
        assert_eq!(
            resolve_index_path(None, Some(&other), Some(&home)),
            home.join(INDEX_FILE_NAME)
        );
    }

    #[test]
    fn repo_root_is_the_nearest_dot_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(find_repo_root(&deep), Some(root.clone()));
        assert_eq!(find_repo_root(&root), Some(root.clone()));

        // A worktree's `.git` is a file, and still marks the root.
        let wt = dir.path().join("wt");
        fs::create_dir_all(wt.join("x")).unwrap();
        fs::write(wt.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(find_repo_root(&wt.join("x")), Some(wt));
    }
}
