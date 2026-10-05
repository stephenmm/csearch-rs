//! Automatic refresh: `--if-changed`, the stamp it compares against, the
//! staleness note, `--background`, and the git hooks. These drive the real
//! binaries. The ones that need `git` skip with a message if it is missing.

mod common;

use common::{command_from, git, have_git, run_from, set_mtime, settle, text, CINDEX, CSEARCH};
use csearch::listing::{snapshot, ListOptions, Root, Source};
use csearch::lock;
use csearch::names::INDEX_FILE_NAME;
use csearch::stamp::{self, Verdict};
use csearch::trigram::MAX_FILE_LEN;
use csearch::write::{build_from, BuildOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A committed one-file repo, plus a prepared private home.
fn scene() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();
    assert!(git(&root, &["init", "-q"]));
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "one"]));
    settle(&root);
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    (dir, root, home)
}

/// A directory that no version-control system knows about.
fn plain_scene() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("plain");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), "alpha\n").unwrap();
    fs::write(root.join("sub/b.txt"), "beta\n").unwrap();
    settle(&root);
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    (dir, root, home)
}

/// `cindex-rs --local --if-changed --verbose`, which says what it decided.
fn refresh(root: &Path, home: &Path) -> Output {
    let out = run_from(
        CINDEX,
        root,
        home,
        &["--local", "--if-changed", "--verbose"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    out
}

fn skipped(out: &Output) -> bool {
    text(&out.stderr).contains("up to date")
}

fn finds(root: &Path, home: &Path, pattern: &str, file: &str) -> bool {
    let out = run_from(CSEARCH, root, home, &["-l", pattern]);
    text(&out.stdout).lines().any(|l| l.ends_with(file))
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
fn if_changed_skips_until_something_changes_then_rebuilds() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());

    // Nothing changed: --if-changed does no work and says so.
    assert!(skipped(&refresh(&root, &home)));

    // A new committed file must be picked up.
    fs::write(root.join("b.rs"), "fn beta() {}\n").unwrap();
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "two"]));
    settle(&root);
    let out = refresh(&root, &home);
    assert!(!skipped(&out), "should have rebuilt: {}", text(&out.stderr));
    assert!(finds(&root, &home, "beta", "b.rs"));
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
    // Edit without committing: nothing git records has moved, and the edit
    // must still force a rebuild.
    fs::write(root.join("a.rs"), "fn alpha() {}\nfn gamma() {}\n").unwrap();
    settle(&root);
    let out = refresh(&root, &home);
    assert!(
        !skipped(&out),
        "an edited file must rebuild: {}",
        text(&out.stderr)
    );
    assert!(finds(&root, &home, "gamma", "a.rs"));
}

#[test]
fn if_changed_catches_a_second_edit_to_a_file_that_was_already_modified() {
    // What comparing `git status` could not see: a file that is already
    // "modified" stays "modified" however many more times it is edited, so
    // the second edit left the index stale with nothing to say so.
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    fs::write(root.join("a.rs"), "fn alpha() {}\nfn first_edit() {}\n").unwrap();
    settle(&root);
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    assert!(skipped(&refresh(&root, &home)));

    fs::write(root.join("a.rs"), "fn alpha() {}\nfn second_edit() {}\n").unwrap();
    settle(&root);
    let out = refresh(&root, &home);
    assert!(
        !skipped(&out),
        "the second edit went unnoticed: {}",
        text(&out.stderr)
    );
    assert!(finds(&root, &home, "second_edit", "a.rs"));
}

