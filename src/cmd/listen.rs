//! The channel-listener half of `roster server run`: the Discord gateway
//! client (inbound). Dials out to Discord with the worker's bot token from the
//! vault, discovers the channels it can see, and turns messages into content
//! tasks or warm-session turns for the worker.

use crate::discord;
use crate::paths;
use crate::util::now_rfc3339;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

type BErr = Box<dyn std::error::Error>;

/// Run the inbound Discord gateway for one worker until it exits. The lock
/// guarantees one listener per worker across processes (a second `server run`
/// must not double-connect the bot and double-file tasks).
pub async fn listen_worker(worker: &str, credential: &str) -> Result<(), BErr> {
    let _listener_lock = ListenerLock::acquire(worker)?;

    let cred = crate::vault::get_credential(credential)
        .ok_or_else(|| format!("no \"{credential}\" credential in the vault — run: roster server vault connect discord"))?;
    let token = cred.get("token").and_then(|v| v.as_str()).ok_or("discord credential has no token")?.to_string();

    eprintln!("listener {worker}: connecting the Discord gateway");
    discord::run_gateway(worker, &token).await;
    Ok(())
}

/// The listener locks currently held (worker, pid, since, alive) — for status.
pub fn active_listeners() -> Vec<(String, u32, String, bool)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::locks_dir()) {
        for entry in entries.filter_map(|e| e.ok()) {
            if let Some(record) = read_lock(&entry.path()) {
                let alive = process_alive(record.pid);
                out.push((record.worker, record.pid, record.started_at, alive));
            }
        }
    }
    out.sort();
    out
}

#[derive(Debug, Serialize, Deserialize)]
struct LockRecord {
    pid: u32,
    worker: String,
    started_at: String,
    token: String,
}

struct ListenerLock {
    path: PathBuf,
    token: String,
}

impl ListenerLock {
    fn acquire(worker: &str) -> Result<Self, BErr> {
        if worker.is_empty() || !worker.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(format!("invalid worker name \"{worker}\"").into());
        }
        Self::acquire_path(worker, paths::worker_listener_lock(worker))
    }

    fn acquire_path(worker: &str, path: PathBuf) -> Result<Self, BErr> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        for _ in 0..2 {
            let token = uuid::Uuid::new_v4().to_string();
            let record = LockRecord {
                pid: std::process::id(),
                worker: worker.to_string(),
                started_at: now_rfc3339(),
                token: token.clone(),
            };
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", serde_json::to_string(&record)?)?;
                    return Ok(Self { path, token });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(existing) = read_lock(&path) {
                        if process_alive(existing.pid) {
                            return Err(format!(
                                "listener for worker {worker} is already running as pid {} (since {})",
                                existing.pid, existing.started_at
                            )
                            .into());
                        }
                    }
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(format!("could not acquire listener lock for {worker}").into())
    }
}

impl Drop for ListenerLock {
    fn drop(&mut self) {
        let owned = read_lock(&self.path).map(|record| record.token == self.token).unwrap_or(false);
        if owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_lock(path: &Path) -> Option<LockRecord> {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok())
}

fn process_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_lock_excludes_a_second_owner_and_cleans_up() {
        let path = std::env::temp_dir().join(format!("roster-listener-test-{}.lock", uuid::Uuid::new_v4()));
        let first = ListenerLock::acquire_path("yuko", path.clone()).unwrap();
        assert!(ListenerLock::acquire_path("yuko", path.clone()).is_err());
        drop(first);
        assert!(!path.exists());
        let second = ListenerLock::acquire_path("yuko", path.clone()).unwrap();
        drop(second);
        assert!(!path.exists());
    }

    #[test]
    fn stale_listener_lock_is_replaced() {
        let path = std::env::temp_dir().join(format!("roster-listener-stale-{}.lock", uuid::Uuid::new_v4()));
        let stale = LockRecord {
            pid: u32::MAX,
            worker: "yuko".into(),
            started_at: "old".into(),
            token: "old".into(),
        };
        std::fs::write(&path, serde_json::to_string(&stale).unwrap()).unwrap();
        let lock = ListenerLock::acquire_path("yuko", path.clone()).unwrap();
        drop(lock);
        assert!(!path.exists());
    }
}
