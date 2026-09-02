//! Trigram extraction.
//!
//! A trigram is three consecutive bytes packed big-endian into a `u32`
//! (`b0<<16 | b1<<8 | b2`), exactly as in the Go original. For each file we
//! need the *set* of distinct trigrams it contains.
//!
//! Hot loop is `pack_trigrams`, which turns a byte slice into `len-2` packed
//! values. On x86-64 with AVX2 it processes 8 trigrams per iteration using
//! `vpmovzxbd` + shift + or; elsewhere (or on old CPUs) it falls back to a
//! scalar loop the compiler can still auto-vectorise. Dedup uses either a
//! sort (small files) or a 16M-bit thread-local bitmap (large files).

use std::cell::RefCell;

/// Limits copied from the Go implementation.
pub const MAX_FILE_LEN: u64 = 1 << 30;
pub const MAX_LINE_LEN: usize = 2000;
pub const MAX_TEXT_TRIGRAMS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Contains a NUL byte.
    Binary,
    /// Not valid UTF-8.
    InvalidUtf8,
    /// A line is longer than `MAX_LINE_LEN`.
    LongLine,
    /// More than `MAX_TEXT_TRIGRAMS` distinct trigrams.
    TooManyTrigrams,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Skip::Binary => "binary file (NUL byte)",
            Skip::InvalidUtf8 => "invalid UTF-8",
            Skip::LongLine => "very long lines",
            Skip::TooManyTrigrams => "too many trigrams",
        })
    }
}

#[inline]
pub fn pack(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])
}

/// Render a packed trigram for display (lossy for non-UTF-8 byte splits).
pub fn to_string(t: u32) -> String {
    let bytes = [(t >> 16) as u8, (t >> 8) as u8, t as u8];
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Fill `out[i] = pack(&data[i..i+3])` for every i. `out.len()` must equal
/// `data.len() - 2`.
pub fn pack_trigrams(data: &[u8], out: &mut [u32]) {
    assert_eq!(out.len() + 2, data.len());
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: feature detected at runtime; lengths validated above.
            unsafe { pack_trigrams_avx2(data, out) };
            return;
        }
    }
    pack_trigrams_scalar(data, out);
}

