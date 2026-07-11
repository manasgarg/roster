//! `roster deploy` — compile org.toml + workers/<name>/worker.toml into the
//! runtime config the gateway reads, tagging each rule/limit with its scope.
//! Validates by deserializing into the gateway's OWN types (schema::Policy,
//! budget::BudgetPolicy) — so the compiled output can't drift from what the
//! gateway expects. This is the payoff of one language, one schema (D20).

use crate::action::ActionPolicy;
use crate::budget::BudgetPolicy;
use crate::context::{CompiledContextPolicy, ContextPolicy};
use crate::memory::{CompiledMemoryPolicy, MemoryPolicy};
use crate::schema::Policy;
use crate::storage::{CompiledStoragePolicy, StoragePolicy};
use crate::util::root;
use serde_json::{json, Value};
use std::fs;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let root = root();

    let org = read_toml(&root.join("org.toml"))?;
    let mut rules: Vec<Value> = Vec::new();
    let mut limits: Vec<Value> = Vec::new();
    let mut actions: Vec<Value> = Vec::new();
    let mut trust: Vec<Value> = Vec::new();
    let mut triggers: Vec<Value> = Vec::new();
    let default_memory = memory_policy(org.get("memory"), None)?;
    let default_context = context_policy(org.get("context"), None)?;
    let default_storage = storage_policy(&org, None)?;
    let mut worker_memory = std::collections::HashMap::new();
    let mut worker_context = std::collections::HashMap::new();
    let mut worker_storage = std::collections::HashMap::new();

    for g in array(&org, "grant") {
        rules.push(with_scope(g, "org"));
    }
    for a in array(&org, "action") {
        actions.push(with_scope(a, "org"));
    }
    for t in array(&org, "trust") {
        trust.push(with_scope(t, "org"));
    }
    let org_budget = org.get("budget");
    for l in org_budget.map(|b| array(b, "limit")).unwrap_or_default() {
        limits.push(with_scope(l, "org"));
    }

    let workers_dir = root.join("workers");
    let mut workers: Vec<String> = Vec::new();
    if workers_dir.is_dir() {
        let mut names: Vec<String> = fs::read_dir(&workers_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            let spec = workers_dir.join(&name).join("worker.toml");
            if !spec.exists() {
                continue;
            }
            let w = read_toml(&spec)?;
            let declared = w.get("name").and_then(|v| v.as_str());
            if declared != Some(name.as_str()) {
                return Err(format!("{}: name {:?} != folder \"{}\"", spec.display(), declared, name).into());
            }
            let scope = format!("org/{name}");
            workers.push(name.clone());
            worker_memory.insert(name.clone(), memory_policy(w.get("memory"), Some(&default_memory))?);
            worker_context.insert(name.clone(), context_policy(w.get("context"), Some(&default_context))?);
            let storage = storage_policy(&w, Some(&default_storage))?;
            crate::storage::validate_worker_overlay(&default_storage, &storage)
                .map_err(|error| format!("{name} storage policy is invalid: {error}"))?;
            worker_storage.insert(name.clone(), storage);
            for g in array(&w, "grant") {
                rules.push(with_scope(g, &scope));
            }
            for a in array(&w, "action") {
                actions.push(with_scope(a, &scope));
            }
            for t in array(&w, "trust") {
                trust.push(with_scope(t, &scope));
            }
            for tr in array(&w, "trigger") {
                // Triggers name their worker so the supervisor can dispatch them.
                let mut j = to_json(tr);
                if let Some(obj) = j.as_object_mut() {
                    obj.insert("worker".to_string(), json!(name));
                }
                triggers.push(j);
            }
            if let Some(b) = w.get("budget") {
                for l in array(b, "limit") {
                    limits.push(with_scope(l, &scope));
                }
            }
        }
    }

    let policy = json!({ "rules": rules });
    let budget = json!({
        "scope": "org",
        "currencies": org_budget.and_then(|b| b.get("currencies")).map(to_json).unwrap_or(json!([])),
        "vars": org_budget.and_then(|b| b.get("vars")).map(to_json).unwrap_or(json!({})),
        "meters": org_budget.map(|b| array(b, "meter")).unwrap_or_default().iter().map(|m| to_json(m)).collect::<Vec<_>>(),
        "limits": limits,
    });
    let action_policy = json!({ "actions": actions, "trust": trust });
    let memory_policy = CompiledMemoryPolicy { default: default_memory, workers: worker_memory };
    let context_policy = CompiledContextPolicy { default: default_context, workers: worker_context };
    let storage_policy = CompiledStoragePolicy { default: default_storage, workers: worker_storage };

    // Validate against the gateway's own types.
    serde_json::from_value::<Policy>(policy.clone()).map_err(|e| format!("compiled policy is invalid: {e}"))?;
    serde_json::from_value::<BudgetPolicy>(budget.clone()).map_err(|e| format!("compiled budget is invalid: {e}"))?;
    serde_json::from_value::<ActionPolicy>(action_policy.clone()).map_err(|e| format!("compiled actions are invalid: {e}"))?;
    serde_json::to_value(&memory_policy).map_err(|e| format!("compiled memory policy is invalid: {e}"))?;
    serde_json::to_value(&context_policy).map_err(|e| format!("compiled context policy is invalid: {e}"))?;
    serde_json::to_value(&storage_policy).map_err(|e| format!("compiled storage policy is invalid: {e}"))?;

    let out = root.join("runs").join("compiled");
    fs::create_dir_all(&out)?;
    fs::write(out.join("policy.json"), format!("{}\n", serde_json::to_string_pretty(&policy)?))?;
    fs::write(out.join("budget.json"), format!("{}\n", serde_json::to_string_pretty(&budget)?))?;
    fs::write(out.join("actions.json"), format!("{}\n", serde_json::to_string_pretty(&action_policy)?))?;
    fs::write(out.join("triggers.json"), format!("{}\n", serde_json::to_string_pretty(&json!({ "triggers": triggers }))?))?;
    fs::write(out.join("memory.json"), format!("{}\n", serde_json::to_string_pretty(&memory_policy)?))?;
    fs::write(out.join("context.json"), format!("{}\n", serde_json::to_string_pretty(&context_policy)?))?;
    fs::write(out.join("storage.json"), format!("{}\n", serde_json::to_string_pretty(&storage_policy)?))?;

    println!(
        "deployed: {} worker(s) [{}], {} rule(s), {} action(s), {} trigger(s), {} limit(s)",
        workers.len(),
        workers.join(", "),
        rules.len(),
        actions.len(),
        triggers.len(),
        limits.len()
    );
    println!("compiled → runs/compiled/{{policy,budget,actions,triggers,memory,context,storage}}.json (the control plane reads these)");
    Ok(())
}

