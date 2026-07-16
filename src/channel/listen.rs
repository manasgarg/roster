//! The channel-listener half of `roster server start`: the inbound clients
//! (Discord gateway, Slack Socket Mode). Each dials out with the worker's bot
//! credential from the vault, discovers what it can see, and turns messages
//! into content tasks or warm-session turns for the worker.

use crate::channel::{discord, slack};
use crate::paths;
use crate::util::{now_rfc3339, BErr};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// (worker, platform, vault credential) triples — straight from live config.
pub fn plan() -> Vec<(String, String, String)> {
    crate::config::snapshot()
        .map(|c| c.listeners.clone())
        .unwrap_or_default()
}

/// Run one worker's listener on one platform forever, restarting with backoff.
/// An exit or an error never takes the gateway or dispatch down with it.
pub async fn supervised(worker: String, platform: String, credential: String) {
    let mut backoff = 5u64;
    loop {
        match listen_worker(&worker, &platform, &credential).await {
            // The gateway/socket loop handles transient reconnects internally and
            // only RETURNS on a fatal stop (bad token, disallowed intent). Re-
            // dialing would just re-hit it and hammer the platform, so stop.
            Ok(()) => {
                eprintln!("listener {worker}/{platform}: stopped (fatal — see the reason above)");
                return;
            }
            // A setup error (e.g. the credential isn't connected yet) may resolve,
            // so retry those with backoff.
            Err(e) => eprintln!("listener {worker}/{platform}: {e}; retrying in {backoff}s"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(300);
    }
}

/// Run one worker's inbound client until it exits. The lock guarantees one
/// listener per (worker, platform) across processes (a second `server start`
/// must not double-connect a bot and double-file tasks).
pub async fn listen_worker(worker: &str, platform: &str, credential: &str) -> Result<(), BErr> {
    let _listener_lock = ListenerLock::acquire(worker, platform)?;

    let cred = crate::credential::vault::get_credential(credential).ok_or_else(|| {
        format!(
            "no \"{credential}\" credential in the vault — run: roster connection add {platform}"
        )
    })?;
    let field = |name: &str| -> Result<String, BErr> {
        Ok(cred
            .get(name)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{platform} credential has no {name}"))?
            .to_string())
    };

    match platform {
        "discord" => {
            let token = field("token")?;
            eprintln!("listener {worker}: connecting the Discord gateway");
            discord::run_gateway(worker, &token).await;
            Ok(())
        }
        "slack" => {
            let bot_token = field("bot_token")?;
            let app_token = field("app_token")?;
            eprintln!("listener {worker}: connecting Slack Socket Mode");
            slack::run_socket_mode(worker, &bot_token, &app_token).await;
            Ok(())
        }
        other => Err(format!("unknown channel platform \"{other}\"").into()),
    }
}

/// The listener locks currently held (label, pid, since, alive) — for status.
/// The label is "worker" or "worker/platform" for non-discord platforms.
pub fn active_listeners() -> Vec<(String, u32, String, bool)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::locks_dir()) {
        for entry in entries.filter_map(|e| e.ok()) {
            if let Some(record) = read_lock(&entry.path()) {
                let alive = process_alive(record.pid);
                let label = if record.platform.is_empty() || record.platform == "discord" {
                    record.worker
                } else {
                    format!("{}/{}", record.worker, record.platform)
                };
                out.push((label, record.pid, record.started_at, alive));
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
    #[serde(default)]
    platform: String,
    started_at: String,
    token: String,
}

struct ListenerLock {
    path: PathBuf,
    token: String,
}

impl ListenerLock {
    fn acquire(worker: &str, platform: &str) -> Result<Self, BErr> {
        if worker.is_empty()
            || !worker
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("invalid worker name \"{worker}\"").into());
        }
        // Discord keeps the legacy `<worker>.lock` name; other platforms get
        // their own lock so one worker can listen on several.
        let lock_name = if platform == "discord" {
            worker.to_string()
        } else {
            format!("{worker}.{platform}")
        };
        Self::acquire_path(worker, platform, paths::worker_listener_lock(&lock_name))
    }

    fn acquire_path(worker: &str, platform: &str, path: PathBuf) -> Result<Self, BErr> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        for _ in 0..2 {
            let token = uuid::Uuid::new_v4().to_string();
            let record = LockRecord {
                pid: std::process::id(),
                worker: worker.to_string(),
                platform: platform.to_string(),
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
        let owned = read_lock(&self.path)
            .map(|record| record.token == self.token)
            .unwrap_or(false);
        if owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_lock(path: &Path) -> Option<LockRecord> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn process_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_lock_excludes_a_second_owner_and_cleans_up() {
        let path = std::env::temp_dir().join(format!(
            "roster-listener-test-{}.lock",
            uuid::Uuid::new_v4()
        ));
        let first = ListenerLock::acquire_path("yuko", "discord", path.clone()).unwrap();
        assert!(ListenerLock::acquire_path("yuko", "discord", path.clone()).is_err());
        drop(first);
        assert!(!path.exists());
        let second = ListenerLock::acquire_path("yuko", "slack", path.clone()).unwrap();
        drop(second);
        assert!(!path.exists());
    }

    #[test]
    fn stale_listener_lock_is_replaced() {
        let path = std::env::temp_dir().join(format!(
            "roster-listener-stale-{}.lock",
            uuid::Uuid::new_v4()
        ));
        let stale = LockRecord {
            pid: u32::MAX,
            worker: "yuko".into(),
            platform: "discord".into(),
            started_at: "old".into(),
            token: "old".into(),
        };
        std::fs::write(&path, serde_json::to_string(&stale).unwrap()).unwrap();
        let lock = ListenerLock::acquire_path("yuko", "discord", path.clone()).unwrap();
        drop(lock);
        assert!(!path.exists());
    }
}
