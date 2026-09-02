//! Memory-mapped index reader and boolean query evaluation.
//!
//! `Index::open` validates every section boundary and every index entry up
//! front, so a truncated or damaged file is reported as such rather than
//! panicking part-way through a query (the release profile aborts on panic,
//! which on Windows is a crash dialog, not an error message).

use crate::query::{Op, Query};
use crate::varint;
use crate::write::{MAGIC, POST_ENTRY_LEN, TRAILER_LEN, TRAILER_MAGIC};
use anyhow::{anyhow, bail, Context, Result};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

/// Every format version starts with this; only the current one matches
/// `MAGIC` in full.
const MAGIC_FAMILY: &[u8] = b"csearch-rs index ";

pub struct Index {
    map: Mmap,
    paths_off: usize,
    names_off: usize,
    nameidx_off: usize,
    posts_off: usize,
    postidx_off: usize,
    nfiles: u32,
    ntrigrams: u32,
}

fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

impl Index {
    pub fn open(path: &Path) -> Result<Index> {
        let f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("no index at {} -- run `cindex <dir>` to create one", path.display())
            }
            Err(e) => return Err(e).with_context(|| format!("opening index {}", path.display())),
        };
        // SAFETY: the index file is treated as read-only; rebuilds install a
        // new file by rename, so an existing mapping stays consistent.
        let map = unsafe { Mmap::map(&f)? };
        let corrupt = |what: &str| {
            anyhow!(
                "{}: {what} -- the index is corrupt; run `cindex --reset` and re-index",
                path.display()
            )
        };

        if !map.starts_with(MAGIC) {
            if map.starts_with(MAGIC_FAMILY) {
                bail!(
                    "{}: written by a different index format version -- run `cindex --reset` and re-index",
                    path.display()
                );
            }
            bail!("{}: not a csearch-rs index", path.display());
        }
        if map.len() < MAGIC.len() + TRAILER_LEN {
            return Err(corrupt("file is truncated"));
        }
        let t = map.len() - TRAILER_LEN;
        if &map[map.len() - 8..] != TRAILER_MAGIC {
            return Err(corrupt("bad trailer"));
        }
        let paths_off = rd_u64(&map, t) as usize;
        let names_off = rd_u64(&map, t + 8) as usize;
        let nameidx_off = rd_u64(&map, t + 16) as usize;
        let posts_off = rd_u64(&map, t + 24) as usize;
        let postidx_off = rd_u64(&map, t + 32) as usize;
        let nfiles = rd_u32(&map, t + 40);
        let ntrigrams = rd_u32(&map, t + 44);

        // Sections must be in order and inside the file, and their sizes must
        // agree with the counts. Every later read is bounded by these checks.
        let ordered = paths_off == MAGIC.len()
            && paths_off <= names_off
            && names_off <= nameidx_off
            && nameidx_off <= posts_off
            && posts_off <= postidx_off
            && postidx_off <= t;
        if !ordered {
            return Err(corrupt("section offsets out of order"));
        }
        let want_posts = (nfiles as usize).checked_mul(4).and_then(|n| n.checked_add(nameidx_off));
        let want_trailer =
            (ntrigrams as usize).checked_mul(POST_ENTRY_LEN).and_then(|n| n.checked_add(postidx_off));
        if want_posts != Some(posts_off) || want_trailer != Some(t) {
            return Err(corrupt("section sizes disagree with the file counts"));
        }

        // Every name-index entry must point inside the names section.
        let names_len = nameidx_off - names_off;
        for i in 0..nfiles as usize {
            if rd_u32(&map, nameidx_off + i * 4) as usize >= names_len {
                return Err(corrupt("name index entry out of range"));
            }
        }
        // Every posting-index entry must point inside the postings, with room
        // for its count (each posting is at least one byte), and the trigrams
        // must be sorted or the binary search is meaningless.
        let posts_len = postidx_off - posts_off;
        let mut prev: Option<u32> = None;
        for i in 0..ntrigrams as usize {
            let at = postidx_off + i * POST_ENTRY_LEN;
            let trigram = rd_u32(&map, at);
            let count = rd_u32(&map, at + 4) as usize;
            let off = rd_u64(&map, at + 8) as usize;
            match off.checked_add(count) {
                Some(end) if end <= posts_len => {}
                _ => return Err(corrupt("posting index entry out of range")),
            }
            if prev.is_some_and(|p| p >= trigram) {
                return Err(corrupt("posting index is not sorted"));
            }
            prev = Some(trigram);
        }

        Ok(Index { map, paths_off, names_off, nameidx_off, posts_off, postidx_off, nfiles, ntrigrams })
    }

    /// Root directories that were indexed.
    pub fn roots(&self) -> Vec<String> {
        let section = &self.map[self.paths_off..self.names_off];
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < section.len() {
            let Some(len) = memchr::memchr(0, &section[pos..]) else { break };
            if len == 0 {
                break;
            }
            out.push(String::from_utf8_lossy(&section[pos..pos + len]).into_owned());
            pos += len + 1;
        }
        out
    }

    pub fn num_files(&self) -> u32 {
        self.nfiles
    }
    pub fn num_trigrams(&self) -> u32 {
        self.ntrigrams
    }

    /// File name for an id; empty for an id the index does not have.
    pub fn name(&self, id: u32) -> &str {
        if id >= self.nfiles {
            return "";
        }
        let names = &self.map[self.names_off..self.nameidx_off];
        let start = rd_u32(&self.map, self.nameidx_off + id as usize * 4) as usize;
        let end = start + memchr::memchr(0, &names[start..]).unwrap_or(names.len() - start);
        std::str::from_utf8(&names[start..end]).unwrap_or("<invalid utf-8 name>")
    }

    /// (count, byte offset into postings) for a trigram, via binary search.
    fn find_post(&self, t: u32) -> Option<(u32, usize)> {
        let (mut lo, mut hi) = (0usize, self.ntrigrams as usize);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let at = self.postidx_off + mid * POST_ENTRY_LEN;
            let tt = rd_u32(&self.map, at);
            if tt < t {
                lo = mid + 1;
            } else if tt > t {
                hi = mid;
            } else {
                return Some((rd_u32(&self.map, at + 4), rd_u64(&self.map, at + 8) as usize));
            }
        }
        None
    }

    pub fn posting_count(&self, t: u32) -> u32 {
        self.find_post(t).map_or(0, |(c, _)| c)
    }

    /// Sorted file ids containing trigram `t`.
    pub fn posting_list(&self, t: u32) -> Vec<u32> {
        let Some((count, off)) = self.find_post(t) else {
            return Vec::new();
        };
        let postings = &self.map[..self.postidx_off];
        let mut out = Vec::with_capacity(count as usize);
        let mut pos = self.posts_off + off;
        let mut id = 0u32;
        for i in 0..count {
            let Some(v) = varint::get(postings, &mut pos) else { break };
            id = if i == 0 { v } else { id.wrapping_add(v) };
            if id >= self.nfiles {
                break; // damaged data: stop rather than name a file that does not exist
            }
            out.push(id);
        }
        out
    }

    /// Evaluate a query, returning the sorted set of candidate file ids.
    pub fn posting_query(&self, q: &Query) -> Vec<u32> {
        self.eval(q, None)
    }

    fn eval(&self, q: &Query, restrict: Option<&[u32]>) -> Vec<u32> {
        match q.op {
            Op::None => Vec::new(),
            Op::All => match restrict {
                Some(r) => r.to_vec(),
                None => (0..self.nfiles).collect(),
            },
            Op::And => {
                let mut list: Option<Vec<u32>> = restrict.map(<[u32]>::to_vec);
                // Rarest trigrams first so the candidate set shrinks fastest.
                let mut tris: Vec<(u32, u32)> =
                    q.trigrams.iter().map(|&t| (self.posting_count(t), t)).collect();
                tris.sort_unstable();
                for (_, t) in tris {
                    let p = self.posting_list(t);
                    list = Some(match list {
                        None => p,
                        Some(l) => intersect(&l, &p),
                    });
                    if list.as_ref().is_some_and(Vec::is_empty) {
                        return Vec::new();
                    }
                }
                for sub in &q.subs {
                    let r = self.eval(sub, list.as_deref());
                    if r.is_empty() {
                        return r;
                    }
                    list = Some(r);
                }
                list.unwrap_or_else(|| (0..self.nfiles).collect())
            }
            Op::Or => {
                let mut list: Vec<u32> = Vec::new();
                for &t in &q.trigrams {
                    let mut p = self.posting_list(t);
                    if let Some(r) = restrict {
                        p = intersect(r, &p);
                    }
                    list = union(&list, &p);
                }
                for sub in &q.subs {
                    list = union(&list, &self.eval(sub, restrict));
                }
                list
            }
        }
    }
}

pub fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            i += 1;
        } else if a[i] > b[j] {
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

pub fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if a[i] > b[j] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}