#[test]
fn a_commit_that_changes_no_file_needs_no_rebuild_and_leaves_no_stale_note() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (_dir, root, home) = scene();
    fs::write(root.join("b.rs"), "fn beta() {}\n").unwrap();
    settle(&root);
    // Indexed with the new file in place but not yet committed.
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());

    // Committing it moves HEAD and touches nothing in the work tree...
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "two"]));
    // ...so a search right now is told the index is behind,
    let out = run_from(CSEARCH, &root, &home, &["beta"]);
    assert!(
        text(&out.stderr).contains("behind HEAD"),
        "{}",
        text(&out.stderr)
    );
    // the refresh has nothing to rebuild,
    let out = refresh(&root, &home);
    assert!(
        skipped(&out),
        "no file changed, yet it rebuilt: {}",
        text(&out.stderr)
    );
    // and having looked, it records where HEAD is now -- or the note would
    // go on appearing after every search until some file happened to change.
    let out = run_from(CSEARCH, &root, &home, &["beta"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stderr), "", "the note outlived the refresh");
}

#[test]
fn if_changed_needs_no_version_control_at_all() {
    // A plain directory used to be rebuilt every time: with only git to ask,
    // a root that was not a repository could never be shown unchanged.
    let (_dir, root, home) = plain_scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    assert!(skipped(&refresh(&root, &home)), "unchanged, yet rebuilt");

    // Each kind of change is seen, and seen once. Every write is settled
    // before the refresh looks, so that nothing here is "too recent to trust"
    // and only the comparison of the listings can explain a rebuild.
    fs::write(root.join("a.txt"), "alpha and more\n").unwrap();
    settle(&root);
    assert!(!skipped(&refresh(&root, &home)), "an edit went unnoticed");
    assert!(skipped(&refresh(&root, &home)));

    fs::write(root.join("sub/new.txt"), "gamma\n").unwrap();
    settle(&root);
    assert!(
        !skipped(&refresh(&root, &home)),
        "a new file went unnoticed"
    );
    assert!(finds(&root, &home, "gamma", "new.txt"));
    assert!(skipped(&refresh(&root, &home)));

    fs::remove_file(root.join("sub/b.txt")).unwrap();
    assert!(
        !skipped(&refresh(&root, &home)),
        "a deletion went unnoticed"
    );
    assert!(skipped(&refresh(&root, &home)));

    fs::rename(root.join("a.txt"), root.join("renamed.txt")).unwrap();
    assert!(!skipped(&refresh(&root, &home)), "a rename went unnoticed");
    assert!(skipped(&refresh(&root, &home)));

    // Same size, different time: an edit that kept the length. The time is
    // set by hand -- as old as the others, but not the same -- so the size
    // is equal, nothing is recent, and the timestamp is all there is to see.
    let before = fs::metadata(root.join("renamed.txt")).unwrap();
    fs::write(root.join("renamed.txt"), "ALPHA AND MORE\n").unwrap();
    let earlier = before.modified().unwrap() - Duration::from_secs(20);
    set_mtime(&root.join("renamed.txt"), earlier);
    let after = fs::metadata(root.join("renamed.txt")).unwrap();
    assert_eq!(after.len(), before.len(), "the edit must keep the size");
    assert_ne!(after.modified().unwrap(), before.modified().unwrap());
    assert!(
        !skipped(&refresh(&root, &home)),
        "a same-size edit went unnoticed"
    );
    assert!(finds(&root, &home, "ALPHA", "renamed.txt"));
    assert!(skipped(&refresh(&root, &home)));
}

#[test]
fn a_change_of_listing_is_a_change() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    // The same root with the same files in it is a different index when it
    // is walked rather than listed through git.
    let (_dir, root, home) = scene();
    fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(root.join("ignored.rs"), "fn hidden() {}\n").unwrap();
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "ignore"]));
    settle(&root);
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    assert!(skipped(&refresh(&root, &home)));
    assert!(!finds(&root, &home, "hidden", "ignored.rs"));

    let out = run_from(
        CINDEX,
        &root,
        &home,
        &["--walk", "--if-changed", "--verbose"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!skipped(&out), "{}", text(&out.stderr));
    assert!(finds(&root, &home, "hidden", "ignored.rs"));
}

