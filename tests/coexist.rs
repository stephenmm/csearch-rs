//! csearch-rs is meant to be installed next to the original csearch. These
//! check that neither can get at the other's files: first against stand-ins
//! for what the original leaves on disk, then -- when it is installed --
//! against the real thing.

mod common;

use common::{command_from, git, have_git, text, CINDEX, CSEARCH};
use csearch::names::{self, original, INDEX_ENV, INDEX_FILE_NAME};
use csearch::paths::is_csearch_rs_index;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

/// What an index written by the original looks like from the outside: its own
/// magic, then data that means nothing to csearch-rs.
const THEIRS: &[u8] = b"csearch index 1\n\0\0\0 not a csearch-rs index \0\0\0";

/// A directory with one file to find.
fn corpus(dir: &Path) -> PathBuf {
    let root = dir.join("corpus");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), "needle here\n").unwrap();
    root
}

fn home_in(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();
    home
}

/// Enough to tell whether a file was rewritten, even with identical bytes.
fn state(path: &Path) -> (Vec<u8>, SystemTime) {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    (bytes, fs::metadata(path).unwrap().modified().unwrap())
}

fn lists(out: &Output, file: &str) -> bool {
    text(&out.stdout).lines().any(|l| l.ends_with(file))
}

#[test]
fn the_originals_variable_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = corpus(dir.path());
    let home = home_in(dir.path());
    let theirs = dir.path().join("theirs.index");
    // Only the original's variable is set -- as on a machine where someone
    // uses the original and has not configured csearch-rs at all.
    let run = |exe: &str, args: &[&str]| {
        command_from(exe, dir.path(), &home)
            .env(original::INDEX_ENV, &theirs)
            .args(args)
            .output()
            .unwrap()
    };

    let out = run(CINDEX, &[root.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !theirs.exists(),
        "the index was written where ${} points",
        original::INDEX_ENV
    );
    assert!(
        home.join(INDEX_FILE_NAME).is_file(),
        "the index should be at the home default: {}",
        text(&out.stderr)
    );

    // Searching resolves the same way, and stays quiet about a variable that
    // is somebody else's business.
    let out = run(CSEARCH, &["-l", "needle"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(lists(&out, "a.txt"), "{}", text(&out.stdout));
    assert_eq!(text(&out.stderr), "", "nothing to say about their variable");

    // With the original's index really there, it is still not opened.
    fs::write(&theirs, THEIRS).unwrap();
    let before = state(&theirs);
    let out = run(CSEARCH, &["-l", "needle"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(run(CINDEX, &[]).status.success());
    assert!(run(CINDEX, &["--reset"]).status.success());
    assert_eq!(state(&theirs), before, "the original's index was touched");
}

#[test]
fn the_originals_index_files_are_never_touched() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = home_in(dir.path());
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/a.rs"), "needle\n").unwrap();
    assert!(git(&repo, &["init", "-q"]));
    assert!(git(&repo, &["add", "-A"]));
    assert!(git(&repo, &["commit", "-q", "-m", "one"]));

    // The original's index in both places csearch-rs keeps its own: the home
    // directory and (as some forks of the original do) the project root.
    let at_home = home.join(original::INDEX_FILE_NAME);
    let at_root = repo.join(original::INDEX_FILE_NAME);
    fs::write(&at_home, THEIRS).unwrap();
    fs::write(&at_root, THEIRS).unwrap();
    let before = (state(&at_home), state(&at_root));

    let run = |exe: &str, cwd: &Path, args: &[&str]| {
        let out = command_from(exe, cwd, &home).args(args).output().unwrap();
        assert_eq!(
            (state(&at_home), state(&at_root)),
            before,
            "{exe} {args:?} touched the original's index"
        );
        // Their file is not a leftover of ours, so there is nothing to say
        // about it.
        assert!(
            !text(&out.stderr).contains("note:"),
            "{exe} {args:?}: {}",
            text(&out.stderr)
        );
        out
    };

    // Every operation that reads, writes or deletes an index, against both
    // the project index and the home one.
    let src = repo.join("src");
    assert!(run(CINDEX, &src, &["--local"]).status.success());
    assert!(run(CSEARCH, &src, &["needle"]).status.success());
    assert!(run(CINDEX, &src, &["--local", "--if-changed"])
        .status
        .success());
    assert!(run(CINDEX, &src, &["--list"]).status.success());
    assert!(run(CINDEX, &src, &["--install-hooks"]).status.success());
    assert!(run(CINDEX, &src, &["--uninstall-hooks"]).status.success());
    assert!(run(CINDEX, &src, &["--reset"]).status.success());
    assert!(run(CINDEX, dir.path(), &[repo.to_str().unwrap()])
        .status
        .success());
    assert!(run(CSEARCH, dir.path(), &["needle"]).status.success());
    assert!(run(CINDEX, dir.path(), &["--reset"]).status.success());
    // With no index of ours left, a search fails -- and still must not fall
    // back to theirs, nor describe it as one of ours.
    assert_eq!(run(CSEARCH, &src, &["needle"]).status.code(), Some(2));

    // git was told to ignore our file, and only ours.
    let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|l| l == INDEX_FILE_NAME), "{exclude}");
    assert!(
        !exclude.lines().any(|l| l == original::INDEX_FILE_NAME),
        "{exclude}"
    );
}

#[test]
fn an_index_from_before_the_rename_is_pointed_out_but_never_used() {
    let dir = tempfile::tempdir().unwrap();
    let home = home_in(dir.path());
    let proj = dir.path().join("proj");
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::write(proj.join("src/a.txt"), "needle\n").unwrap();
    let run = |exe: &str, cwd: &Path, args: &[&str]| {
        command_from(exe, cwd, &home).args(args).output().unwrap()
    };

    // What csearch-rs 0.2 left at a project root: a real index of ours, under
    // the original's file name.
    let old = proj.join(original::INDEX_FILE_NAME);
    let out = run(
        CINDEX,
        &proj,
        &["--indexpath", old.to_str().unwrap(), proj.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(is_csearch_rs_index(&old));
    let before = state(&old);

    // It is a perfectly good index, and it must still not be used: the name
    // is the original's now. The error says where it is and what to do.
    let out = run(CSEARCH, &proj.join("src"), &["needle"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stdout));
    let err = text(&out.stderr);
    assert!(err.contains("no index at"), "{err}");
    let note = err
        .lines()
        .find(|l| l.starts_with("note: "))
        .unwrap_or_else(|| panic!("no note for someone upgrading: {err}"));
    assert!(note.contains(old.to_str().unwrap()), "{note}");
    assert!(note.contains(&format!("`{}`", names::CINDEX)), "{note}");

    // The same puzzle from the indexer's side: "re-index" finds nothing.
    let out = run(CINDEX, &proj.join("src"), &[]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(
        err.lines()
            .any(|l| l.starts_with("note: ") && l.contains(old.to_str().unwrap())),
        "{err}"
    );

    // Building the new index mentions the leftover once more, and leaves it.
    let out = run(CINDEX, &proj, &["--local"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(
        err.lines()
            .any(|l| l.contains("note: ") && l.contains(old.to_str().unwrap())),
        "{err}"
    );
    assert_eq!(state(&old), before, "the leftover must not be modified");
    assert!(proj.join(INDEX_FILE_NAME).is_file());

    // Now there is an index, so searching works and has nothing to add.
    let out = run(CSEARCH, &proj.join("src"), &["-l", "needle"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(lists(&out, "a.txt"));
    assert_eq!(text(&out.stderr), "");

    // Once the leftover is deleted, so is the note.
    fs::remove_file(&old).unwrap();
    let out = run(CINDEX, &proj, &["--local"]);
    assert!(out.status.success());
    assert!(
        !text(&out.stderr).contains("note:"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn the_originals_variable_is_mentioned_only_when_no_index_turns_up() {
    let dir = tempfile::tempdir().unwrap();
    let root = corpus(dir.path());
    let home = home_in(dir.path());
    let theirs = dir.path().join("theirs.index");
    let ours = dir.path().join("ours.index");
    let run = |exe: &str, env: &[(&str, &Path)], args: &[&str]| {
        let mut cmd = command_from(exe, dir.path(), &home);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.args(args).output().unwrap()
    };
    let their_var = format!("${}", original::INDEX_ENV);
    let our_var = format!("${INDEX_ENV}");

    // Someone who set the variable for csearch-rs 0.2 and has just upgraded:
    // no index is found, and the reason is not obvious without being told.
    let only_theirs = [(original::INDEX_ENV, theirs.as_path())];
    let out = run(CSEARCH, &only_theirs, &["needle"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    let note = err
        .lines()
        .find(|l| l.starts_with("note: "))
        .unwrap_or_else(|| panic!("no note about the variable: {err}"));
    assert!(
        note.contains(&their_var) && note.contains(&our_var),
        "{note}"
    );

    // Once our own variable is set, theirs is nobody's mistake -- even when
    // the index it names does not exist yet.
    let both = [
        (original::INDEX_ENV, theirs.as_path()),
        (INDEX_ENV, ours.as_path()),
    ];
    let out = run(CSEARCH, &both, &["needle"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(err.contains(ours.to_str().unwrap()), "{err}");
    assert!(!err.contains("note:"), "{err}");

    // And with an index in place, a machine that runs both tools must not be
    // nagged about the other one's configuration on every search.
    assert!(run(CINDEX, &both, &[root.to_str().unwrap()])
        .status
        .success());
    assert!(ours.is_file() && !theirs.exists());
    let out = run(CSEARCH, &both, &["-l", "needle"]);
    assert!(out.status.success());
    assert_eq!(text(&out.stderr), "");
    let out = run(CSEARCH, &only_theirs, &["needle"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "without our variable the index at {} is not the one in use",
        ours.display()
    );
}

// ---------------------------------------------------------------- the real one

/// Set to `1` where the original is known to be installed (CI does this), so
/// that its absence fails the test instead of skipping it.
const REQUIRE_ORIGINAL: &str = "CSEARCH_RS_REQUIRE_ORIGINAL";

/// The first line `cindex -help` prints, for every release of the original to
/// date. It is how a binary called `cindex` is told from a csearch-rs build
/// from before the rename, which answers to the same name.
const ORIGINAL_USAGE: &str = "usage: cindex [-list] [-reset] [path...]";

/// Google's own `cindex` and `csearch`, or why they could not be found.
fn the_original() -> Result<(PathBuf, PathBuf), String> {
    let exe = std::env::consts::EXE_SUFFIX;
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    // `go install` puts them here, which is often not on PATH.
    if let Ok(out) = Command::new("go").args(["env", "GOPATH"]).output() {
        if out.status.success() {
            for gopath in std::env::split_paths(text(&out.stdout).trim()) {
                dirs.push(gopath.join("bin"));
            }
        }
    }
    let mut rejected = Vec::new();
    for dir in dirs {
        let cindex = dir.join(format!("cindex{exe}"));
        let csearch = dir.join(format!("csearch{exe}"));
        if !(cindex.is_file() && csearch.is_file()) {
            continue;
        }
        let Ok(out) = Command::new(&cindex).arg("-help").output() else {
            continue;
        };
        let said = text(&out.stdout) + &text(&out.stderr);
        if said.lines().next() == Some(ORIGINAL_USAGE) {
            return Ok((cindex, csearch));
        }
        rejected.push(format!(
            "{} is not the original (its -help begins {:?})",
            cindex.display(),
            said.lines().next().unwrap_or("")
        ));
    }
    if rejected.is_empty() {
        Err("no cindex/csearch pair on PATH or in `go env GOPATH`/bin".into())
    } else {
        Err(rejected.join("; "))
    }
}

#[test]
fn side_by_side_with_the_real_original() {
    let (go_cindex, go_csearch) = match the_original() {
        Ok(pair) => pair,
        Err(why) => {
            assert!(
                std::env::var(REQUIRE_ORIGINAL).as_deref() != Ok("1"),
                "{REQUIRE_ORIGINAL}=1 but the original csearch was not found: {why}"
            );
            eprintln!("skipping: the original csearch is not installed ({why})");
            return;
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let root = corpus(dir.path());
    let home = home_in(dir.path());
    let root_arg = root.to_str().unwrap();
    // One environment for all four programs: a private home, and neither
    // index variable -- the defaults are what must not collide.
    let run = |exe: &Path, args: &[&str]| {
        command_from(exe.to_str().unwrap(), dir.path(), &home)
            .args(args)
            .output()
            .unwrap()
    };
    let ok = |exe: &Path, args: &[&str]| {
        let out = run(exe, args);
        assert!(
            out.status.success(),
            "{} {args:?}: {}{}",
            exe.display(),
            text(&out.stdout),
            text(&out.stderr)
        );
        out
    };
    let (rs_cindex, rs_csearch) = (Path::new(CINDEX), Path::new(CSEARCH));
    let theirs = home.join(original::INDEX_FILE_NAME);
    let ours = home.join(INDEX_FILE_NAME);

    // Each builds its own index in the same home directory.
    ok(&go_cindex, &[root_arg]);
    assert!(theirs.is_file(), "the original did not write {theirs:?}");
    assert!(!ours.exists(), "the original wrote our file");
    assert!(!is_csearch_rs_index(&theirs));
    let theirs_v1 = state(&theirs);

    ok(rs_cindex, &[root_arg]);
    assert!(ours.is_file());
    assert!(is_csearch_rs_index(&ours));
    assert_eq!(state(&theirs), theirs_v1, "we rewrote the original's index");

    // Each searches its own.
    assert!(lists(&ok(&go_csearch, &["-l", "needle"]), "a.txt"));
    assert!(lists(&ok(rs_csearch, &["-l", "needle"]), "a.txt"));

    // A new file, re-indexed by the original only: it finds it, we do not --
    // so neither run reached across. (`-reset <path>` rather than a bare
    // `cindex`: on Windows the original cannot replace an index it has
    // mapped, so its in-place refresh reports "done" and changes nothing.)
    fs::write(root.join("b.txt"), "marker_two\n").unwrap();
    let ours_v1 = state(&ours);
    ok(&go_cindex, &["-reset", root_arg]);
    assert_eq!(state(&ours), ours_v1, "the original rewrote our index");
    assert!(lists(&ok(&go_csearch, &["-l", "marker_two"]), "b.txt"));
    assert_eq!(
        run(rs_csearch, &["-l", "marker_two"]).status.code(),
        Some(1),
        "our index cannot know about a file only the original re-indexed"
    );
    // Then by us: now both do, and theirs was not rewritten to get there.
    let theirs_v2 = state(&theirs);
    ok(rs_cindex, &[]);
    assert_eq!(state(&theirs), theirs_v2, "we rewrote the original's index");
    assert!(lists(&ok(rs_csearch, &["-l", "marker_two"]), "b.txt"));

    // With both variables exported at once, each tool obeys only its own.
    let go_idx = dir.path().join("x").join("go.idx");
    let rs_idx = dir.path().join("x").join("rs.idx");
    fs::create_dir_all(go_idx.parent().unwrap()).unwrap();
    for exe in [go_cindex.as_path(), rs_cindex] {
        let out = command_from(exe.to_str().unwrap(), dir.path(), &home)
            .env(original::INDEX_ENV, &go_idx)
            .env(INDEX_ENV, &rs_idx)
            .arg(root_arg)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    assert!(go_idx.is_file() && !is_csearch_rs_index(&go_idx));
    assert!(rs_idx.is_file() && is_csearch_rs_index(&rs_idx));

    // Deleting one index leaves the other.
    ok(rs_cindex, &["--reset"]);
    assert!(!ours.exists());
    assert_eq!(state(&theirs), theirs_v2, "our --reset touched theirs");
    ok(rs_cindex, &[root_arg]);
    ok(&go_cindex, &["-reset"]);
    assert!(
        !theirs.exists(),
        "the original's -reset should remove its index"
    );
    assert!(ours.is_file(), "the original's -reset removed our index");
    assert!(lists(&ok(rs_csearch, &["-l", "needle"]), "a.txt"));
}
