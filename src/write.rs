//! Parallel index construction.
//!
//! Files are discovered sequentially (sorted, so file ids are in path order),
//! then processed in batches: each batch is trigram-analyzed in parallel with
//! rayon, then appended in file-id order to per-trigram delta-varint posting
//! lists through a dense trigram->slot table (no hashing, no global sort).
//! Memory is bounded by the batch size plus the compressed postings.
//!
//! On-disk layout (all integers little-endian):
//!
//! ```text
//! magic          "csearch-rs index 1\n"
//! paths          root paths, each NUL-terminated, then an extra NUL
//! names          file names, each NUL-terminated (sorted)
//! name index     u32 offset (relative to `names`) per file
//! postings       per trigram: varint first id, then varint deltas
//! posting index  per trigram: u32 trigram, u32 count, u64 offset (16 B)
//! trailer        5 × u64 section offsets, u32 nfiles, u32 ntrigrams,
//!                "CSRSIDX1"
//! ```

use crate::paths::{canonical_string, strip_verbatim};
use crate::trigram::{self, MAX_FILE_LEN};
use crate::varint;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use walkdir::WalkDir;

pub const MAGIC: &[u8] = b"csearch-rs index 1\n";
pub const TRAILER_MAGIC: &[u8; 8] = b"CSRSIDX1";
pub const TRAILER_LEN: usize = 5 * 8 + 4 + 4 + 8;
pub const POST_ENTRY_LEN: usize = 16;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub verbose: bool,
    /// Approximate bytes of source text processed per batch.
    pub batch_bytes: u64,
    /// Files larger than this are skipped, and counted as skipped.
    pub max_file_bytes: u64,
    /// Take the file list from `git ls-files` for roots inside a work tree,
    /// so ignored files are never indexed. Roots outside a repository, or a
    /// machine without git, fall back to walking.
    pub git: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            verbose: false,
            batch_bytes: 256 << 20,
            max_file_bytes: MAX_FILE_LEN,
            git: false,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub files_seen: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub bytes_indexed: u64,
    pub distinct_trigrams: usize,
    pub posting_entries: u64,
    pub index_bytes: u64,
}

#[derive(Default)]
struct Posting {
    trigram: u32,
    last: u32,
    count: u32,
    bytes: Vec<u8>,
}

impl Posting {
    #[inline]
    fn push(&mut self, id: u32) {
        if self.count == 0 {
            varint::put(&mut self.bytes, id);
        } else {
            varint::put(&mut self.bytes, id - self.last);
        }
        self.last = id;
        self.count += 1;
    }
}

fn skip_name(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('#') || name.starts_with('~') || name.ends_with('~')
}

/// True when `child` is `parent` itself or lies somewhere beneath it. Both
/// must be canonical path strings; the check is textual, so `C:\code-other`
/// is correctly not inside `C:\code`.
fn is_within(child: &str, parent: &str) -> bool {
    if !child.starts_with(parent) {
        return false;
    }
    if child.len() == parent.len() {
        return true;
    }
    let sep = |c: u8| c == b'/' || c == b'\\';
    parent.as_bytes().last().is_some_and(|&c| sep(c)) || sep(child.as_bytes()[parent.len()])
}

/// Sort and dedup roots, dropping any that lie inside another so that no
/// file is ever indexed (and reported) twice. Returns the kept roots and,
/// for each dropped one, the root that already covers it.
pub fn collapse_roots(mut roots: Vec<String>) -> (Vec<String>, Vec<(String, String)>) {
    roots.sort();
    roots.dedup();
    let mut kept: Vec<String> = Vec::new();
    let mut dropped = Vec::new();
    for r in roots {
        match kept.iter().find(|k| is_within(&r, k)) {
            Some(k) => dropped.push((r, k.clone())),
            None => kept.push(r),
        }
    }
    (kept, dropped)
}

