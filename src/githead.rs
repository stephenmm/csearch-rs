//! Where a git work tree's `HEAD` points, read from the repository's files.
//!
//! `csearch-rs` looks at this on every search, to say when the index is
//! behind. Asking git means starting a process, and on Windows that alone
//! costs several times the search it decorates: measured on a git-rooted
//! index, 38-61 ms per search with `git rev-parse` in it against 7 ms
//! without. So this reads the two or three small files git itself would
//! read, and the check no longer shows up in the timing at all.
//!
//! It answers only what it can answer exactly. Anything unusual -- a ref
//! store that is not plain files, a symbolic ref that points at another one
//! -- is `None`, which every caller treats as "unknown": no note at all,
//! rather than a wrong one.

use std::fs;
use std::path::{Path, PathBuf};

/// The repository directory of the work tree containing `start`: `.git`
/// itself, or the place a `.git` *file* names (a linked work tree, a
/// submodule).
fn git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot = dir.join(".git");
        let Ok(meta) = fs::metadata(&dot) else {
            continue;
        };
        if meta.is_dir() {
            return Some(dot);
        }
        let text = fs::read_to_string(&dot).ok()?;
        let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        // `join` keeps an absolute target as it is and resolves a relative
        // one against the directory the `.git` file is in, as git does.
        return Some(dir.join(target));
    }
    None
}

/// A commit id: 40 hex digits, or 64 in a SHA-256 repository.
fn is_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A ref name that can be looked up as a path under the repository: it stays
/// inside `refs/`, with no empty or `..` component to wander off through.
fn is_plain_ref(name: &str) -> bool {
    name.starts_with("refs/")
        && name
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// The commit `HEAD` names in the work tree containing `root`, or `None` if
/// `root` is not in a git work tree or the answer cannot be read off the
/// files (see the module comment).
pub fn head(root: &Path) -> Option<String> {
    let git = git_dir(root)?;
    let head = fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(name) = head.strip_prefix("ref:") else {
        // Detached: HEAD holds the commit itself.
        return is_hash(head).then(|| head.to_string());
    };
    let name = name.trim();
    if !is_plain_ref(name) {
        return None;
    }
    // Branches are shared between work trees. A linked one says where the
    // main repository is in `commondir`; the main one is its own.
    let common = match fs::read_to_string(git.join("commondir")) {
        Ok(rel) => git.join(rel.trim()),
        Err(_) => git.clone(),
    };
    // A loose ref, if there is one, is newer than anything packed.
    for base in [&git, &common] {
        if let Ok(text) = fs::read_to_string(base.join(name)) {
            let text = text.trim();
            return is_hash(text).then(|| text.to_string());
        }
    }
    let packed = fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (hash, packed_name) = line.split_once(' ')?;
        (packed_name == name && is_hash(hash)).then(|| hash.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    /// A hand-made repository directory: just the files `head` reads.
    fn repo(dir: &Path, head: &str) -> PathBuf {
        let git = dir.join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), head).unwrap();
        git
    }

    #[test]
    fn a_branch_is_read_loose_then_packed() {
        let dir = tempfile::tempdir().unwrap();
        let git = repo(dir.path(), "ref: refs/heads/main\n");
        // No commit yet: an unborn branch has no value.
        assert_eq!(head(dir.path()), None);

        fs::write(
            git.join("packed-refs"),
            format!("# pack-refs with: peeled fully-peeled sorted\n{B} refs/heads/other\n{A} refs/heads/main\n^{B}\n"),
        )
        .unwrap();
        assert_eq!(head(dir.path()).as_deref(), Some(A));

        // A loose ref is newer than the packed one.
        fs::write(git.join("refs/heads/main"), format!("{B}\n")).unwrap();
        assert_eq!(head(dir.path()).as_deref(), Some(B));

        // From anywhere below the root, as git would find it.
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(head(&deep).as_deref(), Some(B));
    }

    #[test]
    fn a_detached_head_is_the_commit_itself() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path(), &format!("{A}\n"));
        assert_eq!(head(dir.path()).as_deref(), Some(A));
        // SHA-256 repositories have longer ids.
        let long = "ab".repeat(32);
        repo(dir.path(), &long);
        assert_eq!(head(dir.path()), Some(long));
    }

    #[test]
    fn a_linked_work_tree_finds_its_branch_in_the_main_repository() {
        let dir = tempfile::tempdir().unwrap();
        let main = repo(&dir.path().join("main"), "ref: refs/heads/main\n");
        fs::write(main.join("refs/heads/main"), A).unwrap();
        fs::write(main.join("refs/heads/side"), B).unwrap();
        // What `git worktree add` leaves: a `.git` file in the work tree, and
        // a private directory in the main repository with its own HEAD.
        let private = main.join("worktrees").join("wt");
        fs::create_dir_all(&private).unwrap();
        fs::write(private.join("HEAD"), "ref: refs/heads/side\n").unwrap();
        fs::write(private.join("commondir"), "../..\n").unwrap();
        let wt = dir.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", private.display())).unwrap();

        assert_eq!(head(&wt).as_deref(), Some(B));
        assert_eq!(head(&dir.path().join("main")).as_deref(), Some(A));
    }

    #[test]
    fn what_cannot_be_read_exactly_is_unknown_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        // Not a repository at all. (Checked below a `.git`-less directory of
        // our own making, since the temp directory's parents are not ours.)
        let plain = dir.path().join("plain");
        fs::create_dir_all(plain.join(".git")).unwrap();
        assert_eq!(head(&plain), None, "a .git with no HEAD in it");

        // The reftable backend keeps a placeholder in HEAD and the refs
        // somewhere this does not look.
        let git = repo(dir.path(), "ref: refs/heads/.invalid\n");
        assert_eq!(head(dir.path()), None);
        // A symbolic ref to a symbolic ref.
        fs::write(git.join("HEAD"), "ref: refs/heads/alias\n").unwrap();
        fs::write(git.join("refs/heads/alias"), "ref: refs/heads/main\n").unwrap();
        fs::write(git.join("refs/heads/main"), A).unwrap();
        assert_eq!(head(dir.path()), None);
        // Something that is not a commit id.
        for odd in ["garbage\n", ""] {
            fs::write(git.join("HEAD"), odd).unwrap();
            assert_eq!(head(dir.path()), None, "{odd:?}");
        }
        // A name that climbs out of refs/ -- to a file that does hold a
        // commit id, so that following it would give an answer.
        fs::write(git.join("stray"), A).unwrap();
        for wandering in [
            "ref: refs/../stray\n",
            "ref: stray\n",
            "ref: refs//../stray\n",
        ] {
            fs::write(git.join("HEAD"), wandering).unwrap();
            assert_eq!(head(dir.path()), None, "{wandering:?}");
        }
    }
}
