//! The worker's durable store — the auto-provisioned rw host-dir connection
//! every worker gets (docs/plans/worker-environment.md). A plain directory
//! under `data/workers/<name>/store/`, bind-mounted read-write at
//! `$HOME/store` in every run; the layout inside is the worker's own.
//!
//! The host treats the store as INERT BYTES — a standing rule, not a habit:
//! rsync it, list it, back it up; never run git in it, never parse it, never
//! execute from it. A box-written `.git/config` or hook is an execution
//! vector, and this rule is the whole defense.
//!
//! Coordination between concurrent instances is `flock(2)` under
//! `store/.locks/` (the box helper `roster-lock` and the host's snapshot
//! pass both use it): bind mounts share the inode, so a lock taken in one
//! box excludes every other box and the host, and the kernel releases it if
//! the holder dies.

use std::path::{Path, PathBuf};

/// The lock name the backup pass and `roster-lock` agree on for whole-store
/// operations. Lives inside the store so every box sees the same inode.
pub const STORE_LOCK: &str = ".locks/store";

pub fn store_dir(worker: &str) -> PathBuf {
    crate::paths::worker_store_dir(crate::paths::short_worker(worker))
}

/// Ensure the store (and its `.locks/`) exists. Idempotent; called by every
/// run provision and by `worker init`.
pub fn provision(worker: &str) -> Result<PathBuf, String> {
    let dir = store_dir(worker);
    std::fs::create_dir_all(dir.join(".locks"))
        .map_err(|e| format!("store {}: {e}", dir.display()))?;
    Ok(dir)
}

/// What a snapshot pass did. `changes` counts rsync-itemized entries against
/// the previous snapshot — the run's "what did it change" audit surface.
#[derive(Debug)]
pub struct SnapshotOutcome {
    pub dir: PathBuf,
    pub changes: usize,
}

fn snapshots_dir(worker: &str) -> PathBuf {
    crate::paths::worker_store_snapshots_dir(crate::paths::short_worker(worker))
}

/// Newest-first snapshot names (timestamps sort lexicographically).
pub fn list_snapshots(worker: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(snapshots_dir(worker))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with(".tmp"))
        .collect();
    names.sort();
    names.reverse();
    names
}

/// Snapshot the store: rsync with `--link-dest` against the newest previous
/// snapshot, so N snapshots cost roughly one full copy plus deltas. Holds
/// the store lock for the copy — a git repo inside is never captured
/// mid-ref-write, and a box holding `roster-lock store` blocks the pass.
///
/// A snapshot identical to the previous one is discarded (`Ok(None)`), so an
/// idle worker's per-run passes don't rotate real history away. With a
/// `run_id`, the itemized change list also lands in the run dir as
/// `store-changes.txt`. `keep = 0` disables snapshotting entirely.
pub fn snapshot(
    worker: &str,
    run_id: Option<&str>,
    keep: usize,
) -> Result<Option<SnapshotOutcome>, String> {
    if keep == 0 {
        return Ok(None);
    }
    let store = store_dir(worker);
    if !store.is_dir() {
        return Ok(None);
    }
    let _lock = store_lock(worker)?;
    let outcome = snapshot_locked(worker, &store, run_id)?;
    prune(worker, keep);
    Ok(outcome)
}

fn store_lock(worker: &str) -> Result<crate::statefile::FileLock, String> {
    let path = store_dir(worker).join(STORE_LOCK);
    crate::statefile::FileLock::acquire_path(&path).map_err(|e| format!("store lock: {e}"))
}

fn snapshot_locked(
    worker: &str,
    store: &Path,
    run_id: Option<&str>,
) -> Result<Option<SnapshotOutcome>, String> {
    let root = snapshots_dir(worker);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let prev = list_snapshots(worker).into_iter().next();
    // Fixed-width UTC name so lexicographic order IS chronological order
    // (list_snapshots and prune depend on that); milliseconds keep two passes
    // in one second (run end + sweep) from colliding.
    let now = time::OffsetDateTime::now_utc();
    let name = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}.{:03}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    );
    let final_dir = root.join(&name);
    if final_dir.exists() {
        // Two passes inside one second (run end + sweep): the store didn't
        // change in between, nothing to record.
        return Ok(None);
    }
    // Copy into a .tmp name and rename at the end: a crash mid-copy must
    // never leave something list_snapshots would count as history.
    let tmp = root.join(format!("{name}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cmd = std::process::Command::new("rsync");
    cmd.arg("-a")
        .arg("--delete")
        .arg("--itemize-changes")
        // The store is inert bytes, but locks are coordination state, not
        // content — a snapshot (and a restore) must not carry them.
        .arg("--exclude=/.locks");
    if let Some(p) = &prev {
        cmd.arg(format!("--link-dest={}", root.join(p).display()));
    }
    let out = cmd
        .arg(format!("{}/", store.display()))
        .arg(&tmp)
        .output()
        .map_err(|e| format!("rsync: {e} (is rsync installed?)"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "rsync failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // With --link-dest, unchanged files produce no itemize output — the lines
    // ARE the delta. Keep only real itemize records (update-type char, then
    // file-type char — e.g. ">f", "cd", "*deleting"), dropping rsync's info
    // messages ("created directory …") and directory-timestamp noise (".d").
    let itemized: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            let b = l.as_bytes();
            l.starts_with("*deleting")
                || (b.len() > 11
                    && matches!(b[0], b'<' | b'>' | b'c' | b'h' | b'.')
                    && matches!(b[1], b'f' | b'd' | b'L' | b'D' | b'S')
                    && !l.starts_with(".d"))
        })
        .collect();
    if prev.is_some() && itemized.is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(None);
    }
    std::fs::rename(&tmp, &final_dir).map_err(|e| e.to_string())?;
    if let Some(rid) = run_id {
        let _ = std::fs::write(
            crate::paths::run_dir(rid).join("store-changes.txt"),
            itemized.join("\n") + "\n",
        );
    }
    Ok(Some(SnapshotOutcome {
        dir: final_dir,
        changes: itemized.len(),
    }))
}

