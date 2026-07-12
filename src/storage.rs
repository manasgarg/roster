//! Owner-controlled policy for the worker knowledge repository. These settings
//! are enforcement inputs; they are compiled off-box and are never learned from
//! prompt text.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgePolicy {
    pub enabled: bool,
    pub normal_mode: String,
    pub max_file_chars: usize,
    pub max_repo_bytes: u64,
    pub checkpoint_on_clean_exit: bool,
    pub reorganization_requires_exclusive_lease: bool,
}

impl Default for KnowledgePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            normal_mode: "append".into(),
            max_file_chars: 200_000,
            max_repo_bytes: 1_000_000_000,
            checkpoint_on_clean_exit: true,
            reorganization_requires_exclusive_lease: true,
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
    if policy.knowledge.normal_mode != "append" {
        return Err("knowledge.normal_mode currently supports only \"append\"".into());
    }
    if policy.knowledge.max_file_chars == 0 || policy.knowledge.max_repo_bytes == 0 {
        return Err("knowledge size limits must be positive".into());
    }
    if !policy.knowledge.reorganization_requires_exclusive_lease {
        return Err("knowledge reorganization must require an exclusive lease".into());
    }
    Ok(())
}

/// A worker overlay may reduce capabilities and quotas but cannot broaden the
/// fleet policy authored by the owner.
pub fn validate_worker_overlay(base: &StoragePolicy, worker: &StoragePolicy) -> Result<(), String> {
    validate(worker)?;
    if worker.knowledge.enabled && !base.knowledge.enabled {
        return Err("worker cannot enable knowledge disabled by org policy".into());
    }
    if worker.knowledge.checkpoint_on_clean_exit && !base.knowledge.checkpoint_on_clean_exit {
        return Err("worker cannot enable checkpoints disabled by org policy".into());
    }
    if worker.knowledge.max_file_chars > base.knowledge.max_file_chars
        || worker.knowledge.max_repo_bytes > base.knowledge.max_repo_bytes
    {
        return Err("worker knowledge limits cannot exceed org limits".into());
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
    }
}
