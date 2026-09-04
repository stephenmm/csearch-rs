//! `cindex --git`: the file list comes from `git ls-files`, so files git
//! ignores are never indexed and the file set matches ripgrep's defaults.
//!
//! These need `git` on PATH and skip with a message if it is missing; CI has
//! it on both platforms.

use csearch::read::Index;
use csearch::write::{build_index, BuildOptions};
use std::fs;
use std::path::Path;
use std::process::Command;

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Indexed names with '/' separators, so assertions read the same on Windows.
fn names(index: &Path) -> Vec<String> {
    let idx = Index::open(index).unwrap();
    (0..idx.num_files())
        .map(|i| idx.name(i).replace('\\', "/"))
        .collect()
}

fn has(names: &[String], suffix: &str) -> bool {
    names.iter().any(|n| n.ends_with(suffix))
}

/// A repository with something ignored, something untracked, and something
/// hidden -- one of each thing the flag has to get right.
fn make_repo(root: &Path) -> bool {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("build")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
    fs::write(root.join("src/a.rs"), "needle tracked\n").unwrap();
    fs::write(root.join("build/out.txt"), "needle ignored\n").unwrap();
    fs::write(root.join("app.log"), "needle ignored too\n").unwrap();
    fs::write(root.join(".hidden"), "needle hidden\n").unwrap();
    if !(git(root, &["init", "-q"])
        && git(root, &["add", "-A"])
        && git(root, &["commit", "-q", "-m", "init"]))
    {
        return false;
    }
    // Untracked but not ignored: a file you just created must still be found.
    fs::write(root.join("src/new.rs"), "needle untracked\n").unwrap();
    true
}

#[test]
fn git_listing_honours_gitignore() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    assert!(make_repo(&root), "could not set up a git repository");

    let out = dir.path().join("index-git");
    let opts = BuildOptions {
        git: true,
        ..Default::default()
    };
    build_index(std::slice::from_ref(&root), &out, &opts).unwrap();
    let n = names(&out);
    assert!(has(&n, "src/a.rs"), "{n:?}");
    assert!(
        has(&n, "src/new.rs"),
        "untracked-but-not-ignored must be indexed: {n:?}"
    );
    assert!(
        !has(&n, "build/out.txt"),
        "ignored directory was indexed: {n:?}"
    );
    assert!(!has(&n, "app.log"), "ignored pattern was indexed: {n:?}");
    assert!(!has(&n, ".hidden"), "dotfile was indexed: {n:?}");
    assert!(!has(&n, ".gitignore"), "dotfile was indexed: {n:?}");

    // The default walk must still index the ignored files -- otherwise the
    // flag is a no-op and this test proves nothing.
    let out2 = dir.path().join("index-walk");
    build_index(std::slice::from_ref(&root), &out2, &BuildOptions::default()).unwrap();
    let n2 = names(&out2);
    assert!(has(&n2, "build/out.txt") && has(&n2, "app.log"), "{n2:?}");
}

#[test]
fn git_listing_falls_back_outside_a_repository() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("f.txt"), "needle\n").unwrap();
    let out = dir.path().join("index");
    let opts = BuildOptions {
        git: true,
        ..Default::default()
    };
    let stats = build_index(std::slice::from_ref(&plain), &out, &opts).unwrap();
    assert_eq!(
        stats.files_indexed, 1,
        "the fallback walk must still index the file"
    );
}
