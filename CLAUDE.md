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

Measured against the Go original on a mixed 6,022-file corpus: search is
1.3x-2.4x faster and indexing 1.85x faster, with per-file match counts
identical on every pattern tried. See the README for the full table and why a
Rust-heavy corpus scores higher.

## Conventions

- Line endings are normalised to LF via `.gitattributes`, so the repository is
  identical whether it is checked out on Windows or Unix.
- Commit at meaningful milestones, one logical change per commit, with the
  regression test that would have caught the bug in the same commit.
- `rustfmt` defaults and `clippy -D warnings`; CI enforces both on Linux and
  Windows before it runs the tests.

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
tests/cli.rs               drives the real binaries (roots, exit codes, pipes)
tests/corruption.rs        every field of an index damaged in turn
tests/superset.rs          randomised: matches are always candidates

compare_csearch.py         parity + timing harness vs the Go original
build_standalone.py        static binaries for Windows and Linux
setup_csearch.py           build, test, install to the user's bin directory
```

## How to run

```
cargo build --release      # binaries in target/release/{cindex,csearch}
cargo test                 # the full suite
cargo clippy --all-targets -- -D warnings
cargo fmt --check

CSEARCH_PROP_ITERS=40 cargo test --test superset   # property test, harder
py compare_csearch.py --corpus /path/to/code       # parity/timing vs Go
py build_standalone.py                             # static binaries -> dist/
```

The index lives at `$CSEARCHINDEX`, else `~/.csearchindex`
(`%USERPROFILE%\.csearchindex` on Windows). Exit status follows grep: 0
matched, 1 nothing matched, 2 an error.

## Status

Complete and verified. 37 tests; CI builds and tests on Linux and Windows,
gating on rustfmt and clippy before the suite.

**Correctness.** Per-file match counts are identical to the Go original on
11/11 patterns across two corpora, and to `grep -Ec` on every pattern tried.
`tests/superset.rs` checks the guarantee the design rests on -- every file a
regexp matches is among the candidates the index returns -- over randomised
corpora and patterns; 8,000 checks have produced no false negatives. The
static Linux and Windows binaries return byte-identical results on the same
tree.

**Robustness.** A damaged or truncated index is reported, never a panic: every
section offset, name-index entry and posting-index entry is validated on open,
and an index from another format version says so. Index replacement is atomic,
so a `csearch` running mid-rebuild keeps reading the old file and a failed
build leaves nothing behind; on Windows a mapped file cannot be deleted, so the
old index is parked aside and removed afterwards. Roots are collapsed by
containment, and a stored root that has vanished is dropped with a note instead
of wedging every future run.

**Behaviour.** Output matches grep where the two overlap: one match counted per
line, CRLF-aware anchors, no phantom line after a trailing newline, quiet exit
on a closed pipe. Results stream in ordered batches of 64 rather than being
buffered whole, which bounds memory and lets the first lines appear before the
search finishes. Read and permission errors are reported without `--verbose`;
`csearch` says once if indexed files have since been deleted.

**Distribution.** BSD-3-Clause, matching upstream, with the derivation recorded
in NOTICE. `build_standalone.py` produces a static-CRT Windows binary (no VC++
redistributable) and a static musl Linux binary (no glibc floor) from this
machine; it cannot produce macOS binaries, since that needs Apple's own
toolchain. CI additionally builds native macOS binaries for Apple Silicon and
Intel (they link only system libraries -- there is no static-linking
equivalent on macOS, and none is needed). A `v*` tag publishes all four to a
GitHub release. A PR cannot merge unless `Cargo.toml`'s version increased
(the `version-check` job); this only gates the merge button, not a direct
`git push` to main, since GitHub has no server-side hook for that outside
Enterprise Server.

## Open questions / TODO

- Indexing is a full rebuild of every stored root; there is no incremental
  merge, so adding one directory to a large index re-walks everything. This is
  a deliberate trade (it is what makes the parallel, sort-free build possible),
  not an oversight.
- Postings are held in memory until the index is written. `--batch-mib` bounds
  the file buffers, not the postings, so peak memory scales with the corpus's
  distinct-trigram count. Fine at the sizes tested; a ceiling at very large
  scale.
- On Windows, a reader that opens the index without `FILE_SHARE_DELETE`
  (Python's `open()`, some older tools -- not `csearch`, which shares it) blocks
  a rebuild. Nothing in user space can move a file held that way; the failure is
  clean and the message says to close the other program.
- 32-bit is untried and would regress: the AVX2 kernel is gated on
  `#[cfg(target_arch = "x86_64")]`, so an `i686-*` build silently falls back to
  the scalar path and loses the headline performance win. Widen the gate first
  if a 32-bit target is ever wanted.