fn prune(worker: &str, keep: usize) {
    let root = snapshots_dir(worker);
    for name in list_snapshots(worker).into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(root.join(name));
    }
}

/// Restore the store from a snapshot (the newest when `from` is None). The
/// current state is snapshotted first — a restore is always undoable by
/// another restore. Returns (restored-from, undo-snapshot-if-any).
pub fn restore(
    worker: &str,
    from: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let snaps = list_snapshots(worker);
    let pick = match from {
        Some(name) => snaps
            .iter()
            .find(|n| n.as_str() == name)
            .ok_or_else(|| {
                format!(
                    "no snapshot \"{name}\" — have: {}",
                    if snaps.is_empty() { "none".into() } else { snaps.join(", ") }
                )
            })?
            .clone(),
        None => snaps
            .first()
            .ok_or("no snapshots yet — nothing to restore from")?
            .clone(),
    };
    let store = provision(worker)?;
    let _lock = store_lock(worker)?;
    let undo = snapshot_locked(worker, &store, None)?.map(|o| o.dir);
    let src = snapshots_dir(worker).join(&pick);
    let out = std::process::Command::new("rsync")
        .arg("-a")
        .arg("--delete")
        .arg("--exclude=/.locks")
        .arg(format!("{}/", src.display()))
        .arg(&store)
        .output()
        .map_err(|e| format!("rsync: {e} (is rsync installed?)"))?;
    if !out.status.success() {
        return Err(format!(
            "rsync failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok((src, undo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_is_idempotent_and_creates_locks() {
        let _guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());
        let p = provision("yuko").unwrap();
        assert!(p.join(".locks").is_dir());
        let p2 = provision("org/yuko").unwrap();
        assert_eq!(p, p2, "org/ prefix resolves to the same store");
    }

    #[test]
    fn snapshot_rotate_restore_lifecycle() {
        if std::process::Command::new("rsync")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping — rsync not installed");
            return;
        }
        let _guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());
        let store = provision("yuko").unwrap();

        std::fs::write(store.join("notes.md"), "v1").unwrap();
        let first = snapshot("yuko", None, 5).unwrap().expect("first snapshot");
        assert!(first.changes >= 1);
        assert_eq!(
            std::fs::read_to_string(first.dir.join("notes.md")).unwrap(),
            "v1"
        );
        assert!(!first.dir.join(".locks").exists(), "locks are not content");

        // Unchanged store → no new snapshot piles up.
        assert!(snapshot("yuko", None, 5).unwrap().is_none());

        // Different length: rsync's quick-check (size+mtime, second
        // granularity — the standard backup trade-off) must see the change
        // even when the test runs sub-second.
        std::fs::write(store.join("notes.md"), "v2 with more text").unwrap();
        let second = snapshot("yuko", None, 5).unwrap().expect("second snapshot");
        assert_eq!(second.changes, 1);
        assert_eq!(list_snapshots("yuko").len(), 2);

        // Restore the older state by name; the pre-restore state is
        // auto-snapshotted, so the restore itself is undoable.
        let older = list_snapshots("yuko").pop().unwrap();
        std::fs::write(store.join("notes.md"), "wrecked-by-a-bad-run").unwrap();
        let (from, undo) = restore("yuko", Some(&older)).unwrap();
        assert!(from.ends_with(&older));
        assert!(undo.is_some(), "wrecked state was preserved for undo");
        assert_eq!(
            std::fs::read_to_string(store.join("notes.md")).unwrap(),
            "v1"
        );
        assert!(store.join(".locks").is_dir(), "restore keeps the lock dir");

        // keep=1 prunes history down to the newest.
        std::fs::write(store.join("notes.md"), "v3").unwrap();
        snapshot("yuko", None, 1).unwrap().expect("third snapshot");
        assert_eq!(list_snapshots("yuko").len(), 1);

        // keep=0 disables the pass entirely.
        std::fs::write(store.join("notes.md"), "v4").unwrap();
        assert!(snapshot("yuko", None, 0).unwrap().is_none());
    }
}
