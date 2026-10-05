//! The stamp: a sidecar beside the index (`<index>.meta`) recording what the
//! index was built from, so that `cindex-rs --if-changed` can skip a rebuild
//! when nothing has changed and `csearch-rs` can say when a git checkout has
//! moved on.
//!
//! It holds the fingerprint of the file listing the build started from (see
//! [`crate::listing`]), when that listing was taken, the size of the index it
//! describes, and the `HEAD` of every root that is inside a git repository.
//! Only the last of those is specific to git, and it is used for nothing but
//! the search-time note.
//!
//! Everything here is best-effort and errs towards rebuilding: a stamp that
//! is missing, unreadable or from another version means "unknown", which is
//! never "unchanged". Losing or mangling the stamp only ever costs one extra
//! rebuild, so it needs no versioning beyond its header line.

use crate::githead;
use crate::listing::{Root, Snapshot};
use crate::names::{CINDEX, CSEARCH};
use crate::paths::sidecar;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HEADER: &str = "csearch-rs stamp 2";

/// The longest timestamp tick of any file system in use: two seconds, on FAT
/// and on exFAT as Windows mounts it (measured: every modification time lands
/// on an even second, rounded up). HFS+ and ext3 keep whole seconds. Within
/// one tick a file can be written twice and look untouched.
const COARSE_TICK: Duration = Duration::from_secs(2);

/// What was recorded when the index was built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub fingerprint: u64,
    pub taken_at: SystemTime,
    /// The size of the index this was written for. A stamp found beside some
    /// other index -- one copied into place, say -- describes nothing.
    pub index_bytes: u64,
    /// `(HEAD, root)` for each root inside a git repository.
    pub heads: Vec<(String, String)>,
}

/// The stamp sidecar path for an index file.
pub fn stamp_path(index: &Path) -> PathBuf {
    sidecar(index, "meta")
}

fn heads_of(roots: &[Root]) -> Vec<(String, String)> {
    roots
        .iter()
        .filter_map(|r| Some((githead::head(Path::new(&r.path))?, r.path.clone())))
        .collect()
}

fn render(stamp: &Stamp) -> String {
    let since = stamp
        .taken_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut text = format!(
        "{HEADER}\nfingerprint {:016x}\ntaken {} {}\nindex {}\n",
        stamp.fingerprint,
        since.as_secs(),
        since.subsec_nanos(),
        stamp.index_bytes
    );
    for (head, root) in &stamp.heads {
        // A hash has no space in it, so the root -- which may -- comes last.
        // Neither can contain a newline.
        let _ = writeln!(text, "head {head} {root}");
    }
    text
}

fn parse(text: &str) -> Option<Stamp> {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return None;
    }
    let (mut fingerprint, mut taken_at, mut index_bytes) = (None, None, None);
    let mut heads = Vec::new();
    for line in lines {
        let (key, value) = line.split_once(' ')?;
        match key {
            "fingerprint" => fingerprint = Some(u64::from_str_radix(value, 16).ok()?),
            "taken" => {
                let (secs, nanos) = value.split_once(' ')?;
                let nanos: u32 = nanos.parse().ok()?;
                if nanos >= 1_000_000_000 {
                    return None;
                }
                let since = Duration::new(secs.parse().ok()?, nanos);
                taken_at = Some(UNIX_EPOCH.checked_add(since)?);
            }
            "index" => index_bytes = Some(value.parse().ok()?),
            "head" => {
                let (head, root) = value.split_once(' ')?;
                heads.push((head.to_string(), root.to_string()));
            }
            _ => {} // a later version's field: not ours to interpret
        }
    }
    Some(Stamp {
        fingerprint: fingerprint?,
        taken_at: taken_at?,
        index_bytes: index_bytes?,
        heads,
    })
}

/// The stamp beside `index`, if there is one this version understands.
pub fn read(index: &Path) -> Option<Stamp> {
    parse(&std::fs::read_to_string(stamp_path(index)).ok()?)
}

fn store(index: &Path, stamp: &Stamp) {
    // Best-effort: an unwritable stamp costs a rebuild next time, no more.
    let _ = std::fs::write(stamp_path(index), render(stamp));
}

