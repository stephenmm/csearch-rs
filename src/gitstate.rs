//! Git-state stamping, so `cindex` can skip a rebuild when nothing has changed
//! and `csearch` can warn when the index is behind the working tree.
//!
//! Everything here is best-effort. Any git failure means "state unknown",
//! which callers treat conservatively: `cindex --if-changed` rebuilds, and
//! `csearch` simply stays quiet. The stamp is a sidecar next to the index
//! (`<index>.meta`); losing or mangling it only ever costs one extra rebuild,
//! so it needs no format versioning beyond a header line.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The recorded git state of one indexed root: its `HEAD` and a fingerprint of
/// the working-tree changes (`git status --porcelain`), so that both a new
/// commit and an uncommitted edit count as a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootState {
    pub root: String,
    pub head: String,
    pub dirty: u64,
}

fn git_output(root: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// FNV-1a over the porcelain bytes: a cheap change fingerprint. A collision
/// would at worst skip one needed rebuild, which the next git event corrects.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Full state of `root` (HEAD + working-tree fingerprint), or `None` when it is
/// not a git repository or git cannot be run.
pub fn current_state(root: &str) -> Option<RootState> {
    let head = git_output(root, &["rev-parse", "HEAD"])?;
    let status = git_output(root, &["status", "--porcelain"])?;
    Some(RootState {
        root: root.to_string(),
        head: String::from_utf8_lossy(&head).trim().to_string(),
        dirty: fnv1a(&status),
    })
}

/// Just `HEAD` -- one cheap git call for the search-time staleness check, which
/// runs on every search and so must stay fast.
pub fn current_head(root: &str) -> Option<String> {
    let head = git_output(root, &["rev-parse", "HEAD"])?;
    Some(String::from_utf8_lossy(&head).trim().to_string())
}

/// The stamp sidecar path for an index file.
pub fn stamp_path(index: &Path) -> PathBuf {
    let mut s = index.as_os_str().to_owned();
    s.push(".meta");
    PathBuf::from(s)
}

/// Record the state of every git root in `roots` beside `index`. Roots that are
/// not git repositories are simply omitted; if none are, any old stamp is
/// removed so a stale one never lingers. Best-effort: write errors are ignored.
pub fn write_stamp(index: &Path, roots: &[String]) {
    let mut text = String::from("csearch-rs stamp 1\n");
    let mut any = false;
    for r in roots {
        if let Some(st) = current_state(r) {
            // head has no tab; dirty is decimal; root is last so it may hold
            // spaces (paths do). None of the three can contain a newline.
            text.push_str(&format!("{}\t{}\t{}\n", st.head, st.dirty, st.root));
            any = true;
        }
    }
    let path = stamp_path(index);
    if any {
        let _ = std::fs::write(path, text);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// The states recorded in the stamp beside `index`; empty if there is no stamp
/// or it cannot be parsed (treated as "unknown").
pub fn read_stamp(index: &Path) -> Vec<RootState> {
    let Ok(text) = std::fs::read_to_string(stamp_path(index)) else {
        return Vec::new();
    };
    let mut lines = text.lines();
    if lines.next() != Some("csearch-rs stamp 1") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        let mut it = line.splitn(3, '\t');
        if let (Some(h), Some(d), Some(r)) = (it.next(), it.next(), it.next()) {
            if let Ok(dirty) = d.parse::<u64>() {
                out.push(RootState {
                    root: r.to_string(),
                    head: h.to_string(),
                    dirty,
                });
            }
        }
    }
    out
}

/// Whether an index covering exactly `roots` is still current: there is a
/// stamp, it names the same roots, and every one is a git repository whose
/// state is unchanged. Anything unknown returns `false` -- rebuild, never risk
/// a stale skip.
pub fn is_current(index: &Path, roots: &[String]) -> bool {
    let stamped = read_stamp(index);
    if stamped.is_empty() || stamped.len() != roots.len() {
        return false;
    }
    for r in roots {
        let Some(cur) = current_state(r) else {
            return false; // a non-git root can't be proven unchanged
        };
        match stamped.iter().find(|s| &s.root == r) {
            Some(s) if s.head == cur.head && s.dirty == cur.dirty => {}
            _ => return false,
        }
    }
    true
}

/// A one-line staleness message if any stamped root's `HEAD` has moved since
/// the index was built, else `None`. HEAD-only so it stays cheap on the search
/// path; uncommitted edits are not flagged here (a git event, and its hook,
/// is what refreshes the index).
pub fn staleness(index: &Path) -> Option<String> {
    let stamped = read_stamp(index);
    let behind = stamped
        .iter()
        .filter(|s| current_head(&s.root).is_some_and(|h| h != s.head))
        .count();
    match behind {
        0 => None,
        1 => Some("csearch: the index is behind HEAD in 1 root -- run cindex to refresh".into()),
        n => Some(format!(
            "csearch: the index is behind HEAD in {n} roots -- run cindex to refresh"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("idx");
        std::fs::write(&index, b"x").unwrap();
        // A stamp written by hand (no git needed) reads back intact, including
        // a root path containing spaces.
        let text = "csearch-rs stamp 1\ndeadbeef\t42\tC:\\code\\my project\n";
        std::fs::write(stamp_path(&index), text).unwrap();
        let got = read_stamp(&index);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].head, "deadbeef");
        assert_eq!(got[0].dirty, 42);
        assert_eq!(got[0].root, r"C:\code\my project");
    }

    #[test]
    fn a_missing_or_foreign_stamp_reads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("idx");
        assert!(read_stamp(&index).is_empty()); // no sidecar
        std::fs::write(stamp_path(&index), "something else\n").unwrap();
        assert!(read_stamp(&index).is_empty()); // wrong header
    }

    #[test]
    fn dirty_fingerprint_changes_with_content() {
        assert_ne!(fnv1a(b" M a.rs\n"), fnv1a(b" M b.rs\n"));
        assert_eq!(fnv1a(b""), fnv1a(b""));
    }
}
