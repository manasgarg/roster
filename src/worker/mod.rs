//! What a worker is and knows: trusted context compilation, scoped interaction
//! memory, Git-backed world knowledge, the journal, and knowledge policy.

pub mod boundary;
pub mod context;
pub mod journal;
pub mod knowledge;
pub mod memory;
pub mod storage;

use crate::util::BErr;

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
