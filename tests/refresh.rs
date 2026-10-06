//! Automatic refresh: the git-state stamp, `--if-changed`, the staleness
//! warning, `--background`, and the git hooks. These drive the real binaries;
//! they need `git` on PATH and skip with a message if it is missing.

mod common;

use common::{git, have_git, run_from, text, CINDEX, CSEARCH};
use csearch::names::INDEX_FILE_NAME;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A committed one-file repo, plus a prepared private home.
fn scene() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();
    assert!(git(&root, &["init", "-q"]));
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "one"]));
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    (dir, root, home)
}

/// The `exec` line of a hook script: the program it runs, and its arguments.
fn hook_command(hook: &Path) -> (String, Vec<String>) {
    let body = fs::read_to_string(hook).unwrap_or_else(|_| panic!("missing {}", hook.display()));
    let line = body
        .lines()
        .find_map(|l| l.strip_prefix("exec "))
        .unwrap_or_else(|| panic!("no exec line in {}: {body}", hook.display()));
    // exec "<program>" <args...> -- the program is quoted because paths have
    // spaces; the arguments are plain flags.
    let rest = line.strip_prefix('"').expect("quoted program");
    let (program, args) = rest.split_once('"').expect("closing quote");
    (
        program.to_string(),
        args.split_whitespace().map(str::to_string).collect(),
    )
}

#[test]
fn if_changed_skips_until_a_commit_then_rebuilds() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());

    // Nothing changed: --if-changed does no work and says so.
    let out = run_from(
        CINDEX,
        &root,
        &home,
        &["--local", "--if-changed", "--verbose"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("up to date"),
        "{}",
        text(&out.stderr)
    );

    // A new committed file changes HEAD, so --if-changed must rebuild and the
    // new content becomes searchable.
    fs::write(root.join("b.rs"), "fn beta() {}\n").unwrap();
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "two"]));

    let out = run_from(
        CINDEX,
        &root,
        &home,
        &["--local", "--if-changed", "--verbose"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !text(&out.stderr).contains("up to date"),
        "should have rebuilt: {}",
        text(&out.stderr)
    );
    let found = run_from(CSEARCH, &root, &home, &["-l", "beta"]);
    assert!(
        found.status.success() && text(&found.stdout).contains("b.rs"),
        "{}",
        text(&found.stdout)
    );
}

#[test]
fn if_changed_rebuilds_after_an_uncommitted_edit() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    // Edit without committing: HEAD is unchanged but the working tree is dirty,
    // and the dirty fingerprint must still force a rebuild.
    fs::write(root.join("a.rs"), "fn alpha() {}\nfn gamma() {}\n").unwrap();
    let out = run_from(
        CINDEX,
        &root,
        &home,
        &["--local", "--if-changed", "--verbose"],
    );
    assert!(
        !text(&out.stderr).contains("up to date"),
        "dirty tree must rebuild: {}",
        text(&out.stderr)
    );
    let found = run_from(CSEARCH, &root, &home, &["-l", "gamma"]);
    assert!(
        text(&found.stdout).contains("a.rs"),
        "{}",
        text(&found.stdout)
    );
}

#[test]
fn search_warns_when_the_index_is_behind_head() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());

    // Fresh index: no warning.
    let out = run_from(CSEARCH, &root, &home, &["alpha"]);
    assert!(
        !text(&out.stderr).contains("behind HEAD"),
        "{}",
        text(&out.stderr)
    );

    // Commit without re-indexing: the next search warns, once.
    fs::write(root.join("b.rs"), "fn beta() {}\n").unwrap();
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "two"]));
    let out = run_from(CSEARCH, &root, &home, &["alpha"]);
    assert!(
        text(&out.stderr).contains("behind HEAD"),
        "expected a staleness note: {}",
        text(&out.stderr)
    );
    // The warning does not change the exit status: alpha still matched.
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn install_hooks_writes_four_hooks_and_the_initial_index() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    let out = run_from(CINDEX, &root, &home, &["--install-hooks"]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let hooks = root.join(".git").join("hooks");
    for name in ["post-checkout", "post-merge", "post-commit", "post-rewrite"] {
        let body =
            fs::read_to_string(hooks.join(name)).unwrap_or_else(|_| panic!("missing {name}"));
        assert!(body.contains("csearch-rs"), "{name}: no marker");
        // The hook must run the indexer that installed it -- by its own name,
        // not the original csearch's `cindex`, which would be a different
        // program entirely on a machine with both.
        let (program, args) = hook_command(&hooks.join(name));
        assert_eq!(
            Path::new(&program).file_stem().unwrap().to_str().unwrap(),
            csearch::names::CINDEX,
            "{name}: runs {program}"
        );
        assert_eq!(
            args,
            ["--local", "--if-changed", "--background"],
            "{name}: wrong command"
        );
    }
    // install-hooks implies --local, so the index exists and is searchable now.
    assert!(root.join(INDEX_FILE_NAME).is_file());
    let found = run_from(CSEARCH, &root, &home, &["-l", "alpha"]);
    assert!(
        text(&found.stdout).contains("a.rs"),
        "{}",
        text(&found.stdout)
    );
}

