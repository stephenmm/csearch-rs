# Perforce

*New in 0.5. Builds on [refreshing from any version-control
system](refresh-from-any-vcs.md).*

## What was asked for, and what Perforce allows

"Refresh the index when Perforce updates the workspace, or when I submit."

git would do that with a hook. Perforce has nothing of the kind on the
client:

- **Triggers run on the server.** They can veto a submit; they cannot touch a
  workstation, and installing one needs an administrator.
- **`p4 aliases` only rewrite p4 commands** into other p4 commands, inside the
  p4 process. An alias cannot start a program.
- **P4V and the IDE plugins** have no post-sync callback either.

So the place to stand is the one every system without hooks leaves: the
shell. `cindex-rs --print-hook p4` wraps `p4` so that `cindex-rs --hook` runs
after it. That part is not specific to Perforce and came with 0.4. This note
is about the three things that are.

## 1. Knowing which commands change nothing

A wrapper fires after every command, and most p4 commands a person types are
questions: `p4 opened`, `p4 changes`, `p4 diff`, `p4 describe`. The hook is
told what ran (`--hook --after p4 -- ARGS`), and for a command that only
reports it stops before it has started a process or asked anything.

The list is of commands known to write no file in the workspace and to make
no file part of it. Anything not on it is assumed to have changed something.
Left off on purpose:

- `print`, because `-o FILE` writes one;
- `set` and `client`, which change which workspace this is or what it maps;
- the `-n` previews of commands that do change things. A wasted check after
  `p4 sync -n` costs less than a table of every command's flags, and less
  than getting one wrong.

Finding the command means reading past p4's own options -- `-c client`,
`-p port`, `-ztag` -- and only as far as that. An option that is not
recognised makes the whole line "unknown", which refreshes. From the csh
wrapper the words arrive quoted, so a redirection the user typed is among
them and a quoted argument still wears its quotes; both come after the
command, where nothing is read.

## 2. Listing the workspace the way Perforce sees it

A Perforce workspace is where the build happens, so walking it indexes the
build. `--p4` takes the list from Perforce instead, and like `--git` the
index remembers that for the root:

```
cindex-rs --local --p4          # the workspace this directory is in, at its root
cindex-rs --p4 DIR              # one directory of a workspace
```

The list is **`p4 have`** -- everything synced from the depot -- **plus the
files opened here** (`p4 fstat -Ro`), which is where a file added and not yet
submitted is to be found. A file opened for delete is in the second list and
not on disk; it drops out when the files are looked at, as any vanished file
does.

What is *not* in it: a file Perforce has never been told about. git lists
untracked files that are not ignored; Perforce has no cheap equivalent --
`p4 status` reconciles the whole workspace against the depot. So a new file
is indexed from the moment it is `p4 add`ed, and not before. `--walk` remains
for anyone who would rather have everything.

### Asking p4 safely

- **`p4 -G`**, which prints each record as a marshalled Python dictionary
  rather than a line of text. It is the one output of p4's that survives a
  path with a space, a `#` or a newline in it. The reader in `src/p4.rs`
  understands exactly what p4 writes (dictionaries of strings and 32-bit
  integers) and refuses anything else rather than guess.
- **No file arguments.** `p4 have` is asked for the whole client and the
  answer narrowed here, and the opened files are asked for by client name
  (`//CLIENT/...`). Nothing of ours is ever written in p4's file syntax,
  where `@`, `#`, `%` and `*` mean something.
- **p4 is told where it is three ways**: as the working directory, as `$PWD`,
  and as `-d`. On Unix p4 believes `$PWD` over the system, and a process
  started by anything other than a shell has a stale one.
- **A warning is not a failure.** "File(s) not on client", for a workspace
  with nothing synced yet, arrives as an error record of severity 2. Only 3
  and above mean the command did not work.

### Paths as p4 spells them

p4 prints local paths under the client's Root *as the client spec spells
it*. The index's roots are canonical. The two differ more often than one
would think:

- a Root that reaches the workspace through a symbolic link;
- macOS, where `/var` and `/tmp` are links into `/private`;
- Windows, where a spec can say `c:\ws` for `C:\ws`, or hold an 8.3 name
  such as `C:\Users\RUNNER~1\...`.

This is observed, not assumed. Asked from the canonical directory, with
`-d` naming it, p4 r24.2 still prints every path under the Root as the spec
spells it: `/tmp/x/link/a.c` for a workspace at `/tmp/x/real`,
`/var/folders/...` on macOS, `c:\users\runner~1\...` on Windows.

So the client root is taken from `p4 info`, made canonical once, and every
path p4 prints has the one exchanged for the other before it is looked for
under the index's root. A client with no single root (`Root: null`, used by
Windows clients spread over several drives) has its paths taken as printed;
`--local --p4` declines it, since there is no root to put an index at, and
`--p4 DIR` works.

## 3. When Perforce cannot answer

