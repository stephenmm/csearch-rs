//! Perforce: which files does this workspace hold?
//!
//! The answer is `p4 have` -- everything synced from the depot -- together
//! with the files opened here that have not been submitted yet, which `p4
//! have` does not know about. Between them that is what a developer means by
//! "the files in my workspace", and it leaves out what Perforce was never
//! told of: build products, editor droppings, anything in P4IGNORE.
//!
//! p4 is run with `-G`, which makes it print each record as a marshalled
//! Python dictionary instead of text meant for reading. That is the one
//! output format of p4's that is safe to parse: a path with a space, a `#`
//! or a newline in it comes through whole.
//!
//! Unlike a git listing, a Perforce one needs the server. When the server
//! cannot be reached, or the ticket has run out, there is no listing -- and
//! no fallback either: see [`crate::listing::snapshot`].

use crate::paths::canonical_string;
use crate::write::is_within;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One record of `p4 -G` output: a flat dictionary whose values are strings,
/// or -- for an error's `severity` and `generic` -- integers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Record {
    strings: BTreeMap<String, Vec<u8>>,
    ints: BTreeMap<String, i32>,
}

impl Record {
    /// A string value, with anything that is not UTF-8 replaced. A file name
    /// that is not UTF-8 will not survive this, and is then not found on
    /// disk and not indexed; a git listing treats such names the same way.
    pub fn text(&self, key: &str) -> Option<String> {
        self.strings
            .get(key)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn int(&self, key: &str) -> Option<i32> {
        self.ints.get(key).copied()
    }

    fn is(&self, code: &str) -> bool {
        self.strings
            .get("code")
            .is_some_and(|c| c == code.as_bytes())
    }
}

/// Why Perforce gave no answer.
#[derive(Debug)]
pub enum Error {
    /// `p4` could not be started at all.
    NotRun(std::io::Error),
    /// p4 ran and said no. These are its own words.
    Failed(String),
    /// p4 ran, and what it printed was not what `p4 -G` prints.
    Garbled(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotRun(e) => write!(f, "p4 could not be run: {e}"),
            Error::Failed(said) => f.write_str(said),
            Error::Garbled(what) => write!(f, "p4 -G printed something unexpected: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// Perforce's severities, as `p4 -G` numbers them. An "error" record of
/// severity 1 or 2 is a remark -- `file(s) not on client` when nothing has
/// been synced yet -- and only 3 and 4 mean the command did not work.
const SEVERITY_FAILED: i32 = 3;

fn take<'a>(bytes: &mut &'a [u8], n: usize, what: &str) -> Result<&'a [u8], String> {
    if bytes.len() < n {
        return Err(format!("the output ends in the middle of {what}"));
    }
    let (head, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(head)
}

fn marshalled_u32(bytes: &mut &[u8], what: &str) -> Result<u32, String> {
    let raw = take(bytes, 4, what)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn marshalled_string<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let len = marshalled_u32(bytes, "a string's length")? as usize;
    take(bytes, len, "a string")
}

/// Decode what `p4 -G` prints: Python's marshal format, as much of it as p4
/// writes. That is a run of dictionaries (`{` ... `0`), each key a string
/// (`s`, a four-byte little-endian length, the bytes) and each value a string
/// or a 32-bit integer (`i`, four bytes little-endian).
pub fn parse_marshal(mut bytes: &[u8]) -> Result<Vec<Record>, String> {
    let mut records = Vec::new();
    while let Some((&tag, rest)) = bytes.split_first() {
        if tag != b'{' {
            return Err(format!(
                "byte {tag:#04x} where a record should begin, after {} record(s)",
                records.len()
            ));
        }
        bytes = rest;
        let mut record = Record::default();
        loop {
            let tag = take(&mut bytes, 1, "a record")?[0];
            match tag {
                b'0' => break,
                b's' => {}
                other => return Err(format!("byte {other:#04x} where a key should be")),
            }
            let key = String::from_utf8_lossy(marshalled_string(&mut bytes)?).into_owned();
            match take(&mut bytes, 1, "a record")?[0] {
                b's' => {
                    let value = marshalled_string(&mut bytes)?.to_vec();
                    record.strings.insert(key, value);
                }
                b'i' => {
                    // The same four bytes as a length, read as signed.
                    let value = marshalled_u32(&mut bytes, "an integer")? as i32;
                    record.ints.insert(key, value);
                }
                other => {
                    return Err(format!(
                        "byte {other:#04x} where the value of `{key}` should be"
                    ))
                }
            }
        }
        records.push(record);
    }
    Ok(records)
}

/// The first line of some output that says anything.
fn first_line(text: &[u8]) -> Option<String> {
    String::from_utf8_lossy(text)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// What p4 said went wrong, if `records` hold a failure: the text of every
/// error of severity "failed" or worse, on one line.
fn failure(records: &[Record]) -> Option<String> {
    let said: Vec<String> = records
        .iter()
        .filter(|r| {
            r.is("error") && r.int("severity").unwrap_or(SEVERITY_FAILED) >= SEVERITY_FAILED
        })
        .filter_map(|r| r.text("data"))
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|d| !d.is_empty())
        .collect();
    (!said.is_empty()).then(|| said.join(" "))
}

/// Run `p4 -G ARGS` for the workspace `dir` is in, and return its records.
///
/// p4 finds its configuration from where it is run -- and it takes "where"
/// from `$PWD` rather than asking the system, so the directory is given all
/// three ways: as the process's working directory, as `PWD`, and as `-d`.
/// Everything else in the environment is passed on untouched: `P4PORT`,
/// `P4CLIENT`, `P4CONFIG` and the ticket file are how p4 knows which server
/// and which workspace this is.
fn run(dir: &Path, args: &[&str]) -> Result<Vec<Record>, Error> {
    let out = Command::new("p4")
        .arg("-G")
        .arg("-d")
        .arg(dir)
        .args(args)
        .current_dir(dir)
        .env("PWD", dir)
        .stdin(Stdio::null())
        .output()
        .map_err(Error::NotRun)?;
    let records = match parse_marshal(&out.stdout) {
        Ok(records) => records,
        // A p4 that failed before it got as far as -G says so in plain text.
        Err(_) if !out.status.success() => {
            return Err(Error::Failed(
                first_line(&out.stderr)
                    .or_else(|| first_line(&out.stdout))
                    .unwrap_or_else(|| format!("p4 {} failed ({})", args.join(" "), out.status)),
            ));
        }
        Err(what) => return Err(Error::Garbled(what)),
    };
    if let Some(said) = failure(&records) {
        return Err(Error::Failed(said));
    }
    if !out.status.success() {
        return Err(Error::Failed(first_line(&out.stderr).unwrap_or_else(
            || format!("p4 {} failed ({})", args.join(" "), out.status),
        )));
    }
    Ok(records)
}

/// The workspace p4 would use from a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The client's name.
    pub client: String,
    /// Its root as the client spec spells it -- which need not be how the
    /// file system does. `None` for a client with no single root (`Root:
    /// null`, which Windows clients spread over several drives use).
    pub root: Option<String>,
}

/// Ask p4 which workspace `dir` belongs to.
pub fn workspace(dir: &Path) -> Result<Workspace, Error> {
    let records = run(dir, &["info"])?;
    let info = records
        .iter()
        .find(|r| r.is("stat"))
        .ok_or_else(|| Error::Garbled("`p4 info` returned no record".into()))?;
    let client = info
        .text("clientName")
        .ok_or_else(|| Error::Garbled("`p4 info` did not name a client".into()))?;
    // `p4 info` succeeds for a client the server has never heard of. It just
    // has no root to report.
    match info.text("clientRoot") {
        None => Err(Error::Failed(format!(
            "the server knows no workspace called `{client}` -- check P4CLIENT, or the \
             P4CONFIG file, for this directory"
        ))),
        Some(root) if root == "null" => Ok(Workspace { client, root: None }),
        Some(root) => Ok(Workspace {
            client,
            root: Some(root),
        }),
    }
}

/// The root of the Perforce workspace `dir` is in, as a canonical path.
pub fn workspace_root(dir: &Path) -> Result<PathBuf, Error> {
    let ws = workspace(dir)?;
    let Some(root) = ws.root else {
        return Err(Error::Failed(format!(
            "workspace `{}` has no single root (its Root is `null`); name the directory \
             to index instead of using --local",
            ws.client
        )));
    };
    let canon = canonical_string(Path::new(&root)).map_err(|e| {
        Error::Failed(format!(
            "the root of workspace `{}`, {root}, cannot be resolved: {e}",
            ws.client
        ))
    })?;
    let here =
        canonical_string(dir).map_err(|e| Error::Failed(format!("{}: {e}", dir.display())))?;
    if !is_within(&here, &canon) {
        return Err(Error::Failed(format!(
            "workspace `{}` is rooted at {canon}, which does not contain {here} -- check \
             P4CLIENT, or the P4CONFIG file, for this directory",
            ws.client
        )));
    }
    Ok(PathBuf::from(canon))
}

fn is_separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// The part of `path` below the directory `dir`, if it is below it. Compared
/// the way the platform compares paths: on Windows without regard to case or
/// to which way the slashes lean.
fn below<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let dir = dir.trim_end_matches(is_separator);
    let head = path.get(..dir.len())?;
    let same = if cfg!(windows) {
        head.chars()
            .zip(dir.chars())
            .all(|(a, b)| a.eq_ignore_ascii_case(&b) || (is_separator(a) && is_separator(b)))
    } else {
        head == dir
    };
    if !same {
        return None;
    }
    let rest = path[dir.len()..].strip_prefix(is_separator)?;
    (!rest.is_empty()).then_some(rest)
}

fn join(dir: &str, rest: &str) -> String {
    let dir = dir.trim_end_matches(is_separator);
    let sep = std::path::MAIN_SEPARATOR;
    if cfg!(windows) {
        format!("{dir}{sep}{}", rest.replace('/', "\\"))
    } else {
        format!("{dir}{sep}{rest}")
    }
}

/// Where a path that p4 printed lies under the index's `root`, spelled the
/// way `root` is -- or `None` if it lies somewhere else.
///
/// p4 spells local paths the way the client spec spells its Root: through a
/// symbolic link, perhaps, or with a lower-case drive letter. The index's
/// roots are canonical. So the client's root is first exchanged for its
/// canonical form (`spelled` for `canonical`), and what results is then
/// looked for under `root`, which may be the whole workspace or a directory
/// inside it.
fn place(path: &str, spelled: Option<&str>, canonical: Option<&str>, root: &str) -> Option<String> {
    let moved = match (spelled, canonical) {
        (Some(spelled), Some(canonical)) => below(path, spelled).map(|rest| join(canonical, rest)),
        _ => None,
    };
    let path = moved.as_deref().unwrap_or(path);
    below(path, root).map(|rest| join(root, rest))
}

/// The files Perforce holds in the workspace under `root`: those synced from
/// the depot, and those opened here. A file opened for delete is in the
/// second set and not on disk; the caller, which looks at every file anyway,
/// drops it there.
pub fn files(root: &str) -> Result<Vec<PathBuf>, Error> {
    let dir = Path::new(root);
    let ws = workspace(dir)?;
    let spelled = ws.root.as_deref();
    let canonical = spelled.and_then(|r| canonical_string(Path::new(r)).ok());

    let mut found = BTreeSet::new();
    let mut note = |printed: Option<String>| {
        if let Some(placed) = printed.and_then(|p| place(&p, spelled, canonical.as_deref(), root)) {
            found.insert(placed);
        }
    };
    // Everything the workspace has from the depot. No file argument: the
    // whole client is asked for and narrowed down here, so that no path of
    // ours has to be written in p4's syntax, where `@`, `#`, `%` and `*`
    // mean something.
    for record in run(dir, &["have"])? {
        note(record.text("path"));
    }
    // And what is opened here: files added and not yet submitted are in no
    // have list. The client's name contains none of p4's special characters.
    let everything = format!("//{}/...", ws.client);
    for record in run(dir, &["fstat", "-Ro", &everything])? {
        note(record.text("clientFile"));
    }
    Ok(found.into_iter().map(PathBuf::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Vec<u8> {
        let mut out = vec![b's'];
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        out
    }

    fn i(value: i32) -> Vec<u8> {
        let mut out = vec![b'i'];
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    /// A marshalled dictionary, the way `p4 -G` writes one.
    fn dict(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![b'{'];
        for (key, value) in pairs {
            out.extend_from_slice(&s(key));
            out.extend_from_slice(value);
        }
        out.push(b'0');
        out
    }

    #[test]
    fn records_are_read_one_after_another() {
        let mut bytes = dict(&[
            ("code", s("stat")),
            ("depotFile", s("//depot/a b#1.c")),
            ("path", s("/ws/a b#1.c")),
            ("haveRev", s("3")),
        ]);
        bytes.extend(dict(&[
            ("code", s("error")),
            ("data", s("//ws/... - file(s) not on client.\n")),
            ("severity", i(2)),
            ("generic", i(17)),
        ]));
        let records = parse_marshal(&bytes).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records[0].is("stat"));
        assert_eq!(records[0].text("path").as_deref(), Some("/ws/a b#1.c"));
        assert_eq!(records[0].text("nothing"), None);
        assert!(records[1].is("error"));
        assert_eq!(records[1].int("severity"), Some(2));
        assert_eq!(records[1].int("code"), None);

        assert_eq!(parse_marshal(b"").unwrap(), Vec::new());
        // A path with a newline in it is one value, not two lines.
        let odd = dict(&[("path", s("/ws/two\nlines"))]);
        assert_eq!(
            parse_marshal(&odd).unwrap()[0].text("path").as_deref(),
            Some("/ws/two\nlines")
        );
        // An integer is signed.
        let negative = dict(&[("n", i(-2))]);
        assert_eq!(parse_marshal(&negative).unwrap()[0].int("n"), Some(-2));
    }

    #[test]
    fn output_that_is_not_marshal_is_refused_not_guessed_at() {
        // Plain text, which is what p4 prints without -G.
        assert!(parse_marshal(b"Perforce client error:\n").is_err());
        // Cut short at every possible place.
        let whole = dict(&[("code", s("stat")), ("severity", i(3))]);
        for cut in 1..whole.len() {
            assert!(parse_marshal(&whole[..cut]).is_err(), "cut at {cut}");
        }
        // A length that runs past the end.
        let mut long = vec![b'{', b's'];
        long.extend_from_slice(&1000u32.to_le_bytes());
        long.extend_from_slice(b"code");
        assert!(parse_marshal(&long).is_err());
        // A kind of value p4 does not write -- in a record that would be
        // complete if that value were simply passed over.
        let mut list = vec![b'{'];
        list.extend_from_slice(&s("code"));
        list.extend_from_slice(b"[0");
        assert!(parse_marshal(&list).is_err());
        // And something other than a string where a key belongs.
        let mut keyed = vec![b'{'];
        keyed.extend_from_slice(&i(1));
        keyed.extend_from_slice(&s("stat"));
        keyed.push(b'0');
        assert!(parse_marshal(&keyed).is_err());
    }

    #[test]
    fn only_a_real_error_is_a_failure() {
        let warning = parse_marshal(&dict(&[
            ("code", s("error")),
            ("data", s("//ws/... - file(s) not on client.\n")),
            ("severity", i(2)),
        ]))
        .unwrap();
        assert_eq!(failure(&warning), None);

        let failed = parse_marshal(&dict(&[
            ("code", s("error")),
            (
                "data",
                s("Perforce password (P4PASSWD) invalid or unset.\n"),
            ),
            ("severity", i(3)),
        ]))
        .unwrap();
        assert_eq!(
            failure(&failed).as_deref(),
            Some("Perforce password (P4PASSWD) invalid or unset.")
        );

        // Several lines of it come out as one.
        let fatal = parse_marshal(&dict(&[
            ("code", s("error")),
            (
                "data",
                s("Connect to server failed; check $P4PORT.\nTCP connect to x failed.\n"),
            ),
            ("severity", i(4)),
        ]))
        .unwrap();
        assert_eq!(
            failure(&fatal).as_deref(),
            Some("Connect to server failed; check $P4PORT. TCP connect to x failed.")
        );

        // An error that does not say how bad it is, is taken at its word.
        let unsaid = parse_marshal(&dict(&[("code", s("error")), ("data", s("no\n"))])).unwrap();
        assert_eq!(failure(&unsaid).as_deref(), Some("no"));
        // And a record that is not an error is not one, whatever it holds.
        let stat = parse_marshal(&dict(&[("code", s("stat")), ("severity", i(4))])).unwrap();
        assert_eq!(failure(&stat), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_path_is_placed_under_the_root_as_the_root_is_spelled() {
        // The whole workspace, spelled the same by both.
        assert_eq!(
            place("/ws/src/a.c", Some("/ws"), Some("/ws"), "/ws").as_deref(),
            Some("/ws/src/a.c")
        );
        // The client spec reaches the workspace through a symbolic link.
        assert_eq!(
            place(
                "/home/u/ws/src/a.c",
                Some("/home/u/ws"),
                Some("/data/ws"),
                "/data/ws"
            )
            .as_deref(),
            Some("/data/ws/src/a.c")
        );
        // The index covers one directory of the workspace.
        assert_eq!(
            place(
                "/home/u/ws/src/a.c",
                Some("/home/u/ws/"),
                Some("/data/ws"),
                "/data/ws/src"
            )
            .as_deref(),
            Some("/data/ws/src/a.c")
        );
        assert_eq!(
            place(
                "/home/u/ws/doc/b.txt",
                Some("/home/u/ws"),
                Some("/data/ws"),
                "/data/ws/src"
            ),
            None
        );
        // A directory whose name merely begins the same is not inside.
        assert_eq!(
            place("/ws-other/a.c", Some("/ws"), Some("/ws"), "/ws"),
            None
        );
        assert_eq!(
            place("/ws/srcs/a.c", Some("/ws"), Some("/ws"), "/ws/src"),
            None
        );
        // Case matters here, and a backslash is part of a name.
        assert_eq!(place("/WS/a.c", Some("/ws"), Some("/ws"), "/ws"), None);
        assert_eq!(
            place("/ws/a\\b.c", Some("/ws"), Some("/ws"), "/ws").as_deref(),
            Some("/ws/a\\b.c")
        );
        // A client with no root of its own: paths are taken as printed.
        assert_eq!(
            place("/ws/a.c", None, None, "/ws").as_deref(),
            Some("/ws/a.c")
        );
        // The root itself is not a file under the root.
        assert_eq!(place("/ws", Some("/ws"), Some("/ws"), "/ws"), None);
        // A workspace rooted at the top of the file system.
        assert_eq!(
            place("/a.c", Some("/"), Some("/"), "/").as_deref(),
            Some("/a.c")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_path_is_placed_under_the_root_as_the_root_is_spelled() {
        // p4 prints the drive letter the way the client spec has it.
        assert_eq!(
            place(r"c:\ws\src\a.c", Some(r"c:\ws"), Some(r"C:\ws"), r"C:\ws").as_deref(),
            Some(r"C:\ws\src\a.c")
        );
        // Either slash, and a root written with one on the end.
        assert_eq!(
            place("c:/ws/src/a.c", Some("c:/ws/"), Some(r"C:\ws"), r"C:\ws").as_deref(),
            Some(r"C:\ws\src\a.c")
        );
        // The client spec reaches the workspace through a junction.
        assert_eq!(
            place(
                r"c:\link\src\a.c",
                Some(r"c:\link"),
                Some(r"D:\real"),
                r"D:\real\src"
            )
            .as_deref(),
            Some(r"D:\real\src\a.c")
        );
        assert_eq!(
            place(
                r"c:\link\doc\b.txt",
                Some(r"c:\link"),
                Some(r"D:\real"),
                r"D:\real\src"
            ),
            None
        );
        // A directory whose name merely begins the same is not inside.
        assert_eq!(
            place(r"C:\ws-other\a.c", Some(r"C:\ws"), Some(r"C:\ws"), r"C:\ws"),
            None
        );
        // A client with no root of its own (Root: null): as printed, and
        // still found under a root spelled in another case.
        assert_eq!(
            place(r"d:\work\a.c", None, None, r"D:\work").as_deref(),
            Some(r"D:\work\a.c")
        );
        // A workspace that is a whole drive.
        assert_eq!(
            place(r"e:\a.c", Some("e:\\"), Some("E:\\"), "E:\\").as_deref(),
            Some(r"E:\a.c")
        );
        assert_eq!(
            place(r"C:\ws", Some(r"C:\ws"), Some(r"C:\ws"), r"C:\ws"),
            None
        );
    }
}
