//! cindex — build the trigram index.
//!
//!   cindex [--verbose] [--indexpath FILE] [-j N] [PATH...]
//!   cindex --list        show indexed roots
//!   cindex --reset       delete the index
//!
//! With paths, they are added to the set of roots and the whole index is
//! rebuilt (rebuilding is fully parallel, so this is fast). With no paths,
//! the existing roots are re-indexed.

use anyhow::{bail, Result};
use clap::Parser;
use csearch::paths::default_index_path;
use csearch::read::Index;
use csearch::write::{build_index, BuildOptions};
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
        rayon::ThreadPoolBuilder::new().num_threads(n).build_global()?;
    }

    // Merge requested paths with existing roots.
    let mut roots: Vec<PathBuf> = args.paths.clone();
    if let Ok(idx) = Index::open(&index_path) {
        roots.extend(idx.roots().into_iter().map(PathBuf::from));
    }
    if roots.is_empty() {
        bail!("no paths given and no existing index at {}", index_path.display());
    }
    for r in &roots {
        if !r.is_dir() {
            bail!("{}: not a directory", r.display());
        }
    }

    let opts = BuildOptions { verbose: args.verbose, batch_bytes: args.batch_mib << 20 };
    let stats = build_index(&roots, &index_path, &opts)?;
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
