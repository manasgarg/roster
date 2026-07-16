//! Durable state primitives shared by every multi-writer store: a
//! cross-process advisory lock and a crash-safe atomic write.
//!
//! Roster keeps its live state as plain files that several processes touch at
//! once — the daemon, a hand-run `roster` CLI, and the Discord/Slack slash
//! handlers inside the daemon. A per-process `Mutex` cannot coordinate them,
//! so every read-modify-write on those files was a lost-update (or, for gates,
//! a double-execute) race. This module is the one locking discipline they all
//! use instead.
//!
//! The lock is a `flock(2)` on a file under `state/locks/`. Two properties
//! matter: it is honored across processes (so the CLI and the daemon actually
//! exclude each other), and the kernel releases it when the holding fd closes —
//! including when the process dies. A crash therefore never wedges a lock, the
//! way the old `mkdir`-based knowledge lease could.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// One process-wide lock for tests that repoint the global `ROSTER_ROOT` env
/// var. Every test module that sets it must hold this for the test's duration,
/// or two modules running in parallel repoint each other's deployment mid-test.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// An exclusive advisory lock held for the lifetime of the guard. Dropping it
/// (or the process exiting) releases it. Acquire → mutate → drop; never hold
/// one across an `.await` you don't control.
#[must_use = "the lock releases as soon as the guard is dropped"]
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Take the exclusive lock named `name` (resolved under `state/locks/`),
    /// blocking until it is free. The name identifies a logical resource, not a
    /// data file — e.g. `"tms-yuko"`, `"gates-yuko"`, `"channels"`.
    pub fn acquire(name: &str) -> io::Result<FileLock> {
        Self::acquire_path(&crate::paths::lock_file(name))
    }

    /// Take the exclusive lock at an explicit path — for stores whose data file
    /// is redirected under test (the ledger), so the lock follows it into the
    /// temp dir instead of touching the real deployment. Pass a sibling of the
    /// data file (e.g. `usage.jsonl.lock`).
    pub fn acquire_path(path: &Path) -> io::Result<FileLock> {
        let file = Self::open_lock_file(path)?;
        // LOCK_EX blocks until no other open file description holds the lock,
        // across processes and across threads in this one.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileLock { _file: file })
    }

    /// Non-blocking exclusive lock: `Ok(None)` if someone else holds it right
    /// now. Lets a caller keep its own bounded-wait/timeout policy while still
    /// getting a lock the OS reclaims on crash.
    pub fn try_acquire_path(path: &Path) -> io::Result<Option<FileLock>> {
        let file = Self::open_lock_file(path)?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            // EWOULDBLOCK — held elsewhere; not an error, just "not now".
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err);
        }
        Ok(Some(FileLock { _file: file }))
    }

    fn open_lock_file(path: &Path) -> io::Result<File> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        OpenOptions::new()
            .create(true)
            // The file's contents are irrelevant — it exists only to hang a lock
            // on — so never truncate it (and never race to).
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
    }
}

/// Replace `path`'s contents atomically and durably: write a sibling temp file,
/// fsync it, rename it over the target, then fsync the directory so the rename
/// survives power loss. A reader either sees the whole old file or the whole new
/// one — never a truncated file, and never nothing.
///
/// Callers that share the file with other processes must hold the resource lock
/// around their read-modify-write; this function only makes the write itself
/// crash-safe, not the read-then-write concurrent.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    // The pid keeps concurrent writers to distinct temp files even if a caller
    // forgot the lock; the rename is what makes the swap atomic.
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => {}
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if let Ok(dirf) = File::open(dir) {
        let _ = dirf.sync_all();
    }
    Ok(())
}

/// Append one JSONL record durably. The caller holds the resource lock, so this
/// is the only writer at the moment it runs; a single `write_all` keeps the
/// line whole, and `sync_data` makes it durable before we tell anyone it
/// happened. `line` must not contain a newline; the terminator is added here.
pub fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    f.write_all(buf.as_bytes())?;
    f.sync_data()?;
    Ok(())
}

/// Read a file, telling apart "not there yet" (a fresh deployment — `Ok(None)`)
/// from "there but unreadable/corrupt" (`Err`). The second must never be
/// silently coerced to an empty default and written back over the real file.
pub fn read_if_present(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }
    fn env() -> Env {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());
        Env {
            _guard: guard,
            _dir: dir,
        }
    }

    #[test]
    fn atomic_write_roundtrips_and_replaces() {
        let _e = env();
        let p = crate::paths::state_root().join("t.json");
        write_atomic(&p, b"one").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one");
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        // no temp files left behind
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
    }

    #[test]
    fn read_if_present_distinguishes_absent_from_content() {
        let _e = env();
        let p = crate::paths::state_root().join("maybe.json");
        assert!(read_if_present(&p).unwrap().is_none());
        write_atomic(&p, b"hi").unwrap();
        assert_eq!(read_if_present(&p).unwrap().as_deref(), Some("hi"));
    }

    #[test]
    fn lock_is_reentrant_across_sequential_acquires() {
        let _e = env();
        // Sequential acquire/release of the same name must not deadlock.
        {
            let _g = FileLock::acquire("unit-test").unwrap();
        }
        let _g = FileLock::acquire("unit-test").unwrap();
    }

    #[test]
    fn lock_grants_mutual_exclusion_under_contention() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let _e = env();
        // Two threads race for the same lock; the count inside the critical
        // section must never exceed one if the lock actually excludes.
        let inside = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        // Resolve the lock path on this thread (it reads the ROSTER_ROOT env) and
        // hand the concrete path to the workers.
        let lock_path = crate::paths::lock_file("contended");
        let _ = std::fs::create_dir_all(lock_path.parent().unwrap());
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let inside = inside.clone();
                let max_seen = max_seen.clone();
                let lock_path = lock_path.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let _g = FileLock::acquire_path(&lock_path).unwrap();
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        std::thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1, "lock allowed overlap");
    }
}