#[test]
fn the_stamp_describes_the_files_as_listed_not_as_they_are_after_the_build() {
    // A file that changes while the build is running was read in one state
    // or the other; either way it must not be recorded as indexed. The stamp
    // used to be taken after the build, and recorded exactly that.
    let (_dir, root, _home) = plain_scene();
    let index = root.parent().unwrap().join("index");
    let roots = [Root {
        path: csearch::paths::canonical_string(&root).unwrap(),
        source: Source::Walk,
    }];
    let opts = ListOptions {
        max_file_bytes: MAX_FILE_LEN,
        strict: false,
    };

    let listed = snapshot(&roots, &opts).unwrap();
    // The build has listed the files and is about to read them...
    fs::write(root.join("a.txt"), "alpha, changed mid-build\n").unwrap();
    let built = build_from(&listed, &index, &BuildOptions::default()).unwrap();
    stamp::write(&index, &listed, built.index_bytes);

    // (Settled, so that the file is not merely "too recent to trust": the
    // listings themselves have to differ.)
    settle(&root);
    let now = snapshot(&roots, &opts).unwrap();
    assert_ne!(now.fingerprint, listed.fingerprint);
    assert!(
        matches!(stamp::check(&index, &now).0, Verdict::Stale(_)),
        "a file modified during the build was recorded as indexed"
    );

    // With nothing happening in between, the same sequence is current. (The
    // file just written is settled first: on a file system with whole-second
    // timestamps it would otherwise be too recent to vouch for.)
    settle(&root);
    let listed = snapshot(&roots, &opts).unwrap();
    let built = build_from(&listed, &index, &BuildOptions::default()).unwrap();
    stamp::write(&index, &listed, built.index_bytes);
    let now = snapshot(&roots, &opts).unwrap();
    assert_eq!(stamp::check(&index, &now).0, Verdict::Current);
}

/// The start of a second, `offset` seconds from the current one: a timestamp
/// as a file system that keeps whole seconds would store it.
fn whole_second(offset: i64) -> SystemTime {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    UNIX_EPOCH + Duration::from_secs(now.as_secs().checked_add_signed(offset).unwrap())
}

#[test]
fn whole_second_timestamps_are_not_trusted_near_the_build() {
    // FAT keeps two-second timestamps, HFS+ and ext3 whole seconds. There, a
    // file written just before the build and again just after it can show
    // the same size and the same time, so "nothing changed" is not provable
    // for anything modified within a tick of the build.
    let (_dir, root, home) = plain_scene();
    let files = [root.join("a.txt"), root.join("sub/b.txt")];

    // Every timestamp on a whole second, and one of them not safely older
    // than the build. (A few seconds ahead rather than "just now", so the
    // test does not depend on how quickly the indexer starts.)
    set_mtime(&files[0], whole_second(-40));
    set_mtime(&files[1], whole_second(3));
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    let out = refresh(&root, &home);
    assert!(
        !skipped(&out),
        "trusted a whole-second timestamp no older than the build: {}",
        text(&out.stderr)
    );
    assert!(
        text(&out.stderr).contains("too close"),
        "{}",
        text(&out.stderr)
    );

    // The same files, long settled: now it can be believed.
    set_mtime(&files[1], whole_second(-30));
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    assert!(skipped(&refresh(&root, &home)), "old files, yet rebuilt");

    // And where the file system does keep fractions of a second, a file as
    // recent as the first one is no reason to rebuild: a single sub-second
    // timestamp shows what the file system can do.
    let fine = whole_second(3) + Duration::from_millis(250);
    set_mtime(&files[1], fine);
    if fs::metadata(&files[1]).unwrap().modified().unwrap() != fine {
        eprintln!("skipping the last part: this file system rounded a 250 ms timestamp");
        return;
    }
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    assert!(
        skipped(&refresh(&root, &home)),
        "a file system with sub-second timestamps was treated as coarse"
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
    assert!(finds(&root, &home, "alpha", "a.rs"));
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

/// Everything in `dir` that belongs to the index: the index and its sidecars.
fn beside_the_index(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(INDEX_FILE_NAME))
        .collect();
    names.sort();
    names
}

#[test]
fn a_second_indexer_waits_for_the_first() {
    // Two builds of one index used to share a temporary file, and whichever
    // finished second installed what the two of them had made of it.
    let (_dir, root, home) = plain_scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    let index = root.join(INDEX_FILE_NAME);
    fs::write(root.join("a.txt"), "alpha, edited\n").unwrap();

    // A refresh is "in progress": this test holds its lock.
    let running = lock::acquire(&index, || {});
    let before = fs::read(&index).unwrap();
    let mut second = command_from(CINDEX, &root, &home)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        second.try_wait().unwrap().is_none(),
        "a second indexer ran to completion while the first held the lock"
    );
    assert_eq!(
        fs::read(&index).unwrap(),
        before,
        "it wrote the index anyway"
    );
    assert!(
        !finds(&root, &home, "edited", "a.txt"),
        "it wrote the index anyway"
    );

    // Released, it goes ahead -- and it said it had been waiting.
    drop(running);
    let out = second.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("waiting for another"),
        "{}",
        text(&out.stderr)
    );
    assert!(finds(&root, &home, "edited", "a.txt"));
}