pub fn pack_trigrams_scalar(data: &[u8], out: &mut [u32]) {
    for (i, o) in out.iter_mut().enumerate() {
        *o = pack(&data[i..i + 3]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn pack_trigrams_avx2(data: &[u8], out: &mut [u32]) {
    use std::arch::x86_64::*;
    let n = out.len();
    let src = data.as_ptr();
    let dst = out.as_mut_ptr();
    let mut i = 0usize;
    // Each iteration reads bytes [i, i+10) and writes out[i..i+8].
    while i + 10 <= data.len() {
        let a = _mm256_cvtepu8_epi32(_mm_loadl_epi64(src.add(i) as *const __m128i));
        let b = _mm256_cvtepu8_epi32(_mm_loadl_epi64(src.add(i + 1) as *const __m128i));
        let c = _mm256_cvtepu8_epi32(_mm_loadl_epi64(src.add(i + 2) as *const __m128i));
        let v = _mm256_or_si256(
            _mm256_or_si256(_mm256_slli_epi32(a, 16), _mm256_slli_epi32(b, 8)),
            c,
        );
        _mm256_storeu_si256(dst.add(i) as *mut __m256i, v);
        i += 8;
    }
    while i < n {
        *dst.add(i) = pack(&data[i..i + 3]);
        i += 1;
    }
}

thread_local! {
    // 2^24 bits = 2 MiB, one per worker thread, reused across files. Only
    // the bits a file set are cleared afterwards, so small files stay cheap.
    static BITMAP: RefCell<Vec<u64>> = RefCell::new(Vec::new());
}

const CHUNK: usize = 2048;

/// Validate a file the way cindex does and return its sorted set of distinct
/// trigrams. Files shorter than 3 bytes yield an empty set (they are still
/// listed in the index).
///
/// Trigrams are packed with SIMD in `CHUNK`-sized pieces and deduplicated
/// through the thread-local bitmap as they are produced, so the only sort is
/// over the (at most `MAX_TEXT_TRIGRAMS`) distinct values.
pub fn analyze(data: &[u8]) -> Result<Vec<u32>, Skip> {
    if memchr::memchr(0, data).is_some() {
        return Err(Skip::Binary);
    }
    if std::str::from_utf8(data).is_err() {
        return Err(Skip::InvalidUtf8);
    }
    // Longest line check (SIMD newline scan via memchr).
    let mut last = 0usize;
    for nl in memchr::memchr_iter(b'\n', data) {
        if nl - last > MAX_LINE_LEN {
            return Err(Skip::LongLine);
        }
        last = nl + 1;
    }
    if data.len() - last > MAX_LINE_LEN {
        return Err(Skip::LongLine);
    }
    if data.len() < 3 {
        return Ok(Vec::new());
    }

    BITMAP.with(|cell| {
        let mut bm = cell.borrow_mut();
        if bm.is_empty() {
            bm.resize(1 << 18, 0);
        }
        let n = data.len() - 2;
        let mut out: Vec<u32> = Vec::with_capacity(n.min(4096));
        let mut buf = [0u32; CHUNK];
        let mut overflow = false;
        let mut i = 0usize;
        while i < n && !overflow {
            let m = (n - i).min(CHUNK);
            pack_trigrams(&data[i..i + m + 2], &mut buf[..m]);
            for &t in &buf[..m] {
                let w = &mut bm[(t >> 6) as usize];
                let bit = 1u64 << (t & 63);
                if *w & bit == 0 {
                    *w |= bit;
                    out.push(t);
                    if out.len() > MAX_TEXT_TRIGRAMS {
                        overflow = true;
                        break;
                    }
                }
            }
            i += m;
        }
        for &t in &out {
            bm[(t >> 6) as usize] &= !(1u64 << (t & 63));
        }
        if overflow {
            return Err(Skip::TooManyTrigrams);
        }
        out.sort_unstable();
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_matches_scalar() {
        let mut data = Vec::new();
        for i in 0..1000u32 {
            data.push((i.wrapping_mul(2654435761) >> 13) as u8);
        }
        for len in [3usize, 4, 9, 10, 11, 17, 64, 65, 999, 1000] {
            let d = &data[..len];
            let mut a = vec![0u32; len - 2];
            let mut b = vec![0u32; len - 2];
            pack_trigrams(d, &mut a);
            pack_trigrams_scalar(d, &mut b);
            assert_eq!(a, b, "len {len}");
        }
    }

    #[test]
    fn analyze_basic() {
        let t = analyze(b"hello").unwrap();
        let s: Vec<String> = t.iter().map(|&x| to_string(x)).collect();
        assert_eq!(s, vec!["ell", "hel", "llo"]);
        assert_eq!(analyze(b"hi").unwrap(), Vec::<u32>::new());
        assert_eq!(analyze(b"a\0b"), Err(Skip::Binary));
        assert_eq!(analyze(&[0xff, 0xfe, 0x41]), Err(Skip::InvalidUtf8));
        let long = vec![b'x'; MAX_LINE_LEN + 1];
        assert_eq!(analyze(&long), Err(Skip::LongLine));
    }

    #[test]
    fn analyze_matches_naive_set() {
        use std::collections::BTreeSet;
        let mut x = 12345u64;
        let mut data = Vec::new();
        while data.len() < 300_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // ASCII letters plus newlines: many repeats, some distinct.
            let b = b"abcdefghij\n"[(x % 11) as usize];
            data.push(b);
        }
        let naive: Vec<u32> = data
            .windows(3)
            .map(pack)
            .collect::<BTreeSet<u32>>()
            .into_iter()
            .collect();
        assert_eq!(analyze(&data).unwrap(), naive);
        // Bitmap must be fully cleared between calls.
        assert_eq!(analyze(b"xyz").unwrap(), vec![pack(b"xyz")]);
        // Overflow path also clears the bitmap.
        let mut wide = Vec::new();
        for a in 0u8..26 {
            for b in 0u8..26 {
                for c in 0u8..40 {
                    wide.extend_from_slice(&[b'A' + a, b'a' + b, b'0' + c, b'\n']);
                }
            }
        }
        // ensure > 20000 distinct trigrams
        let distinct = wide.windows(3).map(pack).collect::<BTreeSet<u32>>().len();
        assert!(distinct > MAX_TEXT_TRIGRAMS, "{distinct}");
        assert_eq!(analyze(&wide), Err(Skip::TooManyTrigrams));
        assert_eq!(analyze(b"xyz").unwrap(), vec![pack(b"xyz")]);
    }
}
