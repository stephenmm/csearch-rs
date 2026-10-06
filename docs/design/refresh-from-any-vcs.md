# Refreshing the index from any version-control system

*New in 0.4. Builds on [per-project indexes](per-repo-index.md).*

## Problem

Automatic refresh was built for git and nothing else. Three things tied it
there:

- **Change detection asked git.** `--if-changed` compared `HEAD` and
  `git status`, so a root that was not a git repository could never be shown
  to be unchanged, and was rebuilt every time.
- **The hook command was git-shaped.** Hooks ran `cindex-rs --local
  --if-changed --background`, and `--local` means "find the enclosing `.git`".
- **How a root was listed was not remembered.** `--git` applied to one run. A
  later plain `cindex-rs` on an index built from `git ls-files` walked the
  directory instead, and every ignored file arrived in the index.

Two more were waiting behind those, and matter more once anything other than
git's four hooks can start a refresh:

- **Two refreshes at once shared a temporary file**, and whichever finished
  second installed what the two had made of it. A `git pull --rebase` already
  fires several hooks in a row; a wrapper round a command-line client fires on
  every command.
- **The stamp was taken after the build.** A file that changed while the build
  was running was recorded as indexed.

## What a refresh needs to know

Nothing about version control, as it turns out:

1. *Which index?* The one that covers this directory -- the ordinary rule.
2. *Which files?* However that index's roots were listed when it was built.
3. *Did they change?* Compare what is on disk with what was indexed.

So a version-control system has one thing to do: say "something may have
happened", by running a command. The command is the same for all of them.

## `cindex-rs --hook`

Refresh the index that covers the working directory, if its files have
changed. Made to be run by anything, any number of times:

- **Silent, and always exit 0.** A hook must neither break nor decorate the
  command that set it off. Whatever goes wrong, the caller does not hear of
  it.
- **Detached.** The work goes to a process of its own and the hook returns.
  That process works from the index's directory, not the one the hook fired
  in: on Windows a running process pins its working directory, and a refresh
  that may be waiting its turn has no claim on a build directory somebody is
  about to delete.
- **A no-op where it has no business.** No index, or an index none of whose
  roots contains the working directory: nothing happens, and nothing is
  created. (`--local --hook` does create one, as `--local` always has. That
  is what git's hooks run, so an index removed by `git clean -fdx` is back
  after the next checkout.)
- **Cheap when nothing changed.** See *Change detection*.
- **Safe to fire in bursts.** See *Concurrency*.
- **Never improvises.** If a root cannot be listed the way the index records
  -- git missing from the hook's `PATH`, say -- the index is left as it is. A
  refresh somebody runs by hand walks the directory instead and says so; a
  hook that did the same would swap in a different set of files with nobody
  watching, and the next one would swap them back.

`cindex-rs --hook --verbose` runs in the foreground and says what it decided
and why. That is how to find out why an index is not being refreshed.

### Wiring it up

| System | How |
|---|---|
| git | `cindex-rs --install-hooks`. Real hooks, so GUI clients and IDEs set them off too. |
| Mercurial | Two lines under `[hooks]` in `.hg/hgrc`: `update.csearch-rs = cindex-rs --hook` and `commit.csearch-rs = cindex-rs --hook`. |
| Anything typed into a shell (Subversion, Perforce, Jujutsu, Fossil, a build tool) | `cindex-rs --print-hook TOOL`: a wrapper for that command. |
| Anything at all | Run `cindex-rs --hook` in the tree from cron, Task Scheduler, launchd, a file watcher, an editor's save hook. |

git's hooks and the Mercurial recipe are each run for real by
`tests/hooks.rs`: a checkout, a commit, and for git a `git clean -fdx`
followed by another checkout.

## Wrappers: `--print-hook TOOL`

Subversion, Perforce and most other systems have no client-side hooks.
Perforce's triggers run on the server; its client has command aliases, but an
alias can only expand to other `p4` commands. For these the one place to
stand is the shell. `cindex-rs --print-hook TOOL --shell SHELL` prints a
function called `TOOL` that runs the real `TOOL` and then `cindex-rs --hook`:

```sh
eval "$(cindex-rs --print-hook svn --shell bash)"      # sh, bash, zsh, ksh, dash
cindex-rs --print-hook svn --shell fish | source
eval "`cindex-rs --print-hook svn --shell tcsh`"       # csh, tcsh
cindex-rs --print-hook svn --shell powershell | Out-String | Invoke-Expression
```

Without `--shell` the shell is taken from `$SHELL`; where that is unset (plain
Windows) `--shell` has to be given, rather than guessed.

