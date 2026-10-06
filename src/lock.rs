//! One refresh of an index at a time.
//!
//! Two builds of the same index used to write the same temporary file, and
//! whichever finished second installed whatever the two had made of it. That
//! was survivable while the only triggers were a person and git's four hooks.
//! With a hook on every command of a version-control client it is routine, so
//! a refresh now holds `<index>.lock` from before it lists the files until
//! after it has written the stamp.
//!
//! The lock is the kernel's, not a file that says "locked": `flock` on Unix,
//! an open that refuses other writers on Windows. It vanishes with the
//! process that holds it, however that process ends, so there is no such
//! thing as a stale lock and nothing to clean up. The files themselves are
//! left in place -- removing a lock file is how lock files stop working.
//!
//! If the file system cannot lock at all (some network mounts), the refresh
//! goes ahead unlocked, as it always used to.

use crate::paths::sidecar;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Held for as long as this value lives.
#[derive(Debug)]
pub struct Lock {
    _file: Option<File>,
}

impl Lock {
    /// Give the lock up for good and remove its files: for `--reset`, which
    /// is deleting the index they belong to.
    ///
    /// The order is what keeps this from being the classic way to break a
    /// lock file. On Unix the files are unlinked while the lock is still
    /// held: nobody else can be holding them, and whoever comes next creates
    /// fresh ones. Windows will not delete an open file, so there the lock is
    /// let go first -- and if someone takes it in that instant, the delete
    /// simply fails and the file stays, which is correct.
    pub fn retire(self, index: &Path) {
        let files = [sidecar(index, "lock"), sidecar(index, "queue")];
        if cfg!(unix) {
            for f in &files {
                let _ = std::fs::remove_file(f);
            }
            drop(self);
        } else {
            drop(self);
            for f in &files {
                let _ = std::fs::remove_file(f);
            }
        }
    }
}

/// How often to look again while waiting.
const POLL: Duration = Duration::from_millis(50);

/// How long a taken queue slot is watched before concluding that someone
/// really is waiting in it, rather than passing through.
const SLOT_PATIENCE: Duration = Duration::from_millis(200);

/// Take `path` exclusively without waiting: `Ok(None)` if someone else has it.
#[cfg(unix)]
fn try_exclusive(path: &Path) -> io::Result<Option<File>> {
    use std::os::unix::io::AsRawFd;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: flock is given a descriptor this function owns and keeps open
    // across the call. It reads and writes no memory of ours.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(err)
    }
}

/// Take `path` exclusively without waiting: `Ok(None)` if someone else has it.
#[cfg(windows)]
fn try_exclusive(path: &Path) -> io::Result<Option<File>> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    // Opened for writing and shared for reading only: a second open for
    // writing is refused for as long as this handle lives, and Windows closes
    // the handle when the process goes away. Readers are let in on purpose --
    // a virus scanner glancing at the file must not look like a lock holder.
    match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(not(any(unix, windows)))]
fn try_exclusive(_: &Path) -> io::Result<Option<File>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no file locking",
    ))
}

