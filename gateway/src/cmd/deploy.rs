//! `roster deploy` — compile org.toml + workers/<name>/worker.toml into the
//! runtime config the gateway reads, tagging each rule/limit with its scope.
//! Validates by deserializing into the gateway's OWN types (schema::Policy,
//! budget::BudgetPolicy) — so the compiled output can't drift from what the
//! gateway expects. This is the payoff of one language, one schema (D20).

use crate::budget::BudgetPolicy;
use crate::schema::Policy;
use crate::util::root;
use serde_json::{json, Value};
use std::fs;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let root = root();

    let org = read_toml(&root.join("org.toml"))?;
    let mut rules: Vec<Value> = Vec::new();
    let mut limits: Vec<Value> = Vec::new();

    for g in array(&org, "grant") {
        rules.push(with_scope(g, "org"));
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
            for g in array(&w, "grant") {
                rules.push(with_scope(g, &scope));
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

    // Validate against the gateway's own types.
    serde_json::from_value::<Policy>(policy.clone()).map_err(|e| format!("compiled policy is invalid: {e}"))?;
    serde_json::from_value::<BudgetPolicy>(budget.clone()).map_err(|e| format!("compiled budget is invalid: {e}"))?;

    let out = root.join("runs").join("compiled");
    fs::create_dir_all(&out)?;
    fs::write(out.join("policy.json"), format!("{}\n", serde_json::to_string_pretty(&policy)?))?;
    fs::write(out.join("budget.json"), format!("{}\n", serde_json::to_string_pretty(&budget)?))?;

    println!(
        "deployed: {} worker(s) [{}], {} rule(s), {} limit(s)",
        workers.len(),
        workers.join(", "),
        rules.len(),
        limits.len()
    );
    println!("compiled → runs/compiled/{{policy,budget}}.json (the gateway reads these)");
    Ok(())
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
