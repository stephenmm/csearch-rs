# Project: csearch-rs

Rust port of Google Code Search (cindex/csearch) with SIMD trigram extraction
and parallel indexing/search. Its own binaries are `cindex-rs` and
`csearch-rs`.

## What this project is

A from-scratch Rust reimplementation of Russ Cox's `codesearch` (the Go
`cindex` + `csearch` pair). `cindex-rs` walks directory trees, extracts the set
of distinct 3-byte trigrams from every text file, and writes a compact
posting-list index; `csearch-rs` compiles a regexp into a boolean trigram
query, evaluates it against the index to get a small candidate set, then greps
only those files.

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

- **Nothing this project installs may share a name with the original.** The
  two are meant to be installed side by side, and their index formats are
  unrelated, so a shared file name means each tool reporting the other's index
  as corrupt. `cindex`, `csearch`, `cgrep`, `.csearchindex` and
  `$CSEARCHINDEX` are the original's; ours are in `src/names.rs`, which is the
  only place they are spelled out in the source. `tests/names.rs` and
  `tests/coexist.rs` enforce it -- the latter against the real Go binaries.
  In prose, a bare `cindex`/`csearch` therefore always means the original.
- Line endings are normalised to LF via `.gitattributes`, so the repository is
  identical whether it is checked out on Windows or Unix.
- Commit at meaningful milestones, one logical change per commit, with the
  regression test that would have caught the bug in the same commit.
- **A hook is a guest.** `cindex-rs --hook` prints nothing, exits 0 whatever
  happens, returns before the work is done, and changes nothing where there
  is no index. Anything added to that path keeps to it; `--hook --verbose` is
  where explanations go.
- **Tests wait for evidence, not for time.** When a test needs another process
  to have reached some point, it waits for a line on stderr, a file, or a
  lock -- never `sleep(1500)`. Process start-up on a busy Windows machine has
  been seen to take five seconds.
- `rustfmt` defaults and `clippy -D warnings`; CI enforces both on every
  platform before it runs the tests.

## Key files / layout

```
src/names.rs     every name the project installs; nothing else spells them
src/trigram.rs   AVX2/scalar trigram packing, file validation, bitmap dedup
src/query.rs     boolean trigram Query (And/Or/All/None) + simplification
src/regexp.rs    regexp -> Query analysis (port of Cox's index/regexp.go)
src/write.rs     parallel index builder + on-disk format; planning the roots
src/read.rs      mmap reader, delta-varint posting lists, query evaluation
src/varint.rs    varint encode/decode
src/paths.rs     index path resolution; pre-0.3 leftovers; repo root; sidecars
src/listing.rs   which files belong to a root (walk | git | p4); fingerprint
src/p4.rs        Perforce: `p4 -G` reader, workspace root, have + opened files
src/stamp.rs     <index>.meta: what the index was built from (--if-changed)
src/lock.rs      one refresh of an index at a time; the hooks' queue
src/githead.rs   HEAD read from the repository's files, without running git
src/hook.rs      --print-hook: the wrapper for each shell; p4's read-only commands
src/bin/cindex.rs   -> cindex-rs
src/bin/csearch.rs  -> csearch-rs

tests/common/mod.rs        the built binaries + sealed test environments
tests/index_roundtrip.rs   end-to-end index + query round trip
tests/cli.rs               drives the real binaries (roots, exit codes, pipes)
tests/corruption.rs        every field of an index damaged in turn
tests/superset.rs          randomised: matches are always candidates
tests/git_listing.rs       --git: the file list comes from git
tests/listing_source.rs    the index remembers how each root is listed
tests/refresh.rs           --if-changed, --background, the lock, staleness
tests/githead.rs           HEAD from files agrees with git, layout by layout
tests/hooks.rs             --hook; git's and Mercurial's hooks; shell wrappers
tests/p4.rs                Perforce against a real p4d: listing, sync, submit
tests/names.rs             our names are ours; --help never names theirs
tests/coexist.rs           side by side with the original, stand-in and real

compare_csearch.py         parity + timing harness vs the Go original
build_standalone.py        static binaries for Windows and Linux
setup_csearch.py           build, test, install to the user's bin directory
```

## How to run

```
cargo build --release      # binaries in target/release/{cindex-rs,csearch-rs}
cargo test                 # the full suite
cargo clippy --all-targets -- -D warnings
cargo fmt --check

CSEARCH_RS_PROP_ITERS=40 cargo test --test superset   # property test, harder
py compare_csearch.py --corpus /path/to/code          # parity/timing vs Go
py build_standalone.py                                # static binaries -> dist/
```

