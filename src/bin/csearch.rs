//! csearch — regexp search over a cindex index.
//!
//!   csearch [-c] [-f FILEREGEXP] [-h] [-i] [-l] [-n] [--brute] [--verbose] REGEXP

use anyhow::{Context, Result};
use clap::{ArgAction, Parser};
use csearch::paths::default_index_path;
use csearch::read::Index;
use csearch::regexp;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(name = "csearch", version, about = "Search code with a trigram index", disable_help_flag = true)]
struct Args {
    #[arg(long = "help", action = ArgAction::Help, help = "Print help")]
    help: Option<bool>,
    /// Print only a count of matching lines per file.
    #[arg(short = 'c')]
    count: bool,
    /// Only search files whose names match this regexp.
    #[arg(short = 'f', value_name = "FILEREGEXP")]
    file_regexp: Option<String>,
    /// Omit file names from output.
    #[arg(short = 'h')]
    no_names: bool,
    /// Case-insensitive match.
    #[arg(short = 'i')]
    ignore_case: bool,
    /// Print only the names of matching files.
    #[arg(short = 'l')]
    list_files: bool,
    /// Print line numbers.
    #[arg(short = 'n')]
    line_numbers: bool,
    /// Skip the index and grep every file (for benchmarking).
    #[arg(long)]
    brute: bool,
    /// Print the trigram query and timing to stderr.
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Index file (default: $CSEARCHINDEX or ~/.csearchindex).
    #[arg(long)]
    indexpath: Option<PathBuf>,
    /// Worker threads (default: all cores).
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// Regular expression to search for.
    regexp: String,
}

struct Grep {
    re: Regex,
    count: bool,
    list: bool,
    names: bool,
    line_numbers: bool,
}

impl Grep {
    /// Grep one file; returns its output (empty if no match) and match count.
    fn file(&self, name: &str) -> (Vec<u8>, usize) {
        let data = match std::fs::read(name) {
            Ok(d) => d,
            Err(_) => return (Vec::new(), 0),
        };
        let mut out = Vec::new();
        let mut matches = 0usize;
        let mut pos = 0usize;
        let mut line_no = 1usize;
        let mut line_counted_to = 0usize;
        while pos <= data.len() {
            let Some(m) = self.re.find_at(&data, pos) else { break };
            let start = memchr::memrchr(b'\n', &data[..m.start()]).map_or(0, |i| i + 1);
            let end = memchr::memchr(b'\n', &data[m.end()..]).map_or(data.len(), |i| m.end() + i);
            matches += 1;
            if self.list {
                break;
            }
            if !self.count {
                if self.names {
                    out.extend_from_slice(name.as_bytes());
                    out.push(b':');
                }
                if self.line_numbers {
                    line_no += memchr::memchr_iter(b'\n', &data[line_counted_to..start]).count();
                    line_counted_to = start;
                    out.extend_from_slice(line_no.to_string().as_bytes());
                    out.push(b':');
                }
                out.extend_from_slice(&data[start..end]);
                out.push(b'\n');
            }
            pos = end + 1;
        }
        if matches > 0 {
            if self.list {
                out.extend_from_slice(name.as_bytes());
                out.push(b'\n');
            } else if self.count {
                if self.names {
                    out.extend_from_slice(name.as_bytes());
                    out.push(b':');
                }
                out.extend_from_slice(matches.to_string().as_bytes());
                out.push(b'\n');
            }
        }
        (out, matches)
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let t0 = Instant::now();
    if let Some(n) = args.threads {
        rayon::ThreadPoolBuilder::new().num_threads(n).build_global()?;
    }

    let re = RegexBuilder::new(&args.regexp)
        .case_insensitive(args.ignore_case)
        .multi_line(true)
        .build()
        .with_context(|| format!("bad regexp {:?}", args.regexp))?;
    let file_re = match &args.file_regexp {
        Some(f) => Some(regex::Regex::new(f).with_context(|| format!("bad -f regexp {f:?}"))?),
        None => None,
    };

    let index_path = args.indexpath.clone().unwrap_or_else(default_index_path);
    let idx = Index::open(&index_path)?;

    let candidates: Vec<u32> = if args.brute {
        (0..idx.num_files()).collect()
    } else {
        let hir = regexp::parse(&args.regexp, args.ignore_case)?;
        let q = regexp::query_for(&hir);
        if args.verbose {
            eprintln!("query: {q}");
        }
        idx.posting_query(&q)
    };
    let candidates: Vec<&str> = candidates
        .into_iter()
        .map(|id| idx.name(id))
        .filter(|n| file_re.as_ref().map_or(true, |r| r.is_match(n)))
        .collect();
    if args.verbose {
        eprintln!("candidates: {} of {} files ({:.2?})", candidates.len(), idx.num_files(), t0.elapsed());
    }

    let grep = Grep {
        re,
        count: args.count,
        list: args.list_files,
        names: !args.no_names,
        line_numbers: args.line_numbers,
    };

    let results: Vec<(Vec<u8>, usize)> = candidates.par_iter().map(|n| grep.file(n)).collect();

    let stdout = io::stdout();
    let mut w = io::BufWriter::new(stdout.lock());
    let mut total = 0usize;
    let mut files = 0usize;
    for (out, n) in &results {
        if *n > 0 {
            files += 1;
            total += n;
        }
        if !out.is_empty() {
            w.write_all(out)?;
        }
    }
    w.flush()?;
    if args.verbose {
        eprintln!("{total} matches in {files} files ({:.2?})", t0.elapsed());
    }
    if total == 0 {
        std::process::exit(1);
    }
    Ok(())
}
