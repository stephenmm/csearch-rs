//! cindex-rs — build the trigram index.
//!
//!   cindex-rs [--verbose] [--indexpath FILE] [-j N] [PATH...]
//!   cindex-rs --git | --walk     how to list the files (remembered per root)
//!   cindex-rs --local            per-project index at the repository root
//!   cindex-rs --if-changed       rebuild only if a git root has changed
//!   cindex-rs --install-hooks    keep the local index fresh on every git event
//!   cindex-rs --uninstall-hooks  remove those hooks
//!   cindex-rs --remove PATH      drop a root from the index and rebuild
//!   cindex-rs --list             show indexed roots
//!   cindex-rs --reset            delete the index
//!
//! With paths, they are added to the set of roots and the whole index is
//! rebuilt in parallel. With no paths, the existing roots are re-indexed. The
//! index is found by the rule in `csearch::paths::default_index_path`.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use csearch::gitstate;
use csearch::listing::{snapshot, ListOptions, Root, Source};
use csearch::names::CINDEX;
use csearch::paths::{
    default_index_path, find_repo_root, legacy_index_beside, with_upgrade_notes, INDEX_FILE_NAME,
};
use csearch::read::Index;
use csearch::trigram::MAX_FILE_LEN;
use csearch::write::{build_from, plan_roots, BuildOptions, Request};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Env guard: set on the detached child so `--background` does not re-detach
/// forever.
const NO_DETACH: &str = "CSEARCH_RS_NO_DETACH";

/// The git events whose hooks keep the index fresh: a checkout, a merge/pull,
/// a commit, and history rewrites (rebase, amend, filter).
const HOOKS: &[&str] = &["post-checkout", "post-merge", "post-commit", "post-rewrite"];
/// Marks a hook file as ours. It is the project's name rather than a binary's,
/// so hooks installed before the binaries were renamed are still recognised
/// -- and are refreshed, or removed, like any other.
const HOOK_MARKER: &str = "csearch-rs";

#[derive(Parser, Debug)]
#[command(name = CINDEX, version, about = "Build a trigram index for csearch-rs")]
struct Args {
    /// List the paths currently in the index and exit.
    #[arg(long)]
    list: bool,
    /// Delete the index and exit.
    #[arg(long)]
    reset: bool,
    /// Drop this root from the index and rebuild (repeatable).
    #[arg(long, value_name = "PATH")]
    remove: Vec<PathBuf>,
    /// Index the enclosing repository (or the current directory) into a
    /// `.csearch-rs-index` at its root, kept out of git's sight via
    /// info/exclude. A repository is listed through git, as with --git.
    #[arg(long)]
    local: bool,
    /// Take the file list from `git ls-files`, so ignored files are not
    /// indexed. Applies to the paths given, or to every root if none is; the
    /// index remembers it. Roots outside a repository fall back to walking.
    #[arg(long, conflicts_with = "walk")]
    git: bool,
    /// List by walking the directory instead. Applies and is remembered like
    /// --git.
    #[arg(long, alias = "no-git")]
    walk: bool,
    /// Skip the rebuild if no git root has changed since the last one.
    #[arg(long)]
    if_changed: bool,
    /// Do the work in a detached background process and return immediately.
    #[arg(long)]
    background: bool,
    /// Install git hooks that refresh the local index on every git event
    /// (implies --local for the initial build).
    #[arg(long)]
    install_hooks: bool,
    /// Remove the hooks installed by --install-hooks and exit.
    #[arg(long)]
    uninstall_hooks: bool,
    /// Print progress and skipped files.
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Index file (default: $CSEARCH_RS_INDEX, else the nearest
    /// .csearch-rs-index above the working directory, else
    /// ~/.csearch-rs-index).
    #[arg(long)]
    indexpath: Option<PathBuf>,
    /// Worker threads (default: all cores).
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// Source bytes per batch, in MiB (bounds peak memory).
    #[arg(long, default_value_t = 256)]
    batch_mib: u64,
    /// Directories to index.
    paths: Vec<PathBuf>,
}

/// Re-run this same command detached, with stdio to null, and return its spawn
/// result. The child carries `NO_DETACH` so it runs the work instead of
/// detaching again. Called only for `--background`.
fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe().context("locating this executable")?;
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1)) // same arguments, verbatim
        .env(NO_DETACH, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NO_WINDOW: no console window, own session.
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    // On Unix the child is detached enough for our purpose simply by being a
    // separate process we never wait on, with its stdio redirected to null:
    // the parent returns in milliseconds and the child keeps running. (A new
    // session via process_group would need Rust 1.77; the MSRV here is 1.75.)
    cmd.spawn().context("spawning the background process")?;
    Ok(())
}

