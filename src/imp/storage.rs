//! Admin-controlled policy for the imp knowledge repository. These settings
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
    /// The memory/knowledge boundary (docs/knowledge-boundary.md):
    /// "clean-room" — only untainted runs get a writable knowledge mount
    /// (tainted runs read-only, clean runs recall-free); "any-run" — legacy
    /// behavior, participant scanning only.
    pub write_from: String,
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
    pub imps: HashMap<String, StoragePolicy>,
}

pub fn load(imp: &str) -> StoragePolicy {
    let compiled = crate::config::snapshot()
        .map(|c| c.storage.clone())
        .unwrap_or_default();
    compiled
        .imps
        .get(imp.strip_prefix("org/").unwrap_or(imp))
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

/// An imp overlay may reduce capabilities and quotas but cannot broaden the
/// fleet policy authored by the admin.
pub fn validate_imp_overlay(base: &StoragePolicy, imp: &StoragePolicy) -> Result<(), String> {
    validate(imp)?;
    if imp.knowledge.enabled && !base.knowledge.enabled {
        return Err("imp cannot enable knowledge disabled by org policy".into());
    }
    if imp.knowledge.checkpoint_on_clean_exit && !base.knowledge.checkpoint_on_clean_exit {
        return Err("imp cannot enable checkpoints disabled by org policy".into());
    }
    if imp.knowledge.max_file_chars > base.knowledge.max_file_chars
        || imp.knowledge.max_repo_bytes > base.knowledge.max_repo_bytes
    {
        return Err("imp knowledge limits cannot exceed org limits".into());
    }
    if imp.knowledge.write_from == "any-run" && base.knowledge.write_from == "clean-room" {
        return Err("imp cannot relax the clean-room knowledge boundary set by org policy".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imp_storage_policy_can_only_narrow() {
        let base = StoragePolicy::default();
        let mut imp = base.clone();
        imp.knowledge.max_repo_bytes -= 1;
        assert!(validate_imp_overlay(&base, &imp).is_ok());

        imp.knowledge.max_repo_bytes = base.knowledge.max_repo_bytes + 1;
        assert!(validate_imp_overlay(&base, &imp).is_err());
    }
}
