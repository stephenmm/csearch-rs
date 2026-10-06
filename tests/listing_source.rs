//! The index remembers how each root is listed -- walked, or taken from git --
//! and a re-index lists it the same way. Before it did, an index built with
//! `--local` (git's file list) was rebuilt by plain `cindex-rs` as a walk, and
//! every ignored file quietly arrived in it.

mod common;

use common::{
    as_another_format_version, git, have_git, run_from, settle, text, with_index, CINDEX, CSEARCH,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

/// A committed repository with one tracked file and one ignored file, both
/// containing `needle`: which of them a search finds shows how the root was
/// listed.
fn repo_in(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("build")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n").unwrap();
    fs::write(root.join("src/a.rs"), "needle tracked\n").unwrap();
    fs::write(root.join("build/out.txt"), "needle ignored\n").unwrap();
    assert!(git(&root, &["init", "-q"]));
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "init"]));
    root
}

fn home_in(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();
    home
}

/// The file names a search for `needle` lists, sorted.
fn found(out: &Output) -> Vec<String> {
    let mut names: Vec<String> = text(&out.stdout)
        .lines()
        .map(|l| {
            Path::new(l)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// `--list --verbose` as (source, path) pairs.
fn listed_roots(out: &Output) -> Vec<(String, String)> {
    text(&out.stdout)
        .lines()
        .map(|l| {
            let (source, path) = l
                .split_once('\t')
                .unwrap_or_else(|| panic!("not `source<TAB>path`: {l:?}"));
            (source.to_string(), path.to_string())
        })
        .collect()
}

#[test]
fn a_reindex_lists_each_root_the_way_it_was_built() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = repo_in(dir.path());
    let home = home_in(dir.path());
    let search = || found(&run_from(CSEARCH, &root, &home, &["-l", "needle"]));

    // --local takes the file list from git: the ignored file is not there.
    let out = run_from(CINDEX, &root, &home, &["--local"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(search(), ["a.rs"]);

    // "Re-index whichever index applies here" must mean the same files. It
    // used to walk, and `build/out.txt` came back.
    let out = run_from(CINDEX, &root.join("src"), &home, &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(search(), ["a.rs"], "a plain re-index changed the file set");

    // And the index says how it lists the root.
    let out = run_from(CINDEX, &root, &home, &["--list", "--verbose"]);
    let roots = listed_roots(&out);
    assert_eq!(roots.len(), 1, "{roots:?}");
    assert_eq!(roots[0].0, "git");
}

#[test]
fn a_listing_flag_changes_the_source_and_the_change_sticks() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = repo_in(dir.path());
    let home = home_in(dir.path());
    let run = |args: &[&str]| {
        let out = run_from(CINDEX, &root, &home, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        out
    };
    let search = || found(&run_from(CSEARCH, &root, &home, &["-l", "needle"]));

    run(&["--local"]);
    assert_eq!(search(), ["a.rs"]);

    // --walk, with no path, switches every root; a plain re-index keeps it.
    run(&["--walk"]);
    assert_eq!(search(), ["a.rs", "out.txt"]);
    run(&[]);
    assert_eq!(search(), ["a.rs", "out.txt"], "--walk did not stick");
    assert_eq!(listed_roots(&run(&["--list", "--verbose"]))[0].0, "walk");

    // --local on a root that already has a source keeps that source...
    run(&["--local"]);
    assert_eq!(search(), ["a.rs", "out.txt"], "--local overrode --walk");

    // ...and --git switches it back.
    run(&["--git"]);
    assert_eq!(search(), ["a.rs"]);
    run(&[]);
    assert_eq!(search(), ["a.rs"], "--git did not stick");

    // The older spelling of --walk still works.
    run(&["--local", "--no-git"]);
    assert_eq!(search(), ["a.rs", "out.txt"]);
}

#[test]
fn roots_in_one_index_keep_their_own_sources() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = repo_in(dir.path());
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("p.txt"), "needle plain\n").unwrap();
    let index = dir.path().join("index");
    let cindex = |args: &[&str]| {
        let out = with_index(CINDEX, &index, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        out
    };
    let search = || found(&with_index(CSEARCH, &index, &["-l", "needle"]));
    let sources = || -> Vec<String> {
        // In path order: "plain" sorts before "repo".
        listed_roots(&cindex(&["--list", "--verbose"]))
            .into_iter()
            .map(|(source, _)| source)
            .collect()
    };

    // One root through git...
    cindex(&["--git", root.to_str().unwrap()]);
    assert_eq!(search(), ["a.rs"]);
    // ...and a second added with no flag: it is walked, and the first is not
    // converted along with it.
    cindex(&[plain.to_str().unwrap()]);
    assert_eq!(search(), ["a.rs", "p.txt"]);
    assert_eq!(sources(), ["walk", "git"]);

    // A flag with a path applies to that path alone.
    cindex(&["--walk", root.to_str().unwrap()]);
    assert_eq!(search(), ["a.rs", "out.txt", "p.txt"]);
    assert_eq!(sources(), ["walk", "walk"]);
    cindex(&["--git", root.to_str().unwrap()]);
    assert_eq!(sources(), ["walk", "git"]);

    // Naming a root again without a flag leaves its source alone.
    cindex(&[root.to_str().unwrap()]);
    assert_eq!(sources(), ["walk", "git"]);
    assert_eq!(search(), ["a.rs", "p.txt"]);

    // Plain --list is still just the paths, one per line.
    let plain_list = text(&cindex(&["--list"]).stdout);
    assert_eq!(plain_list.lines().count(), 2);
    assert!(!plain_list.contains('\t'), "{plain_list:?}");
}

#[test]
fn local_outside_a_repository_simply_walks() {
    // There is no git to ask and nothing to apologise for: no note about
    // falling back, and the root is recorded as walked.
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("p.txt"), "needle\n").unwrap();
    let home = home_in(dir.path());
    let out = run_from(CINDEX, &plain, &home, &["--local"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !text(&out.stderr).contains("git"),
        "nothing here involves git: {}",
        text(&out.stderr)
    );
    let roots = listed_roots(&run_from(CINDEX, &plain, &home, &["--list", "--verbose"]));
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].0, "walk");
}

#[test]
fn a_source_from_a_newer_version_is_searchable_but_not_guessed_at() {
    // An index written by a later csearch-rs may list a root in a way this
    // one has never heard of. Searching it needs no listing, so it works;
    // re-indexing would have to invent one, so it refuses and says why.
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("p.txt"), "needle\n").unwrap();
    let index = dir.path().join("index");
    assert!(with_index(CINDEX, &index, &[plain.to_str().unwrap()])
        .status
        .success());

    // Same length, so no offset moves: "walk" -> "warp".
    let mut bytes = fs::read(&index).unwrap();
    let at = bytes
        .windows(6)
        .position(|w| w == b"\0walk\0")
        .expect("the source tag follows the root path");
    bytes[at + 1..at + 5].copy_from_slice(b"warp");
    fs::write(&index, &bytes).unwrap();

    let out = with_index(CSEARCH, &index, &["-l", "needle"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(found(&out), ["p.txt"]);

    let out = with_index(CINDEX, &index, &[]);
    assert!(
        !out.status.success(),
        "re-indexed with a source it does not know"
    );
    let err = text(&out.stderr);
    assert!(err.contains("warp"), "{err}");
    // The index is left exactly as it was.
    assert_eq!(fs::read(&index).unwrap(), bytes);
}

#[test]
fn local_builds_again_an_index_written_by_another_version() {
    // The format changed in 0.4. An index is a cache, and --local knows what
    // of, so an old one is no reason to send anybody to --reset.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("p.txt"), "needle\n").unwrap();
    settle(&root);
    let home = home_in(dir.path());
    let index = root.join(csearch::names::INDEX_FILE_NAME);
    // A real index first, so that its stamp is there and matches the tree:
    // the stamp's format need not change when the index's does.
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    let old = as_another_format_version(&index);

    // A search says what is wrong and what to do.
    let out = run_from(CSEARCH, &root, &home, &["-l", "needle"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("different index format version"),
        "{}",
        text(&out.stderr)
    );
    // A plain re-index would need the list of roots, which is in the file it
    // cannot read. It says the same, and leaves the file alone.
    let out = run_from(CINDEX, &root, &home, &[]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("different index format version"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fs::read(&index).unwrap(), old);

    // --local needs no list. And --if-changed, which the hooks of 0.3 pass,
    // must not be talked out of it by a stamp saying no file has changed:
    // none has, and the index still cannot be read.
    let out = run_from(CINDEX, &root, &home, &["--local", "--if-changed"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("written by another version"),
        "{}",
        text(&out.stderr)
    );
    let out = run_from(CSEARCH, &root, &home, &["-l", "needle"]);
    assert_eq!(found(&out), ["p.txt"], "{}", text(&out.stderr));
}

#[test]
fn local_does_not_replace_a_file_it_cannot_recognise() {
    // Something else under the index's name -- damaged, or never an index at
    // all -- is not ours to overwrite on the strength of where it is.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("p.txt"), "needle\n").unwrap();
    let home = home_in(dir.path());
    let index = root.join(csearch::names::INDEX_FILE_NAME);
    for unknown in [
        &b"notes to self: buy milk\n"[..],
        &b"csearch-rs"[..],
        &b""[..],
    ] {
        fs::write(&index, unknown).unwrap();
        let out = run_from(CINDEX, &root, &home, &["--local"]);
        assert!(!out.status.success(), "replaced {unknown:?}");
        assert_eq!(fs::read(&index).unwrap(), unknown);
    }
}
