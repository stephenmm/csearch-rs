# csearch-rs

A Rust port of Google Code Search (Russ Cox's `codesearch`: `cindex` + `csearch`),
rebuilt for modern hardware:

| Piece | Go original | csearch-rs |
|---|---|---|
| Trigram extraction | scalar byte loop | AVX2 kernel (8 trigrams/iter, runtime-detected, scalar fallback) |
| Per-file dedup | sparse set | 16M-bit thread-local bitmap, cleared selectively; only the ≤20k distinct values are sorted |
| Binary / long-line / UTF-8 checks | byte loop | `memchr` SIMD scans + `std::str::from_utf8` |
| Indexing | single-threaded, 3-way merge on disk | rayon: parallel file analysis; postings appended via a dense trigram→slot table (no hashing, no global sort); batch size bounds memory |
| Query analysis | `regexp.go` | full port (exact/prefix/suffix sets, MAX_SET=20, common-trigram factoring) on the `regex-syntax` HIR |
| Grep phase | sequential, custom RE2-lite | rayon across candidate files; `regex` crate (Teddy/Aho-Corasick SIMD prefilters) |
| Index format | custom | custom, mmap'd; delta-varint postings, 16-byte binary-searchable posting index |

## Build

Rust 1.75+ (`Cargo.lock` is pinned to versions that build on 1.75; newer stable works too).

```
cargo build --release
```

Binaries land in `target/release/cindex` and `target/release/csearch`.
On Windows, `setup_csearch.py` builds, tests, and copies them to `%USERPROFILE%\bin`.

## Use

```
cindex E:\proj               # index a tree (adds to existing roots, rebuilds in parallel)
cindex                       # re-index the stored roots
cindex --list                # show roots
cindex --reset               # delete the index
csearch -n 'unsafe impl Send'          # regexp search, line numbers
csearch -l -f '\.rs$' 'TODO|FIXME'     # file names only, restricted by filename regexp
csearch -i -c deprecated               # case-insensitive, count per file
csearch --verbose 'foo.*bar'           # prints the trigram query + timings
```

Flags mirror the original (`-c -f -h -i -l -n`); long flags use `--` (`--brute`,
`--verbose`, `--indexpath`, `-j N` threads). The index lives at `$CSEARCHINDEX`
or `~/.csearchindex` (`%USERPROFILE%\.csearchindex`). Exit status is 1 when
nothing matched, like grep.

Skipped files, as in the original: names starting with `.`, `#`, `~` or ending
in `~`; files containing NUL; invalid UTF-8; lines over 2000 bytes; files with
more than 20,000 distinct trigrams; files over 1 GiB.

## Layout

```
src/trigram.rs   AVX2/scalar packing, validation, dedup
src/query.rs     boolean trigram Query (And/Or/All/None) with simplification
src/regexp.rs    regexp -> Query analysis (port of index/regexp.go)
src/write.rs     parallel index builder + on-disk format
src/read.rs      mmap reader, posting lists, query evaluation
src/bin/cindex.rs, src/bin/csearch.rs
```

## Compare against the Go original

```
py compare_csearch.py --corpus E:\proj
```

Finds or installs both implementations (Rust: `cargo build`; Go: `go install
github.com/google/codesearch/cmd/...@latest`, or `winget install GoLang.Go`
first if needed), indexes the corpus with each into temp index files, runs a
pattern set through `csearch -c` on both, and checks the per-file counts are
identical. Prints a timing table (min / median ms, speedup) and writes
`compare_results.md`. Exit code 1 on any mismatch. `--patterns FILE` for your
own list (prefix `-i ` for case-insensitive), `--runs N`, `--go-bin DIR`,
`--no-build`.

Result on a 41 MB / 2,482-file corpus, single core:

```
pattern                              go ms (min/med) rust ms (min/med)  speedup  parity
fn main                               17.7 /    18.1     3.8 /     4.1    4.66x  OK (149 files)
unsafe impl Send                       2.5 /     3.0     1.7 /     1.8    1.46x  OK (12 files)
TODO|FIXME                            12.1 /    13.1     3.1 /     3.2    3.91x  OK (165 files)
foo.*bar                              10.5 /    11.1     3.0 /     3.1    3.47x  OK (91 files)
^use std::                            21.0 /    21.5     4.6 /     4.7    4.59x  OK (295 files)
impl<[A-Z]> .* for                    34.7 /    36.7     5.7 /     6.0    6.05x  OK (212 files)
-i deprecated                         29.1 /    31.0     5.8 /     6.4    5.00x  OK (174 files)

index build (best): go 0.309s / 5.7 MB   rust 0.327s / 5.8 MB
```

Indexing is at parity with Go on one core (the first cut was 2.8× slower —
the harness caught it); it is the only one of the two that uses more cores.

## Verified

`cargo test` covers the AVX2 kernel against the scalar path, the bitmap dedup
against a naive set, varints, the Cox regexp test cases, and an end-to-end index/query
round trip. On a 41 MB / 2,482-file corpus, every `csearch -c` result was
identical to `grep -rEc` and to the Go csearch.

## Notes

- Lives on the E: drive at `E:\proj\csearch-rs` (`/mnt/e/proj/csearch-rs` in WSL).
- Backed up to a private GitHub repo. Push checkpoints with `/backup` (in Claude Code) or `git push`.