The index is found by, in order: `--indexpath`; `$CSEARCH_RS_INDEX`; the
nearest `.csearch-rs-index` at or above the working directory (a project index
from `cindex-rs --local`); `~/.csearch-rs-index`
(`%USERPROFILE%\.csearch-rs-index` on Windows). Exit status follows grep: 0
matched, 1 nothing matched, 2 an error.

`tests/coexist.rs` runs one test against Google's real binaries. It skips with
a message when they are not installed; `go install
github.com/google/codesearch/cmd/{cindex,csearch}@v1.2.0` provides them, and
`CSEARCH_RS_REQUIRE_ORIGINAL=1` (set in CI) turns the skip into a failure.

`tests/hooks.rs` runs the `--print-hook` wrapper in every shell it finds, and
the Mercurial recipe if `hg` is installed. `CSEARCH_RS_REQUIRE_SHELLS` (a
comma-separated list) and `CSEARCH_RS_REQUIRE_HG=1` do for those what the
variable above does for the original; the CI matrix sets them per platform.
On this Windows machine that means sh, bash and dash from Git, and Windows
PowerShell; zsh, ksh, fish, tcsh, csh, pwsh and Mercurial are exercised only
in CI.

`tests/p4.rs` needs Perforce's `p4` and `p4d` on PATH and skips without them;
`CSEARCH_RS_REQUIRE_P4=1` (set in CI, which downloads both) makes that a
failure. Neither is installed on this machine, so **nothing Perforce-specific
has ever run here** -- every claim about it rests on CI. Each test makes its
own empty server with `P4PORT=rsh:p4d -r ROOT -i`: no daemon, no port.

## Status

Complete and verified. 143 tests; CI builds and tests on Linux, Windows and
macOS, gating on rustfmt and clippy before the suite.

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
so a `csearch-rs` running mid-rebuild keeps reading the old file and a failed
build leaves nothing behind; on Windows a mapped file cannot be deleted, so the
old index is parked aside and removed afterwards. Roots are collapsed by
containment, and a stored root that has vanished is dropped with a note instead
of wedging every future run.

**Behaviour.** Output matches grep where the two overlap: one match counted per
line, CRLF-aware anchors, no phantom line after a trailing newline, quiet exit
on a closed pipe. Results stream in ordered batches of 64 rather than being
buffered whole, which bounds memory and lets the first lines appear before the
search finishes. Read and permission errors are reported without `--verbose`;
`csearch-rs` says once if indexed files have since been deleted.

**Coexistence.** Since 0.3 the binaries, index file and environment variable
have names of their own, and the original's are never read, written or
honoured. A `.csearchindex` that csearch-rs 0.2 left behind is recognised by
its magic and pointed out -- never used, never deleted -- while one belonging
to the original is passed over in silence. Verified against Go codesearch
v1.2.0, which is also how it came to light that the original cannot refresh an
index in place on Windows (a bare `cindex` leaves the old one and
`.csearchindex~` files); the test uses `cindex -reset <path>` for that reason.

**Freshness.** Since 0.4 anything that can run a command keeps an index
fresh with `cindex-rs --hook`: git through `--install-hooks`, Mercurial
through two lines of `hgrc`, a system with no client-side hooks through a
shell wrapper from `--print-hook TOOL`, and anything else through a scheduler.
Change detection is a fingerprint of the listing (path, size and modification
time of every file), so it needs no version-control system and catches what
the old `git status` comparison missed; the index (format 2) records how each
root is listed, so a refresh lists it the same way; a kernel lock makes
refreshes of one index take turns, and a burst of hooks collapses to one
running and one waiting. A hook that cannot list a root the way the index
records leaves the index alone instead of walking. Design:
`docs/design/refresh-from-any-vcs.md`.

**Perforce.** Since 0.5. Perforce has no client-side hooks at all (triggers
are server-side, `p4 aliases` cannot run a program), so its "hook" is the
shell wrapper from `--print-hook p4`, which skips the refresh after commands
that only report. `--p4` lists a root through Perforce -- `p4 have` plus
opened files, read with `p4 -G` -- and `--local --p4` puts the index at the
workspace root. A Perforce listing that fails is always an error, never a
walk: the server being out of reach is routine, and a workspace is where the
build products are. Design: `docs/design/perforce.md`.

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
  (Python's `open()`, some older tools -- not `csearch-rs`, which shares it)
  blocks a rebuild. Nothing in user space can move a file held that way; the
  failure is clean and the message says to close the other program.
- 32-bit is untried and would regress: the AVX2 kernel is gated on
  `#[cfg(target_arch = "x86_64")]`, so an `i686-*` build silently falls back to
  the scalar path and loses the headline performance win. Widen the gate first
  if a 32-bit target is ever wanted.
