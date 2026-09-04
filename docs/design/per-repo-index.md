# Per-project indexes

*Part 2 of [#1](https://github.com/stephenmm/csearch-rs/issues/1).*

## Problem

There was one index, `~/.csearchindex`, covering every root ever added, and
`cindex` rebuilt all of it on every run. Keeping it fresh was therefore
expensive — a hook in one repository would re-walk every other — and a
search could not tell which project it was searching.

## The rule

An index is found by, in order:

1. `--indexpath`
2. `$CSEARCHINDEX`, if set and non-empty
3. the nearest `.csearchindex` **file** at or above the working directory
4. `~/.csearchindex` (`%USERPROFILE%\.csearchindex` on Windows)

Step 3 is the only addition. It generalises the old rule rather than
replacing it: `~/.csearchindex` is simply the last stop of the walk. A
project index is a `.csearchindex` file at the project root, and it exists
only if someone created one with `cindex --local` — so every existing setup
resolves exactly as before. A *directory* of that name is not an index.

The explicit flag and the environment variable keep winning. Someone who set
`CSEARCHINDEX` in a shell profile has asked for a specific index, and gets it.

## `cindex --local`

Finds the enclosing repository root — the nearest ancestor with a `.git`
entry, which is a directory normally and a file for a worktree — or uses the
working directory outside any repository. It indexes that root into
`<root>/.csearchindex`, and:

- **implies `--git`**, so the file list comes from `git ls-files` and ignored
  files never enter the index (`--no-git` walks instead);
- **appends `.csearchindex` to `info/exclude`** (`git rev-parse --git-path
  info/exclude`, which is correct for worktrees; `.git/info/exclude` as the
  fallback). That file is local to the clone, so `git status` stays clean and
  no tracked file — in particular the committed `.gitignore` — is touched.
  Idempotent.

After that, plain `cindex` anywhere inside the repository rebuilds the local
index, and `csearch` anywhere inside it searches that index, with no
configuration. Outside the repository nothing changes.

## Why the index lives in the working tree

`<root>/.csearchindex` needs no git binary to discover, works for
directories that are not repositories, and is skipped by the indexer's own
dotfile rule so it never indexes itself. The cost is that `git clean -fdx`
removes it; it is regenerable, and once hooks exist (part 3) the next
checkout rebuilds it.

The alternative, `.git/csearch.index`, would survive `git clean` and vanish
with the repository, but needs `.git` resolved for worktrees and does not
apply to plain directories. Not chosen; easy to add later as an option.

## Compatibility

- No new flag and no `.csearchindex` above the working directory: both
  binaries behave byte-for-byte as before. The existing CLI tests all set
  `CSEARCHINDEX` and still pass; `tests/cli.rs` adds a case proving
  `--indexpath` beats a local index.
- `git` is invoked only under `--local`, `--git`, or when writing the
  exclude entry.
- The index format is unchanged. A local index is an ordinary index whose
  root list has one entry.

## Deferred

- Relative paths in the index, so it survives `mv` of the repository — needs
  a format version, and `csearch` re-absolutising names on output.
- `csearch --all` over a registry of known local indexes; per-project indexes
  make cross-project search something to ask for explicitly.
- Automatic refresh: hooks, `--if-changed`, staleness warning — part 3.