A git listing needs nothing but the disk. A Perforce one needs the server,
and the server is out of reach every time the VPN is down or the ticket has
run out.

When git cannot list a root, a refresh somebody runs by hand walks the
directory instead and says so. **For Perforce there is no fallback, attended
or not.** Walking a Perforce workspace "for now" indexes every build product
in it, and the next refresh with the server back throws them all out again.
The index stays as it was, and the message carries p4's own words and the way
out:

```
cindex-rs: /home/me/ws: Perforce could not list the files (Connect to server
failed; check $P4PORT. ...). The index is as it was; `cindex-rs --walk` would
index this directory by walking it instead
```

From a hook that is silent, as everything from a hook is; `cindex-rs --hook
--verbose` says it.

A workspace the server has never heard of is a failure of the same kind.
`p4 info` succeeds for one; it simply reports no root -- and reports the
client's name as `*unknown*` rather than as the name it was asked about. So
the message cannot quote the name, and points at where it comes from instead:
`P4CLIENT`, or the `P4CONFIG` file for that directory.

## The index and `p4 reconcile`

git is told to ignore the index in `info/exclude`, a file nobody commits.
Perforce's ignore file is the user's own and is usually checked in, so
csearch-rs does not edit it. Unmentioned, though, the next `p4 reconcile`
would offer `.csearch-rs-index` to the depot. So the first time a project
index is built in a Perforce-listed root, `cindex-rs` says what to add to
`P4IGNORE`: one line, `.csearch-rs-index*`.

## How it is tested

`tests/p4.rs` uses the real thing: Perforce's own `p4` and `p4d`, an empty
server per test, real workspaces. No daemon is started -- `P4PORT=rsh:p4d -r
ROOT -i` runs a p4d over a pipe for each command -- so there is no port to
pick and nothing to wait for. CI downloads both programs on Linux, Windows
and macOS and fails if they are missing (`CSEARCH_RS_REQUIRE_P4`); elsewhere
the tests skip with a message.

They cover:

- a workspace with a synced file, a file opened for add, a file opened for
  delete and a file never added;
- `--local --p4` typed three directories down, and from outside the
  workspace;
- one index over two workspaces, each named by a `P4CONFIG` file, refreshed
  from inside either;
- another workspace submitting, `p4 sync` bringing it in, and the hook
  picking it up; then a submit from here;
- the hook after a reporting command, doing nothing;
- the server out of reach, and a workspace it has never heard of, attended
  and from a hook;
- a Root spelled through a symbolic link (macOS and Windows spell every
  temporary directory two ways, so there every test is this one);
- the note about `P4IGNORE`: once, and only for a root listed through
  Perforce;
- `p4` wrapped in a real shell, with `p4 sync` typed and nothing else.

### Seeing the guards fail

A test that has only ever passed proves nothing, and these cannot run where
the code was written: Perforce is not installed there. So the guards were
broken on purpose in CI instead. Eleven mutations, on throwaway branches
whose workflow does nothing but install Perforce and run `tests/p4.rs`,
grouped so that no two in a run break the same test:

| Broken on purpose | Tests that then failed |
|---|---|
| a listing that fails falls back to walking | the server out of reach |
| a root listed through Perforce is walked once p4 has answered | the workspace listing |
| files opened here are not asked for | the workspace listing |
| the client root's spelling is not exchanged for the canonical one | the Root through a link -- and on macOS and Windows, every test that lists |
| p4 is not told which directory it is being asked about | two workspaces in one index |
| `--local --p4` takes the working directory for the root | the index at the root; asked from outside |
| `--local --p4` is accepted from outside the workspace | asked from outside |
| a reporting command starts a refresh like any other | the reporting command |
| every p4 command is taken for a reporting one | sync and submit; the reporting command; the wrapped `p4 sync` |
| the `P4IGNORE` note is never printed | the note |
| the `P4IGNORE` note is printed on every build | the note |

In each run the tests not named stayed green. The grouping matters: the
first attempt put "walked once p4 has answered" in the same run as the
spelling mutation, and the second hid the first -- a walk does not use p4's
paths at all.

The parts that need no server -- the marshal reader, what counts as a
failure, where a path lands, the table of reporting commands -- have unit
tests, with fifteen more mutations seen red on the development machine.

## Limits

- **A GUI goes round the wrapper.** P4V users need the scheduled
  `cindex-rs --hook` from the 0.4 note. Each check of a Perforce-listed root
  is a round trip to the server; on a short schedule, `--walk` may be the
  kinder listing.
- **A wrapper refreshes the index covering the directory `p4` was run in.**
  `p4 sync //depot/other/...` typed from somewhere else updates files the
  hook is not looking at.
- **New files wait for `p4 add`**, as above.
- **File names that are not UTF-8** are not found on disk under the name
  p4 reports, and are not indexed. git's listing treats them the same way.
- **No staleness note.** `csearch-rs` says when a git index is behind
  `HEAD` because that costs one small file read. Perforce's equivalent is a
  question for the server, and is not asked on every search.