/// Record that the index, `index_bytes` long, now holds exactly what `snap`
/// listed.
///
/// `snap` must be the listing the build read its files from. Its time is the
/// moment the listing *began*, so a file written while the build was running
/// is newer than the stamp and will not match it.
pub fn write(index: &Path, snap: &Snapshot, index_bytes: u64) {
    store(
        index,
        &Stamp {
            fingerprint: snap.fingerprint,
            taken_at: snap.taken_at,
            index_bytes,
            heads: heads_of(&snap.roots),
        },
    );
}

/// The index is still current, but `HEAD` may have moved -- a commit changes
/// no file in the work tree. Record where it is now, so that `csearch-rs` does
/// not go on saying the index is behind.
pub fn refresh_heads(index: &Path, stamp: &Stamp, roots: &[Root]) {
    let heads = heads_of(roots);
    if heads != stamp.heads {
        store(
            index,
            &Stamp {
                heads,
                ..stamp.clone()
            },
        );
    }
}

/// Whether an index with this stamp still matches the files on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Same files, same sizes, same times: nothing to do.
    Current,
    /// Rebuild, and here is why.
    Stale(&'static str),
}

/// Compare what an index was built from with what is there now. Anything
/// unknown is `Stale` -- rebuild, never risk a stale skip.
pub fn compare(stamp: Option<&Stamp>, now: &Snapshot) -> Verdict {
    let Some(stamp) = stamp else {
        return Verdict::Stale("there is no record of what the index was built from");
    };
    if stamp.fingerprint != now.fingerprint {
        return Verdict::Stale("files were added, removed or modified");
    }
    // Same paths, sizes and times. On a file system with whole-second
    // timestamps that is not proof: a file written just before the build
    // listed it, and again after the build read it, shows the same time
    // twice. Git calls this "racy" and resolves it the same way -- anything
    // modified within a tick of the stamp is not trusted.
    let racy = now.newest_coarse_mtime.is_some_and(|newest| {
        newest
            .checked_add(COARSE_TICK)
            .map_or(true, |limit| limit > stamp.taken_at)
    });
    if racy {
        return Verdict::Stale(
            "a file was modified too close to the last build for its timestamp to be trusted",
        );
    }
    Verdict::Current
}

/// Whether the index at `index` still matches the files `now` lists, along
/// with the stamp that was consulted (for [`refresh_heads`]).
pub fn check(index: &Path, now: &Snapshot) -> (Verdict, Option<Stamp>) {
    let stamp = read(index);
    let on_disk = std::fs::metadata(index).map(|m| m.len()).ok();
    let verdict = match (&stamp, on_disk) {
        (_, None) => Verdict::Stale("there is no index yet"),
        (Some(s), Some(len)) if s.index_bytes != len => {
            Verdict::Stale("the index is not the one its stamp was written for")
        }
        _ => compare(stamp.as_ref(), now),
    };
    (verdict, stamp)
}

