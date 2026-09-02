//! cindex — build the trigram index.
//!
//!   cindex [--verbose] [--indexpath FILE] [-j N] [PATH...]
//!   cindex --remove PATH   drop a root from the index and rebuild
//!   cindex --list          show indexed roots
//!   cindex --reset         delete the index
//!
//! With paths, they are added to the set of roots and the whole index is
//! rebuilt (rebuilding is fully parallel, so this is fast). With no paths,
//! the existing roots are re-indexed. A stored root that no longer exists is
//! dropped with a note rather than stopping the build; a root that lies
//! inside another is ignored so nothing is indexed twice.

use anyhow::{bail, Result};
use clap::Parser;
use csearch::paths::default_index_path;
use csearch::read::Index;
use csearch::write::{build_index, resolve_roots, BuildOptions};
use std::path::PathBuf;

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
    /// Print progress and skipped files.
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Index file (default: $CSEARCHINDEX or ~/.csearchindex).
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

fn main() -> Result<()> {
    let args = Args::parse();
    let index_path = args.indexpath.clone().unwrap_or_else(default_index_path);

    if args.list {
        let idx = Index::open(&index_path)?;
        for r in idx.roots() {
            println!("{r}");
        }
        return Ok(());
    }
    if args.reset {
        match std::fs::remove_file(&index_path) {
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
    let plan = resolve_roots(&stored, &args.paths, &args.remove)?;
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
    Ok(())
}