The wrapper is held to one thing: the wrapped command behaves as the command
does. In every shell `tests/hooks.rs` can find (CI requires a named set on
each platform; a missing one is a failure there, not a skip) the same script
is run twice, once with the command as it is and once with it wrapped, and
what the two print is compared. The script covers:

- exit status, of a command that works and of one that fails;
- an argument with a space in it;
- a redirection written after the command;
- the command run with no arguments at all;
- output going down a pipe, and input coming up one;
- an argument that must be evaluated once -- ``-m "`date`"`` runs `date`
  once;
- stderr, where neither run may print anything.

Comparing with the unwrapped run, rather than with what a shell is supposed
to do, is not fussiness. The first version of the test expected a piped `x`
to arrive as `x` and a line ending. On one Windows machine it does. On
another, Windows PowerShell puts a byte-order mark in front of it -- with the
wrapper or without.

The wrapper passes the command line on (`--hook --after TOOL -- ARGS`) so
that a tool whose read-only commands are known can skip the refresh for them.
None is known in 0.4.

### Per-shell notes

- **csh and tcsh have no functions**, so the wrapper is an alias, and an
  alias is text: `\!*` is every word typed after the command, redirections
  included, re-read when the alias runs. The command takes the words as
  typed. The hook takes them quoted (`\!*:q`). Written twice unquoted,
  `p4 sync > log` handed the hook a second `>` -- "Ambiguous output
  redirect", and csh then runs none of the line -- and a backquoted argument
  ran twice.
- **PowerShell sets `$LASTEXITCODE`** to the command's status, as it would be
  without the wrapper. `$?` is another matter, and the one difference the
  test above records rather than forbids: false after a command that failed,
  it is true after a function whatever happened inside. So
  `p4 sync; if ($?) { ... }` does not see a failure. Test `$LASTEXITCODE`.
- **PowerShell pipes text.** What is piped into a wrapped command goes
  through PowerShell's pipeline, as it does for any function, and is encoded
  again on the way out. That is what every PowerShell before 7.4 did to all
  piped input anyway. From 7.4, bytes piped straight from one program into
  another arrive untouched -- but not if the second one is wrapped.
- **`cmd.exe` gets no wrapper.** A doskey macro has to mention `$*` twice,
  and `$*` is everything after the command name: `p4 sync && build` would
  run `build` twice. A batch-file shim mangles `%` in arguments. Neither is
  fit to hand out; PowerShell, Git Bash and a scheduled task all work on
  Windows.
- **An alias of the same name gets there first.** If `p4` is already an
  alias, the shell expands it wherever `p4` is the first word, and the first
  word of the wrapper's definition is `p4`. With the usual kind of alias, one
  that adds arguments, the definition becomes a syntax error, reported at the
  line that evaluates it (bash, sh and dash, tried). Drop the alias, or the
  wrapper.

### What a wrapper cannot see

Only what is typed in that shell. A GUI client or an IDE plugin goes round
it. For those the scheduled job is the mechanism, and with a tree that has
not changed it costs one listing and no file reads.

A wrapper refreshes the index that covers the directory the command was run
in. A command run from somewhere else, naming files by an absolute path,
changes files the hook is not looking at.

## Change detection: a fingerprint of the tree

`--if-changed`, which `--hook` implies, no longer asks git anything. When an
index is built, the listing is hashed -- the roots and how each is listed,
then every file's path, size and modification time -- and the hash goes into
the stamp beside the index (`<index>.meta`). A refresh lists the files again
and compares.

That is the listing a build has to do anyway, so an unchanged tree costs a
directory walk (or one `git ls-files`) plus a `stat` per file, and no file is
opened.

It is also more accurate than what it replaces:

| | git stamp (0.3) | fingerprint (0.4) |
|---|---|---|
| a second edit to a file that was already modified | **missed**: `git status` prints the same thing | caught |
| a commit that changes no file | rebuilt | skipped |
| a root that is not a git repository | always rebuilt | skipped when unchanged |
| needs git | yes | no |

The stamp records the listing the build *started* from, and when that listing
was taken. A file modified while the build runs no longer matches, and the
next refresh picks it up.

### Timestamps that cannot tell two writes apart

A size-and-time check cannot see an edit that keeps the file's size and lands
in the same timestamp tick as the write before it. On NTFS, ext4 and APFS a
tick is far shorter than anything a person or a tool does twice. On FAT and
exFAT it is two seconds; on HFS+ and ext3, one. Measured on this project's
own development disk: exFAT as Windows mounts it puts every modification time
on an even second, rounded *up*.

