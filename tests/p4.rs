//! Perforce, for real: a server, workspaces, and `p4` itself.
//!
//! Each test starts from an empty server of its own. No daemon is involved:
//! `P4PORT=rsh:p4d -r ROOT -i` makes every p4 command run its own p4d over a
//! pipe, so there is no port to choose and nothing to start, wait for or
//! stop.
//!
//! These tests need `p4` and `p4d` on PATH and are skipped, with a message,
//! where they are not. `CSEARCH_RS_REQUIRE_P4=1` (set in CI, which downloads
//! both) turns that skip into a failure.

mod common;

use common::{command_from, settle, text, CINDEX, CSEARCH};
use csearch::lock;
use csearch::names::INDEX_FILE_NAME;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for something a detached process is doing.
const PATIENCE: Duration = Duration::from_secs(60);

fn on_path(program: &str) -> Option<PathBuf> {
    let exe = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(&exe))
        .find(|p| p.is_file())
}

/// A Perforce server with nothing in it, and the means to use it.
struct World {
    dir: tempfile::TempDir,
    p4: PathBuf,
    port: String,
    home: PathBuf,
}

impl World {
    /// `None`, with the reason printed, where Perforce is not installed.
    fn new() -> Option<World> {
        let (Some(p4), Some(p4d)) = (on_path("p4"), on_path("p4d")) else {
            assert!(
                std::env::var("CSEARCH_RS_REQUIRE_P4").as_deref() != Ok("1"),
                "CSEARCH_RS_REQUIRE_P4=1 but p4 and p4d are not both on PATH"
            );
            eprintln!("skipping: Perforce (p4 and p4d) is not installed");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        fs::create_dir_all(&server).unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        // The command p4 runs for each connection. Its pieces are quoted,
        // and must not themselves contain a quote.
        let port = format!(
            "rsh:\"{}\" -r \"{}\" -L log -i",
            p4d.display(),
            server.display()
        );
        assert_eq!(port.matches('"').count(), 4, "a quote in a path: {port}");
        Some(World {
            dir,
            p4,
            port,
            home,
        })
    }

    /// A command run from `cwd` as the workspace `client`, with a Perforce
    /// environment that owes nothing to the machine it is on.
    fn command(&self, exe: &str, cwd: &Path, client: &str) -> Command {
        let scratch = self.dir.path();
        let mut cmd = command_from(exe, cwd, &self.home);
        cmd.env("P4PORT", &self.port)
            .env("P4USER", "tester")
            .env("P4CLIENT", client)
            // A P4CONFIG file outranks the environment. Name one that exists
            // nowhere, so that no such file above the temporary directory
            // can decide which server these tests talk to.
            .env("P4CONFIG", ".csearch-rs-tests-have-no-p4config")
            .env("P4ENVIRO", scratch.join("p4enviro"))
            .env("P4TICKETS", scratch.join("p4tickets"))
            .env("P4TRUST", scratch.join("p4trust"))
            .env_remove("P4PASSWD")
            .env_remove("P4CHARSET")
            .env_remove("P4IGNORE")
            .env_remove("P4HOST")
            // p4 takes its working directory from $PWD, not from the system.
            // A shell keeps the two in step; a test has to.
            .env("PWD", cwd);
        cmd
    }

    fn run(&self, exe: &str, cwd: &Path, client: &str, args: &[&str]) -> Output {
        self.command(exe, cwd, client)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `p4 ARGS` as `client` from `cwd`, which must work. Returns what it
    /// printed.
    fn p4(&self, cwd: &Path, client: &str, args: &[&str]) -> String {
        self.p4_with_input(cwd, client, args, "")
    }

    fn p4_with_input(&self, cwd: &Path, client: &str, args: &[&str], input: &str) -> String {
        let mut child = self
            .command(self.p4.to_str().unwrap(), cwd, client)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "p4 {args:?} ({}): {}{}",
            out.status,
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    /// Create the workspace `name`, rooted at a new directory spelled
    /// `spelled` (which may reach it through a link), and return that root.
    ///
    /// The workspace is `allwrite`: Perforce otherwise makes every file it
    /// syncs read-only, and these tests set file times.
    fn client_at(&self, name: &str, spelled: &Path) -> PathBuf {
        let spec = self.p4(spelled, name, &["client", "-o"]);
        let mut edited = String::new();
        for line in spec.lines() {
            if line.starts_with("Root:") {
                edited.push_str(&format!("Root:\t{}", spelled.display()));
            } else if line.starts_with("Options:") {
                assert!(line.contains("noallwrite"), "{line}");
                edited.push_str(&line.replace("noallwrite", "allwrite"));
            } else {
                edited.push_str(line);
            }
            edited.push('\n');
        }
        self.p4_with_input(spelled, name, &["client", "-i"], &edited);
        spelled.to_path_buf()
    }

    /// Create the workspace `name` in a directory of that name.
    fn client(&self, name: &str) -> PathBuf {
        let root = self.dir.path().join(name);
        fs::create_dir_all(&root).unwrap();
        self.client_at(name, &root)
    }

    /// Add `files` (path, contents) in `client` and submit them.
    fn submit_new(&self, root: &Path, client: &str, files: &[(&str, &str)], message: &str) {
        for (rel, contents) in files {
            // Built a component at a time, so that p4 is handed the
            // platform's own separators.
            let path = rel
                .split('/')
                .fold(root.to_path_buf(), |p, part| p.join(part));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            self.p4(root, client, &["add", path.to_str().unwrap()]);
        }
        self.p4(root, client, &["submit", "-d", message]);
    }

    /// Whether a search from `cwd` finds `pattern` in a file called `file`.
    fn finds(&self, cwd: &Path, pattern: &str, file: &str) -> bool {
        let out = command_from(CSEARCH, cwd, &self.home)
            .args(["-l", pattern])
            .output()
            .unwrap();
        text(&out.stdout).lines().any(|l| l.ends_with(file))
    }

    /// What the wrapper runs after `p4 ARGS`, in the foreground so that it
    /// says what it decided.
    fn hook_after(&self, cwd: &Path, client: &str, p4_args: &[&str]) -> Output {
        let mut args = vec!["--hook", "--verbose", "--after", "p4", "--"];
        args.extend_from_slice(p4_args);
        let out = self.run(CINDEX, cwd, client, &args);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        out
    }
}

/// A workspace `ws` with one submitted file and a project index listed
/// through Perforce.
fn indexed_workspace(w: &World) -> PathBuf {
    let ws = w.client("ws");
    w.submit_new(&ws, "ws", &[("a.c", "int marker_first;\n")], "one");
    settle(&ws);
    let out = w.run(CINDEX, &ws, "ws", &["--local", "--p4"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(w.finds(&ws, "marker_first", "a.c"));
    ws
}

#[test]
fn a_workspace_is_listed_the_way_perforce_sees_it() {
    let Some(w) = World::new() else { return };
    let ws = w.client("ws");
    w.submit_new(
        &ws,
        "ws",
        &[
            ("a.c", "int marker_synced_a;\n"),
            ("sub/b.c", "int marker_synced_b;\n"),
            ("gone.c", "int marker_deleted;\n"),
        ],
        "one",
    );
    // Three more kinds of file: one Perforce was never told about, one
    // opened for add and not submitted, and one opened for delete.
    fs::write(ws.join("build.o"), "int marker_never_added;\n").unwrap();
    fs::write(ws.join("new.c"), "int marker_opened_for_add;\n").unwrap();
    w.p4(&ws, "ws", &["add", ws.join("new.c").to_str().unwrap()]);
    w.p4(&ws, "ws", &["delete", ws.join("gone.c").to_str().unwrap()]);
    assert!(!ws.join("gone.c").exists());
    settle(&ws);

    // From a subdirectory: --local --p4 puts the index at the workspace's
    // root, not where the command happened to be typed.
    let out = w.run(CINDEX, &ws.join("sub"), "ws", &["--local", "--p4"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(ws.join(INDEX_FILE_NAME).is_file(), "{}", text(&out.stderr));
    assert!(!ws.join("sub").join(INDEX_FILE_NAME).exists());
    // Perforce has no ignore file that is ours to write to, so the user is
    // told, this once, what to put in theirs.
    assert!(
        text(&out.stderr).contains("P4IGNORE"),
        "{}",
        text(&out.stderr)
    );

    // The index says how its one root is listed.
    let listed = w.run(CINDEX, &ws, "ws", &["--list", "--verbose"]);
    let listed = text(&listed.stdout);
    let rows: Vec<(&str, &str)> = listed
        .lines()
        .map(|l| l.split_once('\t').unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0].0, "p4");
    assert_eq!(
        fs::canonicalize(rows[0].1).unwrap(),
        fs::canonicalize(&ws).unwrap()
    );

    // What Perforce holds is there, including the file opened for add...
    assert!(w.finds(&ws, "marker_synced_a", "a.c"));
    assert!(w.finds(&ws, "marker_synced_b", "b.c"));
    assert!(w.finds(&ws, "marker_opened_for_add", "new.c"));
    // ...and what it does not hold is not: the file it was never told
    // about, and the one that is opened for delete and gone from disk.
    assert!(!w.finds(&ws, "marker_never_added", "build.o"));
    assert!(!w.finds(&ws, "marker_deleted", "gone.c"));

    // A plain re-index lists the same way, and has no more to say about
    // P4IGNORE.
    let out = w.run(CINDEX, &ws, "ws", &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !text(&out.stderr).contains("P4IGNORE"),
        "{}",
        text(&out.stderr)
    );
    assert!(w.finds(&ws, "marker_opened_for_add", "new.c"));
    assert!(!w.finds(&ws, "marker_never_added", "build.o"));
}

#[test]
fn sync_and_submit_refresh_the_index_through_the_hook() {
    let Some(w) = World::new() else { return };
    let ws = indexed_workspace(&w);

    // Somebody else submits a change to a.c, and a new file.
    let other = w.client("other");
    w.p4(&other, "other", &["sync"]);
    let theirs = other.join("a.c");
    w.p4(&other, "other", &["edit", theirs.to_str().unwrap()]);
    fs::write(&theirs, "int marker_first;\nint marker_from_elsewhere;\n").unwrap();
    w.submit_new(
        &other,
        "other",
        &[("c.c", "int marker_new_elsewhere;\n")],
        "two",
    );

    // `p4 sync` brings both into this workspace. The index knows nothing of
    // them yet...
    w.p4(&ws, "ws", &["sync"]);
    assert!(ws.join("c.c").is_file());
    assert!(!w.finds(&ws, "marker_new_elsewhere", "c.c"));
    // ...until what the wrapper runs after `p4 sync` has run.
    let out = w.hook_after(&ws, "ws", &["sync"]);
    assert!(
        text(&out.stderr).contains("rebuilding:"),
        "{}",
        text(&out.stderr)
    );
    assert!(w.finds(&ws, "marker_new_elsewhere", "c.c"));
    assert!(w.finds(&ws, "marker_from_elsewhere", "a.c"));

    // Publishing from here: edit, submit, and the hook after the submit.
    let mine = ws.join("a.c");
    w.p4(&ws, "ws", &["edit", mine.to_str().unwrap()]);
    fs::write(&mine, "int marker_submitted_here;\n").unwrap();
    w.p4(&ws, "ws", &["submit", "-d", "three"]);
    let out = w.hook_after(&ws, "ws", &["submit", "-d", "three"]);
    assert!(
        text(&out.stderr).contains("rebuilding:"),
        "{}",
        text(&out.stderr)
    );
    assert!(w.finds(&ws, "marker_submitted_here", "a.c"));
    assert!(!w.finds(&ws, "marker_from_elsewhere", "a.c"));

    // With nothing changed, the same hook asks Perforce, compares, and
    // leaves the index alone.
    settle(&ws);
    w.hook_after(&ws, "ws", &["sync"]);
    let out = w.hook_after(&ws, "ws", &["sync"]);
    assert!(
        text(&out.stderr).contains("up to date"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_command_that_only_reports_starts_no_refresh() {
    let Some(w) = World::new() else { return };
    let ws = indexed_workspace(&w);
    let index = ws.join(INDEX_FILE_NAME);

    // An edit that a refresh would pick up.
    let mine = ws.join("a.c");
    w.p4(&ws, "ws", &["edit", mine.to_str().unwrap()]);
    fs::write(&mine, "int marker_edited;\n").unwrap();
    let before = fs::read(&index).unwrap();

    // After `p4 opened` -- or `p4 -c ws changes -m1` -- nothing is even
    // looked at.
    for reporting in [&["opened"][..], &["-c", "ws", "changes", "-m1"][..]] {
        let out = w.hook_after(&ws, "ws", reporting);
        assert!(
            text(&out.stderr).contains("changes no files"),
            "{reporting:?}: {}",
            text(&out.stderr)
        );
        assert_eq!(fs::read(&index).unwrap(), before, "{reporting:?}");
    }
    assert!(!w.finds(&ws, "marker_edited", "a.c"));

    // After a command that may have changed something, it is.
    let out = w.hook_after(&ws, "ws", &["revert", "-k", "..."]);
    assert!(
        text(&out.stderr).contains("rebuilding:"),
        "{}",
        text(&out.stderr)
    );
    assert!(w.finds(&ws, "marker_edited", "a.c"));
}

#[test]
fn when_perforce_cannot_answer_the_index_is_left_alone() {
    let Some(w) = World::new() else { return };
    let ws = indexed_workspace(&w);
    let index = ws.join(INDEX_FILE_NAME);
    // Something a walk would index and Perforce does not hold, and an edit
    // that a refresh would pick up.
    fs::write(ws.join("build.o"), "int marker_never_added;\n").unwrap();
    let mine = ws.join("a.c");
    w.p4(&ws, "ws", &["edit", mine.to_str().unwrap()]);
    fs::write(&mine, "int marker_edited;\n").unwrap();
    let before = fs::read(&index).unwrap();

    // The server is out of reach: nothing listens on port 1.
    let unreachable = |args: &[&str]| {
        w.command(CINDEX, &ws, "ws")
            .env("P4PORT", "127.0.0.1:1")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    // Asked for by a person: an error, in Perforce's words, with the way
    // out -- and no quiet walk of the directory instead.
    let out = unreachable(&[]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("Perforce could not list the files"), "{err}");
    assert!(err.contains("--walk"), "{err}");
    assert_eq!(fs::read(&index).unwrap(), before);
    // From a hook: the same, said only if asked, and never a failure.
    let out = unreachable(&["--hook", "--verbose"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("Perforce could not list the files"),
        "{}",
        text(&out.stderr)
    );
    let out = unreachable(&["--hook"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stderr), "");
    assert_eq!(fs::read(&index).unwrap(), before);

    // The server is there, and has never heard of this workspace.
    let out = w.run(CINDEX, &ws, "no-such-workspace", &[]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("knows no workspace called `no-such-workspace`"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fs::read(&index).unwrap(), before);
    assert!(!w.finds(&ws, "marker_never_added", "build.o"));
    assert!(!w.finds(&ws, "marker_edited", "a.c"));

    // --walk is the way out that the message names, and it is one.
    let out = w.run(CINDEX, &ws, "no-such-workspace", &["--walk"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(w.finds(&ws, "marker_never_added", "build.o"));
    assert!(w.finds(&ws, "marker_edited", "a.c"));
}

#[test]
fn local_p4_must_be_asked_from_inside_the_workspace() {
    let Some(w) = World::new() else { return };
    let ws = indexed_workspace(&w);
    let elsewhere = w.dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("x.c"), "int marker_elsewhere;\n").unwrap();

    // P4CLIENT names the workspace, but this directory is not in it.
    let out = w.run(CINDEX, &elsewhere, "ws", &["--local", "--p4"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("does not contain"),
        "{}",
        text(&out.stderr)
    );
    assert!(!elsewhere.join(INDEX_FILE_NAME).exists());

    // Naming a directory of the workspace works from anywhere, and indexes
    // only that directory.
    fs::create_dir_all(ws.join("lib")).unwrap();
    w.submit_new(&ws, "ws", &[("lib/l.c", "int marker_in_lib;\n")], "lib");
    let index = w.dir.path().join("named-index");
    let out = w
        .command(CINDEX, &elsewhere, "ws")
        .env(csearch::names::INDEX_ENV, &index)
        .args(["--p4", ws.join("lib").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let found = command_from(CSEARCH, &elsewhere, &w.home)
        .env(csearch::names::INDEX_ENV, &index)
        .args(["-l", "marker_"])
        .output()
        .unwrap();
    let found = text(&found.stdout);
    let names: Vec<&str> = found.lines().collect();
    assert_eq!(names.len(), 1, "{found}");
    assert!(names[0].ends_with("l.c"), "{found}");
}

#[cfg(unix)]
#[test]
fn a_workspace_reached_through_a_link_is_still_listed() {
    // The client spec spells the root one way and the file system another:
    // p4 prints paths under the link, the index's root is where the link
    // leads.
    let Some(w) = World::new() else { return };
    let real = w.dir.path().join("real");
    fs::create_dir_all(&real).unwrap();
    let link = w.dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let ws = w.client_at("linked", &link);
    w.submit_new(&ws, "linked", &[("a.c", "int marker_linked;\n")], "one");
    settle(&real);

    let out = w.run(CINDEX, &ws, "linked", &["--local", "--p4"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(real.join(INDEX_FILE_NAME).is_file());
    assert!(w.finds(&real, "marker_linked", "a.c"));
    assert!(w.finds(&link, "marker_linked", "a.c"));
}

/// A shell to run a wrapped `p4` in: the one every machine of this kind has.
fn shell() -> (PathBuf, &'static [&'static str], &'static str) {
    if cfg!(windows) {
        (
            on_path("powershell").expect("Windows PowerShell"),
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ],
            "ps1",
        )
    } else {
        (on_path("sh").expect("sh"), &[], "sh")
    }
}

#[test]
fn a_wrapped_p4_sync_refreshes_the_index() {
    // The whole of it, as a user has it: `p4` wrapped in their shell, a
    // `p4 sync` typed, and nothing else.
    let Some(w) = World::new() else { return };
    let ws = indexed_workspace(&w);
    let index = ws.join(INDEX_FILE_NAME);
    let other = w.client("other");
    w.p4(&other, "other", &["sync"]);
    w.submit_new(
        &other,
        "other",
        &[("c.c", "int marker_new_elsewhere;\n")],
        "two",
    );

    let quoted = |p: &Path| {
        let s = p.to_str().unwrap().replace('\\', "/");
        assert!(!s.contains('\''), "a quote in a test path: {s}");
        s
    };
    let (program, run, ext) = shell();
    let (cindex, here) = (quoted(Path::new(CINDEX)), quoted(&ws));
    let script = if ext == "ps1" {
        format!(
            "& '{cindex}' --print-hook p4 --shell powershell | Out-String | Invoke-Expression\n\
             Set-Location -LiteralPath '{here}'\n\
             p4 opened 2>$null | Out-Null\n\
             p4 sync -q; \"status=$LASTEXITCODE\"\n"
        )
    } else {
        format!(
            "eval \"$('{cindex}' --print-hook p4 --shell sh)\" || exit 90\n\
             cd '{here}' || exit 91\n\
             p4 opened >/dev/null 2>&1\n\
             p4 sync -q; echo \"status=$?\"\n"
        )
    };
    let file = w.dir.path().join(format!("script.{ext}"));
    fs::write(&file, &script).unwrap();
    let out = w
        .command(program.to_str().unwrap(), w.dir.path(), "ws")
        .args(run)
        .arg(&file)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n--- script ---\n{script}\n--- stderr ---\n{}",
        out.status,
        text(&out.stderr)
    );
    assert_eq!(
        text(&out.stdout).trim(),
        "status=0",
        "{}",
        text(&out.stderr)
    );
    assert!(ws.join("c.c").is_file(), "the sync itself did not happen");

    let deadline = Instant::now() + PATIENCE;
    while !w.finds(&ws, "marker_new_elsewhere", "c.c") {
        assert!(
            Instant::now() < deadline,
            "the wrapped `p4 sync` did not refresh the index"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Let the refresh finish before the directory it works in is removed.
    for _ in 0..3 {
        drop(lock::acquire(&index, || {}));
        std::thread::sleep(Duration::from_millis(150));
    }
}
