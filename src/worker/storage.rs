//! Admin-controlled policy for the worker knowledge repository. These settings
//! are enforcement inputs; they are compiled off-box and are never learned from
//! prompt text.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgePolicy {
    pub enabled: bool,
    pub max_file_chars: usize,
    pub max_repo_bytes: u64,
    /// A knowledge_push whose diff deletes more than this many files needs a
    /// human gate (confirm_bulk_delete). History makes deletion recoverable;
    /// a quiet bulk wipe still deserves a speed bump.
    pub max_deletions_ungated: usize,
    /// The memory/knowledge boundary (docs/knowledge.md):
    /// "clean-room" — only untainted runs get a writable knowledge clone
    /// (tainted runs read-only, clean runs recall-free); "any-run" — legacy
    /// behavior, participant scanning only.
    pub write_from: String,
}

impl Default for KnowledgePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_chars: 200_000,
            max_repo_bytes: 1_000_000_000,
            max_deletions_ungated: 20,
            write_from: "clean-room".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StoragePolicy {
    pub knowledge: KnowledgePolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompiledStoragePolicy {
    #[serde(default)]
    pub default: StoragePolicy,
    #[serde(default)]
    pub workers: HashMap<String, StoragePolicy>,
}

pub fn load(worker: &str) -> StoragePolicy {
    let compiled = crate::config::snapshot()
        .map(|c| c.storage.clone())
        .unwrap_or_default();
    compiled
        .workers
        .get(worker.strip_prefix("org/").unwrap_or(worker))
        .cloned()
        .unwrap_or(compiled.default)
}

pub fn validate(policy: &StoragePolicy) -> Result<(), String> {
    if policy.knowledge.max_file_chars == 0 || policy.knowledge.max_repo_bytes == 0 {
        return Err("knowledge size limits must be positive".into());
    }
    if !matches!(
        policy.knowledge.write_from.as_str(),
        "clean-room" | "any-run"
    ) {
        return Err(format!(
            "knowledge.write_from must be \"clean-room\" or \"any-run\", not \"{}\"",
            policy.knowledge.write_from
        ));
    }
    Ok(())
}

/// A worker overlay may reduce capabilities and quotas but cannot broaden the
/// fleet policy authored by the admin.
pub fn validate_worker_overlay(base: &StoragePolicy, worker: &StoragePolicy) -> Result<(), String> {
    validate(worker)?;
    if worker.knowledge.enabled && !base.knowledge.enabled {
        return Err("worker cannot enable knowledge disabled by org policy".into());
    }
    if worker.knowledge.max_file_chars > base.knowledge.max_file_chars
        || worker.knowledge.max_repo_bytes > base.knowledge.max_repo_bytes
        || worker.knowledge.max_deletions_ungated > base.knowledge.max_deletions_ungated
    {
        return Err("worker knowledge limits cannot exceed org limits".into());
    }
    if worker.knowledge.write_from == "any-run" && base.knowledge.write_from == "clean-room" {
        return Err(
            "worker cannot relax the clean-room knowledge boundary set by org policy".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_storage_policy_can_only_narrow() {
        let base = StoragePolicy::default();
        let mut worker = base.clone();
        worker.knowledge.max_repo_bytes -= 1;
        assert!(validate_worker_overlay(&base, &worker).is_ok());

        worker.knowledge.max_repo_bytes = base.knowledge.max_repo_bytes + 1;
        assert!(validate_worker_overlay(&base, &worker).is_err());

        worker.knowledge.max_repo_bytes = base.knowledge.max_repo_bytes;
        worker.knowledge.max_deletions_ungated = base.knowledge.max_deletions_ungated + 1;
        assert!(validate_worker_overlay(&base, &worker).is_err());
    }
}
