//! Run provenance. Every run records WHO was in the room (provider, channel,
//! user, role) — the input to the person-space boundary: tainted runs get
//! gated repos read-only, and the participant scan (worker/boundary.rs) keys
//! off these identifiers. Interaction memory itself lives in the worker's
//! store (`store/memory/`, docs/store.md), owned and organized by the worker;
//! the host keeps no memory machinery.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunContext {
    pub provider: String,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
    pub message_id: Option<String>,
    /// Slack thread the inbound message belongs to (its own ts, or the parent's).
    /// Carried so a reply lands back in the thread, not the channel top level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    pub role: String,
    pub is_dm: bool,
    /// The run's prompt embeds inbound third-party content (a relay task) —
    /// person-tainted even without channel/user identifiers.
    pub inbound: bool,
}

impl RunContext {
    /// Did interaction content or context enter this run? Tainted runs get
    /// gated repos read-only (docs/repos.md). One predicate, shared by
    /// provisioning and the push gate.
    pub fn tainted(&self) -> bool {
        self.channel_id.is_some() || self.user_id.is_some() || self.inbound
    }

    /// A deliberately-tainted context for when a run's real context can't be
    /// read. The participant scan then runs (fail closed) instead of being
    /// silently skipped, so an unreadable context file cannot disable the
    /// person-space boundary. `inbound` is the only field `tainted()` reads.
    pub fn tainted_unknown() -> Self {
        RunContext {
            inbound: true,
            ..Default::default()
        }
    }
}

fn run_context_path(run_id: &str) -> PathBuf {
    paths::run_dir(run_id).join("run-context.json")
}

/// The context file's pre-store name. Read-only fallback so runs recorded
/// before the rename keep their provenance.
fn legacy_run_context_path(run_id: &str) -> PathBuf {
    paths::run_dir(run_id).join("memory-context.json")
}

pub fn save_run_context(run_id: &str, context: &RunContext) -> Result<(), String> {
    if run_id.is_empty() {
        return Ok(());
    }
    let path = run_context_path(run_id);
    let dir = path.parent().ok_or("bad run context path")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        format!(
            "{}\n",
            serde_json::to_string_pretty(context).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

pub fn load_run_context(run_id: &str) -> RunContext {
    if run_id.is_empty() {
        // No run identity to key a context on (host-op / CLI paths). This is a
        // legitimate absence, not a failure, and carries no interaction content.
        return RunContext::default();
    }
    let path = if run_context_path(run_id).exists() {
        run_context_path(run_id)
    } else {
        legacy_run_context_path(run_id)
    };
    match crate::statefile::read_if_present(&path) {
        Ok(Some(s)) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("run context for {run_id} is corrupt ({e}); treating run as tainted");
            RunContext::tainted_unknown()
        }),
        // A dispatched run always writes its context; a missing or unreadable
        // one means that write was lost. Fail closed so the participant scan
        // still runs, rather than silently disabling the boundary.
        Ok(None) => {
            eprintln!("no run context for {run_id}; treating run as tainted");
            RunContext::tainted_unknown()
        }
        Err(e) => {
            eprintln!("could not read run context for {run_id} ({e}); treating run as tainted");
            RunContext::tainted_unknown()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_context_roundtrips_and_fails_tainted() {
        let _guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());

        let ctx = RunContext {
            provider: "discord".into(),
            channel_id: Some("c1".into()),
            user_id: Some("u1".into()),
            role: "trusted".into(),
            ..Default::default()
        };
        save_run_context("r1", &ctx).unwrap();
        let loaded = load_run_context("r1");
        assert!(loaded.tainted());
        assert_eq!(loaded.channel_id.as_deref(), Some("c1"));

        // Missing context fails closed: tainted, not clean.
        assert!(load_run_context("r-missing").tainted());
        // The CLI/host-op path (no run id) is legitimately clean.
        assert!(!load_run_context("").tainted());
    }
}