/// Wait for `path`, up to `limit`. `Ok(None)` means the limit passed.
/// `waiting` is called once, if the wait reaches a second.
fn wait_exclusive(
    path: &Path,
    limit: Option<Duration>,
    mut waiting: impl FnMut(),
) -> io::Result<Option<File>> {
    let started = Instant::now();
    let mut announced = false;
    loop {
        if let Some(file) = try_exclusive(path)? {
            return Ok(Some(file));
        }
        // Before the limit is looked at, so that a wait of a second or more
        // is always announced -- including one that then gives up -- however
        // unevenly the polls happen to fall.
        if !announced && started.elapsed() >= Duration::from_secs(1) {
            waiting();
            announced = true;
        }
        if limit.is_some_and(|l| started.elapsed() >= l) {
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
}

/// Wait for the index's refresh lock, however long that takes. `waiting` is
/// called once if another refresh turns out to be running.
///
/// For a command a person ran: it may be adding or removing a root, and that
/// must happen, not be skipped because something else was busy.
pub fn acquire(index: &Path, waiting: impl FnMut()) -> Lock {
    // An error means locking is unavailable here, not that the lock is taken:
    // carry on without one rather than refuse to index.
    let file = wait_exclusive(&sidecar(index, "lock"), None, waiting).unwrap_or(None);
    Lock { _file: file }
}

/// The result of asking for the lock on behalf of a hook.
#[derive(Debug)]
pub enum Turn {
    /// The lock is held; go ahead.
    Go(Lock),
    /// Another refresh is already waiting behind the one that is running. It
    /// has not looked at the files yet, so whatever prompted this call will be
    /// in what it sees: there is nothing left to do.
    Covered,
    /// The refresh ahead did not finish within the limit.
    GaveUp,
}

/// Next in line for the lock: the holder of the queue slot.
#[derive(Debug)]
pub struct Queued {
    slot: Option<File>,
    lock: PathBuf,
}

/// Step into the queue for `index`'s lock, or learn that the queue is full --
/// it holds one. `None` is [`Turn::Covered`].
pub fn queue_up(index: &Path) -> Option<Queued> {
    let lock = sidecar(index, "lock");
    // A slot that is taken for a moment is someone on their way through to
    // the lock; one that stays taken is someone waiting.
    match wait_exclusive(&sidecar(index, "queue"), Some(SLOT_PATIENCE), || {}) {
        Ok(Some(slot)) => Some(Queued {
            slot: Some(slot),
            lock,
        }),
        Ok(None) => None,
        Err(_) => Some(Queued { slot: None, lock }), // no locking here: just go
    }
}

impl Queued {
    /// Wait for the running refresh to finish and take the lock.
    pub fn wait(self, limit: Duration) -> Turn {
        if self.slot.is_none() {
            return Turn::Go(Lock { _file: None });
        }
        let turn = match wait_exclusive(&self.lock, Some(limit), || {}) {
            Ok(Some(file)) => Turn::Go(Lock { _file: Some(file) }),
            Ok(None) => Turn::GaveUp,
            Err(_) => Turn::Go(Lock { _file: None }),
        };
        // The slot is given up only now -- with the lock in hand, and before
        // the caller lists a single file. Whoever found the slot taken did so
        // before this point, and the listing comes after it; that ordering is
        // the whole of what makes `Covered` true.
        drop(self.slot);
        turn
    }
}

/// Take a turn at refreshing `index`, on behalf of a hook.
///
/// Hooks arrive in bursts -- a rebase fires several, a wrapped client fires
/// one per command -- and each wants the same thing: the index to reflect the
/// files as they are now. So at most one refresh runs and at most one waits;
/// any further caller is told its event is already [`Turn::Covered`]. The
/// number of processes per index stays at two however fast events arrive.
pub fn take_turn(index: &Path, limit: Duration) -> Turn {
    match queue_up(index) {
        Some(queued) => queued.wait(limit),
        None => Turn::Covered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("index")
    }

    #[test]
    fn the_lock_excludes_and_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = sidecar(&index_in(&dir), "lock");
        let held = try_exclusive(&path).unwrap().expect("free at first");
        assert!(
            try_exclusive(&path).unwrap().is_none(),
            "a second taker must be refused while the first holds it"
        );
        drop(held);
        assert!(
            try_exclusive(&path).unwrap().is_some(),
            "and admitted once the first lets go"
        );
    }

    #[test]
    fn retiring_the_lock_removes_its_files_but_not_one_somebody_holds() {
        let dir = tempfile::tempdir().unwrap();
        let index = index_in(&dir);
        let (lock_file, queue_file) = (sidecar(&index, "lock"), sidecar(&index, "queue"));

        // Used by a hook, so both files exist; then retired with nobody else
        // about: both go.
        let Turn::Go(held) = take_turn(&index, Duration::from_secs(5)) else {
            panic!("free at first");
        };
        assert!(lock_file.is_file() && queue_file.is_file());
        held.retire(&index);
        assert!(!lock_file.exists() && !queue_file.exists());

        // The lock works again afterwards, on fresh files.
        let again = acquire(&index, || {});
        assert!(lock_file.is_file());
        assert!(try_exclusive(&lock_file).unwrap().is_none());
        drop(again);
    }

    #[test]
    fn a_reader_does_not_look_like_a_lock_holder() {
        // Something else having the file open to read -- a scanner, a backup
        // tool, `cat` -- is not a refresh in progress.
        let dir = tempfile::tempdir().unwrap();
        let path = sidecar(&index_in(&dir), "lock");
        drop(try_exclusive(&path).unwrap());
        let _reader = File::open(&path).unwrap();
        assert!(try_exclusive(&path).unwrap().is_some());
    }

    #[test]
    fn waiting_gives_up_at_the_limit_and_announces_itself_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = sidecar(&index_in(&dir), "lock");
        let _held = try_exclusive(&path).unwrap().unwrap();
        let mut said = 0;
        let got = wait_exclusive(&path, Some(Duration::from_millis(1300)), || said += 1).unwrap();
        assert!(got.is_none(), "the lock was never released");
        assert_eq!(said, 1, "one notice, after the first second");
    }

    #[test]
    fn a_third_refresh_is_covered_by_the_one_already_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let index = index_in(&dir);
        let limit = Duration::from_secs(30);

        // One refresh is running.
        let Turn::Go(running) = take_turn(&index, limit) else {
            panic!("an idle index must be free to refresh");
        };
        // A second arrives and takes the one place in the queue.
        let second = queue_up(&index).expect("the queue was empty");
        // A third has nothing to add, and is told so without waiting long.
        let asked = Instant::now();
        assert!(matches!(take_turn(&index, limit), Turn::Covered));
        assert!(asked.elapsed() < Duration::from_secs(5));

        // The second waits for the first...
        let waiter = std::thread::spawn(move || second.wait(limit));
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !waiter.is_finished(),
            "it must not start while one is running"
        );
        // All the while it waits, it keeps its place: anyone else arriving
        // now is still covered by it, not admitted to wait alongside.
        assert!(matches!(take_turn(&index, Duration::ZERO), Turn::Covered));
        // ...and goes when the first is done.
        drop(running);
        let Turn::Go(second) = waiter.join().unwrap() else {
            panic!("the queued refresh never got its turn");
        };

        // With the lock in hand the second has left the queue, so a newcomer
        // is admitted to it instead of being turned away -- shown here by
        // giving that newcomer no time to wait.
        assert!(matches!(take_turn(&index, Duration::ZERO), Turn::GaveUp));
        drop(second);
        assert!(matches!(take_turn(&index, limit), Turn::Go(_)));
    }
}
