# Per-project indexes

*Part 2 of [#1](https://github.com/stephenmm/csearch-rs/issues/1). Names
updated for 0.3, which gave the binaries, the index file and the variable
names of their own; nothing else about the design changed.*

## Problem

There was one index in the home directory, covering every root ever added, and
`cindex-rs` rebuilt all of it on every run. Keeping it fresh was therefore
expensive — a hook in one repository would re-walk every other — and a
search could not tell which project it was searching.

## The rule

An index is found by, in order:

1. `--indexpath`
2. `$CSEARCH_RS_INDEX`, if set and non-empty
3. the nearest `.csearch-rs-index` **file** at or above the working directory
4. `~/.csearch-rs-index` (`%USERPROFILE%\.csearch-rs-index` on Windows)

This is the original csearch's rule — `$CSEARCHINDEX`, else `~/.csearchindex`
— with step 3 added, and with every name changed so that the two tools can
never resolve to the same file. Step 3 generalises the rule rather than
replacing it: the home index is simply the last stop of the walk. A project
index is a `.csearch-rs-index` file at the project root, and it exists only if
someone created one with `cindex-rs --local`. A *directory* of that name is
not an index.

The explicit flag and the environment variable keep winning. Someone who set
`CSEARCH_RS_INDEX` in a shell profile has asked for a specific index, and gets
it.

## `cindex-rs --local`

Finds the enclosing repository root — the nearest ancestor with a `.git`
entry, which is a directory normally and a file for a worktree — or uses the
working directory outside any repository. It indexes that root into
`<root>/.csearch-rs-index`, and:

- **implies `--git`**, so the file list comes from `git ls-files` and ignored
  files never enter the index (`--no-git` walks instead);
- **appends `.csearch-rs-index` to `info/exclude`** (`git rev-parse --git-path
  info/exclude`, which is correct for worktrees; `.git/info/exclude` as the
  fallback). That file is local to the clone, so `git status` stays clean and
  no tracked file — in particular the committed `.gitignore` — is touched.
  Idempotent.

After that, plain `cindex-rs` anywhere inside the repository rebuilds the
local index, and `csearch-rs` anywhere inside it searches that index, with no
configuration. Outside the repository nothing changes.

## Why the index lives in the working tree

`<root>/.csearch-rs-index` needs no git binary to discover, works for
directories that are not repositories, and is skipped by the indexer's own
dotfile rule so it never indexes itself. The cost is that `git clean -fdx`
removes it; it is regenerable, and once hooks exist (part 3) the next
checkout rebuilds it.

The alternative, a file inside `.git`, would survive `git clean` and vanish
with the repository, but needs `.git` resolved for worktrees and does not
apply to plain directories. Not chosen; easy to add later as an option.

## Compatibility

- No `--local`, and no `.csearch-rs-index` above the working directory: the
  index is the home one, exactly as with the single-index design. The CLI
  tests that name an index through `$CSEARCH_RS_INDEX` are unaffected by the
  walk; `tests/cli.rs` has a case proving `--indexpath` beats a local index.
- `git` is invoked only under `--local`, `--git`, or when writing the
  exclude entry.
- The index format is unchanged. A local index is an ordinary index whose
  root list has one entry.
- The original csearch's files are not part of any of this. Its
  `.csearchindex` is not a candidate at step 3 or step 4 and its
  `$CSEARCHINDEX` is not consulted at step 2 (`tests/coexist.rs`).

## Automatic refresh (part 3)

Built on the per-project index:

- **A git-state stamp**, a sidecar `<index>.meta`, records each git root's
  `HEAD` and a fingerprint of `git status --porcelain` after each build. It is
  a plain text file with a header line; a lost or unreadable stamp only ever
  costs one extra rebuild, so it needs no format versioning. No `serde`
  dependency was added.
- **`cindex-rs --if-changed`** rebuilds only when the planned root set differs
  from the stamp, or any root's `HEAD` or working-tree fingerprint has moved.
  Conservative: anything unknown (no stamp, a non-git root, a git error)
  rebuilds. Never skips when a rebuild might be needed.
- **`cindex-rs --background`** re-execs itself detached with stdio to null and
  returns immediately, guarded by an env var so it detaches exactly once. A
  hook can therefore refresh the index without making git wait.
- **`cindex-rs --install-hooks`** writes `post-checkout`, `post-merge`,
  `post-commit` and `post-rewrite`, each running
  `cindex-rs --local --if-changed --background`, into the repository's hooks
  directory (honouring `core.hooksPath`). It leaves foreign hooks alone —
  overwriting only files carrying the `csearch-rs` marker, which includes
  hooks installed before the binaries were renamed — and implies `--local` so
  the initial index is built. `--uninstall-hooks` removes only the marked
  ones.
- **`csearch-rs` staleness note**: a one-line stderr warning when a stamped
  root's `HEAD` has moved since the build. HEAD-only, so it costs one
  `git rev-parse` per stamped root and nothing for a non-git index. It does
  not change the exit status.

The staleness note is HEAD-only for speed on the search path, while
`--if-changed` also checks the working-tree fingerprint for correctness on the
indexing path: an uncommitted edit should force a rebuild, but need not slow
every search.

## Deferred

- Relative paths in the index, so it survives `mv` of the repository — needs
  a format version, and `csearch-rs` re-absolutising names on output.
- `csearch-rs --all` over a registry of known local indexes; per-project
  indexes make cross-project search something to ask for explicitly.
- A filesystem watcher for edits between git events — a resident process, a
  heavier dependency, behind a Cargo feature.