/// A one-line note if any stamped root's `HEAD` has moved since the index was
/// built, else `None`. This runs on every search, so it is HEAD only, and
/// HEAD is read from the repository's files rather than by starting git: a
/// handful of small reads per git root, and nothing for any other index.
pub fn staleness(index: &Path) -> Option<String> {
    let stamp = read(index)?;
    let behind = stamp
        .heads
        .iter()
        .filter(|(head, root)| githead::head(Path::new(root)).is_some_and(|now| &now != head))
        .count();
    match behind {
        0 => None,
        1 => Some(format!(
            "{CSEARCH}: the index is behind HEAD in 1 root -- run {CINDEX} to refresh"
        )),
        n => Some(format!(
            "{CSEARCH}: the index is behind HEAD in {n} roots -- run {CINDEX} to refresh"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::Source;

    fn at(secs: u64, nanos: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(secs, nanos)
    }

    fn snap(fingerprint: u64, newest_coarse_mtime: Option<SystemTime>) -> Snapshot {
        Snapshot {
            roots: vec![Root {
                path: "/r".into(),
                source: Source::Walk,
            }],
            files: Vec::new(),
            too_large: 0,
            taken_at: at(5_000, 0),
            fingerprint,
            newest_coarse_mtime,
        }
    }

    fn stamp(fingerprint: u64, taken_secs: u64) -> Stamp {
        Stamp {
            fingerprint,
            taken_at: at(taken_secs, 500),
            index_bytes: 4096,
            heads: Vec::new(),
        }
    }

    #[test]
    fn a_stamp_round_trips() {
        let s = Stamp {
            fingerprint: 0x0123_4567_89ab_cdef,
            taken_at: at(1_759_680_000, 123_456_789),
            index_bytes: 10_517_262,
            heads: vec![
                ("deadbeef".into(), r"C:\code\my project".into()),
                ("cafe".into(), "/srv/a b/c".into()),
            ],
        };
        assert_eq!(parse(&render(&s)), Some(s));
        // A fingerprint with leading zeros keeps them.
        let low = Stamp {
            fingerprint: 7,
            ..stamp(0, 1)
        };
        assert_eq!(parse(&render(&low)), Some(low));
    }

    #[test]
    fn anything_unreadable_is_no_stamp_at_all() {
        let good = render(&stamp(42, 1_000));
        assert!(parse(&good).is_some());
        // A stamp from before this format, or from nothing we know.
        assert_eq!(parse("csearch-rs stamp 1\ndeadbeef\t42\t/r\n"), None);
        assert_eq!(parse("something else\n"), None);
        assert_eq!(parse(""), None);
        // Damaged fields.
        assert_eq!(parse(&good.replace("fingerprint ", "fingerprint zz")), None);
        assert_eq!(parse(&good.replace("index ", "index x")), None);
        assert_eq!(
            parse(&good.replace(" 500\n", " 1000000000\n")),
            None,
            "a nanosecond count that is not one"
        );
        // Missing fields: each of the three is required.
        for missing in ["fingerprint ", "taken ", "index "] {
            let without: String = good
                .lines()
                .filter(|l| !l.starts_with(missing))
                .map(|l| format!("{l}\n"))
                .collect();
            assert_ne!(
                without, good,
                "{missing:?} was not in the stamp to begin with"
            );
            assert_eq!(
                parse(&without),
                None,
                "accepted a stamp with no {missing:?}"
            );
        }
        // A field from some later version is skipped, not fatal.
        let extended = good.replace("taken ", "colour blue\ntaken ");
        assert_eq!(parse(&extended), parse(&good));
    }

    #[test]
    fn unchanged_means_same_fingerprint_and_no_recent_coarse_write() {
        let built = stamp(42, 1_000);
        assert_eq!(compare(Some(&built), &snap(42, None)), Verdict::Current);

        // No stamp, or a different listing: rebuild.
        assert!(matches!(compare(None, &snap(42, None)), Verdict::Stale(_)));
        assert!(matches!(
            compare(Some(&built), &snap(43, None)),
            Verdict::Stale(_)
        ));

        // Whole-second timestamps. A file last written well before the
        // listing began can be trusted...
        let old = Some(at(990, 0));
        assert_eq!(compare(Some(&built), &snap(42, old)), Verdict::Current);
        // ...one written within a tick of it, or after it, cannot: the same
        // second could hold another write.
        for recent in [at(999, 0), at(1_000, 0), at(1_001, 0), at(2_000, 0)] {
            assert!(
                matches!(
                    compare(Some(&built), &snap(42, Some(recent))),
                    Verdict::Stale(_)
                ),
                "{recent:?}"
            );
        }
        // Exactly one tick back is the boundary: 998 s + 2 s is not later
        // than a stamp taken at 1000 s and a bit.
        assert_eq!(
            compare(Some(&built), &snap(42, Some(at(998, 0)))),
            Verdict::Current
        );
    }

    #[test]
    fn a_stamp_beside_a_different_index_vouches_for_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index");
        let now = snap(42, None);

        // No index at all.
        store(&index, &stamp(42, 1_000));
        assert!(matches!(check(&index, &now).0, Verdict::Stale(_)));

        // The index the stamp was written for.
        std::fs::write(&index, vec![0u8; 4096]).unwrap();
        let (verdict, recorded) = check(&index, &now);
        assert_eq!(verdict, Verdict::Current);
        assert_eq!(recorded, Some(stamp(42, 1_000)));

        // Some other file in its place.
        std::fs::write(&index, vec![0u8; 4097]).unwrap();
        assert!(matches!(check(&index, &now).0, Verdict::Stale(_)));
    }
}
