//! cindex — build the trigram index.
//!
//!   cindex [--verbose] [--indexpath FILE] [-j N] [PATH...]
//!   cindex --local         per-project index at the repository root
//!   cindex --remove PATH   drop a root from the index and rebuild
//!   cindex --list          show indexed roots
//!   cindex --reset         delete the index
//!
//! With paths, they are added to the set of roots and the whole index is
//! rebuilt (rebuilding is fully parallel, so this is fast). With no paths,
//! the existing roots are re-indexed. A stored root that no longer exists is
//! dropped with a note rather than stopping the build; a root that lies
//! inside another is ignored so nothing is indexed twice.
//!
//! The index is found by the rule in `csearch::paths::default_index_path`:
//! `--indexpath`, then `$CSEARCHINDEX`, then the nearest `.csearchindex`
//! above the working directory, then `~/.csearchindex`.

use anyhow::{bail, Context, Result};
use clap::Parser;
use csearch::paths::{default_index_path, find_repo_root, INDEX_FILE_NAME};
use csearch::read::Index;
use csearch::write::{build_index, resolve_roots, BuildOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// .git/info/exclude. Implies --git.
    #[arg(long)]
    local: bool,
    /// Take the file list from `git ls-files`, so ignored files are not
    /// indexed. Roots outside a repository fall back to walking.
    #[arg(long)]
    git: bool,
    /// With --local, walk the directory instead of asking git.
    #[arg(long, requires = "local")]
    no_git: bool,
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

/// Ask git where `info/exclude` is for the repository at `root`. This is the
/// reliable answer for worktrees, where `.git` is a file and the exclude list
/// lives in the shared directory.
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

/// Make sure `.csearchindex` is listed in the repository's `info/exclude`, so
/// `git status` never shows the index and nothing commits it. That file is
/// local to the clone, unlike `.gitignore`, so no tracked file is touched.
/// Returns the path if a line was added, `None` if nothing needed doing.
fn exclude_index_from_git(root: &Path) -> Result<Option<PathBuf>> {
    let exclude = match git_exclude_path(root) {
        Some(p) => p,
        None => {
            let dot_git = root.join(".git");
            if !dot_git.is_dir() {
                return Ok(None); // not a repository, nothing to exclude from
            }
            dot_git.join("info").join("exclude")
        }
    };
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == INDEX_FILE_NAME) {
        return Ok(None);
    }
    if let Some(parent) = exclude.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(INDEX_FILE_NAME);
    text.push('\n');
    fs::write(&exclude, text).with_context(|| format!("writing {}", exclude.display()))?;
    Ok(Some(exclude))
}

fn main() -> Result<()> {
    let args = Args::parse();

    // --local: the index lives at the repository root and covers it.
    let local_root: Option<PathBuf> = if args.local {
        let cwd = std::env::current_dir().context("reading the working directory")?;
        Some(find_repo_root(&cwd).unwrap_or(cwd))
    } else {
        None
    };
    let index_path = match (&args.indexpath, &local_root) {
        (Some(p), _) => p.clone(),
        (None, Some(root)) => root.join(INDEX_FILE_NAME),
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
        match fs::remove_file(&index_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("removing {}: {e}", index_path.display()),
        }
        return Ok(());
    }

    if let Some(n) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }

    // Roots already in the index. No index yet is simply an empty set; a
    // damaged one is an error, so it is never silently rebuilt over.
    let stored: Vec<String> = if index_path.exists() {
        Index::open(&index_path)?.roots()
    } else {
        Vec::new()
    };
    let mut add = args.paths.clone();
    if let Some(root) = &local_root {
        add.push(root.clone()); // duplicates collapse inside build_index
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

    let opts = BuildOptions {
        verbose: args.verbose,
        batch_bytes: args.batch_mib << 20,
        git: args.git || (args.local && !args.no_git),
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

    if let Some(root) = &local_root {
        eprintln!("cindex: local index at {}", index_path.display());
        if let Some(exclude) = exclude_index_from_git(root)? {
            eprintln!("cindex: added {INDEX_FILE_NAME} to {}", exclude.display());
        }
    }
    Ok(())
}