#[test]
fn reset_removes_the_index_and_everything_beside_it() {
    let (_dir, root, home) = plain_scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    // The index, its stamp and its lock -- and nothing left over from the
    // build itself.
    let sidecars = |suffixes: &[&str]| -> Vec<String> {
        suffixes
            .iter()
            .map(|s| format!("{INDEX_FILE_NAME}{s}"))
            .collect()
    };
    assert_eq!(beside_the_index(&root), sidecars(&["", ".lock", ".meta"]));

    let out = run_from(CINDEX, &root, &home, &["--reset"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(beside_the_index(&root), Vec::<String>::new());

    // Resetting where there is nothing to reset leaves nothing behind either
    // -- in particular, not a lock file for an index that does not exist.
    let out = run_from(CINDEX, &root, &home, &["--reset"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(beside_the_index(&root), Vec::<String>::new());
    assert_eq!(beside_the_index(&home), Vec::<String>::new());
}

#[test]
fn a_run_with_nothing_to_index_creates_no_lock_file() {
    // "No paths given and no existing index" is an error; it must not leave
    // a lock beside an index that was never there.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let out = run_from(CINDEX, dir.path(), &home, &[]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no paths given and no existing index"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(beside_the_index(&home), Vec::<String>::new());
    assert_eq!(beside_the_index(dir.path()), Vec::<String>::new());
}

#[test]
fn background_lets_go_of_whoever_started_it() {
    // `--background` exists so that whatever ran it -- a git hook, usually --
    // is not kept waiting. Returning quickly is not enough: a caller that
    // reads our output through a pipe (an IDE running git, `$out = git pull`
    // in PowerShell, this test) waits for the pipe to close, and on Windows a
    // child process is handed every handle its parent could inherit, pipes
    // included. The background process then held them open, and the caller
    // waited until the whole index had been rebuilt.
    //
    // So the background work is made impossible to finish -- this test holds
    // the lock it needs -- and the output is read to its end regardless.
    let (_dir, root, home) = plain_scene();
    assert!(run_from(CINDEX, &root, &home, &["--local"])
        .status
        .success());
    let index = root.join(INDEX_FILE_NAME);
    fs::write(root.join("a.txt"), "alpha, edited\n").unwrap();

    let running = lock::acquire(&index, || {});
    let child = command_from(CINDEX, &root, &home)
        .arg("--background")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (done, read_to_the_end) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let out = read_to_the_end
        .recv_timeout(Duration::from_secs(30))
        .expect("still waiting on the pipes of a process that was meant to have been let go")
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), String::new())
    );
    // The work really was left behind for later: nothing is indexed yet.
    assert!(!finds(&root, &home, "edited", "a.txt"));

    // And it does get done, once it can be.
    drop(running);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !finds(&root, &home, "edited", "a.txt") {
        assert!(
            Instant::now() < deadline,
            "the background refresh never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Let it finish before the directory it is working in is removed.
    drop(lock::acquire(&index, || {}));
}
