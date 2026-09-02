//! The property the trigram index promises: for any regexp, the candidate set
//! it returns is a superset of the files the regexp actually matches.
//!
//! A false negative here is invisible to a user -- the file simply never
//! shows up -- and the query analysis (prefix/suffix cross products, MAX_SET
//! truncation, case folding, common-trigram factoring) is exactly where a
//! subtle bug would produce one. So this generates random corpora and random
//! patterns from a grammar that covers those features, and checks that every
//! real match is a candidate. It is deterministic: a failure prints the seed
//! and pattern that reproduce it. `CSEARCH_PROP_ITERS=50 cargo test --test
//! superset` runs more corpora.

use csearch::read::Index;
use csearch::regexp;
use csearch::write::{build_index, BuildOptions};
use regex::bytes::RegexBuilder;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        let state = 0x9E37_79B9_7F4A_7C15 ^ (seed + 1).wrapping_mul(0x9E37_79B9);
        Rng(if state == 0 { 1 } else { state })
    }
    fn next(&mut self) -> u64 {
        // xorshift64*: plenty for test data.
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// Twelve letters: a few-hundred-byte file holds only a small fraction of the
/// 1,728 possible letter trigrams, so trigram queries genuinely prune.
const ALPHABET: &[u8] = b"abcdefghijkl";

fn gen_file(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..1 + rng.below(12) {
        for w in 0..1 + rng.below(6) {
            if w > 0 {
                out.push(b' ');
            }
            for _ in 0..1 + rng.below(7) {
                out.push(ALPHABET[rng.below(ALPHABET.len())]);
            }
        }
        if rng.chance(10) {
            out.push(b'\r'); // the odd CRLF line
        }
        out.push(b'\n');
    }
    if rng.chance(20) {
        out.pop(); // no trailing newline
    }
    if rng.chance(5) {
        out.truncate(rng.below(3)); // tiny file: listed, no trigrams
    }
    out
}

fn literal(rng: &mut Rng, files: &[Vec<u8>]) -> String {
    // Half the time lift a real substring, so matches actually happen and the
    // exact/prefix/suffix machinery sees strings that occur (including ones
    // spanning a space, '\r' or '\n').
    if rng.chance(50) {
        let f = &files[rng.below(files.len())];
        if f.len() >= 3 {
            let len = 2 + rng.below(f.len().min(5) - 1);
            let start = rng.below(f.len() - len + 1);
            return f[start..start + len].iter().map(|&b| b as char).collect();
        }
    }
    (0..2 + rng.below(4))
        .map(|_| ALPHABET[rng.below(ALPHABET.len())] as char)
        .collect()
}

fn atom(rng: &mut Rng, files: &[Vec<u8>], depth: u32) -> String {
    let mut s = match rng.below(10) {
        0..=4 => literal(rng, files),
        5 => match rng.below(3) {
            0 => {
                let letters: String = (0..2 + rng.below(3))
                    .map(|_| ALPHABET[rng.below(ALPHABET.len())] as char)
                    .collect();
                format!("[{letters}]")
            }
            1 => "[a-d]".to_string(),
            _ => "[^x]".to_string(), // a huge class: the analyser must treat it as "anything"
        },
        6 => ".".to_string(),
        7 if depth < 2 => format!("({})", expr(rng, files, depth + 1)),
        _ => literal(rng, files),
    };
    if rng.chance(30) {
        s.push_str(["*", "+", "?", "{2}", "{1,3}"][rng.below(5)]);
    }
    s
}

fn term(rng: &mut Rng, files: &[Vec<u8>], depth: u32) -> String {
    (0..1 + rng.below(3))
        .map(|_| atom(rng, files, depth))
        .collect()
}

fn expr(rng: &mut Rng, files: &[Vec<u8>], depth: u32) -> String {
    let mut s = term(rng, files, depth);
    if rng.chance(25) {
        s = format!("{s}|{}", term(rng, files, depth));
    }
    s
}

/// A pattern and whether to match it case-insensitively.
fn pattern(rng: &mut Rng, files: &[Vec<u8>]) -> (String, bool) {
    let mut p = expr(rng, files, 0);
    if rng.chance(15) {
        p = format!("^{p}");
    }
    if rng.chance(15) {
        p.push('$');
    }
    if rng.chance(10) {
        p = format!("\\b{p}\\b");
    }
    (p, rng.chance(30))
}

#[test]
fn candidates_are_a_superset_of_true_matches() {
    let corpora: u64 = std::env::var("CSEARCH_PROP_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let dir = tempfile::tempdir().unwrap();
    let (mut checks, mut pruned, mut nonempty) = (0usize, 0usize, 0usize);

    for seed in 0..corpora {
        let mut rng = Rng::new(seed);
        let root = dir.path().join(format!("corpus{seed}"));
        fs::create_dir_all(&root).unwrap();
        let files: Vec<Vec<u8>> = (0..20 + rng.below(30))
            .map(|_| gen_file(&mut rng))
            .collect();
        for (i, f) in files.iter().enumerate() {
            fs::write(root.join(format!("f{i:03}.txt")), f).unwrap();
        }
        let out = dir.path().join(format!("index{seed}"));
        build_index(std::slice::from_ref(&root), &out, &BuildOptions::default()).unwrap();
        let idx = Index::open(&out).unwrap();
        assert_eq!(
            idx.num_files() as usize,
            files.len(),
            "seed {seed}: every file should be indexed"
        );
        let id_of: HashMap<String, u32> = (0..idx.num_files())
            .map(|id| {
                let base = Path::new(idx.name(id))
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (base, id)
            })
            .collect();

        for _ in 0..200 {
            let (pat, icase) = pattern(&mut rng, &files);
            let re = RegexBuilder::new(&pat)
                .case_insensitive(icase)
                .multi_line(true)
                .crlf(true)
                .build()
                .unwrap_or_else(|e| {
                    panic!("seed {seed}: generated an invalid pattern {pat:?}: {e}")
                });
            let query = regexp::regexp_query(&pat, icase)
                .unwrap_or_else(|e| panic!("seed {seed}: analyser rejected {pat:?}: {e}"));
            let cands = idx.posting_query(&query);
            assert!(
                cands.windows(2).all(|w| w[0] < w[1]),
                "seed {seed}: candidates not sorted and unique for {pat:?}: {cands:?}"
            );
            checks += 1;
            if cands.len() < files.len() {
                pruned += 1;
            }
            let mut matched_any = false;
            for (i, content) in files.iter().enumerate() {
                if !re.is_match(content) {
                    continue;
                }
                matched_any = true;
                let id = id_of[&format!("f{i:03}.txt")];
                assert!(
                    cands.binary_search(&id).is_ok(),
                    "FALSE NEGATIVE\n  seed     {seed}\n  pattern  {pat:?} (case-insensitive: {icase})\n  \
                     query    {query}\n  file     f{i:03}.txt = {:?}\n  index returned {} of {} files",
                    String::from_utf8_lossy(content),
                    cands.len(),
                    files.len()
                );
            }
            if matched_any {
                nonempty += 1;
            }
        }
    }

    // A superset check passes trivially if the index never prunes or the
    // patterns never match; make sure this test is actually testing something.
    println!("{checks} checks: {pruned} pruned by the index, {nonempty} had real matches");
    assert!(
        pruned * 5 > checks,
        "the index never pruned anything -- the test is vacuous ({pruned}/{checks})"
    );
    assert!(
        nonempty * 5 > checks,
        "patterns rarely matched -- the test is vacuous ({nonempty}/{checks})"
    );
}
