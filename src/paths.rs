//! The on-disk layout, in one place. Every path the control plane reads or
//! writes is minted here, so the layout is defined in exactly one file.
//!
//! Impyard follows the XDG Base Directory standard — the deployment lives
//! outside the code, in three roots:
//!
//!   config  $XDG_CONFIG_HOME/impyard   (~/.config/impyard)     hand-edited only
//!     org.toml  providers.toml  imps/<name>/{imp.toml,identity.md}
//!   data    $XDG_DATA_HOME/impyard     (~/.local/share/impyard)  durable — THE BACKUP SET
//!     vault/  ca/  channels/<id>/  audit/*.jsonl
//!     imps/<name>/{queue,journal,gates,memory.jsonl,knowledge}
//!   state   $XDG_STATE_HOME/impyard    (~/.local/state/impyard)  reconstructible/prunable
//!     runs/<run-id>/  identity/  locks/  outbox/  trigger-state.json
//!
//! Self-contained mode: if `IMPYARD_ROOT` is set, the three roots are
//! `$IMPYARD_ROOT/{config,data,state}` — for tests, scratch deployments, and
//! side-by-side instances. There is no config or state in the code checkout,
//! and the box mounts none of these directories — nothing to shadow.

use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn base(xdg_env: &str, xdg_default: &str, root_sub: &str) -> PathBuf {
    if let Ok(root) = std::env::var("IMPYARD_ROOT") {
        return PathBuf::from(root).join(root_sub);
    }
    std::env::var(xdg_env)
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(xdg_default))
        .join("impyard")
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

/// An imp handle may arrive as a bare name ("yuko") or a subject
/// ("org/yuko"); directories are keyed by the bare name.
pub fn short_imp(imp: &str) -> &str {
    imp.rsplit('/').next().unwrap_or(imp)
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
/// machine-scaffolded by `impyard server connect`, human-owned thereafter.
pub fn connections_dir() -> PathBuf {
    config_root().join("connections")
}

pub fn imps_dir() -> PathBuf {
    config_root().join("imps")
}

pub fn imp_dir(name: &str) -> PathBuf {
    imps_dir().join(short_imp(name))
}

// ── data: secrets ─────────────────────────────────────────────────────────────

pub fn vault_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("IMPYARD_VAULT_DIR") {
        return PathBuf::from(dir);
    }
    data_root().join("vault")
}

pub fn ca_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("IMPYARD_CA_DIR") {
        return PathBuf::from(dir);
    }
    data_root().join("ca")
}

// ── data: per-imp footprint (one subtree = one imp; D11 export) ────────

pub fn imps_data_dir() -> PathBuf {
    data_root().join("imps")
}

pub fn imp_data_dir(imp: &str) -> PathBuf {
    imps_data_dir().join(short_imp(imp))
}

pub fn imp_queue_dir(imp: &str) -> PathBuf {
    imp_data_dir(imp).join("queue")
}

pub fn imp_journal_file(imp: &str) -> PathBuf {
    imp_data_dir(imp).join("journal").join("events.jsonl")
}

pub fn imp_memory_file(imp: &str) -> PathBuf {
    imp_data_dir(imp).join("memory.jsonl")
}

/// Pre-`memory.jsonl` interaction-memory log; read-only fallback until
/// `imp memory compact` finishes the physical migration.
pub fn imp_notes_legacy_file(imp: &str) -> PathBuf {
    imp_data_dir(imp).join("notes-legacy.jsonl")
}

pub fn imp_knowledge_dir(imp: &str) -> PathBuf {
    imp_data_dir(imp).join("knowledge")
}

pub fn imp_gates_pending_dir(imp: &str) -> PathBuf {
    imp_data_dir(imp).join("gates").join("pending")
}

pub fn imp_gates_resolved_dir(imp: &str) -> PathBuf {
    imp_data_dir(imp).join("gates").join("resolved")
}

// ── data: channels and the permanent record ──────────────────────────────────

pub fn channels_dir() -> PathBuf {
    data_root().join("channels")
}

pub fn channel_dir(channel_id: &str) -> PathBuf {
    channels_dir().join(channel_id)
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

pub fn imp_listener_lock(imp: &str) -> PathBuf {
    locks_dir().join(format!("{}.lock", short_imp(imp)))
}

pub fn outbox_dir() -> PathBuf {
    state_root().join("outbox")
}

pub fn trigger_state_file() -> PathBuf {
    state_root().join("trigger-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Imp paths accept both bare names and subjects.
    #[test]
    fn imp_handles_normalize() {
        assert_eq!(imp_data_dir("yuko"), imp_data_dir("org/yuko"));
        assert_eq!(imp_queue_dir("org/yuko").file_name().unwrap(), "queue");
    }
}
