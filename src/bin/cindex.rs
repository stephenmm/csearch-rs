//! cindex — build the trigram index.
//!
//!   cindex [--verbose] [--indexpath FILE] [-j N] [PATH...]
//!   cindex --local            per-project index at the repository root
//!   cindex --if-changed       rebuild only if a git root has changed
//!   cindex --install-hooks    keep the local index fresh on every git event
//!   cindex --uninstall-hooks  remove those hooks
//!   cindex --remove PATH      drop a root from the index and rebuild
//!   cindex --list             show indexed roots
//!   cindex --reset            delete the index
//!
//! With paths, they are added to the set of roots and the whole index is
//! rebuilt in parallel. With no paths, the existing roots are re-indexed. The
//! index is found by the rule in `csearch::paths::default_index_path`.

use anyhow::{bail, Context, Result};
use clap::Parser;
use csearch::gitstate;
use csearch::paths::{default_index_path, find_repo_root, INDEX_FILE_NAME};
use csearch::read::Index;
use csearch::write::{build_index, resolve_roots, BuildOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Env guard: set on the detached child so `--background` does not re-detach
/// forever.
const NO_DETACH: &str = "CSEARCH_NO_DETACH";

/// The git events whose hooks keep the index fresh: a checkout, a merge/pull,
/// a commit, and history rewrites (rebase, amend, filter).
const HOOKS: &[&str] = &["post-checkout", "post-merge", "post-commit", "post-rewrite"];
const HOOK_MARKER: &str = "csearch-rs";

#[derive(Parser, Debug)]
#[command(name = "cindex", version, about = "Build a trigram index for csearch")]
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
    /// `.csearchindex` at its root, kept out of git's sight via
    /// info/exclude. Implies --git.
    #[arg(long)]
    local: bool,
    /// Take the file list from `git ls-files`, so ignored files are not
    /// indexed. Roots outside a repository fall back to walking.
    #[arg(long)]
    git: bool,
    /// With --local, walk the directory instead of asking git.
    #[arg(long, requires = "local")]
    no_git: bool,
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
    /// Index file (default: $CSEARCHINDEX, else the nearest .csearchindex
    /// above the working directory, else ~/.csearchindex).
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
                    "cindex: {} already exists and is not ours -- leaving it alone",
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
    eprintln!("cindex: installed refresh hooks in {}", dir.display());
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
        "cindex: removed {removed} refresh hook(s) from {}",
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
            println!("{r}");
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
            eprintln!("cindex: added {INDEX_FILE_NAME} to {}", exclude.display());
        }
    }

    if let Some(n) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }

    let stored: Vec<String> = if index_path.exists() {
        Index::open(&index_path)?.roots()
    } else {
        Vec::new()
    };
    let mut add = args.paths.clone();
    if let Some(r) = &root {
        add.push(r.clone()); // duplicates collapse inside build_index
    }
    let plan = resolve_roots(&stored, &add, &args.remove)?;
    for note in &plan.notes {
        eprintln!("cindex: {note}");
    }
    if plan.roots.is_empty() {
        if stored.is_empty() {
            bail!(
                "no paths given and no existing index at {}",
                index_path.display()
            );
        }
        bail!("no roots left to index; use --reset to delete the index");
    }

    // --if-changed: if the index already covers exactly these roots and no git
    // root has changed, there is nothing to do. Conservative -- any doubt
    // rebuilds. This is what makes the hooks cheap to fire on every event.
    if args.if_changed && index_path.exists() {
        let target = planned_root_strings(&plan.roots);
        if let Some(target) = target {
            if gitstate::is_current(&index_path, &target) {
                if args.verbose {
                    eprintln!("cindex: index is up to date, nothing to do");
                }
                return Ok(());
            }
        }
    }

    let opts = BuildOptions {
        verbose: args.verbose,
        batch_bytes: args.batch_mib << 20,
        git: args.git || (want_local && !args.no_git),
        ..Default::default()
    };
    let stats = build_index(&plan.roots, &index_path, &opts)?;
    eprintln!(
        "cindex: {} files indexed ({} skipped), {} trigrams, {} posting entries, index {} bytes",
        stats.files_indexed,
        stats.files_skipped,
        stats.distinct_trigrams,
        stats.posting_entries,
        stats.index_bytes
    );

    // Record git state so the next --if-changed can skip and csearch can warn.
    // Use the roots as stored, so the strings match what a later run sees.
    let stored_now = Index::open(&index_path)?.roots();
    gitstate::write_stamp(&index_path, &stored_now);

    if root.is_some() {
        eprintln!("cindex: local index at {}", index_path.display());
    }
    Ok(())
}

/// The canonical, containment-collapsed strings for the planned roots -- the
/// same set `build_index` would store -- or `None` if any cannot be
/// canonicalised (in which case --if-changed should just rebuild).
fn planned_root_strings(roots: &[PathBuf]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(roots.len());
    for r in roots {
        out.push(csearch::paths::canonical_string(r).ok()?);
    }
    Some(csearch::write::collapse_roots(out).0)
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
