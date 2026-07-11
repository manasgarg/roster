//! Owner-controlled policy for the worker knowledge repository, temporary
//! scratch space, and governed publication. These settings are enforcement
//! inputs; they are compiled off-box and are never learned from prompt text.

use crate::util::root;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScratchPolicy {
    pub max_bytes: u64,
    pub max_files: usize,
    pub cleanup_on_exit: bool,
    pub cleanup_on_crash: bool,
}

impl Default for ScratchPolicy {
    fn default() -> Self {
        Self {
            max_bytes: 2_000_000_000,
            max_files: 10_000,
            cleanup_on_exit: true,
            cleanup_on_crash: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PublishingPolicy {
    pub max_blob_bytes: u64,
    pub allowed_media_types: Vec<String>,
    pub default_visibility: String,
    pub public_requires_gate: bool,
}

impl Default for PublishingPolicy {
    fn default() -> Self {
        Self {
            max_blob_bytes: 100_000_000,
            allowed_media_types: vec![
                "text/markdown".into(),
                "text/html".into(),
                "application/pdf".into(),
            ],
            default_visibility: "private".into(),
            public_requires_gate: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StoragePolicy {
    pub knowledge: KnowledgePolicy,
    pub scratch: ScratchPolicy,
    pub publishing: PublishingPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompiledStoragePolicy {
    #[serde(default)]
    pub default: StoragePolicy,
    #[serde(default)]
    pub workers: HashMap<String, StoragePolicy>,
}

pub fn load(worker: &str) -> StoragePolicy {
    let compiled = std::fs::read_to_string(root().join("runs/compiled/storage.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<CompiledStoragePolicy>(&text).ok())
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
    if policy.scratch.max_bytes == 0 || policy.scratch.max_files == 0 {
        return Err("scratch limits must be positive".into());
    }
    if policy.publishing.max_blob_bytes == 0 {
        return Err("publishing.max_blob_bytes must be positive".into());
    }
    if policy.publishing.default_visibility != "private"
        && policy.publishing.default_visibility != "public"
    {
        return Err("publishing.default_visibility must be private or public".into());
    }
    if policy.publishing.allowed_media_types.is_empty() {
        return Err("publishing.allowed_media_types cannot be empty".into());
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
    if worker.knowledge.max_file_chars > base.knowledge.max_file_chars
        || worker.knowledge.max_repo_bytes > base.knowledge.max_repo_bytes
    {
        return Err("worker knowledge limits cannot exceed org limits".into());
    }
    if worker.scratch.max_bytes > base.scratch.max_bytes
        || worker.scratch.max_files > base.scratch.max_files
    {
        return Err("worker scratch limits cannot exceed org limits".into());
    }
    if worker.publishing.max_blob_bytes > base.publishing.max_blob_bytes {
        return Err("worker publishing limit cannot exceed org limit".into());
    }
    if worker
        .publishing
        .allowed_media_types
        .iter()
        .any(|kind| !base.publishing.allowed_media_types.contains(kind))
    {
        return Err("worker publishing media types must be a subset of org policy".into());
    }
    if base.publishing.public_requires_gate && !worker.publishing.public_requires_gate {
        return Err("worker cannot remove the public publication gate".into());
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
        worker.scratch.max_files -= 1;
        worker.publishing.allowed_media_types = vec!["text/markdown".into()];
        assert!(validate_worker_overlay(&base, &worker).is_ok());

        worker.scratch.max_bytes = base.scratch.max_bytes + 1;
        assert!(validate_worker_overlay(&base, &worker).is_err());
    }
}
