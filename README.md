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

### Measured on this machine (2026-09-01)

Windows 10, Skylake i7 (8 cpus), Go codesearch v1.2.0, rustc 1.98.0. Corpus
`E:\proj` — 6,022 files / 720 MB of mixed Python, C++, CUDA and Markdown
(9.1 MB index). `compare_csearch.py --corpus E:\proj`:

```
pattern                              go ms (min/med) rust ms (min/med)  speedup  parity
fn main                               17.0 /    18.7     9.6 /    11.1    1.77x  OK (24 files)
unsafe impl Send                       9.4 /     9.5     7.2 /     7.7    1.30x  OK (4 files)
TODO|FIXME                            25.0 /    27.8    13.6 /    14.4    1.83x  OK (178 files)
foo.*bar                              26.6 /    27.3    12.8 /    14.2    2.07x  OK (101 files)
hello                                 33.5 /    36.1    15.4 /    17.3    2.17x  OK (72 files)
Hello.*[Ww]orld                       12.9 /    13.7     9.1 /     9.6    1.41x  OK (28 files)
^use std::                            20.5 /    23.3    12.0 /    13.6    1.72x  OK (98 files)
impl<[A-Z]> .* for                     8.7 /    10.1     6.6 /     6.7    1.31x  OK (0 files)
[0-9]{4}-[0-9]{2}-[0-9]{2}            62.2 /    64.8    31.4 /    33.3    1.98x  OK (178 files)
-i license                            68.3 /    72.8    33.8 /    36.9    2.02x  OK (647 files)
-i deprecated                         47.7 /    48.7    19.6 /    23.3    2.43x  OK (225 files)

index build (best): go 1.710s / 9.1 MB   rust 0.924s / 9.3 MB   (1.85x)
```

**Parity: 11/11 patterns, per-file counts identical to the Go original.**

Speed is lower here than the table above: **1.3x-2.4x, not up to 6x**. The
difference is corpus composition, not a regression. The 41 MB corpus in the
first table is Rust-heavy, so its patterns hit large candidate sets (`^use
std::` matched 295 files there, 98 here; `impl<[A-Z]> .* for ` matched 212
there, 0 here) and the grep phase — where the SIMD prefilters and rayon
actually pay — dominates. On this mixed corpus more of each measurement is
process startup and index load, which compresses the ratio; the floor for a
zero-candidate query is ~6.6 ms rust vs ~8.7 ms go. Rust was faster on every
pattern of every run, and indexing is 1.85x faster here because it uses all
8 cores.

## Verified

`cargo test` covers the AVX2 kernel against the scalar path, the bitmap dedup
against a naive set, varints, the Cox regexp test cases, and an end-to-end index/query
round trip. On a 41 MB / 2,482-file corpus, every `csearch -c` result was
identical to `grep -rEc` and to the Go csearch.

## Notes

- Lives on the E: drive at `E:\proj\csearch-rs` (`/mnt/e/proj/csearch-rs` in WSL).
- Backed up to a private GitHub repo. Push checkpoints with `/backup` (in Claude Code) or `git push`.