/// Compare two root strings the way the index stores them: ignoring a
/// trailing separator, and case-insensitively on Windows.
fn same_root(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches(['/', '\\']);
    let b = b.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// The roots the next build should cover, plus notes for the user.
#[derive(Debug, Default)]
pub struct RootPlan {
    pub roots: Vec<PathBuf>,
    pub notes: Vec<String>,
}

/// Work out the root set for a rebuild: the stored roots, minus `remove`,
/// minus any that no longer exist (noted, not fatal, so one deleted
/// directory cannot wedge the index), plus `add`. Roots named explicitly in
/// `add` must exist.
pub fn resolve_roots(stored: &[String], add: &[PathBuf], remove: &[PathBuf]) -> Result<RootPlan> {
    let mut plan = RootPlan::default();
    for p in add {
        if !p.is_dir() {
            bail!("{}: not a directory", p.display());
        }
    }
    // A root being removed may itself have vanished, in which case it cannot
    // be canonicalised; fall back to the string as typed.
    let removed: Vec<String> = remove
        .iter()
        .map(|p| canonical_string(p).unwrap_or_else(|_| strip_verbatim(&p.to_string_lossy())))
        .collect();
    for r in &removed {
        if !stored.iter().any(|s| same_root(s, r)) {
            plan.notes.push(format!("{r}: not in the index"));
        }
    }
    plan.roots.extend(add.iter().cloned());
    for s in stored {
        if removed.iter().any(|r| same_root(s, r)) {
            plan.notes.push(format!("{s}: removed"));
        } else if !Path::new(s).is_dir() {
            plan.notes
                .push(format!("{s}: no longer exists, dropped from the index"));
        } else {
            plan.roots.push(PathBuf::from(s));
        }
    }
    Ok(plan)
}

/// Collect regular files under `roots` in sorted order with their sizes,
/// plus the number of files skipped for being over `max_file_bytes`.
fn walk(roots: &[String], max_file_bytes: u64) -> (Vec<(PathBuf, u64)>, usize) {
    let mut files = Vec::new();
    let mut too_large = 0usize;
    for root in roots {
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
                    eprintln!("cindex: {err}");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if len > max_file_bytes {
                eprintln!(
                    "cindex: {}: {len} bytes is over the {max_file_bytes}-byte limit, skipping",
                    entry.path().display()
                );
                too_large += 1;
                continue;
            }
            files.push((entry.into_path(), len));
        }
    }
    (files, too_large)
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

/// The files to index under `roots`: git's list when asked for and
/// available, otherwise a directory walk. Returns (path, size) pairs and the
/// number skipped for being over the size limit.
fn collect_files(roots: &[String], opts: &BuildOptions) -> (Vec<(PathBuf, u64)>, usize) {
    if !opts.git {
        return walk(roots, opts.max_file_bytes);
    }
    let mut files = Vec::new();
    let mut too_large = 0usize;
    for root in roots {
        let listed = match git_files(root) {
            GitList::Files(f) => f,
            GitList::NotARepo => {
                eprintln!("cindex: {root}: not a git work tree, walking the directory instead");
                let (walked, n) = walk(std::slice::from_ref(root), opts.max_file_bytes);
                files.extend(walked);
                too_large += n;
                continue;
            }
            GitList::Unavailable(msg) => {
                // Show git's own words: silently walking would index the very
                // files --git was meant to exclude, so the user should see why.
                eprintln!("cindex: {root}: could not use git, walking the directory instead");
                for line in msg.lines() {
                    eprintln!("cindex:   {line}");
                }
                let (walked, n) = walk(std::slice::from_ref(root), opts.max_file_bytes);
                files.extend(walked);
                too_large += n;
                continue;
            }
        };
        for path in listed {
            // The same name rules as the walk, applied to every component
            // below the root, so `.github/` and editor droppings are treated
            // exactly as they are without --git (and as ripgrep treats them).
            let hidden = path.strip_prefix(root).is_ok_and(|rel| {
                rel.components()
                    .any(|c| c.as_os_str().to_str().is_some_and(skip_name))
            });
            if hidden {
                continue;
            }
            // git may list a file deleted since the last commit, a symlink,
            // or a submodule / nested-repository directory; only regular files
            // are indexed, as with walking.
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let len = meta.len();
            if len > opts.max_file_bytes {
                eprintln!(
                    "cindex: {}: {len} bytes is over the {}-byte limit, skipping",
                    path.display(),
                    opts.max_file_bytes
                );
                too_large += 1;
                continue;
            }
            files.push((path, len));
        }
    }
    // git lists in index order; sort so file ids are deterministic and
    // path-ordered, as they are when walking.
    files.sort();
    (files, too_large)
}

/// Build a fresh index of `roots` at `out`.
pub fn build_index(roots: &[PathBuf], out: &Path, opts: &BuildOptions) -> Result<Stats> {
    let t0 = Instant::now();
    let mut root_strs: Vec<String> = Vec::new();
    for r in roots {
        root_strs.push(canonical_string(r).with_context(|| format!("resolving {}", r.display()))?);
    }
    let (root_strs, dropped) = collapse_roots(root_strs);
    for (child, parent) in &dropped {
        eprintln!("cindex: {child} is inside {parent}, not indexing it twice");
    }

    let (files, too_large) = collect_files(&root_strs, opts);
    let mut stats = Stats {
        files_seen: files.len() + too_large,
        files_skipped: too_large,
        ..Default::default()
    };
    if opts.verbose {
        eprintln!(
            "cindex: {} files found in {:.2?}",
            files.len(),
            t0.elapsed()
        );
    }

    let mut names: Vec<String> = Vec::with_capacity(files.len());
    // Dense trigram -> posting slot map replaces a hash lookup per posting;
    // postings are appended in file-id order so the lists come out sorted
    // without any global sort. Entries hold `slot + 1` so that zero means
    // "unseen": a zero-filled 64 MiB Vec is a calloc whose pages are only
    // touched for trigrams that actually occur, so a tiny corpus costs a
    // tiny amount of memory rather than the whole table.
    let mut slot: Vec<u32> = vec![0; 1 << 24];
    let mut postings: Vec<Posting> = Vec::new();

    let mut start = 0usize;
    while start < files.len() {
        // Cut a batch.
        let mut end = start;
        let mut bytes = 0u64;
        while end < files.len() && (end == start || bytes + files[end].1 <= opts.batch_bytes) {
            bytes += files[end].1;
            end += 1;
        }
        let batch = &files[start..end];

        let analyzed: Vec<Option<(String, Vec<u32>, u64)>> = batch
            .par_iter()
            .map(|(path, _)| {
                let data = match fs::read(path) {
                    Ok(d) => d,
                    Err(err) => {
                        // Always reported: unlike a binary file, an unreadable
                        // one is rare and something the user can act on.
                        eprintln!("cindex: {}: {err}", path.display());
                        return None;
                    }
                };
                match trigram::analyze(&data) {
                    Ok(tris) => {
                        let name = crate::paths::strip_verbatim(&path.to_string_lossy());
                        Some((name, tris, data.len() as u64))
                    }
                    Err(skip) => {
                        if opts.verbose {
                            eprintln!("cindex: {}: {skip}, skipping", path.display());
                        }
                        None
                    }
                }
            })
            .collect();

        // Assign ids in order and append to posting lists.
        for item in analyzed {
            match item {
                Some((name, tris, len)) => {
                    let id = names.len() as u32;
                    names.push(name);
                    stats.bytes_indexed += len;
                    stats.posting_entries += tris.len() as u64;
                    for t in tris {
                        let s = slot[t as usize];
                        let p = if s == 0 {
                            slot[t as usize] = postings.len() as u32 + 1;
                            postings.push(Posting {
                                trigram: t,
                                ..Default::default()
                            });
                            postings.last_mut().unwrap()
                        } else {
                            &mut postings[(s - 1) as usize]
                        };
                        p.push(id);
                    }
                }
                None => stats.files_skipped += 1,
            }
        }

        if opts.verbose {
            eprintln!(
                "cindex: {}/{} files, {} distinct trigrams, {:.2?}",
                names.len() + stats.files_skipped,
                files.len(),
                postings.len(),
                t0.elapsed()
            );
        }
        start = end;
    }
    drop(slot);
    stats.files_indexed = names.len();
    stats.distinct_trigrams = postings.len();

    // Write.
    postings.par_sort_unstable_by_key(|p| p.trigram);

    let tmp = out.with_extension("tmp");
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
        }
    }
    let off = match write_index_file(&tmp, &root_strs, &names, &postings) {
        Ok(off) => off,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };
    if let Err(e) = replace_file(&tmp, out) {
        let _ = fs::remove_file(&tmp);
        return Err(e.context(format!("installing {}", out.display())));
    }
    stats.index_bytes = off;

    if opts.verbose {
        eprintln!(
            "cindex: wrote {} ({} bytes, {} files, {} trigrams) in {:.2?}",
            out.display(),
            off,
            names.len(),
            postings.len(),
            t0.elapsed()
        );
    }
    Ok(stats)
}

