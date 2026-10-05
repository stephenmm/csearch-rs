//! Index-file location and path normalisation helpers.

use crate::names::{original, CINDEX, INDEX_ENV};
use crate::write::MAGIC_FAMILY;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub use crate::names::INDEX_FILE_NAME;

/// Where the index is, in order of precedence:
///
/// 1. `$CSEARCH_RS_INDEX`, if set and non-empty;
/// 2. the nearest `.csearch-rs-index` **file** at or above the working
///    directory -- a per-project index created by `cindex-rs --local`;
/// 3. `~/.csearch-rs-index` (`%USERPROFILE%\.csearch-rs-index` on Windows).
///
/// This is the original csearch's rule -- `$CSEARCHINDEX`, else
/// `~/.csearchindex` -- with step 2 added and every name changed, so the two
/// tools can never resolve to the same file. Neither of the original's names
/// is consulted. `--indexpath` overrides all of this.
pub fn default_index_path() -> PathBuf {
    let over = env::var_os(INDEX_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let cwd = env::current_dir().ok();
    resolve_index_path(over, cwd.as_deref(), home_dir().as_deref())
}

/// The user's home directory: `$HOME`, else `%USERPROFILE%`.
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
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

/// A file that lives beside the index and belongs to it: `<index>.<suffix>`.
/// The suffix is appended to the whole name rather than replacing an
/// extension, so `a.one` and `a.two` in one directory never share a sidecar.
pub fn sidecar(index: &Path, suffix: &str) -> PathBuf {
    let mut name = index.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// The nearest `.csearch-rs-index` file at or above `start`. A directory of
/// that name does not count.
pub fn find_local_index(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join(INDEX_FILE_NAME))
        .find(|p| p.is_file())
}

/// True when `path` is an index csearch-rs itself wrote: it starts with this
/// project's magic, whatever the format version. An index belonging to the
/// original csearch -- or any other file -- does not.
pub fn is_csearch_rs_index(path: &Path) -> bool {
    let mut head = [0u8; MAGIC_FAMILY.len()];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && head == *MAGIC_FAMILY
}

/// An index csearch-rs left under the original's name before 0.3: the nearest
/// `.csearchindex` at or above `cwd`, else the one in `home` -- but only if
/// this project wrote it. That name belongs to the original csearch alone
/// now, so such a file is never read as an index and never modified; callers
/// just point it out, so that it does not sit there forgotten while searches
/// report "no index".
pub fn find_legacy_index(cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    cwd.into_iter()
        .flat_map(Path::ancestors)
        .chain(home)
        .map(|d| d.join(original::INDEX_FILE_NAME))
        .find(|p| p.is_file() && is_csearch_rs_index(p))
}

/// The pre-0.3 index sitting next to `index`, if there is one of ours. See
/// [`find_legacy_index`].
pub fn legacy_index_beside(index: &Path) -> Option<PathBuf> {
    let old = index.parent()?.join(original::INDEX_FILE_NAME);
    // `--indexpath .csearchindex` is someone choosing that name on purpose;
    // the index they just built is not a leftover.
    if index.file_name() == old.file_name() {
        return None;
    }
    (old.is_file() && is_csearch_rs_index(&old)).then_some(old)
}

/// True when only the original's `$CSEARCHINDEX` is set. csearch-rs 0.2 read
/// that variable; it now belongs to the original csearch alone, so someone
/// upgrading may be wondering why their index is no longer found.
pub fn only_original_env_is_set() -> bool {
    let set = |name: &str| env::var_os(name).is_some_and(|v| !v.is_empty());
    set(original::INDEX_ENV) && !set(INDEX_ENV)
}

/// What to add to a "no index" error for someone who looks to be upgrading
/// from csearch-rs 0.2, when the index file and the variable still had the
/// original's names. Empty for everyone else -- in particular for someone who
/// simply runs the original csearch as well, whose `.csearchindex` is not
/// ours and whose `$CSEARCHINDEX` is only mentioned because no index of ours
/// turned up.
pub fn upgrade_notes() -> Vec<String> {
    let mut notes = Vec::new();
    let cwd = env::current_dir().ok();
    if let Some(old) = find_legacy_index(cwd.as_deref(), home_dir().as_deref()) {
        notes.push(format!(
            "{} was written by csearch-rs 0.2 or earlier. That file name now belongs to \
             the original csearch alone, so it is not read here -- re-index with `{CINDEX}`, \
             then delete it.",
            old.display()
        ));
    }
    if only_original_env_is_set() {
        notes.push(format!(
            "${} is set, but that variable now belongs to the original csearch alone -- \
             csearch-rs reads ${INDEX_ENV}.",
            original::INDEX_ENV
        ));
    }
    notes
}

/// `error` with [`upgrade_notes`] appended, one `note:` line each.
pub fn with_upgrade_notes(error: anyhow::Error) -> anyhow::Error {
    let notes = upgrade_notes();
    if notes.is_empty() {
        return error;
    }
    let mut text = format!("{error:#}");
    for note in notes {
        text.push_str("\nnote: ");
        text.push_str(&note);
    }
    anyhow::anyhow!(text)
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
    fn the_originals_index_file_is_not_a_candidate() {
        // A `.csearchindex` right here, and one at home, are the original
        // csearch's. Resolution must walk straight past both.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let proj = home.join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join(original::INDEX_FILE_NAME), b"csearch index 1\n").unwrap();
        fs::write(home.join(original::INDEX_FILE_NAME), b"csearch index 1\n").unwrap();
        assert_eq!(find_local_index(&proj), None);
        assert_eq!(
            resolve_index_path(None, Some(&proj), Some(&home)),
            home.join(INDEX_FILE_NAME)
        );
    }

    #[test]
    fn a_legacy_index_is_recognised_by_its_magic_not_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let proj = home.join("proj");
        let deep = proj.join("src");
        fs::create_dir_all(&deep).unwrap();
        let old = proj.join(original::INDEX_FILE_NAME);

        // Nothing there at all.
        assert_eq!(find_legacy_index(Some(&deep), Some(&home)), None);

        // The original csearch's own index carries that name by right: it is
        // not ours, so it is not reported -- nor is an empty or unrelated file.
        for theirs in [&b"csearch index 1\n...."[..], b"", b"csearch-rs"] {
            fs::write(&old, theirs).unwrap();
            assert_eq!(find_legacy_index(Some(&deep), Some(&home)), None);
            assert_eq!(legacy_index_beside(&proj.join(INDEX_FILE_NAME)), None);
            assert!(!is_csearch_rs_index(&old));
        }

        // One written by csearch-rs 0.2 is, from anywhere beneath it, and
        // whatever its format version.
        for ours in [&b"csearch-rs index 1\nrest"[..], b"csearch-rs index 7\n"] {
            fs::write(&old, ours).unwrap();
            assert_eq!(
                find_legacy_index(Some(&deep), Some(&home)),
                Some(old.clone())
            );
            assert_eq!(
                legacy_index_beside(&proj.join(INDEX_FILE_NAME)),
                Some(old.clone())
            );
        }

        // The nearest one wins, and the home directory is the last resort.
        fs::remove_file(&old).unwrap();
        let at_home = home.join(original::INDEX_FILE_NAME);
        fs::write(&at_home, b"csearch-rs index 1\n").unwrap();
        assert_eq!(
            find_legacy_index(Some(&deep), Some(&home)),
            Some(at_home.clone())
        );
        let outside = dir.path().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        assert_eq!(
            find_legacy_index(Some(&outside), Some(&home)),
            Some(at_home)
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
