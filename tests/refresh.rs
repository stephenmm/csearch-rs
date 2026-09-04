//! Automatic refresh: the git-state stamp, `--if-changed`, the staleness
//! warning, `--background`, and the git hooks. These drive the real binaries;
//! they need `git` on PATH and skip with a message if it is missing.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Run a binary from `cwd` with a private, git-usable home and no inherited
/// index configuration -- so index resolution is only the walk-up rule and git
/// is not tripped by the empty home on an ownership-less filesystem.
fn run(exe: &str, cwd: &Path, home: &Path, args: &[&str]) -> Output {
    let gitconfig = home.join(".gitconfig");
    if !gitconfig.exists() {
        fs::write(&gitconfig, "[safe]\n\tdirectory = *\n").unwrap();
    }
    Command::new(exe)
        .current_dir(cwd)
        .env_remove("CSEARCHINDEX")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .expect("run")
}

fn git(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
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

/// A committed one-file repo, plus a prepared private home.
fn scene() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
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

const CINDEX: &str = env!("CARGO_BIN_EXE_cindex");
const CSEARCH: &str = env!("CARGO_BIN_EXE_csearch");

#[test]
fn if_changed_skips_until_a_commit_then_rebuilds() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    assert!(run(CINDEX, &root, &home, &["--local"]).status.success());

    // Nothing changed: --if-changed does no work and says so.
    let out = run(
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

    let out = run(
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
    let found = run(CSEARCH, &root, &home, &["-l", "beta"]);
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
    assert!(run(CINDEX, &root, &home, &["--local"]).status.success());
    // Edit without committing: HEAD is unchanged but the working tree is dirty,
    // and the dirty fingerprint must still force a rebuild.
    fs::write(root.join("a.rs"), "fn alpha() {}\nfn gamma() {}\n").unwrap();
    let out = run(
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
    let found = run(CSEARCH, &root, &home, &["-l", "gamma"]);
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
    assert!(run(CINDEX, &root, &home, &["--local"]).status.success());

    // Fresh index: no warning.
    let out = run(CSEARCH, &root, &home, &["alpha"]);
    assert!(
        !text(&out.stderr).contains("behind HEAD"),
        "{}",
        text(&out.stderr)
    );

    // Commit without re-indexing: the next search warns, once.
    fs::write(root.join("b.rs"), "fn beta() {}\n").unwrap();
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "two"]));
    let out = run(CSEARCH, &root, &home, &["alpha"]);
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
    let out = run(CINDEX, &root, &home, &["--install-hooks"]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let hooks = root.join(".git").join("hooks");
    for name in ["post-checkout", "post-merge", "post-commit", "post-rewrite"] {
        let body =
            fs::read_to_string(hooks.join(name)).unwrap_or_else(|_| panic!("missing {name}"));
        assert!(body.contains("csearch-rs"), "{name}: no marker");
        assert!(
            body.contains("--if-changed") && body.contains("--background"),
            "{name}: wrong command"
        );
    }
    // install-hooks implies --local, so the index exists and is searchable now.
    assert!(root.join(".csearchindex").is_file());
    let found = run(CSEARCH, &root, &home, &["-l", "alpha"]);
    assert!(
        text(&found.stdout).contains("a.rs"),
        "{}",
        text(&found.stdout)
    );
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

    let out = run(CINDEX, &root, &home, &["--install-hooks"]);
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

    let out = run(CINDEX, &root, &home, &["--uninstall-hooks"]);
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
    let index = root.join(".csearchindex");

    let started = Instant::now();
    let out = run(CINDEX, &root, &home, &["--local", "--background"]);
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
            let found = run(CSEARCH, &root, &home, &["-l", "alpha"]);
            if found.status.success() && text(&found.stdout).contains("a.rs") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "background index never appeared");
        std::thread::sleep(Duration::from_millis(100));
    }
}
