//! Which files belong to a root.
//!
//! Each root is *listed* from a source: a directory walk, or what git
//! considers part of the work tree. The index records the source of every
//! root, so that a re-index lists a root the way the index was built. Without
//! that, an index made from `git ls-files` would be rebuilt by walking, and
//! every ignored file would arrive in it.

use crate::names::CINDEX;
use anyhow::{bail, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use walkdir::WalkDir;

/// How the files under a root are found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Every regular file under the directory, apart from names beginning
    /// `.`, `#` or `~`, or ending `~`.
    Walk,
    /// What git considers part of the work tree: tracked files, plus
    /// untracked ones that are not ignored.
    Git,
}

impl Source {
    pub const ALL: [Source; 2] = [Source::Walk, Source::Git];

    /// The name stored in the index and shown by `--list --verbose`.
    pub fn tag(self) -> &'static str {
        match self {
            Source::Walk => "walk",
            Source::Git => "git",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Source> {
        Self::ALL.into_iter().find(|s| s.tag() == tag)
    }
}

/// A root directory, as a canonical path string, and how it is listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: String,
    pub source: Source,
}

/// One file to index.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub len: u64,
}

/// The files under a set of roots at one moment.
#[derive(Debug)]
pub struct Snapshot {
    pub roots: Vec<Root>,
    /// In path order within each root, roots in order, so file ids are
    /// deterministic and search output comes out sorted.
    pub files: Vec<FileEntry>,
    /// Files left out for being over the size limit.
    pub too_large: usize,
}

/// How to list.
#[derive(Debug, Clone, Copy)]
pub struct ListOptions {
    /// Files larger than this are left out, and counted.
    pub max_file_bytes: u64,
    /// When a version-control listing cannot be had, fail instead of walking.
    pub strict: bool,
}

fn skip_name(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('#') || name.starts_with('~') || name.ends_with('~')
}

/// Every regular file under `root`, in path order, appended to `files`.
fn walk(root: &str, opts: &ListOptions, files: &mut Vec<FileEntry>, too_large: &mut usize) {
    let it = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_str().is_some_and(skip_name));
    for entry in it {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                // A directory we cannot enter is worth a line even without
                // --verbose; the user would otherwise never know.
                eprintln!("{CINDEX}: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if len > opts.max_file_bytes {
            eprintln!(
                "{CINDEX}: {}: {len} bytes is over the {}-byte limit, skipping",
                entry.path().display(),
                opts.max_file_bytes
            );
            *too_large += 1;
            continue;
        }
        files.push(FileEntry {
            path: entry.into_path(),
            len,
        });
    }
}

/// Outcome of asking git for the file list under a root.
enum GitList {
    /// The work-tree files: tracked, plus untracked but not ignored.
    Files(Vec<PathBuf>),
    /// `root` is simply not inside a git repository -- expected, walk quietly.
    NotARepo,
    /// git could not list the files for some other reason (not installed, or
    /// an error such as "dubious ownership" on a filesystem without ownership,
    /// common on exFAT). Carries git's own message so the caller can show it,
    /// because silently walking would index files the user asked git to skip.
    Unavailable(String),
}

/// What git considers part of the work tree under `root`: tracked files plus
/// untracked-but-not-ignored ones -- what a developer means by "the repo".
fn git_files(root: &str) -> GitList {
    let out = match Command::new("git")
        .args([
            "-C",
            root,
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return GitList::Unavailable("git could not be run".into()),
    };
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // git reports both "not a repository" and real errors as exit 128;
        // only the former is the routine "this root isn't a repo" case.
        if msg.contains("not a git repository") {
            return GitList::NotARepo;
        }
        return GitList::Unavailable(msg);
    }
    let mut files = Vec::new();
    for rel in out.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rel = String::from_utf8_lossy(rel);
        // git always prints '/'; rebuild with the platform separator so the
        // stored names look the same as walked ones do.
        let mut path = PathBuf::from(root);
        path.extend(rel.split('/'));
        files.push(path);
    }
    GitList::Files(files)
}

/// The files a version-control system listed under `root`, filtered by the
/// same rules a walk applies, in path order, appended to `files`.
fn keep_listed(
    root: &str,
    listed: Vec<PathBuf>,
    opts: &ListOptions,
    files: &mut Vec<FileEntry>,
    too_large: &mut usize,
) {
    let start = files.len();
    for path in listed {
        // The same name rules as the walk, applied to every component below
        // the root, so `.github/` and editor droppings are treated exactly as
        // they are when walking (and as ripgrep treats them).
        let hidden = path.strip_prefix(root).is_ok_and(|rel| {
            rel.components()
                .any(|c| c.as_os_str().to_str().is_some_and(skip_name))
        });
        if hidden {
            continue;
        }
        // The list may name a file deleted since, a symlink, or a submodule
        // or nested-repository directory; only regular files are indexed, as
        // with walking.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let len = meta.len();
        if len > opts.max_file_bytes {
            eprintln!(
                "{CINDEX}: {}: {len} bytes is over the {}-byte limit, skipping",
                path.display(),
                opts.max_file_bytes
            );
            *too_large += 1;
            continue;
        }
        files.push(FileEntry { path, len });
    }
    // A version-control system lists in its own order; sort so file ids are
    // deterministic and path-ordered, as they are when walking.
    files[start..].sort_by(|a, b| a.path.cmp(&b.path));
}

/// List the files under `roots`, each from its own source.
///
/// A root whose version-control listing is unavailable is walked instead,
/// with the reason on stderr -- unless `opts.strict`, in which case that is
/// an error and nothing is listed.
pub fn snapshot(roots: &[Root], opts: &ListOptions) -> Result<Snapshot> {
    let mut files = Vec::new();
    let mut too_large = 0usize;
    for root in roots {
        match root.source {
            Source::Walk => walk(&root.path, opts, &mut files, &mut too_large),
            Source::Git => match git_files(&root.path) {
                GitList::Files(listed) => {
                    keep_listed(&root.path, listed, opts, &mut files, &mut too_large)
                }
                GitList::NotARepo => {
                    eprintln!(
                        "{CINDEX}: {}: not a git work tree, walking the directory instead",
                        root.path
                    );
                    walk(&root.path, opts, &mut files, &mut too_large);
                }
                GitList::Unavailable(msg) => {
                    if opts.strict {
                        bail!(
                            "{}: git could not list the files ({})",
                            root.path,
                            msg.lines().next().unwrap_or("no message")
                        );
                    }
                    // Show git's own words: silently walking would index the
                    // very files --git was meant to exclude, so the user
                    // should see why.
                    eprintln!(
                        "{CINDEX}: {}: could not use git, walking the directory instead",
                        root.path
                    );
                    for line in msg.lines() {
                        eprintln!("{CINDEX}:   {line}");
                    }
                    walk(&root.path, opts, &mut files, &mut too_large);
                }
            },
        }
    }
    Ok(Snapshot {
        roots: roots.to_vec(),
        files,
        too_large,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_tags_round_trip() {
        for s in Source::ALL {
            assert_eq!(Source::from_tag(s.tag()), Some(s));
        }
        assert_eq!(Source::from_tag("svn"), None);
        assert_eq!(Source::from_tag(""), None);
    }
}
