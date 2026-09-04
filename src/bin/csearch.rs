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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "csearch",
    version,
    about = "Search code with a trigram index",
    disable_help_flag = true
)]
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
    // Deliberately no `-v` alias: to every grep user that means invert-match,
    // which a trigram index cannot do. An error beats a silent misread.
    #[arg(long)]
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
    /// Indexed files that no longer exist, or could not be read: counted
    /// here and reported once at the end, so a stale index is never silent.
    missing: AtomicUsize,
    unreadable: AtomicUsize,
}

impl Grep {
    /// Grep one file by name; returns its output (empty if no match) and match count.
    fn file(&self, name: &str) -> (Vec<u8>, usize) {
        match std::fs::read(name) {
            Ok(data) => self.grep_bytes(name, &data),
            Err(e) => {
                let counter = if e.kind() == io::ErrorKind::NotFound {
                    &self.missing
                } else {
                    &self.unreadable
                };
                counter.fetch_add(1, Ordering::Relaxed);
                (Vec::new(), 0)
            }
        }
    }

    /// Grep an in-memory file. At most one hit is counted per line, like
    /// `grep -c`, and the output is formatted per the flags.
    fn grep_bytes(&self, name: &str, data: &[u8]) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        let mut matches = 0usize;
        let mut pos = 0usize;
        let mut line_no = 1usize;
        let mut line_counted_to = 0usize;
        // A file that is empty or ends in '\n' has no line after that final
        // newline, but the regex engine will still report an empty match
        // there for patterns like `$`, `^$` or `x*`. Such a match belongs to
        // a line that does not exist; grep never reports it, so neither do we.
        let no_line_at_end = data.is_empty() || data.ends_with(b"\n");
        while pos < data.len() {
            let Some(m) = self.re.find_at(data, pos) else {
                break;
            };
            if no_line_at_end && m.start() >= data.len() {
                break;
            }
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

/// Candidates grepped per parallel batch. Small enough that a batch of
/// worst-case output stays modest; large enough to keep every core busy.
const CHUNK: usize = 64;

/// Grep one batch of candidates in parallel; results come back in order.
fn grep_chunk(grep: &Grep, names: &[&str]) -> Vec<(Vec<u8>, usize)> {
    names.par_iter().map(|n| grep.file(n)).collect()
}

/// `csearch ... | head` closes our stdout early. That is the reader being
/// done, not a failure: exit quietly and successfully, as grep and ripgrep
/// do, instead of printing "Broken pipe".
fn quiet_on_closed_pipe(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        other => other,
    }
}

fn main() {
    // grep's convention, so scripts can tell a typo from an empty result:
    // 0 matched, 1 nothing matched, 2 an error. clap already exits 2 on a
    // usage error.
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:?}");
            2
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let args = Args::parse();
    let t0 = Instant::now();
    if let Some(n) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }

    // crlf: `$` must match before "\r\n" as well as "\n", or every
    // end-of-line anchor silently fails on Windows-edited files.
    let re = RegexBuilder::new(&args.regexp)
        .case_insensitive(args.ignore_case)
        .multi_line(true)
        .crlf(true)
        .build()
        .with_context(|| format!("bad regexp {:?}", args.regexp))?;
    let file_re = match &args.file_regexp {
        Some(f) => Some(regex::Regex::new(f).with_context(|| format!("bad -f regexp {f:?}"))?),
        None => None,
    };

    let index_path = args.indexpath.clone().unwrap_or_else(default_index_path);
    if args.verbose {
        eprintln!("index: {}", index_path.display());
    }
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
        eprintln!(
            "candidates: {} of {} files ({:.2?})",
            candidates.len(),
            idx.num_files(),
            t0.elapsed()
        );
    }

    let grep = Grep {
        re,
        count: args.count,
        list: args.list_files,
        names: !args.no_names,
        line_numbers: args.line_numbers,
        missing: AtomicUsize::new(0),
        unreadable: AtomicUsize::new(0),
    };

    // Grep in chunks, in candidate (path) order, writing each chunk while
    // rayon greps the next one. Memory is bounded by two chunks of output
    // rather than every match in the corpus, and the first lines appear as
    // soon as the first chunk is done -- both matter for `csearch . | head`.
    // Stdout is not locked across the loop: a lock guard is not Send, and the
    // writer runs inside rayon::join.
    let mut w = io::BufWriter::with_capacity(1 << 16, io::stdout());
    let mut total = 0usize;
    let mut files = 0usize;
    let mut chunks = candidates.chunks(CHUNK);
    let mut pending = chunks.next().map(|c| grep_chunk(&grep, c));
    while let Some(done) = pending.take() {
        let next = chunks.next();
        let (written, following) = rayon::join(
            || -> io::Result<()> {
                for (out, n) in &done {
                    if *n > 0 {
                        files += 1;
                        total += n;
                    }
                    if !out.is_empty() {
                        quiet_on_closed_pipe(w.write_all(out))?;
                    }
                }
                Ok(())
            },
            || next.map(|c| grep_chunk(&grep, c)),
        );
        written?;
        pending = following;
    }
    quiet_on_closed_pipe(w.flush())?;
    let missing = grep.missing.load(Ordering::Relaxed);
    let unreadable = grep.unreadable.load(Ordering::Relaxed);
    if missing > 0 {
        eprintln!(
            "csearch: {missing} indexed file(s) no longer exist -- run cindex to refresh the index"
        );
    }
    if unreadable > 0 {
        eprintln!("csearch: {unreadable} indexed file(s) could not be read");
    }
    if args.verbose {
        eprintln!("{total} matches in {files} files ({:.2?})", t0.elapsed());
    }
    Ok(if total == 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep(
        pattern: &str,
        data: &[u8],
        count: bool,
        list: bool,
        names: bool,
        line_numbers: bool,
    ) -> (String, usize) {
        let re = RegexBuilder::new(pattern)
            .multi_line(true)
            .crlf(true)
            .build()
            .unwrap();
        let g = Grep {
            re,
            count,
            list,
            names,
            line_numbers,
            missing: AtomicUsize::new(0),
            unreadable: AtomicUsize::new(0),
        };
        let (out, n) = g.grep_bytes("f", data);
        (String::from_utf8(out).unwrap(), n)
    }

    fn count(pattern: &str, data: &[u8]) -> usize {
        grep(pattern, data, true, false, false, false).1
    }

    #[test]
    fn no_phantom_line_after_final_newline() {
        // Regression: the loop ran once more at pos == len, where the engine
        // reports an empty match after the trailing newline -- a line grep
        // never sees. Expected values are what `grep -Ec` prints.
        assert_eq!(count("x*", b"abc\n"), 1);
        assert_eq!(count("$", b"abc\n"), 1);
        assert_eq!(count("^$", b"abc\n"), 0);
        assert_eq!(count("^$", b"a\n\nb\n"), 1);
        assert_eq!(count("^", b"a\nb\n"), 2);
        assert_eq!(count("x*", b""), 0);
        assert_eq!(count("$", b""), 0);
        // A final line without a newline is still a line.
        assert_eq!(count("x*", b"abc"), 1);
        assert_eq!(count("$", b"abc"), 1);
        assert_eq!(count("c$", b"abc"), 1);
        assert_eq!(count("^$", b"a\n"), 0);
    }

    #[test]
    fn crlf_line_endings_anchor_correctly() {
        // Windows-edited files: `$` must match before "\r\n", as grep does.
        assert_eq!(count("foo$", b"foo\r\nbar\r\n"), 1);
        assert_eq!(count("^bar$", b"foo\r\nbar\r\n"), 1);
        assert_eq!(count("^$", b"a\r\n\r\nb\r\n"), 1);
        assert_eq!(count("^$", b"a\r\nb\r\n"), 0);
    }

    #[test]
    fn one_hit_per_line() {
        assert_eq!(count("a", b"aaa\naa\nb\n"), 2);
        let (out, _) = grep("a", b"aaa\n", false, false, false, false);
        assert_eq!(out, "aaa\n");
    }

    #[test]
    fn output_formats() {
        let text = b"hit one\nmiss\nmiss\nhit four\n\n\nhit 7, hit again\nmiss\nhit nine";
        let (out, n) = grep("hit", text, false, false, false, true);
        assert_eq!(n, 4);
        assert_eq!(
            out,
            "1:hit one\n4:hit four\n7:hit 7, hit again\n9:hit nine\n"
        );

        let (out, _) = grep("hit", b"hit\n", false, false, true, true);
        assert_eq!(out, "f:1:hit\n");

        let (out, n) = grep("hit", b"hit\nhit\n", true, false, true, false);
        assert_eq!((out.as_str(), n), ("f:2\n", 2));

        // -l stops at the first hit.
        let (out, n) = grep("hit", b"hit\nhit\n", false, true, true, false);
        assert_eq!((out.as_str(), n), ("f\n", 1));

        let (out, n) = grep("zzz", b"hit\n", true, false, true, false);
        assert_eq!((out.as_str(), n), ("", 0));
    }
}