#[test]
fn install_hooks_takes_over_hooks_left_by_the_pre_rename_binary() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    let hooks = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    // Exactly what csearch-rs 0.2 wrote: the marker, and an absolute path to
    // a binary called `cindex`. Left alone, this hook would go on running the
    // old binary -- or, once that is deleted, whatever else is named cindex.
    let old = "#!/bin/sh\n\
               # csearch-rs hook: keep the trigram index fresh (safe to delete)\n\
               exec \"/old/place/cindex\" --local --if-changed --background\n";
    for name in ["post-checkout", "post-merge"] {
        fs::write(hooks.join(name), old).unwrap();
    }

    let out = run_from(CINDEX, &root, &home, &["--install-hooks"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !text(&out.stderr).contains("leaving it alone"),
        "a pre-rename hook is ours and must be refreshed: {}",
        text(&out.stderr)
    );
    for name in ["post-checkout", "post-merge", "post-commit", "post-rewrite"] {
        let (program, _) = hook_command(&hooks.join(name));
        assert_ne!(
            program, "/old/place/cindex",
            "{name} still runs the old binary"
        );
        assert_eq!(
            Path::new(&program).file_stem().unwrap().to_str().unwrap(),
            csearch::names::CINDEX,
            "{name}: runs {program}"
        );
    }

    // And --uninstall-hooks removes a pre-rename hook like any other.
    fs::write(hooks.join("post-merge"), old).unwrap();
    let out = run_from(CINDEX, &root, &home, &["--uninstall-hooks"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    for name in ["post-checkout", "post-merge", "post-commit", "post-rewrite"] {
        assert!(!hooks.join(name).exists(), "{name} survived uninstall");
    }
}

#[test]
fn hooks_leave_foreign_hooks_alone_and_uninstall_only_ours() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    let hooks = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let foreign = hooks.join("post-commit");
    fs::write(&foreign, "#!/bin/sh\necho not ours\n").unwrap();

    let out = run_from(CINDEX, &root, &home, &["--install-hooks"]);
    assert!(out.status.success());
    assert!(
        text(&out.stderr).contains("leaving it alone"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(&foreign).unwrap(),
        "#!/bin/sh\necho not ours\n"
    );
    assert!(
        hooks.join("post-checkout").is_file(),
        "our hooks should still be written"
    );

    let out = run_from(CINDEX, &root, &home, &["--uninstall-hooks"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !hooks.join("post-checkout").exists(),
        "our hook should be gone"
    );
    assert!(foreign.is_file(), "foreign hook must survive uninstall");
}

#[test]
fn background_returns_at_once_and_the_index_appears() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    let index = root.join(INDEX_FILE_NAME);

    let started = Instant::now();
    let out = run_from(CINDEX, &root, &home, &["--local", "--background"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "--background should return promptly"
    );

    // The detached child builds the index shortly after; poll for a search to
    // succeed rather than for the file, so we never read a half-written index.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if index.is_file() {
            let found = run_from(CSEARCH, &root, &home, &["-l", "alpha"]);
            if found.status.success() && text(&found.stdout).contains("a.rs") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "background index never appeared");
        std::thread::sleep(Duration::from_millis(100));
    }
}
