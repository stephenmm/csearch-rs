//! `csearch::githead::head` reads `HEAD` from a repository's files instead of
//! asking git. The unit tests feed it hand-made files; these check it against
//! the only authority there is, on repositories git itself made -- every
//! shape of checkout it has to get right, and the ones it must admit it
//! cannot read.

mod common;

use common::{git, have_git, text};
use csearch::githead::head;
use std::fs;
use std::path::Path;
use std::process::Command;

/// What git says `HEAD` is, or `None` if it has no answer.
fn rev_parse(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| text(&out.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
}

fn commit(dir: &Path, file: &str) {
    fs::write(dir.join(file), format!("{file}\n")).unwrap();
    assert!(git(dir, &["add", "-A"]));
    assert!(git(dir, &["commit", "-q", "-m", file]));
}

/// `head` and git agree, and git does have an answer.
fn agree(dir: &Path, what: &str) {
    let theirs = rev_parse(dir);
    assert!(theirs.is_some(), "{what}: git has no HEAD here");
    assert_eq!(head(dir), theirs, "{what}");
}

#[test]
fn agrees_with_git_on_every_kind_of_checkout() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(git(&repo, &["init", "-q", "-b", "main"]));

    // No commit yet: neither has an answer.
    assert_eq!(rev_parse(&repo), None);
    assert_eq!(head(&repo), None, "an unborn branch");

    commit(&repo, "one");
    agree(&repo, "a branch, as a loose ref");

    assert!(git(&repo, &["pack-refs", "--all"]));
    assert!(
        !repo.join(".git/refs/heads/main").exists(),
        "pack-refs should have removed the loose ref, or this is not testing a packed one"
    );
    agree(&repo, "a branch that exists only in packed-refs");

    commit(&repo, "two");
    agree(&repo, "a loose ref newer than the packed one");

    let deep = repo.join("a").join("b");
    fs::create_dir_all(&deep).unwrap();
    assert_eq!(head(&deep), rev_parse(&repo), "from a subdirectory");

    assert!(git(&repo, &["checkout", "-q", "--detach"]));
    agree(&repo, "a detached HEAD");
    assert!(git(&repo, &["checkout", "-q", "main"]));

    // A linked work tree: its `.git` is a file, its HEAD lives in a private
    // directory of the main repository, and its branch among the shared refs.
    let wt = dir.path().join("wt");
    assert!(git(
        &repo,
        &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]
    ));
    assert!(wt.join(".git").is_file());
    agree(&wt, "a linked work tree");
    commit(&wt, "three");
    agree(&wt, "a linked work tree, after a commit there");
    assert_ne!(
        head(&wt),
        head(&repo),
        "the two work trees are on different commits"
    );
    agree(&repo, "the main work tree, unaffected");

    // A tag checked out: detached again, through a different route.
    assert!(git(&repo, &["tag", "v1"]));
    assert!(git(&repo, &["checkout", "-q", "v1"]));
    agree(&repo, "a tag checked out");
}

#[test]
fn agrees_with_git_in_a_sha256_repository() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    if !git(
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    ) {
        eprintln!("skipping: this git cannot create a SHA-256 repository");
        return;
    }
    commit(&repo, "one");
    agree(&repo, "a SHA-256 repository");
    assert_eq!(head(&repo).unwrap().len(), 64);
}

#[test]
fn a_reftable_repository_is_unknown_rather_than_wrong() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    if !git(
        &repo,
        &["init", "-q", "-b", "main", "--ref-format=reftable"],
    ) {
        eprintln!("skipping: this git has no reftable backend");
        return;
    }
    commit(&repo, "one");
    assert!(rev_parse(&repo).is_some());
    // The refs are not in files this reads. Saying nothing is right; naming
    // some other commit would make every search claim the index is behind.
    assert_eq!(head(&repo), None);
}

#[test]
fn outside_any_repository_there_is_no_head() {
    // The temp directory's parents are not ours to vouch for, so only make
    // the claim if git agrees there is no repository above.
    let dir = tempfile::tempdir().unwrap();
    if have_git() && rev_parse(dir.path()).is_some() {
        eprintln!("skipping: the temp directory is inside a git repository");
        return;
    }
    assert_eq!(head(dir.path()), None);
}
