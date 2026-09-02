# csearch-rs

[![build](https://github.com/stephenmm/csearch-rs/actions/workflows/build.yml/badge.svg)](https://github.com/stephenmm/csearch-rs/actions/workflows/build.yml)

**Regular-expression search across a large codebase, fast.** Index the tree
once; after that every search consults a trigram index to work out the handful
of files that *could* contain a match, and greps only those.

`grep -r` and `ripgrep` read every file every time. csearch-rs reads a few.

```console
$ cindex /usr/include
cindex: 4905 files indexed (5 skipped), 148595 trigrams, 7390023 posting entries, index 10517262 bytes

$ csearch -n 'pthread_mutex_t'
/usr/include/pthread.h:781:extern int pthread_mutex_init (pthread_mutex_t *__mutex,
/usr/include/pthread.h:786:extern int pthread_mutex_destroy (pthread_mutex_t *__mutex)
/usr/include/pthread.h:790:extern int pthread_mutex_trylock (pthread_mutex_t *__mutex)
...
```

`--verbose` shows why that is quick — 63 MB of headers, and the regexp only
had to be run against twelve files:

```console
$ csearch --verbose 'pthread_mutex_t'
query: _mu ad_ d_m ead ex_ hre mut pth rea tex thr ute x_t
candidates: 12 of 4905 files (2.28ms)
58 matches in 7 files (5.85ms)
```

The index is built once (1.5 s for those 4,905 files) and reused by every
search until the files change.

## Install

**Prebuilt binaries** — [latest release](https://github.com/stephenmm/csearch-rs/releases/latest).
Both are fully static: the Linux build is musl-linked and runs on any x86-64
distribution, the Windows build needs no Visual C++ redistributable. Unpack
them anywhere on your `PATH`.

**From source** — Rust 1.75 or newer:

```
cargo build --release      # target/release/{cindex,csearch}
```

Dependencies are caret ranges; `Cargo.lock` records a set verified to build on
1.75, so use `cargo build --locked` if you want exactly that set.

## Using it

### Index a tree — `cindex`

```
cindex ~/src                 # index a tree (added to any existing roots)
cindex                       # re-index the roots already stored
cindex --list                # show the indexed roots
cindex --remove ~/src/old    # drop one root and rebuild
cindex --reset               # delete the index
```

Adding a path indexes it *and* everything already indexed, so the index always
covers every root you have added. Re-run `cindex` whenever you want results to
reflect changed files — there is no watcher and no incremental update.

| Flag | Meaning |
|---|---|
| `--indexpath FILE` | use a different index file |
| `-j N` | worker threads (default: all cores) |
| `--batch-mib N` | source bytes buffered per batch (default 256) |
| `--verbose` | list every skipped file and progress |

### Search — `csearch`

```
csearch 'unsafe impl Send'             # plain search
csearch -n 'fn main'                   # with line numbers
csearch -l -f '\.rs$' 'TODO|FIXME'     # file names only, limited to .rs files
csearch -i -c deprecated               # case-insensitive, count per file
csearch --verbose 'foo.*bar'           # show the trigram query and timings
```

| Flag | Meaning |
|---|---|
| `-c` | print only a count of matching lines per file |
| `-f REGEXP` | only search files whose *name* matches |
| `-h` | omit file names from the output |
| `-i` | case-insensitive |
| `-l` | print only the names of matching files |
| `-n` | print line numbers |
| `--brute` | ignore the index and search every file |
| `--verbose` | print the trigram query and timings to stderr |
| `--indexpath FILE` | use a different index file |
| `-j N` | worker threads (default: all cores) |

Syntax is the [`regex` crate's](https://docs.rs/regex/latest/regex/#syntax) —
Perl-like, but with no backtracking, so matching is linear-time and cannot blow
up on a pathological pattern.

The index lives at `$CSEARCHINDEX`, else `~/.csearchindex`
(`%USERPROFILE%\.csearchindex` on Windows). Exit status follows grep: **0**
matched, **1** nothing matched, **2** an error.

## Good to know

- **Results reflect the last `cindex` run.** Files edited since then are
  searched as they were indexed; deleted files are reported once on stderr.
- **Not everything is indexed.** Skipped, as in the original: names beginning
  `.`, `#` or `~` or ending in `~`; files containing NUL; invalid UTF-8; lines
  over 2000 bytes; more than 20,000 distinct trigrams; larger than 1 GiB.
  Read and permission errors are always reported; the routine skips need
  `--verbose`.
- **There is no `-v`.** Everywhere else it means invert-match, which a trigram
  index fundamentally cannot do — the index finds files that *may* contain a
  match, not files that do not. Use `--verbose` for verbose output.
- **Some patterns cannot be narrowed.** `[a-z]+` or `.` yield no usable
  trigrams, so every file becomes a candidate and the search degrades to a
  parallel grep. `--verbose` prints `query: +` when that happens.
- **Output matches grep** where the two overlap: one match counted per line,
  CRLF-aware `^` and `$`, and no phantom line after a file's final newline.
- **Results stream** in path order as they are found, so `csearch pattern | head`
  produces output immediately and exits quietly when the reader goes away.
- **The index is replaced atomically.** A search running during a rebuild keeps
  reading the old index; a failed rebuild leaves the previous one intact. A
  damaged index is reported rather than crashing.

## How it works

A *trigram* is three consecutive bytes. `cindex` records, for every trigram,
the sorted list of files containing it.

`csearch` compiles your regexp into a boolean query over trigrams — searching
for `pthread_mutex_t` requires `pth`, `thr`, `hre` … `x_t`, all of them — and
intersects those posting lists to get a candidate set. Only the candidates are
then matched with the real regexp. The saving is in what is never read: twelve
files instead of 4,905 in the example above.

This is Russ Cox's design, described in
[Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html).
The query analysis here is a direct port of his `index/regexp.go`; the index
format, the trigram extraction and both engines are new.

```
src/trigram.rs   AVX2/scalar trigram packing, file validation, dedup
src/query.rs     boolean trigram Query (And/Or/All/None) + simplification
src/regexp.rs    regexp -> Query analysis
src/write.rs     parallel index builder + on-disk format
src/read.rs      mmap reader, posting lists, query evaluation
```

## csearch-rs and ripgrep

[ripgrep](https://github.com/BurntSushi/ripgrep) showed what modern hardware
does for brute-force search: SIMD in the inner loops and every core busy.
csearch-rs applies that same thinking *on top of* Cox's index rather than
instead of it.

The sharing is literal rather than spiritual. ripgrep matches through its
`grep-regex` crate, which is built on `regex-automata` and `regex-syntax` — the
same crates csearch-rs uses directly, by the same author, both bottoming out in
`memchr` and `aho-corasick`'s Teddy SIMD prefilters. Cox's Go original had
neither: a scalar byte loop for trigram extraction and a single-threaded custom
matcher. So csearch-rs is close to ripgrep's matching engine aimed at a
candidate set instead of at the whole tree, with AVX2 and rayon in the indexer
as well.

That does not make it strictly better. Which tool wins depends on two things,
and both were measured on the Linux kernel source — 94,842 files, 1.6 GB,
against ripgrep 15.2.0.

### What the pattern looks like

What matters is not how many lines match. It is how much of the tree the
trigram query can rule out before a single file is read:

| pattern | files left as candidates | vs ripgrep |
|---|---|---|
| `pthread_mutex_t` | 140 (0.1%) | **14.0x faster** |
| `foo.*bar` | 421 (0.4%) | **8.2x faster** |
| `kmem_cache_alloc` | 557 (0.6%) | **4.8x faster** |
| `struct task_struct` | 2,989 (3.2%) | 1.2x |
| `spin_lock_irqsave` | 3,746 (4.0%) | 1.0x |
| `devm_kzalloc` | 5,731 (6.1%) | 0.9x |
| `^static inline` | 16,055 (17.0%) | 0.5x — slower |
| `[0-9]{4}-[0-9]{2}-[0-9]{2}` | 22,739 (24.1%) | 0.4x — slower |
| `[a-z]+` | 94,274 (100%) | 0.5x — slower |

Grouped: pruning to **under 1%** of files gives a median **8.2x**; 1–20% is a
wash at 0.9x; above 20% ripgrep wins at 0.4x. The date regexp is the instructive
one — only 1,073 lines match, but digit classes produce common trigrams that
rule almost nothing out, so the index is paid for and then most of the kernel is
grepped anyway.

The rule of thumb: **a distinctive literal substring wins big; a class-heavy
regexp with no literal in it loses.** Symbol searches are the former.

### Whether the tree is in the page cache

The table above is warm-cache — the kernel had already been read and 1.6 GB
fits in this machine's RAM. That is the *worst* case for an index, because
ripgrep's reads are nearly free. Cold, the picture changes completely:

| pattern | ripgrep cold | csearch cold | cold | (warm) |
|---|---|---|---|---|
| `pthread_mutex_t` | 17,049 ms | 533 ms | **32.0x** | 13.0x |
| `kmem_cache_alloc` | 13,119 ms | 574 ms | **22.8x** | 4.1x |
| `spin_lock_irqsave` | 13,181 ms | 1,469 ms | **9.0x** | 1.0x |
| `^static inline` | 14,456 ms | 3,437 ms | **4.2x** | 0.5x |

Cold, csearch-rs is faster on *every* pattern — including the one it loses on
warm — because ripgrep must read 1.6 GB from disk while csearch-rs reads a
175 MB index plus a handful of candidate files. This is the case that matters
for a tree larger than RAM, which never stays cached.

### Which to use

**Reach for ripgrep by default.** It needs no index, so it is never stale, it
honours `.gitignore`, and on a warm tree it is faster for most patterns.

**csearch-rs earns its keep** when you search the same large tree over and over
with specific patterns — symbol names, error strings, identifiers — or when the
tree is too big to stay in the page cache. The index costs 21 s and 175 MB for
the kernel, once.

One caveat when comparing results: the two do not search identical file sets.
csearch-rs omits files it declined to index — very long lines, more than 20,000
distinct trigrams, invalid UTF-8 — which ripgrep still reads. Invisible for a
selective pattern; on `[a-z]+` over the whole kernel it is a 0.2% difference in
match count.

## Compared with Google's codesearch

This is a rewrite of [google/codesearch](https://github.com/google/codesearch)
rather than a translation. The design is the same; the machinery underneath is
not:

| Piece | Go original | csearch-rs |
|---|---|---|
| Trigram extraction | scalar byte loop | AVX2 kernel, 8 trigrams/iteration, runtime-detected with a scalar fallback |
| Per-file dedup | sparse set | 16M-bit thread-local bitmap, cleared selectively; only the ≤20k distinct values are sorted |
| Binary / long-line / UTF-8 checks | byte loop | `memchr` SIMD scans + `std::str::from_utf8` |
| Indexing | single-threaded, 3-way merge on disk | rayon across files; postings appended through a dense trigram→slot table (no hashing, no global sort) |
| Query analysis | `regexp.go` | full port (exact/prefix/suffix sets, MAX_SET=20, common-trigram factoring) on the `regex-syntax` HIR |
| Grep phase | sequential, custom RE2-lite | rayon across candidates; the `regex` crate (Teddy/Aho-Corasick SIMD prefilters) |
| Index format | custom | custom, mmap'd; delta-varint postings, 16-byte binary-searchable posting index |

The trade-off: the Go version merges postings on disk and can index
incrementally, so its memory use is bounded by the merge rather than the
corpus. This one holds postings in memory until the index is written, and
rebuilds every root each time.

**Results are identical.** `compare_csearch.py` indexes a corpus with both
implementations and checks that the per-file match counts agree exactly:

```
py compare_csearch.py --corpus /path/to/some/code
```

It fetches or builds both, runs a pattern set through each, prints a timing
table and exits non-zero on any mismatch. `--patterns FILE` for your own list,
`--runs N`, `--go-bin DIR`, `--no-build`.

### Benchmarks

Windows 10, Skylake i7 (8 cores), Go codesearch v1.2.0, rustc 1.98.0. Corpus:
6,022 files / 720 MB of mixed Python, C++, CUDA and Markdown; 9.1 MB index.

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

**11/11 patterns, per-file counts identical to the Go original**, and faster on
every pattern of every run. Expect **1.3x–2.4x** on search for a corpus like
this: much of each measurement is process startup and index load — the floor
for a query with no candidates is ~6.6 ms against ~8.7 ms — which compresses
the ratio. Indexing is 1.85x faster because it uses every core.

<details>
<summary>The original author's corpus (reported, not reproduced here)</summary>

The first cut was benchmarked on a 41 MB / 2,482-file Rust-heavy corpus that is
not available in this repository, so these are the author's numbers rather than
something verified here. They run higher because that corpus hits much larger
candidate sets (`^use std::` matched 295 files there against 98 above;
`impl<[A-Z]> .* for ` matched 212 against 0), so the grep phase — where the SIMD
prefilters and rayon actually pay — dominates.

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

</details>

## Testing

`cargo test` covers the AVX2 kernel against the scalar path, the bitmap dedup
against a naive set, varints, Cox's original regexp test vectors, an end-to-end
index/query round trip, the grep loop (line numbers, every output format, CRLF,
files with and without a final newline), an index damaged in every field, a
rebuild while the index is memory-mapped, and the real binaries end to end
(nested roots, vanished roots, `--remove`, a closed pipe, a missing index, exit
codes, deleted and unreadable files).

`tests/superset.rs` is a randomised property test of the guarantee the whole
design rests on: **every file a regexp matches must be among the candidates the
index returns.** A false negative there would be invisible in normal use — the
file simply never appears. 8,000 checks over random corpora and patterns have
produced none. `CSEARCH_PROP_ITERS=40 cargo test --test superset` runs it hard.

Beyond the suite: 11/11 parity with the Go original on two corpora, `csearch -c`
matching `grep -Ec` per file on every pattern tried, and identical results from
the static Linux and Windows builds. CI runs the suite, `rustfmt` and
`clippy -D warnings` on both platforms for every push.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security issues: [SECURITY.md](SECURITY.md).

## Licence and attribution

csearch-rs is a Rust port of **Google Code Search**
([github.com/google/codesearch](https://github.com/google/codesearch)) by Russ
Cox, and is distributed under the same 3-clause BSD licence as the original.
The upstream copyright notice is retained in [LICENSE](LICENSE) as that licence
requires; [NOTICE](NOTICE) records which files are derived from it.

`src/regexp.rs` and `src/query.rs` are ports of upstream's `index/regexp.go`;
the trigram extraction, index format, and the parallel indexing and search
engines are new work. Every runtime dependency is permissively licensed
(MIT / Apache-2.0 / Unlicense / Unicode-3.0) — there is no copyleft in the tree.

This project is not affiliated with or endorsed by Google.
