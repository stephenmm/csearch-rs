//! A damaged index must be reported, never panic. The release profile aborts
//! on panic, which on Windows means a crash dialog instead of an error.

use csearch::read::Index;
use csearch::write::{build_index, BuildOptions, MAGIC, TRAILER_LEN};
use std::fs;
use tempfile::TempDir;

fn fresh() -> (TempDir, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    for i in 0..5 {
        fs::write(root.join(format!("f{i}.txt")), format!("hello world {i}\n")).unwrap();
    }
    let out = dir.path().join("index");
    build_index(&[root], &out, &BuildOptions::default()).unwrap();
    let bytes = fs::read(&out).unwrap();
    (dir, bytes)
}

/// Write `bytes` as an index file and return the error opening it produces.
fn open_err(dir: &TempDir, name: &str, bytes: &[u8]) -> String {
    let p = dir.path().join(name);
    fs::write(&p, bytes).unwrap();
    match Index::open(&p) {
        Ok(_) => panic!("{name}: damaged index opened successfully"),
        Err(e) => format!("{e:#}"),
    }
}

fn rd_u64(b: &[u8], at: usize) -> usize {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap()) as usize
}

#[test]
fn intact_index_opens() {
    let (dir, bytes) = fresh();
    let p = dir.path().join("ok");
    fs::write(&p, &bytes).unwrap();
    assert_eq!(Index::open(&p).unwrap().num_files(), 5);
}

#[test]
fn truncation_is_reported() {
    let (dir, bytes) = fresh();
    let n = bytes.len();
    for cut in [0, 5, MAGIC.len(), MAGIC.len() + 10, n / 2, n - TRAILER_LEN - 1, n - 8, n - 1] {
        let err = open_err(&dir, &format!("cut{cut}"), &bytes[..cut]);
        assert!(
            err.contains("cindex --reset") || err.contains("not a csearch-rs index"),
            "cut at {cut}: {err}"
        );
    }
}

#[test]
fn every_trailer_offset_is_validated() {
    let (dir, bytes) = fresh();
    let t = bytes.len() - TRAILER_LEN;
    for field in 0..5 {
        for bad in [bytes.len() as u64 + 1000, u64::MAX / 2, 0] {
            let mut b = bytes.clone();
            b[t + field * 8..t + field * 8 + 8].copy_from_slice(&bad.to_le_bytes());
            let err = open_err(&dir, &format!("off{field}_{bad}"), &b);
            assert!(err.contains("cindex --reset"), "offset {field} = {bad}: {err}");
        }
    }
}

#[test]
fn counts_are_validated() {
    let (dir, bytes) = fresh();
    let t = bytes.len() - TRAILER_LEN;
    for (at, label) in [(t + 40, "nfiles"), (t + 44, "ntrigrams")] {
        let mut b = bytes.clone();
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = open_err(&dir, label, &b);
        assert!(err.contains("cindex --reset"), "{label}: {err}");
    }
}

#[test]
fn index_entries_are_validated() {
    let (dir, bytes) = fresh();
    let t = bytes.len() - TRAILER_LEN;
    let nameidx_off = rd_u64(&bytes, t + 16);
    let postidx_off = rd_u64(&bytes, t + 32);

    let mut b = bytes.clone();
    b[nameidx_off..nameidx_off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let err = open_err(&dir, "nameidx", &b);
    assert!(err.contains("cindex --reset"), "{err}");

    let mut b = bytes.clone();
    b[postidx_off + 8..postidx_off + 16].copy_from_slice(&(u64::MAX / 4).to_le_bytes());
    let err = open_err(&dir, "postidx", &b);
    assert!(err.contains("cindex --reset"), "{err}");

    // Unsorted trigrams would make the binary search meaningless.
    let mut b = bytes.clone();
    b[postidx_off..postidx_off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let err = open_err(&dir, "unsorted", &b);
    assert!(err.contains("cindex --reset"), "{err}");
}

#[test]
fn other_format_version_is_explained() {
    let (dir, bytes) = fresh();
    let mut b = bytes.clone();
    b[MAGIC.len() - 2] = b'9'; // "csearch-rs index 1\n" -> "... index 9\n"
    let err = open_err(&dir, "version", &b);
    assert!(err.contains("format version"), "{err}");

    let err = open_err(&dir, "garbage", b"hello there");
    assert!(err.contains("not a csearch-rs index"), "{err}");
}
