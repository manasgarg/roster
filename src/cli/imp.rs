//! `impyard imp ls|show|trust` — the fleet, one imp, and its earned
//! trust. Computed from specs, ledgers, and records — never model-written.

use crate::action;
use crate::action::gate;
use crate::gateway::budget;
use crate::gateway::ledger;
use crate::gateway::scope::applies;
use crate::imp::memory;
use crate::paths;
use crate::util::now_ms;
use crate::util::BErr;
use crate::work::queue;
use std::collections::BTreeMap;

fn imp_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(paths::imps_dir())
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().join("imp.toml").exists())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn knowledge_head(imp: &str) -> Option<String> {
    let repo = crate::imp::knowledge::repo_path(imp).ok()?;
    let out = std::process::Command::new("git")
        .arg(format!("--git-dir={}", repo.display()))
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn queue_counts(imp: &str) -> BTreeMap<String, usize> {
    let mut by_state = BTreeMap::new();
    for t in queue::list_all().into_iter().filter(|t| t.imp == imp) {
        *by_state.entry(t.state).or_insert(0) += 1;
    }
    by_state
}

pub fn ls(json: bool) -> Result<(), BErr> {
    let names = imp_names();
    if json {
        let rows: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "queue": queue_counts(name),
                    "gates_pending": gate::for_imp(name).iter().filter(|g| g.state == "pending").count(),
                    "memory_notes": memory::list(name).len(),
                    "knowledge_head": knowledge_head(name),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if names.is_empty() {
        println!("no imps — scaffold one: impyard imp init <name>");
        return Ok(());
    }
    println!(
        "{:<12}  {:<24}  {:<6}  {:<7}  KNOWLEDGE",
        "IMP", "QUEUE", "GATES", "MEMORY"
    );
    for name in names {
        let counts = queue_counts(&name);
        let queue_line = if counts.is_empty() {
            "-".to_string()
        } else {
            counts
                .iter()
                .map(|(state, n)| format!("{n} {state}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let gates = gate::for_imp(&name)
            .iter()
            .filter(|g| g.state == "pending")
            .count();
        println!(
            "{:<12}  {:<24}  {:<6}  {:<7}  {}",
            name,
            queue_line,
            gates,
            memory::list(&name).len(),
            knowledge_head(&name).unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

pub fn show(name: &str, json: bool) -> Result<(), BErr> {
    crate::imp::require_imp(name)?;
    let subject = format!("org/{name}");

    // Budget: every limit that applies to this imp, with its current balance
    // (the imp's own caps and the org-wide caps it rolls up into).
    let limits: Vec<budget::Limit> = budget::load_budget()
        .limits
        .into_iter()
        .filter(|l| applies(&l.scope, &subject))
        .collect();
    ledger::rehydrate(&limits);
    let balances = ledger::balances(&limits, now_ms());

    let pending: Vec<_> = gate::for_imp(name)
        .into_iter()
        .filter(|g| g.state == "pending")
        .collect();
    let triggers = imp_triggers(name);
    let counts = queue_counts(name);
    let memory_notes = memory::list(name).len();
    let knowledge = crate::imp::knowledge::repo_path(name).ok();

    if json {
        let out = serde_json::json!({
            "name": name,
            "spec": paths::imp_dir(name).join("imp.toml").display().to_string(),
            "queue": counts,
            "gates_pending": pending.iter().map(|g| serde_json::json!({"id": g.id, "intent": g.intent})).collect::<Vec<_>>(),
            "budget": balances.iter().map(|(l, used)| serde_json::json!({
                "currency": l.currency, "window": l.window.label(), "used": used, "max": l.max, "scope": l.scope,
            })).collect::<Vec<_>>(),
            "triggers": triggers,
            "memory_notes": memory_notes,
            "knowledge": knowledge.as_ref().map(|p| p.display().to_string()),
            "knowledge_head": knowledge_head(name),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("imp    {name}");
    println!(
        "spec      {}",
        paths::imp_dir(name).join("imp.toml").display()
    );
    println!(
        "identity  {}",
        paths::imp_dir(name).join("identity.md").display()
    );
    let queue_line = if counts.is_empty() {
        "empty".to_string()
    } else {
        counts
            .iter()
            .map(|(state, n)| format!("{n} {state}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("queue     {queue_line}");
    if pending.is_empty() {
        println!("gates     none pending");
    } else {
        for g in &pending {
            println!("gate      {}  {}  filed {}", g.id, g.intent, g.filed_at);
        }
    }
    if balances.is_empty() {
        println!("budget    no limits apply");
    } else {
        for (l, used) in &balances {
            println!(
                "budget    {:<14} {:>10.2} / {:<10.2} per {}  (scope {})",
                l.currency,
                used,
                l.max,
                l.window.label(),
                l.scope
            );
        }
    }
    for t in &triggers {
        println!(
            "trigger   {}  (ceiling {} min): {}",
            t.get("schedule").and_then(|v| v.as_str()).unwrap_or("?"),
            t.get("ceiling_min").and_then(|v| v.as_f64()).unwrap_or(0.0),
            crate::run::runlog::one_line(
                t.get("prompt").and_then(|v| v.as_str()).unwrap_or(""),
                60
            )
        );
    }
    println!("memory    {memory_notes} note(s)");
    if let Some(repo) = knowledge {
        println!(
            "knowledge {}  @ {}",
            repo.display(),
            knowledge_head(name).unwrap_or_else(|| "-".into())
        );
    }
    println!("\ntrust: impyard imp trust {name}   work: impyard imp task ls");
    Ok(())
}

fn imp_triggers(name: &str) -> Vec<serde_json::Value> {
    crate::config::snapshot()
        .map(|c| {
            c.triggers
                .iter()
                .filter(|t| t.get("imp").and_then(|w| w.as_str()) == Some(name))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Per-action trust, read-only: what the imp may propose, the default level,
/// the owner's ladder rules, and the earned history behind them. Trust is never
/// set here — it is earned through gate outcomes; grants live in the specs.
pub fn trust(name: &str, json: bool) -> Result<(), BErr> {
    crate::imp::require_imp(name)?;
    let subject = format!("org/{name}");
    let policy = action::load_action_policy();

    let mut rows: Vec<serde_json::Value> = Vec::new();
    for grant in policy
        .actions
        .iter()
        .filter(|g| applies(&g.scope, &subject))
    {
        let (executed, denied) = gate::history(name, &grant.name);
        let rules: Vec<serde_json::Value> = policy
            .trust
            .iter()
            .filter(|r| r.intent == grant.name && applies(&r.scope, &subject))
            .map(|r| {
                serde_json::json!({
                    "level": r.level,
                    "match": r.predicate,
                    "after": r.after,
                    "scope": r.scope,
                })
            })
            .collect();
        rows.push(serde_json::json!({
            "intent": grant.name,
            "executor": grant.executor,
            "default": grant.trust,
            "executed": executed,
            "denied": denied,
            "rules": rules,
        }));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("{name} has no action grants — it can propose nothing");
        return Ok(());
    }
    for row in rows {
        println!(
            "{}  (executor {}, default {})  history: {} executed, {} denied",
            row["intent"].as_str().unwrap_or("?"),
            row["executor"].as_str().unwrap_or("?"),
            row["default"].as_str().unwrap_or("?"),
            row["executed"],
            row["denied"],
        );
        for rule in row["rules"].as_array().into_iter().flatten() {
            let preds = rule["match"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("?")))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty())
                .map(|s| format!(" when {s}"))
                .unwrap_or_default();
            let after = rule["after"]
                .as_u64()
                .map(|n| format!(" after {n} clean approvals"))
                .unwrap_or_default();
            println!(
                "  rule: {}{preds}{after}  (scope {})",
                rule["level"].as_str().unwrap_or("?"),
                rule["scope"].as_str().unwrap_or("?")
            );
        }
    }
    println!("\npromotion is admin-only: rules live in org.toml / imp.toml; a denial revokes earned auto");
    Ok(())
}
