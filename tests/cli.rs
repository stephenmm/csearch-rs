//! End-to-end tests that drive the real `cindex` and `csearch` binaries.
//!
//! These exist because every bug they cover was invisible to the unit tests:
//! the defects were in how the binaries stitch the library together.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn cindex(index: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cindex"))
        .env("CSEARCHINDEX", index)
        .args(args)
        .output()
        .expect("run cindex")
}

fn csearch(index: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_csearch"))
        .env("CSEARCHINDEX", index)
        .args(args)
        .output()
        .expect("run csearch")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn nested_roots_do_not_duplicate_results() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), "needle here\n").unwrap();
    fs::write(root.join("sub/b.txt"), "needle too\n").unwrap();
    let index = dir.path().join("index");

    assert!(cindex(&index, &[root.to_str().unwrap()]).status.success());
    // The original bug: adding a subdirectory of an existing root indexed
    // every file under it a second time, and csearch printed each twice.
    let out = cindex(&index, &[root.join("sub").to_str().unwrap()]);
    assert!(out.status.success());
    assert!(text(&out.stderr).contains("not indexing it twice"), "{}", text(&out.stderr));

    let roots = text(&cindex(&index, &["--list"]).stdout);
    assert_eq!(roots.lines().count(), 1, "nested root must not be stored: {roots}");

    let out = text(&csearch(&index, &["-c", "needle"]).stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    let mut unique = lines.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 2, "duplicate result lines: {out}");
}
