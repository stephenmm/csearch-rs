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

use crate::paths::canonical_string;
use crate::trigram::{self, MAX_FILE_LEN};
use crate::varint;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
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
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions { verbose: false, batch_bytes: 256 << 20 }
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

/// Collect regular files under `roots` in sorted order with their sizes.
fn walk(roots: &[String], verbose: bool) -> Vec<(PathBuf, u64)> {
    let mut files = Vec::new();
    for root in roots {
        let it = WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0 || !e.file_name().to_str().map_or(false, skip_name)
            });
        for entry in it {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    if verbose {
                        eprintln!("cindex: {err}");
                    }
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if len > MAX_FILE_LEN {
                if verbose {
                    eprintln!("cindex: {}: too large, skipping", entry.path().display());
                }
                continue;
            }
            files.push((entry.into_path(), len));
        }
    }
    files
}

/// Build a fresh index of `roots` at `out`.
pub fn build_index(roots: &[PathBuf], out: &Path, opts: &BuildOptions) -> Result<Stats> {
    let t0 = Instant::now();
    let mut root_strs: Vec<String> = Vec::new();
    for r in roots {
        root_strs.push(canonical_string(r).with_context(|| format!("resolving {}", r.display()))?);
    }
    root_strs.sort();
    root_strs.dedup();

    let files = walk(&root_strs, opts.verbose);
    let mut stats = Stats { files_seen: files.len(), ..Default::default() };
    if opts.verbose {
        eprintln!("cindex: {} files found in {:.2?}", files.len(), t0.elapsed());
    }

    let mut names: Vec<String> = Vec::with_capacity(files.len());
    // Dense trigram -> posting slot map (64 MiB, touched sparsely) replaces a
    // hash lookup per posting; postings are appended in file-id order so the
    // lists come out sorted without any global sort.
    let mut slot: Vec<u32> = vec![u32::MAX; 1 << 24];
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
                        if opts.verbose {
                            eprintln!("cindex: {}: {err}", path.display());
                        }
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
                        let p = if s == u32::MAX {
                            slot[t as usize] = postings.len() as u32;
                            postings.push(Posting { trigram: t, ..Default::default() });
                            postings.last_mut().unwrap()
                        } else {
                            &mut postings[s as usize]
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
    let f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, f);
    let mut off: u64 = 0;

    w.write_all(MAGIC)?;
    off += MAGIC.len() as u64;

    let paths_off = off;
    for r in &root_strs {
        w.write_all(r.as_bytes())?;
        w.write_all(&[0])?;
        off += r.len() as u64 + 1;
    }
    w.write_all(&[0])?;
    off += 1;

    let names_off = off;
    let mut name_index: Vec<u8> = Vec::with_capacity(names.len() * 4);
    for n in &names {
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
    for p in &postings {
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
    drop(w);

    if out.exists() {
        fs::remove_file(out).with_context(|| format!("removing old {}", out.display()))?;
    }
    fs::rename(&tmp, out).with_context(|| format!("renaming into {}", out.display()))?;
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