/// The repository's hooks directory, honouring `core.hooksPath`.
fn hooks_dir(root: &Path) -> Option<PathBuf> {
    let root_s = root.to_str()?;
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root_s)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(hp) = run(&["config", "--get", "core.hooksPath"]) {
        let p = PathBuf::from(hp);
        return Some(if p.is_absolute() { p } else { root.join(p) });
    }
    let p = PathBuf::from(run(&["rev-parse", "--git-path", "hooks"])?);
    Some(if p.is_absolute() { p } else { root.join(p) })
}

fn hook_script(exe: &Path) -> String {
    // Forward slashes so the path is safe inside a POSIX sh string on Windows
    // too, where git runs hooks under its bundled shell.
    let exe = exe.to_string_lossy().replace('\\', "/");
    format!(
        "#!/bin/sh\n\
         # {HOOK_MARKER} hook: keep the trigram index fresh (safe to delete)\n\
         exec \"{exe}\" --local --if-changed --background\n"
    )
}

/// Write the four hooks into `root`'s hooks directory. An existing hook that is
/// not ours is left untouched (and reported); one of ours is refreshed.
fn install_hooks(root: &Path) -> Result<()> {
    let dir =
        hooks_dir(root).with_context(|| format!("{}: not a git repository", root.display()))?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = std::env::current_exe().context("locating this executable")?;
    let script = hook_script(&exe);
    for name in HOOKS {
        let path = dir.join(name);
        if let Ok(existing) = fs::read_to_string(&path) {
            if !existing.contains(HOOK_MARKER) {
                eprintln!(
                    "{CINDEX}: {} already exists and is not ours -- leaving it alone",
                    path.display()
                );
                continue;
            }
        }
        fs::write(&path, &script).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .with_context(|| format!("chmod {}", path.display()))?;
        }
    }
    eprintln!("{CINDEX}: installed refresh hooks in {}", dir.display());
    Ok(())
}

fn uninstall_hooks(root: &Path) -> Result<()> {
    let dir =
        hooks_dir(root).with_context(|| format!("{}: not a git repository", root.display()))?;
    let mut removed = 0;
    for name in HOOKS {
        let path = dir.join(name);
        match fs::read_to_string(&path) {
            Ok(text) if text.contains(HOOK_MARKER) => {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                removed += 1;
            }
            _ => {}
        }
    }
    eprintln!(
        "{CINDEX}: removed {removed} refresh hook(s) from {}",
        dir.display()
    );
    Ok(())
}

/// The repository root for --local / --install-hooks: the enclosing repo, or
/// the working directory when there is none.
fn local_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading the working directory")?;
    Ok(find_repo_root(&cwd).unwrap_or(cwd))
}

