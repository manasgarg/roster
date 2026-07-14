//! `roster server gates` — the approval desk. A human lists pending gates,
//! inspects the exact payload that would go out, and approves or denies. No
//! model at the edge (D12/§3.9): a person decides. Approve executes the gate
//! idempotently; deny records the refusal. Both append to the worker's journal
//! and the audit log.

use crate::action;
use crate::action::gate;
use crate::util::BErr;

fn resolve(id_or_prefix: &str) -> Result<String, BErr> {
    let all = gate::list_all();
    crate::util::resolve_prefix("gate", id_or_prefix, all.iter().map(|g| g.id.as_str()))
}

pub fn ls(json: bool) -> Result<(), BErr> {
    let pending = gate::list_pending();
    if json {
        println!("{}", serde_json::to_string_pretty(&pending)?);
        return Ok(());
    }
    if pending.is_empty() {
        println!("no pending gates");
        return Ok(());
    }
    println!("{:<12}  {:<10}  {:<16}  FILED", "GATE", "WORKER", "INTENT");
    for g in pending {
        println!(
            "{:<12}  {:<10}  {:<16}  {}",
            g.id, g.worker, g.intent, g.filed_at
        );
    }
    println!("\napprove: roster server gates approve <id>   deny: roster server gates deny <id> \"reason\"");
    Ok(())
}

pub fn show(id: &str) -> Result<(), BErr> {
    let id = resolve(id)?;
    let g = gate::load(&id).ok_or_else(|| format!("no such gate {id}"))?;
    println!("gate     {}", g.id);
    println!("worker   {}", g.worker);
    println!("intent   {}   (executor: {})", g.intent, g.executor);
    println!("state    {}", g.state);
    println!("filed    {}", g.filed_at);
    if let Some(by) = &g.decided_by {
        println!(
            "decided  {} by {} {}",
            g.decided_at.as_deref().unwrap_or(""),
            by,
            g.decision_note.as_deref().unwrap_or("")
        );
    }
    if let Some(r) = &g.result {
        println!("result   {r}");
    }
    if let Some(e) = &g.error {
        println!("error    {e}");
    }
    if !g.rationale.is_empty() {
        println!("rationale {}", g.rationale);
    }
    // Render the change per executor: a charter gate shows a current-vs-proposed
    // diff; a code gate shows the worktree diff; everything else shows its payload.
    match g.executor.as_str() {
        "identity" => {
            let proposed = g
                .payload
                .get("identity")
                .or_else(|| g.payload.get("charter"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match action::identity_diff(&g.worker, proposed) {
                Some(d) => println!("\nidentity change — current vs proposed:\n{d}"),
                None => println!("\n(the proposed identity is identical to the current one)"),
            }
        }
        "purpose" => {
            let ch = g
                .payload
                .get("channel_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let proposed = g
                .payload
                .get("purpose")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match action::purpose_diff(ch, proposed) {
                Some(d) => println!("\npurpose change (channel {ch}) — current vs proposed:\n{d}"),
                None => println!("\n(the proposed purpose is identical to the current one)"),
            }
        }
        "git-pr" => {
            println!("\npayload:\n{}", serde_json::to_string_pretty(&g.payload)?);
            match action::worktree_diff(&g.run_id) {
                Some(d) if !d.is_empty() => println!("\ndiff — what would be committed:\n{d}"),
                _ => println!("\ndiff — (no changes found in the worktree)"),
            }
        }
        _ => println!(
            "\npayload — the exact action that will run:\n{}",
            serde_json::to_string_pretty(&g.payload)?
        ),
    }
    Ok(())
}

pub async fn approve(id: &str, note: Option<&str>) -> Result<(), BErr> {
    let id = resolve(id)?;
    let who = std::env::var("USER").unwrap_or_else(|_| "admin".into());
    match action::execute_gate(&id, &who, note).await {
        Ok(g) => {
            println!("approved and executed {} ({})", g.id, g.intent);
            if let Some(r) = &g.result {
                println!("result: {r}");
            }
            Ok(())
        }
        Err(e) => Err(format!("gate {id}: {e}").into()),
    }
}

pub fn deny(id: &str, note: Option<&str>) -> Result<(), BErr> {
    let id = resolve(id)?;
    let who = std::env::var("USER").unwrap_or_else(|_| "admin".into());
    let g = action::deny_gate(&id, &who, note)?;
    println!("denied {} ({})", g.id, g.intent);
    Ok(())
}