fn context_policy(value: Option<&toml::Value>, base: Option<&ContextPolicy>) -> Result<ContextPolicy, Box<dyn std::error::Error>> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    if let Some(value) = value {
        merge_json(&mut merged, to_json(value));
    }
    serde_json::from_value(merged).map_err(|e| format!("context policy is invalid: {e}").into())
}

fn memory_policy(value: Option<&toml::Value>, base: Option<&MemoryPolicy>) -> Result<MemoryPolicy, Box<dyn std::error::Error>> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    if let Some(value) = value {
        merge_json(&mut merged, to_json(value));
    }
    let policy: MemoryPolicy = serde_json::from_value(merged)
        .map_err(|e| format!("memory policy is invalid: {e}"))?;
    if let Some(kind) = policy
        .allowed_kinds
        .iter()
        .find(|kind| !crate::memory::SUPPORTED_MEMORY_KINDS.contains(&kind.as_str()))
    {
        return Err(format!(
            "memory policy kind \"{kind}\" is not interaction memory; supported kinds are {}",
            crate::memory::SUPPORTED_MEMORY_KINDS.join(", ")
        )
        .into());
    }
    Ok(policy)
}

fn storage_policy(value: &toml::Value, base: Option<&StoragePolicy>) -> Result<StoragePolicy, Box<dyn std::error::Error>> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    let overlay = json!({
        "knowledge": value.get("knowledge").map(to_json).unwrap_or(json!({})),
        "scratch": value.get("scratch").map(to_json).unwrap_or(json!({})),
        "publishing": value.get("publishing").map(to_json).unwrap_or(json!({})),
    });
    merge_json(&mut merged, overlay);
    let policy: StoragePolicy = serde_json::from_value(merged)
        .map_err(|error| format!("storage policy is invalid: {error}"))?;
    crate::storage::validate(&policy)
        .map_err(|error| format!("storage policy is invalid: {error}"))?;
    Ok(policy)
}

fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn read_toml(path: &std::path::Path) -> Result<toml::Value, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(toml::Value::Table(Default::default()));
    }
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

/// The array of tables under `key` in a TOML table (`[[key]]`), or empty.
fn array<'a>(v: &'a toml::Value, key: &str) -> Vec<&'a toml::Value> {
    v.get(key).and_then(|x| x.as_array()).map(|a| a.iter().collect()).unwrap_or_default()
}

fn to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn with_scope(v: &toml::Value, scope: &str) -> Value {
    let mut j = to_json(v);
    if let Some(obj) = j.as_object_mut() {
        obj.insert("scope".to_string(), json!(scope));
    }
    j
}
