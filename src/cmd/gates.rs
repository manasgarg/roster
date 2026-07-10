//! `roster gates` — the approval desk. A human lists pending gates, inspects the
//! exact payload that would go out, and approves or denies. No model at the edge
//! (D12/§3.9): a person decides. Approve executes the gate idempotently; deny
//! records the refusal. Both append to the worker's journal and the audit log.

use crate::action;
use crate::gate;

type BErr = Box<dyn std::error::Error>;

pub async fn run(args: &[String]) -> Result<(), BErr> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    match sub {
        "ls" | "list" => ls(),
        "show" => show(args.get(1).ok_or("usage: roster gates show <id>")?),
        "approve" => approve(args.get(1).ok_or("usage: roster gates approve <id> [note]")?, args.get(2).map(String::as_str)).await,
        "deny" => deny(args.get(1).ok_or("usage: roster gates deny <id> [note]")?, args.get(2).map(String::as_str)),
        other => Err(format!("unknown gates subcommand \"{other}\" (try: ls, show, approve, deny)").into()),
    }
}

fn ls() -> Result<(), BErr> {
    let pending = gate::list_pending();
    if pending.is_empty() {
        println!("no pending gates");
        return Ok(());
    }
    println!("{:<12}  {:<10}  {:<16}  {}", "GATE", "WORKER", "INTENT", "FILED");
    for g in pending {
        println!("{:<12}  {:<10}  {:<16}  {}", g.id, g.worker, g.intent, g.filed_at);
    }
    println!("\napprove: roster gates approve <id>   deny: roster gates deny <id> \"reason\"");
    Ok(())
}

fn show(id: &str) -> Result<(), BErr> {
    let g = gate::load(id).ok_or_else(|| format!("no such gate {id}"))?;
    println!("gate     {}", g.id);
    println!("worker   {}", g.worker);
    println!("intent   {}   (executor: {})", g.intent, g.executor);
    println!("state    {}", g.state);
    println!("filed    {}", g.filed_at);
    if let Some(by) = &g.decided_by {
        println!("decided  {} by {} {}", g.decided_at.as_deref().unwrap_or(""), by, g.decision_note.as_deref().unwrap_or(""));
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
    println!("\npayload — the exact action that will run:\n{}", serde_json::to_string_pretty(&g.payload)?);
    Ok(())
}

async fn approve(id: &str, note: Option<&str>) -> Result<(), BErr> {
    let who = std::env::var("USER").unwrap_or_else(|_| "owner".into());
    match action::execute_gate(id, &who, note).await {
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

fn deny(id: &str, note: Option<&str>) -> Result<(), BErr> {
    let who = std::env::var("USER").unwrap_or_else(|_| "owner".into());
    let g = action::deny_gate(id, &who, note)?;
    println!("denied {} ({})", g.id, g.intent);
    Ok(())
}
