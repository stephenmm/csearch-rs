# Contributing

Bug reports and patches are welcome. This is a small project maintained in
spare time, so please open an issue before starting anything large.

## Building

Rust 1.75 or newer, and on Windows the MSVC toolchain (for the linker).

```
cargo build --release        # binaries in target/release/{cindex-rs,csearch-rs}
cargo test                   # the full suite
```

There is no build script and no code generation; `cargo build` is the whole
story.

## What CI requires

Every push runs on Linux, Windows, and macOS (Apple Silicon and Intel), and
all four of these must pass. Run them locally before opening a pull request:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked                        # the manifest must match Cargo.lock
```

A pull request also cannot merge unless `[package].version` in `Cargo.toml`
has increased from `main`. Bump it by whatever a semver change of this size
warrants; for a change that genuinely touches nothing release-worthy (CI
config, docs, a comment), put `[skip version]` in the PR title instead.
`.github/scripts/check_version_bump.py` is the check itself, runnable
standalone if you want to see why it passed or failed.

`clippy` runs on both platforms in CI because the index-replacement code is
`#[cfg(windows)]` and is only linted where it compiles. If you touch anything
platform-specific, expect the other platform to have an opinion.

## Tests

- `src/**` unit tests cover the AVX2 trigram kernel against the scalar path,
  the bitmap dedup against a naive set, varints, the regexp analyser against
  Russ Cox's original test vectors, and the grep loop.
- `tests/cli.rs` drives the real `cindex-rs` and `csearch-rs` binaries. Most
  bugs this project has had lived in how the pieces are wired together, not in
  the pieces, so end-to-end coverage matters here.
- `tests/names.rs` and `tests/coexist.rs` check that nothing the project
  installs shares a name with the original codesearch, and run the two side by
  side. One test there needs Google's real `cindex` and `csearch`: it skips
  with a message if they are not installed (`go install
  github.com/google/codesearch/cmd/{cindex,csearch}@v1.2.0` provides them), and
  CI sets `CSEARCH_RS_REQUIRE_ORIGINAL=1` so that there a missing original is
  a failure, not a skip.
- `tests/refresh.rs`, `tests/listing_source.rs`, `tests/githead.rs` and
  `tests/hooks.rs` cover keeping an index fresh: what counts as a change, how
  each root is listed, the lock, and `cindex-rs --hook` as git, Mercurial and a
  shell wrapper run it. The wrapper test runs in every shell it finds and
  skips, with a message, the ones that are not installed.
  `CSEARCH_RS_REQUIRE_SHELLS=bash,zsh,...` and `CSEARCH_RS_REQUIRE_HG=1` turn
  those skips into failures; CI sets both, per platform, in the build matrix.
- `tests/p4.rs` runs against Perforce's own `p4` and `p4d`, with an empty
  server per test and no daemon (`P4PORT=rsh:p4d -r ROOT -i` runs one p4d
  per command, over a pipe). The tests skip, with a message, where the two
  programs are not on `PATH`; CI downloads them and sets
  `CSEARCH_RS_REQUIRE_P4=1`. Both are free downloads from Perforce and need
  no licence for this.
- Those tests start processes and wait for them. When one needs another
  process to have got somewhere, it waits for evidence of that -- a line on
  stderr, a file appearing, a lock being taken -- and never for a length of
  time. Two tests that slept instead failed on a busy machine.
- `tests/corruption.rs` damages every field of an index in turn. A malformed
  index must produce an error, never a panic.
- `tests/superset.rs` is a randomised property test of the guarantee the whole
  design rests on: **every file a regexp matches must be among the candidates
  the index returns.** It runs 8 corpora by default;
  `CSEARCH_RS_PROP_ITERS=40 cargo test --test superset` runs more. A failure
  prints the seed, pattern and file needed to reproduce it.

If you change the query analysis or the index format, run the property test
hard (a few hundred iterations) before sending the change.

New behaviour needs a test that fails without the fix. If you are fixing a
bug, the most useful thing you can include is the smallest input that shows
it.

## Comparing against the Go original

`compare_csearch.py` builds both implementations, indexes a corpus with each,
and checks that the per-file match counts are **identical**:

```
py compare_csearch.py --corpus /path/to/some/code
```

It exits non-zero on any mismatch. Changes to the matching or query code
should keep this at full parity; it is the strongest correctness signal the
project has. Pass `--go-bin DIR` if you already have Google's `cindex` and
`csearch` built.

## Compatibility

- **Names.** csearch-rs installs next to the original codesearch, so nothing
  it puts on a machine may share a name with it: not a binary, not the index
  file, not an environment variable. `cindex`, `csearch`, `cgrep`,
  `.csearchindex` and `$CSEARCHINDEX` are the original's and are never read,
  written or honoured here. Ours live in `src/names.rs` and nowhere else in
  the source — use the constants, including in messages, so that a hint like
  "run `cindex-rs --reset`" can never name the other tool.
- **Index format.** The on-disk format is versioned by the magic string in
  `src/write.rs`. If you change the layout, change the magic too; readers
  report an old index clearly instead of misparsing it.
- **MSRV.** 1.75, declared as `rust-version` in `Cargo.toml`. Please do not
  raise it casually.
- **Output.** `csearch-rs` aims to match `grep` where the two overlap: one
  match counted per line, exit 0/1/2, no phantom line after a trailing newline.
  Divergence from grep is a bug unless there is a stated reason.

## Style

`rustfmt` defaults, and comments that explain *why* rather than what. Several
comments in this codebase exist to record a non-obvious constraint (Windows
cannot delete a mapped file; a trailing newline does not begin a line) — that
kind of note is welcome.
