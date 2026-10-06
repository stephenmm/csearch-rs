//! The names this project installs must be its own, so that it can sit next
//! to the original csearch without either one picking up the other's
//! binaries, index or configuration.

mod common;

use common::{sealed, text, CINDEX, CSEARCH};
use csearch::names::{self, original};
use std::path::Path;

/// Everything the original lays claim to.
fn theirs() -> Vec<&'static str> {
    let mut all = original::BINARIES.to_vec();
    all.push(original::INDEX_FILE_NAME);
    all.push(original::INDEX_ENV);
    all
}

#[test]
fn nothing_shares_a_name_with_the_original() {
    let ours = [
        names::CINDEX,
        names::CSEARCH,
        names::INDEX_FILE_NAME,
        names::INDEX_ENV,
    ];
    for mine in ours {
        for other in theirs() {
            // Case-insensitively: Windows and macOS fold case in file names,
            // and Windows does in environment variable names too.
            assert!(
                !mine.eq_ignore_ascii_case(other),
                "{mine} is the original csearch's {other}"
            );
        }
    }
    // The upstream names themselves, pinned: if these drift, the check above
    // is comparing against the wrong thing.
    assert_eq!(original::BINARIES, ["cindex", "csearch", "cgrep"]);
    assert_eq!(original::INDEX_FILE_NAME, ".csearchindex");
    assert_eq!(original::INDEX_ENV, "CSEARCHINDEX");
}

#[test]
fn the_built_binaries_carry_the_declared_names() {
    for (exe, declared) in [(CINDEX, names::CINDEX), (CSEARCH, names::CSEARCH)] {
        // The file cargo produced...
        let stem = Path::new(exe).file_stem().unwrap().to_str().unwrap();
        assert_eq!(stem, declared, "{exe}");

        // ...and what the program calls itself.
        let out = sealed(exe).arg("--version").output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        let line = text(&out.stdout);
        let mut words = line.split_whitespace();
        assert_eq!(words.next(), Some(declared), "{line}");
        assert_eq!(words.next(), Some(env!("CARGO_PKG_VERSION")), "{line}");
        assert_eq!(words.next(), None, "{line}");
    }
}

/// Words as a reader would see them: runs of name characters, with sentence
/// punctuation trimmed, so `~/.csearch-rs-index).` yields `.csearch-rs-index`.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
        .map(|w| w.trim_end_matches('.'))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn help_never_sends_anyone_to_the_originals_names() {
    // --help is where a user learns which command to run, which file is the
    // index and which variable to set. If it named the original's, following
    // it would operate on the other tool.
    for exe in [CINDEX, CSEARCH] {
        let out = sealed(exe).arg("--help").output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        let help = text(&out.stdout);
        let seen = words(&help);
        for other in theirs() {
            assert!(
                !seen.iter().any(|w| w == other),
                "{exe} --help mentions the original's `{other}`:\n{help}"
            );
        }
        // It does name ours -- otherwise the check above could pass on an
        // empty or truncated help text.
        for mine in [names::INDEX_FILE_NAME, names::INDEX_ENV] {
            assert!(
                seen.iter().any(|w| w == mine),
                "{exe} --help does not mention {mine}:\n{help}"
            );
        }
    }
}

#[test]
fn the_word_splitter_can_tell_the_names_apart() {
    // The help check is only as good as this: it must see the original's
    // names when they are there, and not see them inside ours.
    let w = words("run `cindex --reset`, then cindex-rs; see ~/.csearchindex).");
    assert_eq!(
        w,
        [
            "run",
            "cindex",
            "--reset",
            "then",
            "cindex-rs",
            "see",
            ".csearchindex"
        ]
    );
    let w = words("$CSEARCH_RS_INDEX, else ~/.csearch-rs-index.");
    assert_eq!(w, ["CSEARCH_RS_INDEX", "else", ".csearch-rs-index"]);
}