fn main() -> Result<()> {
    let args = Args::parse();

    // --background: hand off to a detached copy and return at once, so a git
    // hook never blocks. The child (NO_DETACH set) falls through and works.
    if args.background && std::env::var_os(NO_DETACH).is_none() {
        return spawn_detached();
    }

    if args.uninstall_hooks {
        return uninstall_hooks(&local_root()?);
    }

    // --local and --install-hooks both anchor on the repository root.
    let want_local = args.local || args.install_hooks;
    let root = if want_local {
        Some(local_root()?)
    } else {
        None
    };
    let index_path = match (&args.indexpath, &root) {
        (Some(p), _) => p.clone(),
        (None, Some(r)) => r.join(INDEX_FILE_NAME),
        (None, None) => default_index_path(),
    };

    if args.list {
        let idx = Index::open(&index_path)?;
        for r in idx.roots() {
            // Plain --list is paths only, as it always was; --verbose adds how
            // each is listed, first, so a path with a tab in it stays whole.
            if args.verbose {
                println!("{}\t{}", r.source, r.path);
            } else {
                println!("{}", r.path);
            }
        }
        return Ok(());
    }
    if args.reset {
        for p in [index_path.clone(), gitstate::stamp_path(&index_path)] {
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("removing {}: {e}", p.display()),
            }
        }
        return Ok(());
    }

    if args.install_hooks {
        install_hooks(root.as_ref().expect("install-hooks sets root"))?;
        // fall through to build the initial index (install-hooks implies --local)
    }

    // Exclude the index and its stamp from git BEFORE building, so the stamp's
    // `git status` fingerprint never counts our own files -- otherwise the
    // first stamp would see them and a later --if-changed never matches.
    if let Some(r) = &root {
        if let Some(exclude) = exclude_index_from_git(r)? {
            eprintln!("{CINDEX}: added {INDEX_FILE_NAME} to {}", exclude.display());
        }
    }

    if let Some(n) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }

    let stored = if index_path.exists() {
        stored_roots(&index_path)?
    } else {
        Vec::new()
    };
    let listing = match (args.git, args.walk) {
        (true, _) => Some(Source::Git),
        (_, true) => Some(Source::Walk),
        _ => None,
    };
    let plan = plan_roots(&Request {
        stored: &stored,
        add: &args.paths,
        local: root.as_deref(),
        remove: &args.remove,
        listing,
    })?;
    for note in &plan.notes {
        eprintln!("{CINDEX}: {note}");
    }
    if plan.roots.is_empty() {
        if stored.is_empty() {
            // The one place an upgrade from 0.2 shows up as a puzzle: the
            // index is "gone" because it is still under the original's name.
            return Err(with_upgrade_notes(anyhow!(
                "no paths given and no existing index at {}",
                index_path.display()
            )));
        }
        bail!("no roots left to index; use --reset to delete the index");
    }
    let root_paths =
        |roots: &[Root]| -> Vec<String> { roots.iter().map(|r| r.path.clone()).collect() };

    // --if-changed: if the index already covers exactly these roots, listed
    // the same way, and no git root has changed, there is nothing to do.
    // Conservative -- any doubt rebuilds. This is what makes the hooks cheap
    // to fire on every event.
    if args.if_changed
        && plan.roots == stored
        && gitstate::is_current(&index_path, &root_paths(&plan.roots))
    {
        if args.verbose {
            eprintln!("{CINDEX}: index is up to date, nothing to do");
        }
        return Ok(());
    }

    let started = Instant::now();
    let snap = snapshot(
        &plan.roots,
        &ListOptions {
            max_file_bytes: MAX_FILE_LEN,
            strict: false,
        },
    )?;
    if args.verbose {
        eprintln!(
            "{CINDEX}: {} files found in {:.2?}",
            snap.files.len(),
            started.elapsed()
        );
    }
    let opts = BuildOptions {
        verbose: args.verbose,
        batch_bytes: args.batch_mib << 20,
        ..Default::default()
    };
    let stats = build_from(&snap, &index_path, &opts)?;
    eprintln!(
        "{CINDEX}: {} files indexed ({} skipped), {} trigrams, {} posting entries, index {} bytes",
        stats.files_indexed,
        stats.files_skipped,
        stats.distinct_trigrams,
        stats.posting_entries,
        stats.index_bytes
    );

    // Record git state so the next --if-changed can skip and csearch-rs can
    // warn.
    gitstate::write_stamp(&index_path, &root_paths(&snap.roots));

    if root.is_some() {
        eprintln!("{CINDEX}: local index at {}", index_path.display());
    }
    // An index csearch-rs 0.2 left here under the original's name is dead
    // weight now. Say so; never delete it -- that name is not ours any more.
    if let Some(old) = legacy_index_beside(&index_path) {
        eprintln!(
            "{CINDEX}: note: {} is a csearch-rs index from before 0.3 and is no longer \
             used -- delete it (and its .meta) when convenient",
            old.display()
        );
    }
    Ok(())
}

/// The roots an index holds, each with the source it is listed from.
///
/// A source this version does not know -- written by a later one -- is an
/// error here and only here: re-indexing would have to guess how to list that
/// root, and guessing wrong silently changes what the index contains.
fn stored_roots(index_path: &Path) -> Result<Vec<Root>> {
    Index::open(index_path)?
        .roots()
        .into_iter()
        .map(|r| {
            let source = Source::from_tag(&r.source).ok_or_else(|| {
                anyhow!(
                    "{}: the root {} is listed by `{}`, which this version of {CINDEX} does \
                     not know -- upgrade it, or run `{CINDEX} --reset` and index again",
                    index_path.display(),
                    r.path,
                    r.source
                )
            })?;
            Ok(Root {
                path: r.path,
                source,
            })
        })
        .collect()
}

/// Ask git where `info/exclude` is (correct for worktrees), falling back to
/// `.git/info/exclude`.
fn git_exclude_path(root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(if p.is_absolute() { p } else { root.join(p) })
}

/// Ensure the index and its stamp sidecar are in the repository's
/// `info/exclude`, so neither shows in `git status` and no tracked file is
/// touched. Idempotent; returns the path if any line was added.
fn exclude_index_from_git(root: &Path) -> Result<Option<PathBuf>> {
    let exclude = match git_exclude_path(root) {
        Some(p) => p,
        None => {
            let dot_git = root.join(".git");
            if !dot_git.is_dir() {
                return Ok(None);
            }
            dot_git.join("info").join("exclude")
        }
    };
    let meta = format!("{INDEX_FILE_NAME}.meta");
    let wanted = [INDEX_FILE_NAME, meta.as_str()];
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|n| !existing.lines().any(|l| l.trim() == *n))
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }
    if let Some(parent) = exclude.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for name in missing {
        text.push_str(name);
        text.push('\n');
    }
    fs::write(&exclude, text).with_context(|| format!("writing {}", exclude.display()))?;
    Ok(Some(exclude))
}
