//! `roster worker ls|show|trust` — the fleet, one worker, and its earned
//! trust. Computed from specs, ledgers, and records — never model-written.

use crate::action;
use crate::action::gate;
use crate::gateway::budget;
use crate::gateway::ledger;
use crate::gateway::scope::applies;
use crate::worker::memory;
use crate::paths;
use crate::util::now_ms;
use crate::util::BErr;
use crate::work::tms;
use std::collections::BTreeMap;

fn knowledge_head(worker: &str) -> Option<String> {
    let repo = crate::worker::knowledge::repo_path(worker).ok()?;
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

fn queue_counts(worker: &str) -> BTreeMap<String, usize> {
    let mut by_state = BTreeMap::new();
    for t in tms::list_all().into_iter().filter(|t| t.worker == worker) {
        *by_state.entry(t.state).or_insert(0) += 1;
    }
    by_state
}

pub fn ls(json: bool) -> Result<(), BErr> {
    let names = crate::worker::names();
    if json {
        let rows: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "queue": queue_counts(name),
                    "gates_pending": gate::for_worker(name).iter().filter(|g| g.state == "pending").count(),
                    "memory_notes": memory::list(name).len(),
                    "knowledge_head": knowledge_head(name),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if names.is_empty() {
        println!("no workers — scaffold one: roster worker init <name>");
        return Ok(());
    }
    println!(
        "{:<12}  {:<24}  {:<6}  {:<7}  KNOWLEDGE",
        "WORKER", "QUEUE", "GATES", "MEMORY"
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
        let gates = gate::for_worker(&name)
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
    crate::worker::require_worker(name)?;
    // If config doesn't parse, load_budget/load_action_policy/heartbeat all fall
    // back to defaults — "no limits", "every 30m", "no grants" — which would
    // read as fact. Warn loudly and fail the command so an audit can't be misled.
    let bad_config = match crate::config::snapshot() {
        Ok(_) => false,
        Err(e) => {
            eprintln!(
                "warning: configuration does not parse — the budget, heartbeat, and trust shown \
                 below are DEFAULTS, not your configured values:\n{e}\n"
            );
            true
        }
    };
    let subject = format!("org/{name}");

    // Budget: every limit that applies to this worker, with its current balance
    // (the worker's own caps and the org-wide caps it rolls up into).
    let limits: Vec<budget::Limit> = budget::load_budget()
        .limits
        .into_iter()
        .filter(|l| applies(&l.scope, &subject))
        .collect();
    ledger::rehydrate(&limits);
    let balances = ledger::balances(&limits, now_ms());

    let pending: Vec<_> = gate::for_worker(name)
        .into_iter()
        .filter(|g| g.state == "pending")
        .collect();
    let heartbeat = crate::config::snapshot()
        .ok()
        .and_then(|c| c.heartbeats.get(name).cloned())
        .unwrap_or_else(|| "every 30m".into());
    let counts = queue_counts(name);
    let memory_notes = memory::list(name).len();
    let knowledge = crate::worker::knowledge::repo_path(name).ok();

    if json {
        let out = serde_json::json!({
            "name": name,
            "spec": paths::worker_dir(name).join("worker.toml").display().to_string(),
            "queue": counts,
            "gates_pending": pending.iter().map(|g| serde_json::json!({"id": g.id, "intent": g.intent})).collect::<Vec<_>>(),
            "budget": balances.iter().map(|(l, used)| serde_json::json!({
                "currency": l.currency, "window": l.window.label(), "used": used, "max": l.max, "scope": l.scope,
            })).collect::<Vec<_>>(),
            "heartbeat": heartbeat,
            "memory_notes": memory_notes,
            "knowledge": knowledge.as_ref().map(|p| p.display().to_string()),
            "knowledge_head": knowledge_head(name),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return authoritative(bad_config);
    }

    println!("worker    {name}");
    println!(
        "spec      {}",
        paths::worker_dir(name).join("worker.toml").display()
    );
    println!(
        "identity  {}",
        paths::worker_dir(name).join("identity.md").display()
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
    println!("heartbeat {heartbeat}");
    println!("memory    {memory_notes} note(s)");
    if let Some(repo) = knowledge {
        println!(
            "knowledge {}  @ {}",
            repo.display(),
            knowledge_head(name).unwrap_or_else(|| "-".into())
        );
    }
    println!("\ntrust: roster worker trust {name}   work: roster worker task ls");
    authoritative(bad_config)
}

/// Turn "the config didn't parse" into a non-zero exit after the (warned)
/// best-effort view has printed, so scripts and audits don't read defaults as
/// truth.
fn authoritative(bad_config: bool) -> Result<(), BErr> {
    if bad_config {
        return Err("configuration is invalid; the view above is not authoritative".into());
    }
    Ok(())
}

/// Retire a worker: archive its spec and data, never delete. Refuses while
/// live work or pending gates exist — finish, requeue, or cancel first. The
/// archive keeps everything (identity, memory, knowledge, task history) so a
/// removal is auditable and reversible by moving the directories back.
pub fn rm(name: &str, yes: bool) -> Result<(), BErr> {
    crate::worker::require_worker(name)?;
    let live: Vec<String> = tms::load(name)
        .tasks
        .iter()
        .filter(|t| t.live())
        .map(|t| format!("{} ({})", t.id, t.state))
        .collect();
    if !live.is_empty() {
        return Err(format!(
            "{name} still has live work: {} — let it finish or curate it first (roster worker task ls)",
            live.join(", ")
        )
        .into());
    }
    let gates = gate::for_worker(name)
        .iter()
        .filter(|g| g.state == "pending")
        .count();
    if gates > 0 {
        return Err(format!(
            "{name} has {gates} gate(s) pending your approval — resolve them first: roster server approvals ls"
        )
        .into());
    }

    let stamp: String = crate::util::now_rfc3339()
        .chars()
        .take(19)
        .map(|c| if c == ':' { '-' } else { c })
        .collect();
    let spec_trash = paths::config_root().join("trash").join(format!("{name}-{stamp}"));
    let data_trash = paths::data_root().join("trash").join(format!("{name}-{stamp}"));

    if !yes {
        let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if !interactive {
            return Err(format!(
                "removing a worker needs confirmation — re-run with --yes: roster worker rm {name} --yes"
            )
            .into());
        }
        println!(
            "this archives {name}'s spec, identity, memory, knowledge, and task history under:\n  {}\n  {}",
            spec_trash.display(),
            data_trash.display()
        );
        let answer = crate::credential::connect::ask("type the worker's name to confirm: ")?;
        if answer.trim() != name {
            return Err("confirmation did not match — nothing was removed".into());
        }
    }

    // Same-root renames (config→config trash, data→data trash) so this never
    // crosses a filesystem boundary.
    std::fs::create_dir_all(spec_trash.parent().unwrap())?;
    std::fs::rename(paths::worker_dir(name), &spec_trash)?;
    let data_dir = paths::worker_data_dir(name);
    if data_dir.exists() {
        std::fs::create_dir_all(data_trash.parent().unwrap())?;
        std::fs::rename(&data_dir, &data_trash)?;
    }
    println!("archived {name}:");
    println!("  spec  → {}", spec_trash.display());
    if data_dir.exists() || data_trash.exists() {
        println!("  data  → {}", data_trash.display());
    }
    println!("restore: move the directories back; delete for good: remove the trash entries");
    Ok(())
}

/// Per-action trust, read-only: what the worker may propose, the default level,
/// the owner's ladder rules, and the earned history behind them. Trust is never
/// set here — it is earned through gate outcomes; grants live in the specs.
pub fn trust(name: &str, json: bool) -> Result<(), BErr> {
    crate::worker::require_worker(name)?;
    // A broken config makes load_action_policy() return an empty policy, so an
    // admin auditing what a worker may do would read "no grants" as fact. Warn
    // and fail rather than fabricate the answer.
    let bad_config = match crate::config::snapshot() {
        Ok(_) => false,
        Err(e) => {
            eprintln!(
                "warning: configuration does not parse — the action grants shown below are a \
                 DEFAULT empty policy, not your configured trust:\n{e}\n"
            );
            true
        }
    };
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
        return authoritative(bad_config);
    }
    if rows.is_empty() {
        println!("{name} has no action grants — it can propose nothing");
        println!(
            "grant actions in org.toml ([[action]] + [[trust]] rules), then check: roster server validate"
        );
        return authoritative(bad_config);
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
    println!("\npromotion is admin-only: rules live in org.toml / worker.toml; a denial revokes earned auto");
    authoritative(bad_config)
}
