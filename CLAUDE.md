# Project: csearch-rs

Rust port of Google Code Search (cindex/csearch) with SIMD trigram extraction
and parallel indexing/search.

## What this project is

A from-scratch Rust reimplementation of Russ Cox's `codesearch` (the Go
`cindex` + `csearch` pair). `cindex` walks directory trees, extracts the set of
distinct 3-byte trigrams from every text file, and writes a compact posting-list
index; `csearch` compiles a regexp into a boolean trigram query, evaluates it
against the index to get a small candidate set, then greps only those files.

The point of the rewrite is modern hardware: an AVX2 trigram kernel (8 trigrams
per iteration, runtime-detected with a scalar fallback), a 16M-bit thread-local
bitmap for per-file dedup instead of a sparse set, `memchr` SIMD scans for the
binary/long-line checks, rayon across files for both indexing and grep, and the
`regex` crate's Teddy/Aho-Corasick prefilters for the match phase. The regexp
-> trigram query analysis is a full port of Cox's `index/regexp.go` onto the
`regex-syntax` HIR.

Measured on a 41 MB / 2,482-file corpus: search is 1.5x-6x faster than the Go
original, indexing is at parity on one core (and is the only one of the two
that scales across cores).

## Conventions

- This project lives on the **E: drive**: `E:\proj\csearch-rs` (Windows) /
  `/mnt/e/proj/csearch-rs` (WSL). Keep all files here.
- Backed by a **private GitHub repo**. Commit at meaningful milestones; push to back up.
- Line endings are normalized to LF via `.gitattributes` so Windows and WSL git stay in sync.

## Key files / layout

```
src/trigram.rs   AVX2/scalar trigram packing, file validation, bitmap dedup
src/query.rs     boolean trigram Query (And/Or/All/None) + simplification
src/regexp.rs    regexp -> Query analysis (port of Cox's index/regexp.go)
src/write.rs     parallel index builder + on-disk format
src/read.rs      mmap reader, delta-varint posting lists, query evaluation
src/varint.rs    varint encode/decode
src/paths.rs     index path resolution ($CSEARCHINDEX or ~/.csearchindex)
src/bin/cindex.rs, src/bin/csearch.rs
tests/index_roundtrip.rs   end-to-end index + query round trip
compare_csearch.py         parity + timing harness vs the Go original
setup_csearch.py           build, test, install to %USERPROFILE%\bin
```

## How to run

```
cargo build --release      # binaries in target/release/{cindex,csearch}
cargo test                 # AVX2-vs-scalar, dedup, varints, Cox regexp cases, round trip
py setup_csearch.py        # build + test + install to %USERPROFILE%\bin
py compare_csearch.py --corpus E:\proj   # parity/timing vs Go csearch
```

Index lives at `$CSEARCHINDEX` or `%USERPROFILE%\.csearchindex`. Exit status 1
when nothing matched, like grep.

## Open questions / TODO

- **No Rust toolchain on this machine yet** — `cargo`/`rustc`/`rustup` are not
  installed and `~/.cargo` does not exist, so the code in this repo has never
  been compiled here. Nothing below the "imported" line has been verified on
  this box. Install rustup first (note: C: is space-constrained, so consider
  `CARGO_HOME`/`RUSTUP_HOME` on E:). MSVC Build Tools are already present at
  `E:\VS\BuildTools`, so the `x86_64-pc-windows-msvc` host should link.
- Go is not installed either, so `compare_csearch.py` cannot run its Go side
  until `winget install GoLang.Go` (the harness offers to do this).
- The benchmark table in README.md was produced elsewhere; re-run
  `compare_csearch.py` on this machine to get local numbers.
