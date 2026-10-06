//! `cindex-rs --hook`: the one command any version-control system, wrapper or
//! scheduler runs to bring the index up to date -- and the wrappers
//! `--print-hook` writes for systems that have no hooks of their own.
//!
//! The wrapper tests run each shell for real, wrapping `git` because it is on
//! every machine these tests run on. A shell that is not installed is skipped
//! with a message; `CSEARCH_RS_REQUIRE_SHELLS` (set in CI) names the ones
//! whose absence is a failure instead.

mod common;

use common::{command_from, git, have_git, run_from, settle, text, CINDEX, CSEARCH};
use csearch::lock;
use csearch::names::INDEX_FILE_NAME;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for something a detached process is doing.
const PATIENCE: Duration = Duration::from_secs(60);

fn home_in(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();
    home
}

/// A plain directory with a project index in it.
fn indexed_tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), "alpha\n").unwrap();
    settle(&root);
    let home = home_in(dir.path());
    let out = run_from(CINDEX, &root, &home, &["--local"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    (dir, root, home)
}

fn finds(cwd: &Path, home: &Path, pattern: &str, file: &str) -> bool {
    let out = run_from(CSEARCH, cwd, home, &["-l", pattern]);
    text(&out.stdout).lines().any(|l| l.ends_with(file))
}

/// Wait for a detached refresh to make `pattern` findable in `file`.
fn eventually_finds(cwd: &Path, home: &Path, pattern: &str, file: &str) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if finds(cwd, home, pattern, file) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Wait until no refresh of `index` is running or queued, so the temporary
/// directory can be removed (on Windows a process's working directory cannot
/// be deleted from under it).
fn let_refreshes_finish(index: &Path) {
    for _ in 0..3 {
        drop(lock::acquire(index, || {}));
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Run a command that is meant to hand its work on and return, and collect
/// what it printed -- or fail, rather than hang the test run, if it has not
/// come back after `PATIENCE`. That is what happens if it waits for the work
/// after all, or if the process it started still holds the pipes this reads.
fn let_go(cmd: &mut Command) -> Output {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    finished
        .recv_timeout(PATIENCE)
        .expect("the command did not return while the work it started could not run")
        .unwrap()
}

/// `cindex-rs --hook --verbose`: the hook in the foreground, saying what it
/// decided.
fn hook_verbose(cwd: &Path, home: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["--hook", "--verbose"];
    args.extend_from_slice(extra);
    run_from(CINDEX, cwd, home, &args)
}

#[test]
fn the_hook_refreshes_the_index_that_covers_the_working_directory() {
    let (_dir, root, home) = indexed_tree();
    let index = root.join(INDEX_FILE_NAME);
    fs::write(root.join("sub/new.txt"), "fresh_content\n").unwrap();
    assert!(!finds(&root, &home, "fresh_content", "new.txt"));

    // From a subdirectory, with no flags: this is all a hook has to run. It
    // hands the work on and returns -- shown here by seeing to it that the
    // work cannot even begin: this test is holding the index's lock.
    let running = lock::acquire(&index, || {});
    let out = let_go(command_from(CINDEX, &root.join("sub"), &home).arg("--hook"));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), String::new()),
        "a hook prints nothing"
    );
    assert!(!finds(&root, &home, "fresh_content", "new.txt"));

    drop(running);
    assert!(
        eventually_finds(&root, &home, "fresh_content", "new.txt"),
        "the index was never refreshed"
    );
    let_refreshes_finish(&index);
}

#[test]
fn the_hook_says_what_it_decided_when_asked() {
    let (_dir, root, home) = indexed_tree();

    // Nothing has changed.
    let out = hook_verbose(&root, &home, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("up to date"),
        "{}",
        text(&out.stderr)
    );

    // Something has: it rebuilds, here and now, and says why.
    fs::write(root.join("a.txt"), "alpha and beta\n").unwrap();
    let out = hook_verbose(&root, &home, &[]);
    assert_eq!(out.status.code(), Some(0));
    let err = text(&out.stderr);
    assert!(err.contains("rebuilding:"), "{err}");
    assert!(err.contains("1 files indexed"), "{err}");
    assert!(finds(&root, &home, "beta", "a.txt"));
}