/// Write the whole index to `tmp`; returns its size in bytes.
fn write_index_file(
    tmp: &Path,
    root_strs: &[String],
    names: &[String],
    postings: &[Posting],
) -> Result<u64> {
    let f = File::create(tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, f);
    let mut off: u64 = 0;

    w.write_all(MAGIC)?;
    off += MAGIC.len() as u64;

    let paths_off = off;
    for r in root_strs {
        w.write_all(r.as_bytes())?;
        w.write_all(&[0])?;
        off += r.len() as u64 + 1;
    }
    w.write_all(&[0])?;
    off += 1;

    let names_off = off;
    let mut name_index: Vec<u8> = Vec::with_capacity(names.len() * 4);
    for n in names {
        name_index.extend_from_slice(&((off - names_off) as u32).to_le_bytes());
        w.write_all(n.as_bytes())?;
        w.write_all(&[0])?;
        off += n.len() as u64 + 1;
    }

    let nameidx_off = off;
    w.write_all(&name_index)?;
    off += name_index.len() as u64;

    let posts_off = off;
    let mut post_index: Vec<u8> = Vec::with_capacity(postings.len() * POST_ENTRY_LEN);
    for p in postings {
        post_index.extend_from_slice(&p.trigram.to_le_bytes());
        post_index.extend_from_slice(&p.count.to_le_bytes());
        post_index.extend_from_slice(&(off - posts_off).to_le_bytes());
        w.write_all(&p.bytes)?;
        off += p.bytes.len() as u64;
    }

    let postidx_off = off;
    w.write_all(&post_index)?;
    off += post_index.len() as u64;

    for v in [paths_off, names_off, nameidx_off, posts_off, postidx_off] {
        w.write_all(&v.to_le_bytes())?;
    }
    w.write_all(&(names.len() as u32).to_le_bytes())?;
    w.write_all(&(postings.len() as u32).to_le_bytes())?;
    w.write_all(TRAILER_MAGIC)?;
    off += TRAILER_LEN as u64;
    w.flush()?;
    Ok(off)
}

