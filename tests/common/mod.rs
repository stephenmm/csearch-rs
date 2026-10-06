//! Shared by the integration tests: the built binaries under their real
//! names, and the sealed environments the end-to-end tests run them in.

#![allow(dead_code)] // each test crate uses its own subset of this

use csearch::names::{original, INDEX_ENV};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// The binaries cargo built for this test run. The names here are the one
/// place the tests spell them out; `tests/names.rs` checks they agree with
/// `csearch::names`.
pub const CINDEX: &str = env!("CARGO_BIN_EXE_cindex-rs");
pub const CSEARCH: &str = env!("CARGO_BIN_EXE_csearch-rs");

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `git -C dir ...` with an identity supplied, so commits work on a machine
/// that has no git configuration at all.
pub fn git(dir: &Path, args: &[&str]) -> bool {
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

/// A command that inherits no index configuration: neither this project's
/// variable nor the original's, so whatever the developer running the tests
/// has exported cannot decide which index a test reads or writes.
pub fn sealed(exe: &str) -> Command {
    let mut cmd = Command::new(exe);
    cmd.env_remove(INDEX_ENV).env_remove(original::INDEX_ENV);
    cmd
}

/// Run a binary against one explicit index file.
pub fn with_index(exe: &str, index: &Path, args: &[&str]) -> Output {
    sealed(exe)
        .env(INDEX_ENV, index)
        .args(args)
        .output()
        .expect("run")
}

/// A command run from `cwd` with no index variable and a private home, so the
/// only way it can find an index is the walk-up rule -- and the home fallback
/// is a path known not to exist rather than whatever this machine has.
///
/// The private home also needs a git config: an empty home plus a work tree on
/// a filesystem that records no ownership (exFAT, and CI temp dirs) makes git
/// refuse the repo with "dubious ownership", which would make `--local` fall
/// back to walking. `safe.directory = *` restores the behaviour git has in a
/// normally configured environment -- it does not disable anything in the
/// product, only in this synthetic home.
pub fn command_from(exe: &str, cwd: &Path, home: &Path) -> Command {
    let gitconfig = home.join(".gitconfig");
    if !gitconfig.exists() {
        fs::write(&gitconfig, "[safe]\n\tdirectory = *\n").unwrap();
    }
    let mut cmd = sealed(exe);
    cmd.current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

/// [`command_from`], run to completion.
pub fn run_from(exe: &str, cwd: &Path, home: &Path, args: &[&str]) -> Output {
    command_from(exe, cwd, home)
        .args(args)
        .output()
        .expect("run")
}
