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

## Toolchain on this machine (hard-won — trust this)

Rust lives on **D:** (`D:\rust\.cargo`, `D:\rust\.rustup`), not E: and not C:.
`CARGO_HOME`/`RUSTUP_HOME` are set as user env vars and `D:\rust\.cargo\bin` is
on the user PATH. rustc/cargo 1.98.0, host `x86_64-pc-windows-msvc`.

**Do not move CARGO_HOME to E:.** E: is exFAT, whose timestamp epoch starts in
1980, and crates.io tarballs carry a 1970 mtime — so unpacking any crate dies
with `failed to set mtime ... The parameter is incorrect. (os error 87)`.
The same exFAT quirk made rustup itself report the bogus
`toolchain 'stable-x86_64-pc-windows-msvc' is not installable` even though the
toolchain was fully extracted and working; both problems vanished on NTFS.
C: was rejected as the home because it has only ~16 GB free (the toolchain
alone is 1.3 GB, and the crate cache grows without bound).

The winget `Rustlang.Rustup` install exits 1 partway: it downloads and extracts
the toolchain but never writes `settings.toml` or creates the proxy shims. Fix
is `rustup default stable` + `rustup self update` once RUSTUP_HOME is on NTFS.

The project tree itself stays on E: per the global convention; only the
toolchain and crate cache are on D:. `target/` on exFAT builds fine.

Linking uses MSVC Build Tools at `E:\VS\BuildTools`; if a build ever fails to
find `link.exe`, run it from a `vcvars64.bat` shell.

**Go** (for `compare_csearch.py`) is at `D:\go-sdk\go`, `GOPATH=D:\go`, both
bin dirs on the user PATH; `go install github.com/google/codesearch/cmd/...`
put the reference binaries in `D:\goin`. The winget `GoLang.Go` package is
an **MSI that needs admin** and dies with 1602 (user cancelled) in a
non-interactive session — use the official portable zip from `go.dev/dl`
instead (checksum published in `go.dev/dl/?mode=json`), which needs no
elevation and keeps Go off C:.

## Status

**Verified on this machine 2026-09-01** (Windows 10, rustc 1.98.0):

- `cargo test` — 8/8 pass (7 unit: AVX2-vs-scalar kernel, bitmap dedup vs naive
  set, varint round trip, Cox regexp cases, case folding, big-class; 1
  integration: index/query round trip). Clean build, no warnings.
- `cargo build --release` — clean, ~1m20s.
- Live smoke test on `E:\proj\github\flaskhub`: 732 files indexed (320 skipped)
  in 4.0s, 93,801 trigrams, 3.05 MB index; queries resolve candidates in
  ~350 µs.
- **Standalone binaries for both platforms, verified 2026-09-01.**
  `build_standalone.py` (mypy --strict clean) builds both from Windows:
  Windows `x86_64-pc-windows-msvc` + `-C target-feature=+crt-static` (dumpbin
  confirms only OS DLL imports -- `VCRUNTIME140.dll` and the `api-ms-win-crt-*`
  set are gone, so no VC++ redistributable is needed), and Linux
  `x86_64-unknown-linux-musl` inside WSL Ubuntu (`file` reports `static-pie
  linked`). Sizes: 0.7/1.9 MB Windows, 1.2/2.7 MB Linux. Output in `dist/`
  (gitignored). The dependency tree is pure Rust, so the musl build needs
  neither musl-gcc nor sudo -- only a user-scoped rustup inside WSL, which the
  script installs if absent.
- **Cross-platform result parity**: the Linux and Windows binaries indexed the
  same tree and returned byte-identical per-file counts for 6 patterns
  (`def `, `return`, `import numpy`, `TODO|FIXME`, a date regexp, `DoP256`) --
  169/348/40/8/71/15 files. Harness: scratchpad `crossplatform_parity.py`.
- **CI**: `.github/workflows/build.yml` matrix-builds and tests both targets on
  real runners (linux ~1m45s, windows ~3m10s), asserts the static property in
  the job itself (`file | grep static`; dumpbin failing on any CRT import), and
  uploads `.tar.gz` / `.zip` artifacts. A `v*` tag additionally publishes a
  GitHub release. First-party actions only, pinned at v7.
- **Parity against the Go original**: `compare_csearch.py` — 11/11 patterns,
  per-file counts identical, on both `E:\proj\githublaskhub` and `E:\proj`.
  Speed on `E:\proj` (6,022 files): 1.3x-2.4x faster per pattern, 1.85x faster
  indexing. Lower than the README's original 1.5x-6x table because that corpus
  was Rust-heavy with much larger candidate sets — see README for the
  breakdown. Rust won every pattern of every run.
- **Parity against `grep -Ec`**: exact per-file count agreement over the .py
  subset for `def `, `return`, `import numpy`, `self\.[a-z_]+ =`, `^from `
  (140/122/39/21/136 files respectively). Harness: `parity.py` pattern, run
  ad hoc — see git history of this file.

## Open questions / TODO

- **The release job has never run.** It is gated on a `v*` tag and no tag
  exists yet, so the `gh release create` path is the one untested part of CI.
  `git tag v0.1.0 && git push origin v0.1.0` would exercise it.
- `setup_csearch.py` installs the binaries to `%USERPROFILE%\bin` (on C:, already
  on PATH). Not yet run -- the binaries currently only exist in `target/release`
  and `dist/`.
- No index has been built at the default `%USERPROFILE%\.csearchindex`; all runs
  so far used a scratch `CSEARCHINDEX`.
- 32-bit is untried, and would regress: the AVX2 kernel is gated on
  `#[cfg(target_arch = "x86_64")]`, so an `i686-*` build silently falls back to
  the scalar path and loses the headline performance win. Widen the gate first
  if a 32-bit target is ever wanted.
- The original README benchmark table came from a Rust-heavy 41 MB corpus that
  does not exist on this machine; local numbers are in the README's
  "Measured on this machine" section.