/// Move the finished index from `tmp` into place at `out`, replacing any
/// existing one without a moment in which no index exists, and without
/// disturbing readers that still have the old file mapped.
fn replace_file(tmp: &Path, out: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // rename(2) replaces atomically; existing mappings keep the old inode.
        fs::rename(tmp, out)?;
    }
    #[cfg(windows)]
    {
        // A mapped file cannot be deleted or overwritten on Windows, but it
        // can be renamed. Park the old index aside, install the new one, then
        // delete the parked copy -- or leave it for next time if a reader
        // still holds it.
        let old = out.with_extension("old");
        let _ = fs::remove_file(&old);
        let had_old = out.is_file();
        if had_old {
            // Fails only for a reader that opened the file without
            // FILE_SHARE_DELETE (not csearch, which shares it); nothing in
            // user space can move such a file, so say what to do.
            fs::rename(out, &old).context(
                "another program has the index open in a way that blocks replacing it; close it and re-run",
            )?;
        }
        if let Err(e) = fs::rename(tmp, out) {
            if had_old {
                let _ = fs::rename(&old, out);
            }
            return Err(e.into());
        }
        let _ = fs::remove_file(&old);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = fs::remove_file(out);
        fs::rename(tmp, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_roots_tolerates_vanished_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep");
        let add = dir.path().join("add");
        let gone = dir.path().join("gone");
        fs::create_dir(&keep).unwrap();
        fs::create_dir(&add).unwrap();
        let stored = vec![
            canonical_string(&keep).unwrap(),
            strip_verbatim(&gone.to_string_lossy()), // was indexed, then deleted
        ];

        let plan = resolve_roots(&stored, std::slice::from_ref(&add), &[]).unwrap();
        let ends = |s: &str| plan.roots.iter().any(|r| r.to_string_lossy().ends_with(s));
        assert!(
            ends("add") && ends("keep") && !ends("gone"),
            "{:?}",
            plan.roots
        );
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("gone") && n.contains("no longer exists")),
            "{:?}",
            plan.notes
        );

        let plan = resolve_roots(&stored, &[], std::slice::from_ref(&keep)).unwrap();
        assert!(plan.roots.is_empty(), "{:?}", plan.roots);
        assert!(
            plan.notes.iter().any(|n| n.ends_with(": removed")),
            "{:?}",
            plan.notes
        );

        // Removing something never indexed is noted, not fatal.
        let plan = resolve_roots(&stored, &[], std::slice::from_ref(&add)).unwrap();
        assert!(
            plan.notes.iter().any(|n| n.contains("not in the index")),
            "{:?}",
            plan.notes
        );

        // Paths named on the command line must exist.
        assert!(resolve_roots(&stored, std::slice::from_ref(&gone), &[]).is_err());
    }

    #[test]
    fn rebuild_while_index_is_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "first\n").unwrap();
        let out = dir.path().join("index");
        build_index(std::slice::from_ref(&root), &out, &BuildOptions::default()).unwrap();

        // A running csearch holds the index mapped; a rebuild must still
        // succeed, and the old mapping must stay readable.
        let held = crate::read::Index::open(&out).unwrap();
        fs::write(root.join("b.txt"), "second\n").unwrap();
        build_index(std::slice::from_ref(&root), &out, &BuildOptions::default()).unwrap();

        assert_eq!(held.num_files(), 1);
        assert!(held.name(0).ends_with("a.txt"));
        let fresh = crate::read::Index::open(&out).unwrap();
        assert_eq!(fresh.num_files(), 2);
        assert!(!out.with_extension("tmp").exists(), "tmp file left behind");
    }

    #[test]
    fn failed_build_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "x\n").unwrap();
        // The target is a directory, so the final rename cannot succeed.
        let out = dir.path().join("index");
        fs::create_dir(&out).unwrap();
        assert!(build_index(&[root], &out, &BuildOptions::default()).is_err());
        assert!(!out.with_extension("tmp").exists(), "tmp file left behind");
        assert!(out.is_dir(), "the directory in the way must be untouched");
    }

    #[test]
    fn too_large_files_are_counted_as_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("small.txt"), "tiny\n").unwrap();
        fs::write(root.join("big.txt"), "x".repeat(64)).unwrap();
        let out = dir.path().join("index");
        // Over-limit files used to vanish without being counted at all.
        let opts = BuildOptions {
            max_file_bytes: 16,
            ..Default::default()
        };
        let stats = build_index(std::slice::from_ref(&root), &out, &opts).unwrap();
        assert_eq!(
            (stats.files_indexed, stats.files_skipped, stats.files_seen),
            (1, 1, 2)
        );
    }

    #[test]
    fn within_respects_separators() {
        assert!(is_within(r"C:\code\sub", r"C:\code"));
        assert!(is_within("/home/u/proj/sub", "/home/u/proj"));
        assert!(is_within(r"C:\code", r"C:\code"));
        assert!(is_within(r"C:\code", r"C:\")); // a drive root ends in a separator
        assert!(!is_within(r"C:\code-other", r"C:\code"));
        assert!(!is_within(r"E:\pro", r"C:\code"));
        assert!(!is_within("/a", "/a/b"));
    }

    #[test]
    fn collapse_drops_nested_roots() {
        let (kept, dropped) = collapse_roots(vec![
            "/a/b/c".into(),
            "/a/b".into(),
            "/a/bc".into(),
            "/a/b".into(),
            "/x".into(),
        ]);
        assert_eq!(kept, vec!["/a/b", "/a/bc", "/x"]);
        assert_eq!(dropped, vec![("/a/b/c".to_string(), "/a/b".to_string())]);
    }

    #[test]
    fn nested_root_is_indexed_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), "needle\n").unwrap();
        fs::write(root.join("sub/b.txt"), "needle\n").unwrap();
        let out = dir.path().join("index");
        // Adding a subdirectory of an existing root used to index it twice.
        let stats = build_index(
            &[root.clone(), root.join("sub")],
            &out,
            &BuildOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.files_indexed, 2);
        let idx = crate::read::Index::open(&out).unwrap();
        assert_eq!(idx.roots().len(), 1, "the nested root must not be stored");
        assert_eq!(idx.posting_count(trigram::pack(b"nee")), 2);
    }
}
