# Contributing

Bug reports and patches are welcome. This is a small project maintained in
spare time, so please open an issue before starting anything large.

## Building

Rust 1.75 or newer, and on Windows the MSVC toolchain (for the linker).

```
cargo build --release        # binaries in target/release/{cindex,csearch}
cargo test                   # the full suite
```

There is no build script and no code generation; `cargo build` is the whole
story.

## What CI requires

Every push runs on Linux and Windows, and all four of these must pass. Run
them locally before opening a pull request:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked                        # the manifest must match Cargo.lock
```

`clippy` runs on both platforms in CI because the index-replacement code is
`#[cfg(windows)]` and is only linted where it compiles. If you touch anything
platform-specific, expect the other platform to have an opinion.

## Tests

- `src/**` unit tests cover the AVX2 trigram kernel against the scalar path,
  the bitmap dedup against a naive set, varints, the regexp analyser against
  Russ Cox's original test vectors, and the grep loop.
- `tests/cli.rs` drives the real `cindex` and `csearch` binaries. Most bugs
  this project has had lived in how the pieces are wired together, not in the
  pieces, so end-to-end coverage matters here.
- `tests/corruption.rs` damages every field of an index in turn. A malformed
  index must produce an error, never a panic.
- `tests/superset.rs` is a randomised property test of the guarantee the whole
  design rests on: **every file a regexp matches must be among the candidates
  the index returns.** It runs 8 corpora by default;
  `CSEARCH_PROP_ITERS=40 cargo test --test superset` runs more. A failure
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

- **Index format.** The on-disk format is versioned by the magic string in
  `src/write.rs`. If you change the layout, change the magic too; readers
  report an old index clearly instead of misparsing it.
- **MSRV.** 1.75, declared as `rust-version` in `Cargo.toml`. Please do not
  raise it casually.
- **Output.** `csearch` aims to match `grep` where the two overlap: one match
  counted per line, exit 0/1/2, no phantom line after a trailing newline.
  Divergence from grep is a bug unless there is a stated reason.

## Style

`rustfmt` defaults, and comments that explain *why* rather than what. Several
comments in this codebase exist to record a non-obvious constraint (Windows
cannot delete a mapped file; a trailing newline does not begin a line) — that
kind of note is welcome.
