//! The on-disk layout, in one place. Every path the control plane reads or
//! writes is minted here, so the layout is defined in exactly one file.
//!
//! Roster follows the XDG Base Directory standard — the deployment lives
//! outside the code, in three roots:
//!
//!   config  $XDG_CONFIG_HOME/roster   (~/.config/roster)     hand-edited only
//!     org.toml  providers.toml  workers/<name>/{worker.toml,identity.md}
//!   data    $XDG_DATA_HOME/roster     (~/.local/share/roster)  durable — THE BACKUP SET
//!     vault/  ca/  channels/<id>/  audit/*.jsonl
//!     workers/<name>/{queue,journal,gates,memory.jsonl,knowledge}
//!   state   $XDG_STATE_HOME/roster    (~/.local/state/roster)  reconstructible/prunable
//!     runs/<run-id>/  identity/  locks/  outbox/  trigger-state.json
//!
//! Self-contained mode: if `ROSTER_ROOT` is set, the three roots are
//! `$ROSTER_ROOT/{config,data,state}` — for tests, scratch deployments, and
//! side-by-side instances. There is no config or state in the code checkout,
//! and the box mounts none of these directories — nothing to shadow.

use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn base(xdg_env: &str, xdg_default: &str, root_sub: &str) -> PathBuf {
    if let Ok(root) = std::env::var("ROSTER_ROOT") {
        return PathBuf::from(root).join(root_sub);
    }
    std::env::var(xdg_env)
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(xdg_default))
        .join("roster")
}

pub fn config_root() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config", "config")
}

pub fn data_root() -> PathBuf {
    base("XDG_DATA_HOME", ".local/share", "data")
}

pub fn state_root() -> PathBuf {
    base("XDG_STATE_HOME", ".local/state", "state")
}

/// A worker handle may arrive as a bare name ("yuko") or a subject
/// ("org/yuko"); directories are keyed by the bare name.
pub fn short_worker(worker: &str) -> &str {
    worker.rsplit('/').next().unwrap_or(worker)
}

// ── config (hand-edited; nothing here is machine-written) ────────────────────

pub fn org_file() -> PathBuf {
    config_root().join("org.toml")
}

/// Admin overlay on the provider registry shipped in the binary.
pub fn providers_file() -> PathBuf {
    config_root().join("providers.toml")
}

/// Service connections — one file per connected service (docs/connections.md),
/// machine-scaffolded by `roster connection add`, human-owned thereafter.
pub fn connections_dir() -> PathBuf {
    config_root().join("connections")
}

pub fn workers_dir() -> PathBuf {
    config_root().join("workers")
}

pub fn worker_dir(name: &str) -> PathBuf {
    workers_dir().join(short_worker(name))
}

// ── data: secrets ─────────────────────────────────────────────────────────────

pub fn vault_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ROSTER_VAULT_DIR") {
        return PathBuf::from(dir);
    }
    data_root().join("vault")
}

pub fn ca_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ROSTER_CA_DIR") {
        return PathBuf::from(dir);
    }
    data_root().join("ca")
}

// ── data: per-worker footprint (one subtree = one worker; D11 export) ────────

pub fn workers_data_dir() -> PathBuf {
    data_root().join("workers")
}

pub fn worker_data_dir(worker: &str) -> PathBuf {
    workers_data_dir().join(short_worker(worker))
}

pub fn worker_queue_dir(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("queue")
}

/// The TMS partition document (docs/work.md).
pub fn worker_tasks_file(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("tasks").join("tasks.json")
}

/// The TMS journal: completed/failed tasks and expired templates, append-only.
pub fn worker_tasks_journal(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("tasks").join("journal.jsonl")
}

/// The box-facing view of a worker's partition (state: rewritten in place on
/// every mutation so live bind mounts stay fresh; edits to it are scratch).
pub fn tms_view_file(worker: &str) -> PathBuf {
    state_root().join("tms-view").join(format!("{}.json", short_worker(worker)))
}

/// Recurrence cursors (last spawn per template) — reconstructible state.
/// Where the running daemon records its gateway binding (port, addresses,
/// config root, pid) — how the CLI, boxes, and probes find the gateway
/// without assuming the well-known port.
pub fn gateway_state_file() -> PathBuf {
    state_root().join("gateway.json")
}

pub fn tms_cursors_file() -> PathBuf {
    state_root().join("tms-cursors.json")
}

pub fn worker_journal_file(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("journal").join("events.jsonl")
}

pub fn worker_memory_file(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("memory.jsonl")
}

/// Pre-`memory.jsonl` interaction-memory log; read-only fallback until
/// `worker memory compact` finishes the physical migration.
pub fn worker_notes_legacy_file(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("notes-legacy.jsonl")
}

pub fn worker_knowledge_dir(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("knowledge")
}

pub fn worker_gates_pending_dir(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("gates").join("pending")
}

pub fn worker_gates_resolved_dir(worker: &str) -> PathBuf {
    worker_data_dir(worker).join("gates").join("resolved")
}

// ── data: channels and the permanent record ──────────────────────────────────

pub fn channels_dir() -> PathBuf {
    data_root().join("channels")
}

pub fn channel_dir(channel_id: &str) -> PathBuf {
    channels_dir().join(channel_id)
}

/// Human identity of a channel (platform, server, name) — written by the
/// listeners as they learn it, read wherever a bare id would be illegible.
pub fn channel_meta_file(channel_id: &str) -> PathBuf {
    channel_dir(channel_id).join("meta.json")
}

fn audit_dir() -> PathBuf {
    data_root().join("audit")
}

pub fn decisions_log() -> PathBuf {
    audit_dir().join("decisions.jsonl")
}

// Test builds redirect the ledger to a temp file (see gateway::ledger), so the
// real audit path goes unused there.
#[cfg_attr(test, allow(dead_code))]
pub fn usage_log() -> PathBuf {
    audit_dir().join("usage.jsonl")
}

pub fn credentials_log() -> PathBuf {
    audit_dir().join("credentials.jsonl")
}

pub fn messages_log() -> PathBuf {
    audit_dir().join("messages.jsonl")
}

// ── state: reconstructible or prunable ───────────────────────────────────────

pub fn runs_dir() -> PathBuf {
    state_root().join("runs")
}

pub fn run_dir(run_id: &str) -> PathBuf {
    runs_dir().join(run_id)
}

/// Ephemeral per-run box identity tokens (proxy credentials → subject).
pub fn identity_dir() -> PathBuf {
    state_root().join("identity")
}

pub fn locks_dir() -> PathBuf {
    state_root().join("locks")
}

/// The lock file backing a logical resource (see `statefile::FileLock`). The
/// name identifies the resource — `"tms-<worker>"`, `"gates-<worker>"`,
/// `"channels"` — not the data file it guards.
pub fn lock_file(name: &str) -> PathBuf {
    locks_dir().join(format!("{name}.flock"))
}

pub fn worker_listener_lock(worker: &str) -> PathBuf {
    locks_dir().join(format!("{}.lock", short_worker(worker)))
}

pub fn outbox_dir() -> PathBuf {
    state_root().join("outbox")
}

/// Readline history for `roster talk` — operator convenience, prunable.
pub fn talk_history_file() -> PathBuf {
    state_root().join("talk-history")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Worker paths accept both bare names and subjects.
    #[test]
    fn worker_handles_normalize() {
        assert_eq!(worker_data_dir("yuko"), worker_data_dir("org/yuko"));
        assert_eq!(worker_queue_dir("org/yuko").file_name().unwrap(), "queue");
    }
}
