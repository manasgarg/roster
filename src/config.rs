//! Live config. `org.toml` + `workers/*/worker.toml` parse straight into the
//! gateway's OWN types (schema::Policy, budget::BudgetPolicy, …), scope-tagged
//! in memory — there is no compile step and no intermediate artifact. This is
//! still the payoff of one language, one schema (D20): what validates is
//! literally what runs.
//!
//! Consumers call `snapshot()` (mtime-fingerprint cache, so admin edits are
//! live). Invalid config fails closed: the gateway denies, dispatch pauses,
//! `server start` refuses to boot — and `roster server validate` prints every
//! error. `load()` is side-effect free.

use crate::action::ActionPolicy;
use crate::budget::BudgetPolicy;
use crate::context::{CompiledContextPolicy, ContextPolicy};
use crate::memory::{CompiledMemoryPolicy, MemoryPolicy};
use crate::paths;
use crate::schema::Policy;
use crate::storage::{CompiledStoragePolicy, StoragePolicy};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

pub struct Loaded {
    pub policy: Policy,
    pub budget: BudgetPolicy,
    pub actions: ActionPolicy,
    pub triggers: Vec<Value>,
    pub memory: CompiledMemoryPolicy,
    pub context: CompiledContextPolicy,
    pub storage: CompiledStoragePolicy,
    /// (worker, vault credential) — `server start` starts one listener each.
    pub listeners: Vec<(String, String)>,
    pub workers: Vec<String>,
    /// The platform checkout the box mounts (`[engine] dir` in org.toml) —
    /// needed until pi + the extensions are baked into the box image.
    pub engine_dir: Option<PathBuf>,
}

/// Parse and validate everything, collecting every error (not just the first).
/// Side-effect free — this is also `roster server validate`.
pub fn load() -> Result<Loaded, Vec<String>> {
    let mut errors: Vec<String> = Vec::new();
    let org_path = paths::org_file();
    let org = match read_toml(&org_path) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("{}: {e}", org_path.display()));
            toml::Value::Table(Default::default())
        }
    };

    let mut rules: Vec<Value> = Vec::new();
    let mut limits: Vec<Value> = Vec::new();
    let mut actions: Vec<Value> = Vec::new();
    let mut trust: Vec<Value> = Vec::new();
    let mut triggers: Vec<Value> = Vec::new();
    let mut listeners: Vec<(String, String)> = Vec::new();
    let mut workers: Vec<String> = Vec::new();

    let default_memory = memory_policy(org.get("memory"), None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [memory]: {e}"));
        MemoryPolicy::default()
    });
    let default_context = context_policy(org.get("context"), None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [context]: {e}"));
        ContextPolicy::default()
    });
    let default_storage = storage_policy(&org, None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [knowledge]: {e}"));
        StoragePolicy::default()
    });
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

    let engine_dir = org
        .get("engine")
        .and_then(|e| e.get("dir"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    let workers_dir = paths::workers_dir();
    if workers_dir.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&workers_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            let spec = workers_dir.join(&name).join("worker.toml");
            if !spec.exists() {
                continue;
            }
            let w = match read_toml(&spec) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{}: {e}", spec.display()));
                    continue;
                }
            };
            let declared = w.get("name").and_then(|v| v.as_str());
            if declared != Some(name.as_str()) {
                errors.push(format!("{}: name {declared:?} != folder \"{name}\"", spec.display()));
                continue;
            }
            let scope = format!("org/{name}");
            workers.push(name.clone());
            match memory_policy(w.get("memory"), Some(&default_memory)) {
                Ok(p) => {
                    worker_memory.insert(name.clone(), p);
                }
                Err(e) => errors.push(format!("{name} [memory]: {e}")),
            }
            match context_policy(w.get("context"), Some(&default_context)) {
                Ok(p) => {
                    worker_context.insert(name.clone(), p);
                }
                Err(e) => errors.push(format!("{name} [context]: {e}")),
            }
            match storage_policy(&w, Some(&default_storage)) {
                Ok(storage) => match crate::storage::validate_worker_overlay(&default_storage, &storage) {
                    Ok(()) => {
                        worker_storage.insert(name.clone(), storage);
                    }
                    Err(e) => errors.push(format!("{name} [knowledge]: {e}")),
                },
                Err(e) => errors.push(format!("{name} [knowledge]: {e}")),
            }
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
                // Triggers name their worker so dispatch knows whose task to file.
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
            // [channels] — which vault credential this worker's inbound edge
            // uses. Two workers on one credential would double-file every
            // message, so that is a validation error, not a runtime surprise.
            if let Some(credential) = w
                .get("channels")
                .and_then(|c| c.get("discord"))
                .and_then(|v| v.as_str())
            {
                if let Some((taken, _)) = listeners.iter().find(|(_, c)| c == credential) {
                    errors.push(format!(
                        "workers {taken} and {name} both listen with credential \"{credential}\" — one bot cannot serve two workers"
                    ));
                } else {
                    listeners.push((name.clone(), credential.to_string()));
                }
            }
        }
    }

    // Validate by deserializing into the runtime's own types.
    let policy = parse::<Policy>(&mut errors, "policy (grants)", json!({ "rules": rules }));
    let budget = parse::<BudgetPolicy>(
        &mut errors,
        "budget",
        json!({
            "scope": "org",
            "currencies": org_budget.and_then(|b| b.get("currencies")).map(to_json).unwrap_or(json!([])),
            "vars": org_budget.and_then(|b| b.get("vars")).map(to_json).unwrap_or(json!({})),
            "meters": org_budget.map(|b| array(b, "meter")).unwrap_or_default().iter().map(|m| to_json(m)).collect::<Vec<_>>(),
            "limits": limits,
        }),
    );
    let actions = parse::<ActionPolicy>(&mut errors, "actions/trust", json!({ "actions": actions, "trust": trust }));

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Loaded {
        policy,
        budget,
        actions,
        triggers,
        memory: CompiledMemoryPolicy { default: default_memory, workers: worker_memory },
        context: CompiledContextPolicy { default: default_context, workers: worker_context },
        storage: CompiledStoragePolicy { default: default_storage, workers: worker_storage },
        listeners,
        workers,
        engine_dir,
    })
}

