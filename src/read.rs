//! Memory-mapped index reader and boolean query evaluation.

use crate::query::{Op, Query};
use crate::varint;
use crate::write::{MAGIC, POST_ENTRY_LEN, TRAILER_LEN, TRAILER_MAGIC};
use anyhow::{bail, Context, Result};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

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
        // SAFETY: the index file is treated as read-only; concurrent
        // rewrites go through rename, so the mapping stays consistent.
        let map = unsafe { Mmap::map(&f)? };
        if map.len() < MAGIC.len() + TRAILER_LEN || &map[..MAGIC.len()] != MAGIC {
            bail!("{}: not a csearch-rs index", path.display());
        }
        let t = map.len() - TRAILER_LEN;
        if &map[map.len() - 8..] != TRAILER_MAGIC {
            bail!("{}: corrupt index trailer", path.display());
        }
        let idx = Index {
            paths_off: rd_u64(&map, t) as usize,
            names_off: rd_u64(&map, t + 8) as usize,
            nameidx_off: rd_u64(&map, t + 16) as usize,
            posts_off: rd_u64(&map, t + 24) as usize,
            postidx_off: rd_u64(&map, t + 32) as usize,
            nfiles: rd_u32(&map, t + 40),
            ntrigrams: rd_u32(&map, t + 44),
            map,
        };
        if idx.postidx_off + idx.ntrigrams as usize * POST_ENTRY_LEN != t
            || idx.nameidx_off + idx.nfiles as usize * 4 != idx.posts_off
        {
            bail!("{}: corrupt index sections", path.display());
        }
        Ok(idx)
    }

    /// Root directories that were indexed.
    pub fn roots(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut pos = self.paths_off;
        loop {
            let end = pos + memchr::memchr(0, &self.map[pos..]).unwrap_or(0);
            if end == pos {
                break;
            }
            out.push(String::from_utf8_lossy(&self.map[pos..end]).into_owned());
            pos = end + 1;
        }
        out
    }

    pub fn num_files(&self) -> u32 {
        self.nfiles
    }
    pub fn num_trigrams(&self) -> u32 {
        self.ntrigrams
    }

    pub fn name(&self, id: u32) -> &str {
        let start = self.names_off + rd_u32(&self.map, self.nameidx_off + id as usize * 4) as usize;
        let end = start + memchr::memchr(0, &self.map[start..]).unwrap_or(0);
        std::str::from_utf8(&self.map[start..end]).unwrap_or("<invalid utf-8 name>")
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
        let mut out = Vec::with_capacity(count as usize);
        let mut pos = self.posts_off + off;
        let mut id = 0u32;
        for i in 0..count {
            let v = varint::get(&self.map, &mut pos).unwrap_or(0);
            id = if i == 0 { v } else { id + v };
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
                    if list.as_ref().map_or(false, Vec::is_empty) {
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