So a root whose files *all* carry whole-second modification times is taken to
be on such a file system, and if any of its files was modified within two
seconds of the stamp, the stamp is not believed and the index is rebuilt. That
costs one extra rebuild after a change on those file systems, and nothing
anywhere else. It is the rule git applies to its own index, for the same
reason.

## The index remembers how each root is listed

Index format 2 stores a listing source beside every root: `walk` or `git`. A
refresh uses what is stored; a listing flag changes it.

- `cindex-rs --git PATH`: list PATH through git from now on.
- `cindex-rs --walk PATH`: walk it from now on (`--no-git` still works).
- `cindex-rs PATH`: keep whatever PATH already uses; a new root is walked.
- `cindex-rs --git` or `--walk` with no path: every root.
- `cindex-rs --local`: git inside a repository, a walk elsewhere, unless the
  root already has a source.

`cindex-rs --list --verbose` shows the source of each root. A source this
version does not know, written by a later one, can still be searched;
re-indexing it is refused rather than guessed at.

An index in another format (format 1, from 0.3) is reported as such, with the
command that rebuilds it. `--local` does not need telling: it knows the root
without reading the index, so it builds the index again -- and since
`--local --hook` is what git's hooks run, a project index gets over a format
change at the next git event. A stamp that says no file has changed is not
allowed to talk it out of that; the stamp's format need not change when the
index's does. The shared index is different: its roots are in the file that
cannot be read, so it is left for `--reset`. Nor is anything replaced that
does not begin with this project's magic. A file under the index's name that
is damaged, or was never an index, is not ours to overwrite.

## Concurrency

A refresh holds `<index>.lock` from before it lists the files until after it
has written the stamp. The lock is the kernel's -- `flock` on Unix, an open
that refuses other writers on Windows -- so it goes when the process goes,
however the process ends. There is no such thing as a stale one.

`--hook` goes further, because hooks arrive in bursts. If a refresh is
running and another is already waiting behind it, a third exits at once: the
one that is waiting has not listed any files yet, so whatever just happened
will be in what it sees. However many events arrive, there are at most two
processes for an index, one working and one waiting.

A `cindex-rs` that somebody ran always waits its turn instead. It may be
adding or removing a root, and that must not be dropped.

Everything that sits beside the index is now `<index>.<suffix>`: `.meta`,
`.lock`, `.queue`, and during a build `.tmp` and `.old`. (`.tmp` and `.old`
used to *replace* the extension, so indexes named `a.one` and `a.two` shared
a temporary file.) git is told to ignore `.csearch-rs-index*`.

## Found on the way

Each of these was already wrong before this work, and each has a test that
failed first.

- **On Windows, `--background` did not let go of its caller.** Windows hands
  a child process every inheritable handle its parent holds, not only the
  three it is told to use. A caller that reads the indexer's output through a
  pipe -- an IDE running git, `$out = git pull` in PowerShell -- waited for
  the detached process to exit, which is to say for the whole rebuild: 9.4 s
  in one measurement, 20 ms once the handles were kept back.
- **A hook's environment redirected git.** git tells a hook which repository
  it is running for through `GIT_DIR`, `GIT_INDEX_FILE` and a dozen more
  variables, and they override `-C`. A refresh started from a hook in one
  repository and asked about another root got its answer for the hook's
  repository. Every git command now drops the variables git lists as naming a
  repository.
- **Every search started a git process** to see whether `HEAD` had moved:
  38-61 ms of a search that otherwise takes about 7. `HEAD` is now read from
  the repository's files -- loose refs, `packed-refs`, linked work trees. A
  layout it cannot read exactly (reftable) gives no answer rather than a
  wrong one, and the note is simply not printed.
- **A commit that touched no file left the "index is behind HEAD" note on.**
  A refresh that finds nothing changed now records where `HEAD` is.

## What stays git-specific

- `--install-hooks` and `--uninstall-hooks`, which write into a hooks
  directory.
- The search-time note that `HEAD` has moved since the index was built.

## Not done

- A record of where project indexes are, so that a plain `--hook` could put
  back one that was deleted without `--local`'s knowledge of where the root
  is.
- Refresh on search: `csearch-rs` starting a background refresh itself. It
  would cover GUI-driven workflows with no setup at all, for the price of a
  listing per search. Worth a flag, not a default.
- A file-system watcher.
- Unaliasing in the wrappers. Removing somebody's alias in order to define a
  function over it is not this tool's call to make.
