//! Parallel index construction.
//!
//! Files are listed sequentially (sorted, so file ids are in path order),
//! then processed in batches: each batch is trigram-analyzed in parallel with
//! rayon, then appended in file-id order to per-trigram delta-varint posting
//! lists through a dense trigram->slot table (no hashing, no global sort).
//! Memory is bounded by the batch size plus the compressed postings.
//!
//! On-disk layout (all integers little-endian):
//!
//! ```text
//! magic          "csearch-rs index 2\n"
//! roots          per root: path, NUL, listing source, NUL; then an extra NUL
//! names          file names, each NUL-terminated (sorted)
//! name index     u32 offset (relative to `names`) per file
//! postings       per trigram: varint first id, then varint deltas
//! posting index  per trigram: u32 trigram, u32 count, u64 offset (16 B)
//! trailer        5 × u64 section offsets, u32 nfiles, u32 ntrigrams,
//!                "CSRSIDX2"
//! ```
//!
//! Format 1 had no listing source: a root was a bare path, and whether it was
//! walked or taken from git depended on the flags of whichever run came next.

use crate::listing::{snapshot, ListOptions, Root, Snapshot, Source};
use crate::names::CINDEX;
use crate::paths::{canonical_string, sidecar, strip_verbatim};
use crate::trigram::{self, MAX_FILE_LEN};
use crate::varint;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const MAGIC: &[u8] = b"csearch-rs index 2\n";
/// Every format version starts with this; only the current one matches
/// `MAGIC` in full. It is also what tells an index of ours from one written
/// by the original csearch, whose magic is `csearch index`.
pub const MAGIC_FAMILY: &[u8] = b"csearch-rs index ";
pub const TRAILER_MAGIC: &[u8; 8] = b"CSRSIDX2";
pub const TRAILER_LEN: usize = 5 * 8 + 4 + 4 + 8;
pub const POST_ENTRY_LEN: usize = 16;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub verbose: bool,
    /// Approximate bytes of source text processed per batch.
    pub batch_bytes: u64,
    /// Files larger than this are skipped, and counted as skipped.
    pub max_file_bytes: u64,
    /// For [`build_index`] only: list every root through git rather than by
    /// walking. (An index built through [`plan_roots`] has a source per root.)
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
/// for each dropped one, the root that already covers it -- whose listing
/// source is the one that then applies to those files.
pub fn collapse_roots(mut roots: Vec<Root>) -> (Vec<Root>, Vec<(String, String)>) {
    roots.sort_by(|a, b| a.path.cmp(&b.path));
    roots.dedup_by(|b, a| a.path == b.path);
    let mut kept: Vec<Root> = Vec::new();
    let mut dropped = Vec::new();
    for r in roots {
        match kept.iter().find(|k| is_within(&r.path, &k.path)) {
            Some(k) => dropped.push((r.path, k.path.clone())),
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

/// What a run of the indexer was asked to do to the set of roots.
#[derive(Debug, Default)]
pub struct Request<'a> {
    /// The roots the index holds now.
    pub stored: &'a [Root],
    /// Directories named on the command line.
    pub add: &'a [PathBuf],
    /// The root `--local` found, if it was given: the enclosing repository,
    /// or the working directory.
    pub local: Option<&'a Path>,
    /// Roots to drop.
    pub remove: &'a [PathBuf],
    /// `--git` or `--walk`, if either was given.
    pub listing: Option<Source>,
}

/// The roots the next build should cover, plus notes for the user.
#[derive(Debug, Default)]
pub struct RootPlan {
    pub roots: Vec<Root>,
    pub notes: Vec<String>,
}

/// Work out the root set for a rebuild, and how each root is to be listed.
///
/// The set is the stored roots, minus `remove`, minus any that no longer
/// exist (noted, not fatal, so one deleted directory cannot wedge the index),
/// plus the named ones, which must exist. Roots inside other roots collapse
/// into them.
///
/// A root's listing source is remembered, so the question is only when it
/// changes:
///
/// - a listing flag applies to the roots named in this run -- or, when none
///   is named, to every root;
/// - otherwise a root keeps the source it has;
/// - a root seen for the first time is walked, except that `--local` lists a
///   git repository through git.
pub fn plan_roots(req: &Request) -> Result<RootPlan> {
    let mut notes = Vec::new();
    let named: Vec<&Path> = req
        .add
        .iter()
        .map(PathBuf::as_path)
        .chain(req.local)
        .collect();
    for p in &named {
        if !p.is_dir() {
            bail!("{}: not a directory", p.display());
        }
    }
    // A root being removed may itself have vanished, in which case it cannot
    // be canonicalised; fall back to the string as typed.
    let removed: Vec<String> = req
        .remove
        .iter()
        .map(|p| canonical_string(p).unwrap_or_else(|_| strip_verbatim(&p.to_string_lossy())))
        .collect();
    for r in &removed {
        if !req.stored.iter().any(|s| same_root(&s.path, r)) {
            notes.push(format!("{r}: not in the index"));
        }
    }

    let mut wanted: Vec<Root> = Vec::new();
    for s in req.stored {
        if removed.iter().any(|r| same_root(&s.path, r)) {
            notes.push(format!("{}: removed", s.path));
        } else if !Path::new(&s.path).is_dir() {
            notes.push(format!(
                "{}: no longer exists, dropped from the index",
                s.path
            ));
        } else {
            let source = match req.listing {
                Some(flag) if named.is_empty() => flag,
                _ => s.source,
            };
            wanted.push(Root {
                path: canonical_string(Path::new(&s.path)).unwrap_or_else(|_| s.path.clone()),
                source,
            });
        }
    }
    let resolve =
        |p: &Path| canonical_string(p).with_context(|| format!("resolving {}", p.display()));
    let local = req.local.map(resolve).transpose()?;
    for p in &named {
        let path = resolve(p)?;
        let known = wanted.iter().position(|w| same_root(&w.path, &path));
        // Compared as resolved paths: `--local .` names the local root twice,
        // once as typed and once as found.
        let is_local = local.as_deref().is_some_and(|l| same_root(l, &path));
        let source = match (req.listing, known) {
            (Some(flag), _) => flag,
            (None, Some(i)) => wanted[i].source,
            (None, None) if is_local && Path::new(&path).join(".git").exists() => Source::Git,
            (None, None) => Source::Walk,
        };
        match known {
            Some(i) => wanted[i].source = source,
            None => wanted.push(Root { path, source }),
        }
    }

    let (roots, dropped) = collapse_roots(wanted);
    for (child, parent) in dropped {
        notes.push(format!("{child} is inside {parent}, not indexing it twice"));
    }
    Ok(RootPlan { roots, notes })
}

/// Build a fresh index of `roots` at `out`, every root listed the same way:
/// through git if `opts.git`, else by walking.
///
/// This is the library's one-call entry point. The indexer itself goes
/// through [`plan_roots`] and [`build_from`], so that each root keeps the
/// source recorded for it.
pub fn build_index(roots: &[PathBuf], out: &Path, opts: &BuildOptions) -> Result<Stats> {
    let t0 = Instant::now();
    let source = if opts.git { Source::Git } else { Source::Walk };
    let plan = plan_roots(&Request {
        add: roots,
        listing: Some(source),
        ..Default::default()
    })?;
    for note in &plan.notes {
        eprintln!("{CINDEX}: {note}");
    }
    let snap = snapshot(
        &plan.roots,
        &ListOptions {
            max_file_bytes: opts.max_file_bytes,
            strict: false,
        },
    )?;
    if opts.verbose {
        eprintln!(
            "{CINDEX}: {} files found in {:.2?}",
            snap.files.len(),
            t0.elapsed()
        );
    }
    build_from(&snap, out, opts)
}

/// Build a fresh index at `out` from the files `snap` lists.
pub fn build_from(snap: &Snapshot, out: &Path, opts: &BuildOptions) -> Result<Stats> {
    let t0 = Instant::now();
    let files = &snap.files;
    let mut stats = Stats {
        files_seen: files.len() + snap.too_large,
        files_skipped: snap.too_large,
        ..Default::default()
    };

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
        while end < files.len() && (end == start || bytes + files[end].len <= opts.batch_bytes) {
            bytes += files[end].len;
            end += 1;
        }
        let batch = &files[start..end];

        let analyzed: Vec<Option<(String, Vec<u32>, u64)>> = batch
            .par_iter()
            .map(|file| {
                let path = &file.path;
                let data = match fs::read(path) {
                    Ok(d) => d,
                    Err(err) => {
                        // Always reported: unlike a binary file, an unreadable
                        // one is rare and something the user can act on.
                        eprintln!("{CINDEX}: {}: {err}", path.display());
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
                            eprintln!("{CINDEX}: {}: {skip}, skipping", path.display());
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
                "{CINDEX}: {}/{} files, {} distinct trigrams, {:.2?}",
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

    let tmp = sidecar(out, "tmp");
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
        }
    }
    let off = match write_index_file(&tmp, &snap.roots, &names, &postings) {
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
            "{CINDEX}: wrote {} ({} bytes, {} files, {} trigrams) in {:.2?}",
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
    roots: &[Root],
    names: &[String],
    postings: &[Posting],
) -> Result<u64> {
    let f = File::create(tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, f);
    let mut off: u64 = 0;

    w.write_all(MAGIC)?;
    off += MAGIC.len() as u64;

    let paths_off = off;
    for r in roots {
        let source = r.source.tag();
        w.write_all(r.path.as_bytes())?;
        w.write_all(&[0])?;
        w.write_all(source.as_bytes())?;
        w.write_all(&[0])?;
        off += (r.path.len() + 1 + source.len() + 1) as u64;
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
        let old = sidecar(out, "old");
        let _ = fs::remove_file(&old);
        let had_old = out.is_file();
        if had_old {
            // Fails only for a reader that opened the file without
            // FILE_SHARE_DELETE (not csearch-rs, which shares it); nothing in
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

    fn root(path: &str, source: Source) -> Root {
        Root {
            path: path.into(),
            source,
        }
    }

    /// The planned roots as (final path component, source).
    fn summary(plan: &RootPlan) -> Vec<(String, Source)> {
        plan.roots
            .iter()
            .map(|r| {
                let name = Path::new(&r.path).file_name().unwrap();
                (name.to_string_lossy().into_owned(), r.source)
            })
            .collect()
    }

    #[test]
    fn plan_tolerates_vanished_roots_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep");
        let add = dir.path().join("add");
        let gone = dir.path().join("gone");
        fs::create_dir(&keep).unwrap();
        fs::create_dir(&add).unwrap();
        let stored = vec![
            root(&canonical_string(&keep).unwrap(), Source::Walk),
            // was indexed, then deleted
            root(&strip_verbatim(&gone.to_string_lossy()), Source::Walk),
        ];

        let plan = plan_roots(&Request {
            stored: &stored,
            add: std::slice::from_ref(&add),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            summary(&plan),
            [("add".into(), Source::Walk), ("keep".into(), Source::Walk)]
        );
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("gone") && n.contains("no longer exists")),
            "{:?}",
            plan.notes
        );

        let plan = plan_roots(&Request {
            stored: &stored,
            remove: std::slice::from_ref(&keep),
            ..Default::default()
        })
        .unwrap();
        assert!(plan.roots.is_empty(), "{:?}", plan.roots);
        assert!(
            plan.notes.iter().any(|n| n.ends_with(": removed")),
            "{:?}",
            plan.notes
        );

        // Removing something never indexed is noted, not fatal.
        let plan = plan_roots(&Request {
            stored: &stored,
            remove: std::slice::from_ref(&add),
            ..Default::default()
        })
        .unwrap();
        assert!(
            plan.notes.iter().any(|n| n.contains("not in the index")),
            "{:?}",
            plan.notes
        );

        // Paths named on the command line must exist.
        assert!(plan_roots(&Request {
            stored: &stored,
            add: std::slice::from_ref(&gone),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn a_root_keeps_its_source_until_a_flag_says_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let d = dir.path().join(name);
            fs::create_dir(&d).unwrap();
            d
        };
        let (a, b, new) = (mk("a"), mk("b"), mk("new"));
        let stored = vec![
            root(&canonical_string(&a).unwrap(), Source::Git),
            root(&canonical_string(&b).unwrap(), Source::Walk),
        ];
        let plan = |add: &[PathBuf], listing: Option<Source>| {
            summary(
                &plan_roots(&Request {
                    stored: &stored,
                    add,
                    listing,
                    ..Default::default()
                })
                .unwrap(),
            )
        };
        let (git, walk) = (Source::Git, Source::Walk);
        let names = |a, b| vec![("a".to_string(), a), ("b".to_string(), b)];

        // A plain re-index changes nothing.
        assert_eq!(plan(&[], None), names(git, walk));
        // A flag on its own applies to every root.
        assert_eq!(plan(&[], Some(walk)), names(walk, walk));
        assert_eq!(plan(&[], Some(git)), names(git, git));
        // A flag with a path applies to that path only.
        assert_eq!(plan(std::slice::from_ref(&b), Some(git)), names(git, git));
        assert_eq!(
            plan(std::slice::from_ref(&a), Some(walk)),
            names(walk, walk)
        );
        // Naming a root without a flag leaves its source alone.
        assert_eq!(plan(std::slice::from_ref(&a), None), names(git, walk));
        // A new root is walked unless told otherwise, and the others stay put.
        let mut with_new = names(git, walk);
        with_new.push(("new".into(), walk));
        assert_eq!(plan(std::slice::from_ref(&new), None), with_new);
        with_new[2].1 = git;
        assert_eq!(plan(std::slice::from_ref(&new), Some(git)), with_new);
    }

    #[test]
    fn local_lists_a_repository_through_git_and_anything_else_by_walking() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let plain = dir.path().join("plain");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&plain).unwrap();
        let local = |root: &Path, stored: &[Root], listing| {
            summary(
                &plan_roots(&Request {
                    stored,
                    local: Some(root),
                    listing,
                    ..Default::default()
                })
                .unwrap(),
            )
        };
        assert_eq!(local(&repo, &[], None), [("repo".into(), Source::Git)]);
        assert_eq!(local(&plain, &[], None), [("plain".into(), Source::Walk)]);
        // --walk overrides the default for a repository...
        assert_eq!(
            local(&repo, &[], Some(Source::Walk)),
            [("repo".into(), Source::Walk)]
        );
        // ...and from then on --local keeps what is stored.
        let walked = [root(&canonical_string(&repo).unwrap(), Source::Walk)];
        assert_eq!(local(&repo, &walked, None), [("repo".into(), Source::Walk)]);
    }

    #[test]
    fn rebuild_while_index_is_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "first\n").unwrap();
        let out = dir.path().join("index");
        build_index(std::slice::from_ref(&root), &out, &BuildOptions::default()).unwrap();

        // A running csearch-rs holds the index mapped; a rebuild must still
        // succeed, and the old mapping must stay readable.
        let held = crate::read::Index::open(&out).unwrap();
        fs::write(root.join("b.txt"), "second\n").unwrap();
        build_index(std::slice::from_ref(&root), &out, &BuildOptions::default()).unwrap();

        assert_eq!(held.num_files(), 1);
        assert!(held.name(0).ends_with("a.txt"));
        let fresh = crate::read::Index::open(&out).unwrap();
        assert_eq!(fresh.num_files(), 2);
        assert!(!sidecar(&out, "tmp").exists(), "tmp file left behind");
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
        assert!(!sidecar(&out, "tmp").exists(), "tmp file left behind");
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
            root("/a/b/c", Source::Git),
            root("/a/b", Source::Walk),
            root("/a/bc", Source::Walk),
            root("/a/b", Source::Walk),
            root("/x", Source::Git),
        ]);
        // The nested root goes, and with it its own source: the files under
        // it are listed the way the root that covers them is.
        assert_eq!(
            kept,
            [
                root("/a/b", Source::Walk),
                root("/a/bc", Source::Walk),
                root("/x", Source::Git)
            ]
        );
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
