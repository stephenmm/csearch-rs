# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/stephenmm/csearch-rs/security/advisories/new)
rather than opening a public issue.

This is a spare-time project; expect an acknowledgement within a week or so.
There is no bounty.

## Supported versions

The latest release only. There are no maintenance branches.

## What this software does with untrusted input

`cindex` and `csearch` are local command-line tools with no network access and
no privileged operations, so the interesting inputs are the files they read:

- **Files being indexed or searched.** Contents are treated as bytes. Files
  containing NUL, invalid UTF-8, lines over 2000 bytes, more than 20,000
  distinct trigrams, or larger than 1 GiB are skipped. File contents are never
  interpreted as anything but data.
- **The index file.** `csearch` memory-maps it. Every section offset, name
  index entry and posting index entry is validated when the index is opened,
  and a damaged file is reported as corrupt rather than trusted. This matters
  because an index is a file like any other and may not have been written by
  this program. If you find an index that causes a panic, an out-of-bounds
  read, or a hang, that is a bug worth reporting — `tests/corruption.rs` is
  where such a case belongs.
- **The regular expression.** Matching uses the `regex` crate, which has no
  backtracking and guarantees linear-time matching, so a pattern cannot cause
  catastrophic backtracking. Query analysis is bounded (`MAX_SET`), though a
  sufficiently large bounded repetition can still be slow to analyse.

There is one `unsafe` block in the library, in `src/trigram.rs`: an AVX2
kernel guarded by runtime feature detection, with its bounds checked against
the scalar implementation in tests. Memory mapping in `src/read.rs` is
`unsafe` by nature of the API — a file modified while mapped is undefined
behaviour, which is why index replacement goes through an atomic rename
rather than writing in place.

## Out of scope

Indexing a directory means reading every file in it. Pointing `cindex` at
somewhere you do not trust, or at secrets you do not want in an index file, is
a decision for the person running it, not a vulnerability in the tool.
