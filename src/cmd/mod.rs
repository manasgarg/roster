//! Subcommands of the `roster` binary — the trusted host-side control plane
//! (D20). Each is a thin orchestration entry point; the heavy lifting reuses
//! the gateway's own modules (schema, budget, vault, registry) so there is one
//! set of types, not two. The clap grammar lives in main.rs; these are typed
//! handlers.

pub mod channel;
pub mod connect;
pub mod create;
pub mod gates;
pub mod init;
pub mod knowledge;
pub mod listen;
pub mod notes;
pub mod queue;
pub mod relay;
pub mod run_box;
pub mod runs;
pub mod server;
pub mod session;
pub mod supervise;
pub mod vault_sync;
pub mod worker;

type BErr = Box<dyn std::error::Error>;

/// Resolve a worker name against `workers/` — the guard that keeps a typo from
/// filing tasks for a worker that does not exist.
pub fn require_worker(name: &str) -> Result<(), BErr> {
    if crate::paths::worker_dir(name).join("worker.toml").exists() {
        return Ok(());
    }
    let mut have: Vec<String> = std::fs::read_dir(crate::paths::workers_dir())
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().join("worker.toml").exists())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    have.sort();
    Err(format!(
        "no such worker \"{name}\"{}",
        if have.is_empty() {
            String::new()
        } else {
            format!(" (have: {})", have.join(", "))
        }
    )
    .into())
}

/// Resolve an id or unique prefix against a set of ids (tasks, gates).
pub fn resolve_prefix<'a>(
    what: &str,
    id_or_prefix: &str,
    ids: impl Iterator<Item = &'a str>,
) -> Result<String, BErr> {
    let matches: Vec<&str> = ids.filter(|id| id.starts_with(id_or_prefix)).collect();
    match matches.len() {
        0 => Err(format!("no such {what} {id_or_prefix}").into()),
        1 => Ok(matches[0].to_string()),
        n => Err(format!(
            "{what} prefix {id_or_prefix} is ambiguous ({n} matches: {})",
            matches.join(", ")
        )
        .into()),
    }
}