#[test]
fn the_hook_does_nothing_where_there_is_no_index() {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("bare");
    fs::create_dir_all(&bare).unwrap();
    fs::write(bare.join("f.txt"), "content\n").unwrap();
    let home = home_in(dir.path());
    let listing = |d: &Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != ".gitconfig") // the test harness's own
            .collect();
        names.sort();
        names
    };

    let out = hook_verbose(&bare, &home, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("no index covers this directory"),
        "{}",
        text(&out.stderr)
    );
    // It must not have made one, here or at home.
    assert_eq!(listing(&bare), ["f.txt"]);
    assert_eq!(listing(&home), Vec::<String>::new());

    // And without --verbose it has nothing to say about it either.
    let out = run_from(CINDEX, &bare, &home, &["--hook"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stderr), "");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(listing(&bare), ["f.txt"]);
    assert_eq!(listing(&home), Vec::<String>::new());
}

#[test]
fn the_hook_leaves_an_index_alone_when_run_outside_its_roots() {
    // The home index covers `inside`. A hook fired somewhere else entirely --
    // a different checkout, a scratch directory -- has no business rebuilding
    // it, however stale it is.
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("inside");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(inside.join("f.txt"), "content\n").unwrap();
    settle(&inside);
    let home = home_in(dir.path());
    let out = run_from(CINDEX, dir.path(), &home, &[inside.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let index = home.join(INDEX_FILE_NAME);
    let before = fs::read(&index).unwrap();

    fs::write(inside.join("f.txt"), "content, now different\n").unwrap();
    let out = hook_verbose(&outside, &home, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("not under any root"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fs::read(&index).unwrap(), before, "the index was rebuilt");

    // From inside the root, the same command does refresh it.
    let out = hook_verbose(&inside, &home, &[]);
    assert!(
        text(&out.stderr).contains("rebuilding:"),
        "{}",
        text(&out.stderr)
    );
    assert_ne!(fs::read(&index).unwrap(), before);
}

#[test]
fn the_hook_never_fails_the_command_that_ran_it() {
    // Whatever is wrong -- here, an index that is not an index -- the exit
    // status is 0 and nothing is printed. --verbose is how to find out.
    let (_dir, root, home) = indexed_tree();
    let index = root.join(INDEX_FILE_NAME);
    fs::write(&index, b"this is not an index").unwrap();

    let out = run_from(CINDEX, &root, &home, &["--hook"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stderr), "");

    let out = hook_verbose(&root, &home, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("not a csearch-rs index"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fs::read(&index).unwrap(), b"this is not an index");
}

#[test]
fn local_hook_brings_back_an_index_that_was_deleted() {
    // What git's hooks run. `git clean -fdx` takes the index with it, and the
    // next checkout must put it back -- so here, and only with --local, the
    // hook creates one.
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();
    assert!(git(&root, &["init", "-q"]));
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "one"]));
    let home = home_in(dir.path());
    let index = root.join(INDEX_FILE_NAME);
    assert!(!index.exists());

    let out = hook_verbose(&root, &home, &["--local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(index.is_file(), "{}", text(&out.stderr));
    assert!(finds(&root, &home, "alpha", "a.rs"));
}

#[test]
fn a_hook_does_not_swap_the_file_list_when_git_is_in_trouble() {
    // This index was built from git's list, so what git ignores is not in it.
    // If git cannot be used, a refresh somebody runs by hand says so and
    // walks the directory instead. A hook has nobody to say it to: it leaves
    // the index as it is, rather than quietly rebuild it with every ignored
    // file inside -- and have the next hook, git working again, take them
    // all back out.
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
    fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(root.join("ignored.rs"), "fn ignored_marker() {}\n").unwrap();
    assert!(git(&root, &["init", "-q"]));
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "one"]));
    settle(&root);
    let home = home_in(dir.path());
    let out = run_from(CINDEX, &root, &home, &["--local"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let index = root.join(INDEX_FILE_NAME);
    assert!(finds(&root, &home, "kept", "kept.rs"));
    assert!(!finds(&root, &home, "ignored_marker", "ignored.rs"));

    // Trouble: git cannot read its own index file, so it cannot list. And a
    // change, so that there is something for a refresh to do.
    fs::write(root.join(".git").join("index"), b"not an index").unwrap();
    fs::write(root.join("kept.rs"), "fn kept() {}\nfn later() {}\n").unwrap();
    let before = fs::read(&index).unwrap();

    let out = hook_verbose(&root, &home, &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("git could not list the files"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fs::read(&index).unwrap(), before, "the hook rebuilt it");

    // Asked for by a person, the same refresh goes ahead, and says how.
    let out = run_from(CINDEX, &root, &home, &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("could not use git, walking the directory instead"),
        "{}",
        text(&out.stderr)
    );
    assert!(finds(&root, &home, "ignored_marker", "ignored.rs"));
}

/// `git ...` in `root` as a user would run it: git's hooks fire, and inherit
/// the sealed environment rather than this machine's.
fn user_git(root: &Path, home: &Path, args: &[&str]) {
    let out = command_from("git", root, home)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
}

#[test]
fn gits_own_hooks_refresh_the_index_on_a_real_checkout_and_commit() {
    // --install-hooks, then git itself: no wrapper, nothing run by hand. The
    // hook starts with git's environment around it (GIT_DIR and the rest),
    // in whatever shell git runs hooks with on this platform.
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo_with_a_feature_branch(dir.path());
    let index = root.join(INDEX_FILE_NAME);
    let out = run_from(CINDEX, &root, &home, &["--install-hooks"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!finds(&root, &home, "only_on_feature", "b.rs"));

    user_git(&root, &home, &["checkout", "-q", "feature"]);
    assert!(
        eventually_finds(&root, &home, "only_on_feature", "b.rs"),
        "git's post-checkout hook did not refresh the index"
    );
    let_refreshes_finish(&index);

    fs::write(root.join("c.rs"), "fn committed_later() {}\n").unwrap();
    user_git(&root, &home, &["add", "-A"]);
    user_git(&root, &home, &["commit", "-q", "-m", "three"]);
    assert!(
        eventually_finds(&root, &home, "committed_later", "c.rs"),
        "git's post-commit hook did not refresh the index"
    );
    let_refreshes_finish(&index);

    // `git clean -fdx` takes the index with it; the next event brings it back.
    user_git(&root, &home, &["clean", "-q", "-fdx"]);
    assert!(
        !index.exists(),
        "git clean was expected to remove the index"
    );
    user_git(&root, &home, &["checkout", "-q", "main"]);
    let deadline = Instant::now() + PATIENCE;
    while !index.is_file() {
        assert!(
            Instant::now() < deadline,
            "the index removed by git clean was not rebuilt"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let_refreshes_finish(&index);
    assert!(finds(&root, &home, "alpha", "a.rs"));
    assert!(!finds(&root, &home, "only_on_feature", "b.rs"));
}

/// A `cindex-rs --hook --verbose` child, whose output is collected when it
/// exits.
fn spawn_hook(cwd: &Path, home: &Path) -> std::process::Child {
    command_from(CINDEX, cwd, home)
        .args(["--hook", "--verbose"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn of_two_hooks_behind_a_running_refresh_one_waits_and_one_goes_home() {
    // A burst of events while a refresh is running needs exactly one more
    // refresh afterwards, not one per event.
    let (_dir, root, home) = indexed_tree();
    let index = root.join(INDEX_FILE_NAME);
    fs::write(root.join("a.txt"), "alpha, edited\n").unwrap();
    let running = lock::acquire(&index, || {});

    let mut hooks = [spawn_hook(&root, &home), spawn_hook(&root, &home)];
    // Whichever of the two reached the queue first waits there; the other
    // finds it taken and exits. Which is which does not matter.
    let deadline = Instant::now() + PATIENCE;
    let gone = loop {
        let done: Vec<usize> = (0..2)
            .filter(|&i| hooks[i].try_wait().unwrap().is_some())
            .collect();
        match done.len() {
            0 => {}
            1 => break done[0],
            _ => panic!("both hooks finished while the lock was held"),
        }
        assert!(Instant::now() < deadline, "neither hook gave way");
        std::thread::sleep(Duration::from_millis(50));
    };
    let [first, second] = hooks;
    let (went_home, mut waited) = if gone == 0 {
        (first, second)
    } else {
        (second, first)
    };
    let out = went_home.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("already waiting"),
        "{}",
        text(&out.stderr)
    );
    // The other is still there, and has not touched the index.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        waited.try_wait().unwrap().is_none(),
        "the waiter did not wait"
    );
    assert!(!finds(&root, &home, "edited", "a.txt"));

    // When the running refresh finishes, the waiter does the one refresh
    // that covers both events.
    drop(running);
    let out = waited.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("rebuilding:"),
        "{}",
        text(&out.stderr)
    );
    assert!(finds(&root, &home, "edited", "a.txt"));
}

#[test]
fn a_burst_of_hooks_leaves_a_good_index_and_no_debris() {
    // Twenty at once, each started while the tree is still changing. Without
    // the lock they wrote one temporary file between them.
    let (_dir, root, home) = indexed_tree();
    let index = root.join(INDEX_FILE_NAME);
    for i in 0..20 {
        fs::write(root.join(format!("f{i:02}.txt")), format!("burst_{i:02}\n")).unwrap();
        let out = run_from(CINDEX, &root, &home, &["--hook"]);
        assert_eq!(out.status.code(), Some(0));
    }
    assert!(
        eventually_finds(&root, &home, "burst_19", "f19.txt"),
        "the last change never reached the index"
    );
    let_refreshes_finish(&index);
    // Every file is there, the index opens cleanly, and nothing was left
    // behind but the index and its sidecars.
    let out = run_from(CSEARCH, &root, &home, &["-c", "burst_"]);
    assert_eq!(text(&out.stderr), "");
    assert_eq!(text(&out.stdout).lines().count(), 20);
    let mut beside: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(INDEX_FILE_NAME))
        // On Windows a replaced index is parked as `.old` until nothing has
        // it mapped; the searches this test ran while polling can leave one
        // behind for the next build to clear. That is by design, not debris.
        .filter(|n| !(cfg!(windows) && n.ends_with(".old")))
        .collect();
    beside.sort();
    let expected: Vec<String> = ["", ".lock", ".meta", ".queue"]
        .iter()
        .map(|s| format!("{INDEX_FILE_NAME}{s}"))
        .collect();
    assert_eq!(beside, expected);
}

#[test]
fn a_hook_does_not_pin_the_directory_it_was_fired_from() {
    // On Windows the working directory of a running process cannot be
    // removed. A hook fires wherever the command was typed -- often a build
    // directory -- and may then wait its turn for a while; it must not make
    // that directory undeletable in the meantime.
    let (_dir, root, home) = indexed_tree();
    let index = root.join(INDEX_FILE_NAME);
    let scratch = root.join("sub").join("scratch");
    fs::create_dir_all(&scratch).unwrap();

    // Keep the hook waiting: hold the lock it needs.
    let running = lock::acquire(&index, || {});
    let out = let_go(command_from(CINDEX, &scratch, &home).arg("--hook"));
    assert_eq!(out.status.code(), Some(0));
    // Once it is in the queue it has left the directory it started in.
    let queue = csearch::paths::sidecar(&index, "queue");
    let deadline = Instant::now() + PATIENCE;
    while !queue.exists() {
        assert!(
            Instant::now() < deadline,
            "the hook never reached the queue"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::remove_dir(&scratch).expect("the waiting hook is holding the directory it was started in");

    drop(running);
    let_refreshes_finish(&index);
}

// ------------------------------------------------------------- --print-hook

#[test]
fn print_hook_refuses_what_it_cannot_write_safely() {
    let dir = tempfile::tempdir().unwrap();
    let home = home_in(dir.path());
    let run = |args: &[&str]| {
        command_from(CINDEX, dir.path(), &home)
            .env_remove("SHELL")
            .args(args)
            .output()
            .unwrap()
    };

    // A tool name that is not a plain command name would become shell source.
    for bad in ["p4; rm -rf ~", "$(reboot)", "a b", ""] {
        let out = run(&["--print-hook", bad, "--shell", "sh"]);
        assert!(!out.status.success(), "accepted {bad:?}");
        assert_eq!(text(&out.stdout), "", "printed something for {bad:?}");
    }
    // A shell it does not know: name the ones it does.
    let out = run(&["--print-hook", "p4", "--shell", "cmd"]);
    assert!(!out.status.success());
    assert_eq!(text(&out.stdout), "");
    assert!(
        text(&out.stderr).contains("powershell"),
        "{}",
        text(&out.stderr)
    );
    // No --shell and no $SHELL to go by: ask, do not guess.
    let out = run(&["--print-hook", "p4"]);
    assert!(!out.status.success());
    assert_eq!(text(&out.stdout), "");
    assert!(
        text(&out.stderr).contains("--shell"),
        "{}",
        text(&out.stderr)
    );

    // With $SHELL set, that is the shell.
    let out = command_from(CINDEX, dir.path(), &home)
        .env("SHELL", "/usr/bin/fish")
        .args(["--print-hook", "p4"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("function p4 --wraps p4"));
}

/// A shell to exercise a wrapper in.
struct ShellUnderTest {
    /// The program to run, as found on PATH.
    program: &'static str,
    /// What to pass `--print-hook ... --shell`.
    dialect: &'static str,
    /// The file extension its scripts take.
    ext: &'static str,
    /// Arguments that make it run a script file without reading any profile.
    run: &'static [&'static str],
}

const SHELLS: &[ShellUnderTest] = &[
    ShellUnderTest {
        program: "sh",
        dialect: "sh",
        ext: "sh",
        run: &[],
    },
    ShellUnderTest {
        program: "bash",
        dialect: "bash",
        ext: "sh",
        run: &["--noprofile", "--norc"],
    },
    ShellUnderTest {
        program: "dash",
        dialect: "dash",
        ext: "sh",
        run: &[],
    },
    ShellUnderTest {
        program: "zsh",
        dialect: "zsh",
        ext: "sh",
        run: &["-f"],
    },
    ShellUnderTest {
        program: "ksh",
        dialect: "ksh",
        ext: "sh",
        run: &[],
    },
    ShellUnderTest {
        program: "fish",
        dialect: "fish",
        ext: "fish",
        run: &["--no-config"],
    },
    ShellUnderTest {
        program: "tcsh",
        dialect: "tcsh",
        ext: "csh",
        run: &["-f"],
    },
    ShellUnderTest {
        program: "csh",
        dialect: "csh",
        ext: "csh",
        run: &["-f"],
    },
    ShellUnderTest {
        program: "powershell",
        dialect: "powershell",
        ext: "ps1",
        run: &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ],
    },
    ShellUnderTest {
        program: "pwsh",
        dialect: "pwsh",
        ext: "ps1",
        run: &["-NoProfile", "-NonInteractive", "-File"],
    },
];

/// Every file on PATH called `program`, in PATH order.
fn all_on_path(program: &str) -> Vec<PathBuf> {
    let exe = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join(&exe))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default()
}

fn on_path(program: &str) -> Option<PathBuf> {
    all_on_path(program).into_iter().next()
}

/// The first `shell.program` on PATH that runs a script file the way these
/// tests will ask it to. A name is not enough: on Windows, `bash.exe` in
/// System32 is the launcher for the Linux subsystem. It is there on every
/// machine; with no distribution installed it runs nothing, and with one it
/// runs that system's bash, which cannot open a path of this one's.
fn working(shell: &ShellUnderTest) -> Option<PathBuf> {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join(format!("probe.{}", shell.ext));
    let says_ok = if shell.ext == "ps1" {
        "'ok'\n"
    } else {
        "echo ok\n"
    };
    fs::write(&probe, says_ok).unwrap();
    all_on_path(shell.program).into_iter().find(|program| {
        Command::new(program)
            .args(shell.run)
            .arg(&probe)
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|out| out.status.success() && text(&out.stdout).trim() == "ok")
    })
}

/// Shells whose absence is a failure rather than a skip, from
/// `CSEARCH_RS_REQUIRE_SHELLS` (comma-separated).
fn required_shells() -> Vec<String> {
    std::env::var("CSEARCH_RS_REQUIRE_SHELLS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A path written into shell source: forward slashes, which every shell here
/// takes on every platform, and no quote characters to trip over.
fn for_script(p: &Path) -> String {
    let s = p.to_str().unwrap().replace('\\', "/");
    assert!(!s.contains('\''), "test paths must not contain quotes: {s}");
    s
}

/// A script that switches the `git` wrapper on (or not) and then uses git in
/// ways a wrapper could get wrong: an argument with a space in it, a failing
/// command with a redirection of its own, no arguments at all, output down a
/// pipe, an argument that must be worked out once and not twice (it appends
/// a line to `evals` each time it is), and input from a pipe.
fn script_for(shell: &ShellUnderTest, repo: &Path, evals: &Path, wrapped: bool) -> String {
    let cindex = for_script(Path::new(CINDEX));
    let repo = for_script(repo);
    let evals = for_script(evals);
    let dialect = shell.dialect;
    match shell.ext {
        "sh" => {
            let activate = if wrapped {
                format!("eval \"$('{cindex}' --print-hook git --shell {dialect})\" || exit 90\n")
            } else {
                String::new()
            };
            format!(
                "{activate}cd '{repo}' || exit 91\n\
                 git checkout -q feature; echo \"status=$?\"\n\
                 git commit -q --allow-empty -m 'two words'; echo \"status=$?\"\n\
                 git checkout -q no-such-branch 2>/dev/null; echo \"status=$?\"\n\
                 ( git ) >/dev/null 2>&1; echo \"status=$?\"\n\
                 git log -1 --format=\"$(echo %s; echo x >>'{evals}')\" | cat\n\
                 echo x | git hash-object --stdin\n"
            )
        }
        "fish" => {
            let activate = if wrapped {
                format!("'{cindex}' --print-hook git --shell fish | source; or exit 90\n")
            } else {
                String::new()
            };
            format!(
                "{activate}cd '{repo}'; or exit 91\n\
                 git checkout -q feature; echo \"status=$status\"\n\
                 git commit -q --allow-empty -m 'two words'; echo \"status=$status\"\n\
                 git checkout -q no-such-branch 2>/dev/null; echo \"status=$status\"\n\
                 git >/dev/null 2>&1; echo \"status=$status\"\n\
                 git log -1 --format=(echo %s; echo x >>'{evals}') | cat\n\
                 echo x | git hash-object --stdin\n"
            )
        }
        "csh" => {
            let activate = if wrapped {
                format!("eval \"`'{cindex}' --print-hook git --shell {dialect}`\"\n")
            } else {
                String::new()
            };
            format!(
                "{activate}cd '{repo}'\n\
                 git checkout -q feature; echo \"status=$status\"\n\
                 git commit -q --allow-empty -m 'two words'; echo \"status=$status\"\n\
                 git checkout -q no-such-branch >& /dev/null; echo \"status=$status\"\n\
                 ( git ) >& /dev/null; echo \"status=$status\"\n\
                 git log -1 --format=\"`echo %s; echo x >>'{evals}'`\" | cat\n\
                 echo x | git hash-object --stdin\n"
            )
        }
        "ps1" => {
            let activate = if wrapped {
                format!(
                    "& '{cindex}' --print-hook git --shell {dialect} | Out-String | Invoke-Expression\n"
                )
            } else {
                String::new()
            };
            format!(
                "{activate}Set-Location -LiteralPath '{repo}'\n\
                 git checkout -q feature; \"status=$LASTEXITCODE\"\n\
                 git commit -q --allow-empty -m 'two words'; \"status=$LASTEXITCODE\"\n\
                 git checkout -q no-such-branch 2>$null; $question = $?; \"status=$LASTEXITCODE\"\n\
                 git *> $null; \"status=$LASTEXITCODE\"\n\
                 git log -1 \"--format=$('%s'; Add-Content -LiteralPath '{evals}' -Value x)\" | ForEach-Object {{ $_ }}\n\
                 'x' | git hash-object --stdin\n\
                 \"question=$question\"\n"
            )
        }
        other => unreachable!("no script for .{other}"),
    }
}

/// A repository on `main` whose `feature` branch has one more file, with a
/// project index built on `main` and no git hooks installed: only a wrapper
/// can be what refreshes it.
fn repo_with_a_feature_branch(dir: &Path) -> (PathBuf, PathBuf) {
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();
    assert!(git(&root, &["init", "-q", "-b", "main"]));
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "one"]));
    assert!(git(&root, &["checkout", "-q", "-b", "feature"]));
    fs::write(root.join("b.rs"), "fn only_on_feature() {}\n").unwrap();
    assert!(git(&root, &["add", "-A"]));
    assert!(git(&root, &["commit", "-q", "-m", "feature"]));
    assert!(git(&root, &["checkout", "-q", "main"]));
    assert!(!root.join("b.rs").exists());
    settle(&root);
    let home = home_in(dir);
    let out = run_from(CINDEX, &root, &home, &["--local"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    (root, home)
}

/// Run `script` in `shell`, from outside the repository, with the sealed
/// environment and an identity for the commit it makes.
fn run_script(
    shell: &ShellUnderTest,
    program: &Path,
    dir: &Path,
    home: &Path,
    script: &str,
) -> Output {
    let file = dir.join(format!("script.{}", shell.ext));
    fs::write(&file, script).unwrap();
    command_from(program.to_str().unwrap(), dir, home)
        .args(shell.run)
        .arg(&file)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// What `echo x | git hash-object --stdin` prints with no wrapper involved:
/// the hash of exactly the bytes a shell's `echo x` sends down a pipe.
fn hash_of(bytes: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    text(&child.wait_with_output().unwrap().stdout)
        .trim()
        .to_string()
}

#[test]
fn a_wrapped_command_behaves_like_the_real_one_and_refreshes_the_index() {
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let required = required_shells();
    for name in &required {
        // A name these tests do not know would require nothing at all.
        assert!(
            SHELLS.iter().any(|s| s.program == name),
            "CSEARCH_RS_REQUIRE_SHELLS names {name}, which is not a shell these tests know"
        );
    }
    let mut tested = Vec::new();
    for shell in SHELLS {
        let Some(program) = working(shell) else {
            assert!(
                !required.iter().any(|r| r == shell.program),
                "CSEARCH_RS_REQUIRE_SHELLS names {}, and no working one is on PATH",
                shell.program
            );
            eprintln!("skipping: {} is not installed", shell.program);
            continue;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, home) = repo_with_a_feature_branch(dir.path());
        let index = root.join(INDEX_FILE_NAME);
        assert!(!finds(&root, &home, "only_on_feature", "b.rs"));

        let evals = dir.path().join("evals");
        let script = script_for(shell, &root, &evals, true);
        let out = run_script(shell, &program, dir.path(), &home, &script);
        let name = shell.program;
        assert!(
            out.status.success(),
            "{name}: {}\n--- script ---\n{script}\n--- stderr ---\n{}",
            out.status,
            text(&out.stderr)
        );
        let said = text(&out.stdout);
        let lines: Vec<&str> = said.lines().map(str::trim_end).collect();
        // PowerShell prints one line more: what `$?` said after the failure.
        let expected = if shell.ext == "ps1" { 7 } else { 6 };
        assert_eq!(
            lines.len(),
            expected,
            "{name}: {said:?}\n{}",
            text(&out.stderr)
        );
        // Exit statuses pass through: success, success, git's own failure
        // (with a redirection written after it), and its failure again when
        // run with no arguments at all.
        assert_eq!(
            &lines[..4],
            ["status=0", "status=0", "status=1", "status=1"],
            "{name}"
        );
        // An argument with a space in it arrived as one argument, and the
        // command's output went down the pipe.
        assert_eq!(lines[4], "two words", "{name}");
        // The argument that leaves a mark each time it is worked out was
        // worked out once. A wrapper made of text, as csh's is, has to take
        // care not to read the command line a second time.
        let marks = fs::read_to_string(&evals).unwrap_or_default();
        assert_eq!(
            marks.lines().count(),
            1,
            "{name}: an argument was evaluated {} times",
            marks.lines().count()
        );
        // Its input came up a pipe, too. (PowerShell ends a piped line with
        // the platform's line ending, so there the hash is of one or the
        // other.)
        let hashes = [hash_of(b"x\n"), hash_of(b"x\r\n")];
        assert!(hashes.iter().any(|h| h == lines[5]), "{name}: {}", lines[5]);
        // In PowerShell the status is in $LASTEXITCODE, as checked above, but
        // `$?` is true after any function however the command inside it
        // ended -- so `wrapped-command && next` does not see a failure. The
        // README says so; this is where that claim is held to account.
        if shell.ext == "ps1" {
            assert_eq!(lines[6], "question=True", "{name}");
        }
        // The wrapper adds nothing of its own to what the user sees.
        assert_eq!(text(&out.stderr), "", "{name} printed to stderr");

        // And the index followed the checkout.
        assert!(
            eventually_finds(&root, &home, "only_on_feature", "b.rs"),
            "{name}: the wrapped checkout did not refresh the index"
        );
        let_refreshes_finish(&index);
        tested.push(name);
    }
    eprintln!("wrappers tested in: {}", tested.join(", "));
    assert!(
        !tested.is_empty(),
        "no shell at all was available to test a wrapper in"
    );
}

#[test]
fn without_the_wrapper_the_same_commands_refresh_nothing() {
    // The control for the test above: if the index picked up the checkout
    // here too, that test would be showing nothing about the wrapper.
    if !have_git() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let Some((shell, program)) = SHELLS.iter().find_map(|s| Some((s, working(s)?))) else {
        panic!("no working shell at all is available");
    };
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo_with_a_feature_branch(dir.path());
    let script = script_for(shell, &root, &dir.path().join("evals"), false);
    let out = run_script(shell, &program, dir.path(), &home, &script);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        root.join("b.rs").is_file(),
        "the checkout itself did not happen"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !finds(&root, &home, "only_on_feature", "b.rs"),
        "the index changed with no wrapper and no hook installed"
    );
}

#[test]
fn mercurial_refreshes_through_its_own_hooks() {
    // The two lines the README gives for .hg/hgrc, in a real repository.
    let Some(hg) = on_path("hg") else {
        assert!(
            std::env::var("CSEARCH_RS_REQUIRE_HG").as_deref() != Ok("1"),
            "CSEARCH_RS_REQUIRE_HG=1 but hg is not on PATH"
        );
        eprintln!("skipping: Mercurial (hg) is not installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let home = home_in(dir.path());
    let hg = |args: &[&str]| {
        let out = command_from(hg.to_str().unwrap(), &root, &home)
            .env("HGUSER", "t <t@t>")
            .env("HGRCPATH", "")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "hg {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    };
    hg(&["init"]);
    fs::write(root.join("a.py"), "def alpha(): pass\n").unwrap();
    hg(&["add", "a.py"]);
    hg(&["commit", "-m", "one"]);
    fs::write(root.join("b.py"), "def only_in_two(): pass\n").unwrap();
    hg(&["add", "b.py"]);
    hg(&["commit", "-m", "two"]);
    hg(&["update", "-r", "0"]);
    assert!(!root.join("b.py").exists());
    settle(&root);

    let out = run_from(CINDEX, &root, &home, &["--local"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let index = root.join(INDEX_FILE_NAME);
    assert!(!finds(&root, &home, "only_in_two", "b.py"));

    let cindex = for_script(Path::new(CINDEX));
    fs::write(
        root.join(".hg").join("hgrc"),
        format!(
            "[hooks]\n\
             update.csearch-rs = \"{cindex}\" --hook\n\
             commit.csearch-rs = \"{cindex}\" --hook\n"
        ),
    )
    .unwrap();

    // `hg update` brings b.py into the working directory...
    hg(&["update", "-r", "1"]);
    assert!(
        eventually_finds(&root, &home, "only_in_two", "b.py"),
        "hg update did not refresh the index"
    );
    let_refreshes_finish(&index);
    // ...and a commit of a new file is picked up as well.
    fs::write(root.join("c.py"), "def committed_later(): pass\n").unwrap();
    hg(&["add", "c.py"]);
    hg(&["commit", "-m", "three"]);
    assert!(
        eventually_finds(&root, &home, "committed_later", "c.py"),
        "hg commit did not refresh the index"
    );
    let_refreshes_finish(&index);
}
