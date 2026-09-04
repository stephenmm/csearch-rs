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

## Automatic refresh (part 3)

Built on the per-project index:

- **A git-state stamp**, a sidecar `<index>.meta`, records each git root's
  `HEAD` and a fingerprint of `git status --porcelain` after each build. It is
  a plain text file with a header line; a lost or unreadable stamp only ever
  costs one extra rebuild, so it needs no format versioning. No `serde`
  dependency was added.
- **`cindex --if-changed`** rebuilds only when the planned root set differs
  from the stamp, or any root's `HEAD` or working-tree fingerprint has moved.
  Conservative: anything unknown (no stamp, a non-git root, a git error)
  rebuilds. Never skips when a rebuild might be needed.
- **`cindex --background`** re-execs itself detached with stdio to null and
  returns immediately, guarded by an env var so it detaches exactly once. A
  hook can therefore refresh the index without making git wait.
- **`cindex --install-hooks`** writes `post-checkout`, `post-merge`,
  `post-commit` and `post-rewrite`, each running
  `cindex --local --if-changed --background`, into the repository's hooks
  directory (honouring `core.hooksPath`). It leaves foreign hooks alone —
  overwriting only files carrying the `csearch-rs` marker — and implies
  `--local` so the initial index is built. `--uninstall-hooks` removes only
  the marked ones.
- **`csearch` staleness note**: a one-line stderr warning when a stamped
  root's `HEAD` has moved since the build. HEAD-only, so it costs one
  `git rev-parse` per stamped root and nothing for a non-git index. It does
  not change the exit status.

The staleness note is HEAD-only for speed on the search path, while
`--if-changed` also checks the working-tree fingerprint for correctness on the
indexing path: an uncommitted edit should force a rebuild, but need not slow
every search.

## Deferred

- Relative paths in the index, so it survives `mv` of the repository — needs
  a format version, and `csearch` re-absolutising names on output.
- `csearch --all` over a registry of known local indexes; per-project indexes
  make cross-project search something to ask for explicitly.
- A filesystem watcher for edits between git events — a resident process, a
  heavier dependency, behind a Cargo feature.