/// The cached view. Reloads when any config file's fingerprint changes, so
/// admin edits are live without a restart. On invalid config returns Err —
/// callers fail closed.
pub fn snapshot() -> Result<Arc<Loaded>, String> {
    static CACHE: OnceLock<Mutex<Option<(String, Arc<Loaded>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let fp = fingerprint();
    {
        let cached = cache.lock().unwrap();
        if let Some((cached_fp, loaded)) = cached.as_ref() {
            if *cached_fp == fp {
                return Ok(loaded.clone());
            }
        }
    }
    match load() {
        Ok(loaded) => {
            let loaded = Arc::new(loaded);
            *cache.lock().unwrap() = Some((fp, loaded.clone()));
            Ok(loaded)
        }
        Err(errors) => Err(errors.join("\n")),
    }
}

/// mtime+len of every config file, so an edit anywhere invalidates the cache.
fn fingerprint() -> String {
    fn stamp(path: &std::path::Path) -> String {
        std::fs::metadata(path)
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{}:{mtime}:{}", path.display(), m.len())
            })
            .unwrap_or_else(|_| format!("{}:absent", path.display()))
    }
    let mut parts = vec![stamp(&paths::org_file())];
    let mut names: Vec<PathBuf> = std::fs::read_dir(paths::workers_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("worker.toml"))
        .collect();
    names.sort();
    for spec in names {
        parts.push(stamp(&spec));
    }
    parts.join("|")
}

// ── helpers (moved from the retired deploy step) ─────────────────────────────

fn parse<T: serde::de::DeserializeOwned + Default>(errors: &mut Vec<String>, what: &str, v: Value) -> T {
    match serde_json::from_value::<T>(v) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("{what}: {e}"));
            T::default()
        }
    }
}

type BErr = Box<dyn std::error::Error>;

fn context_policy(value: Option<&toml::Value>, base: Option<&ContextPolicy>) -> Result<ContextPolicy, BErr> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    if let Some(value) = value {
        merge_json(&mut merged, to_json(value));
    }
    serde_json::from_value(merged).map_err(|e| format!("context policy is invalid: {e}").into())
}

fn memory_policy(value: Option<&toml::Value>, base: Option<&MemoryPolicy>) -> Result<MemoryPolicy, BErr> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    if let Some(value) = value {
        merge_json(&mut merged, to_json(value));
    }
    let policy: MemoryPolicy = serde_json::from_value(merged).map_err(|e| format!("memory policy is invalid: {e}"))?;
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

fn storage_policy(value: &toml::Value, base: Option<&StoragePolicy>) -> Result<StoragePolicy, BErr> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    let overlay = json!({
        "knowledge": value.get("knowledge").map(to_json).unwrap_or(json!({})),
    });
    merge_json(&mut merged, overlay);
    let policy: StoragePolicy = serde_json::from_value(merged).map_err(|error| format!("storage policy is invalid: {error}"))?;
    crate::storage::validate(&policy).map_err(|error| format!("storage policy is invalid: {error}"))?;
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

fn read_toml(path: &std::path::Path) -> Result<toml::Value, BErr> {
    if !path.exists() {
        return Ok(toml::Value::Table(Default::default()));
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
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
