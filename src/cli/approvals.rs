//! `roster server approvals` — the approval desk, the human seat of the gate
//! mechanism (the system gates; the human approves). A person lists what is
//! pending their approval, inspects the exact payload that would go out, and
//! approves or denies. No model at the edge (D12/§3.9). Approve executes the
//! gate idempotently; deny records the refusal. Both append to the worker's
//! journal and the audit log.

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
        println!("nothing pending approval");
        return Ok(());
    }
    println!("{:<12}  {:<10}  {:<16}  FILED", "GATE", "WORKER", "INTENT");
    for g in pending {
        println!(
            "{:<12}  {:<10}  {:<16}  {}",
            g.id, g.worker, g.intent, g.filed_at
        );
    }
    println!("\napprove: roster server approvals approve <id>   deny: roster server approvals deny <id> \"reason\"");
    Ok(())
}

pub fn show(id: &str) -> Result<(), BErr> {
    print!("{}", render_show(id)?);
    Ok(())
}

/// The exact action a gate would execute, rendered — shared by the CLI and
/// the slash surface (channel::slash), so both show the same thing.
pub fn render_show(id: &str) -> Result<String, BErr> {
    use std::fmt::Write;
    let id = resolve(id)?;
    let g = gate::load(&id).ok_or_else(|| format!("no such gate {id}"))?;
    let mut out = String::new();
    writeln!(out, "gate     {}", g.id)?;
    writeln!(out, "worker   {}", g.worker)?;
    writeln!(out, "intent   {}   (executor: {})", g.intent, g.executor)?;
    writeln!(out, "state    {}", g.state)?;
    writeln!(out, "filed    {}", g.filed_at)?;
    if let Some(by) = &g.decided_by {
        writeln!(
            out,
            "decided  {} by {} {}",
            g.decided_at.as_deref().unwrap_or(""),
            by,
            g.decision_note.as_deref().unwrap_or("")
        )?;
    }
    if let Some(r) = &g.result {
        writeln!(out, "result   {r}")?;
    }
    if let Some(e) = &g.error {
        writeln!(out, "error    {e}")?;
    }
    if !g.rationale.is_empty() {
        writeln!(out, "rationale {}", g.rationale)?;
    }
    // Render the change per executor: a charter gate shows a current-vs-proposed
    // diff; everything else shows its payload.
    match g.executor.as_str() {
        "identity" => {
            let proposed = g
                .payload
                .get("identity")
                .or_else(|| g.payload.get("charter"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match action::identity_diff(&g.worker, proposed) {
                Some(d) => writeln!(out, "\nidentity change — current vs proposed:\n{d}")?,
                None => writeln!(
                    out,
                    "\n(the proposed identity is identical to the current one)"
                )?,
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
                Some(d) => writeln!(
                    out,
                    "\npurpose change (channel {ch}) — current vs proposed:\n{d}"
                )?,
                None => writeln!(
                    out,
                    "\n(the proposed purpose is identical to the current one)"
                )?,
            }
        }
        _ => writeln!(
            out,
            "\npayload — the exact action that will run:\n{}",
            serde_json::to_string_pretty(&g.payload)?
        )?,
    }
    Ok(out)
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
