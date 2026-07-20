//! `roster worker task` — file, list, and inspect tasks: the human window
//! into the TMS partition (docs/work.md).

use crate::util::BErr;
use crate::work::tms;

pub fn add(worker: &str, ceiling: f64, proactive: bool, prompt: String) -> Result<(), BErr> {
    crate::worker::require_worker(worker)?;
    if prompt.trim().is_empty() {
        return Err("task add needs a prompt".into());
    }
    let kind = if proactive {
        "proactive"
    } else {
        "owner-filed"
    };
    let t = tms::add(
        worker,
        tms::Draft {
            prompt,
            created_by: "user".into(),
            standing: if proactive { "proactive" } else { "owner" }.into(),
            ceiling_min: ceiling,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    println!("queued {} for {} ({kind})", t.id, t.worker);
    if gateway_up_quick() {
        println!("watch it: roster worker task show {}", t.id);
    } else {
        println!("note: the server isn't running — this task will wait (roster server start)");
    }
    Ok(())
}

/// A synchronous daemon-liveness probe (the async one lives in cli::server) —
/// cheap enough to answer "will this task actually run?" right after filing.
/// One plain-HTTP /healthz round trip; the config root in the reply keeps a
/// foreign deployment's daemon from counting as ours.
fn gateway_up_quick() -> bool {
    use std::io::{Read, Write};
    let port = crate::gateway::recorded_port();
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body);
    if !body.contains("\"ok\":true") {
        return false;
    }
    match body.find("\"config_root\":\"") {
        None => true, // a daemon from before deployment identity: assume ours
        Some(_) => body.contains(&format!(
            "\"config_root\":{}",
            serde_json::json!(crate::paths::config_root().display().to_string())
        )),
    }
}

/// A live task by unique prefix, or a journaled one by exact id.
fn resolve(id_or_prefix: &str) -> Result<tms::Task, BErr> {
    let tasks = tms::list_all();
    match crate::util::resolve_prefix("task", id_or_prefix, tasks.iter().map(|t| t.id.as_str())) {
        Ok(id) => Ok(tasks.into_iter().find(|t| t.id == id).unwrap()),
        Err(e) => tms::find(id_or_prefix).ok_or(e),
    }
}

/// Put a claimed (dead box) or needs-review task back to pending.
pub fn requeue(id: &str) -> Result<(), BErr> {
    let t = resolve(id)?;
    println!("{}", tms::requeue(&t.id).map_err(|e| e.to_string())?);
    Ok(())
}

pub fn show(id: &str) -> Result<(), BErr> {
    let t = resolve(id)?;
    println!("task     {}", t.id);
    println!("worker   {}", t.worker);
    println!("state    {}", t.state);
    if let Some(error) = &t.error {
        println!("error    {}", crate::run::runlog::one_line(error, 300));
    }
    println!("filed by {}  (standing: {})", t.created_by, t.standing);
    if let Some(ch) = &t.tags.channel {
        println!("channel  {ch}");
    }
    if let Some(u) = &t.tags.user {
        println!("user     {u}");
    }
    println!("created  {}", t.created_at);
    println!("updated  {}", t.updated_at);
    if let Some(at) = &t.scheduled_at {
        println!("scheduled {at}");
    }
    if !t.depends_on.is_empty() {
        println!("depends  {}", t.depends_on.join(", "));
    }
    if let Some(r) = &t.recurring_id {
        println!("recurring {r}");
    }
    println!("ceiling  {} min", t.ceiling_min);
    if let Some(run) = &t.run_id {
        println!("run      {run}   (details: roster server runs show {run})");
    }
    let gates = crate::action::gate::pending_for_task(&t.id);
    if !gates.is_empty() {
        println!(
            "gates    {} pending: {}",
            gates.len(),
            gates
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !t.context.is_null() {
        println!("context  {}", serde_json::to_string_pretty(&t.context)?);
    }

    // The full timeline from this task's run — the worker's own journal.
    if let Some(run) = &t.run_id {
        let events = crate::worker::journal::for_run(&t.subject(), run);
        if !events.is_empty() {
            println!(
                "\njournal (run {run}, {} event{}):",
                events.len(),
                if events.len() == 1 { "" } else { "s" }
            );
            for e in &events {
                println!(
                    "  {}  {:<16}  {}",
                    e.get("ts").and_then(|v| v.as_str()).unwrap_or(""),
                    e.get("kind").and_then(|v| v.as_str()).unwrap_or("?"),
                    e.get("detail").cloned().unwrap_or(serde_json::Value::Null)
                );
            }
        }
    }

    println!("\nprompt:\n{}", t.prompt);
    Ok(())
}

pub fn ls(json: bool) -> Result<(), BErr> {
    let mut tasks = tms::list_all();
    tasks.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    let recurring = tms::list_recurring();
    // Recently finished/failed work stays in view — a task must never simply
    // vanish from the listing when its run ends. (Requeued ids are live again;
    // skip their stale journal records.)
    let recent: Vec<(String, tms::Task)> = tms::journal_recent(8)
        .into_iter()
        .filter(|(_, t)| !tasks.iter().any(|live| live.id == t.id))
        .take(5)
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "tasks": tasks,
                "recurring": recurring,
                "recent": recent
                    .iter()
                    .map(|(ts, t)| serde_json::json!({ "ts": ts, "task": t }))
                    .collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    if tasks.is_empty() && recurring.is_empty() && recent.is_empty() {
        println!("no tasks — file one: roster worker task add <worker> \"<prompt>\"");
        return Ok(());
    }
    if !tasks.is_empty() {
        println!(
            "{:<12}  {:<10}  {:<12}  {:<9}  {:<17}  {:<17}  PROMPT",
            "TASK", "WORKER", "STATE", "STANDING", "SCHEDULED", "UPDATED (UTC)"
        );
        for t in &tasks {
            let prompt = crate::run::runlog::one_line(&t.prompt, 44);
            println!(
                "{:<12}  {:<10}  {:<12}  {:<9}  {:<17}  {:<17}  {}",
                t.id,
                t.worker,
                if t.depends_on.is_empty() {
                    t.state.clone()
                } else {
                    format!("{} ⛓", t.state)
                },
                t.standing,
                t.scheduled_at
                    .as_deref()
                    .map(short_time)
                    .unwrap_or_else(|| "-".into()),
                short_time(&t.updated_at),
                prompt
            );
        }
    }
    if !recurring.is_empty() {
        println!(
            "\n{:<12}  {:<10}  {:<16}  {:<9}  PROMPT",
            "RECURRING", "WORKER", "SCHEDULE", "STANDING"
        );
        for r in &recurring {
            println!(
                "{:<12}  {:<10}  {:<16}  {:<9}  {}{}",
                r.id,
                r.worker,
                r.schedule,
                r.standing,
                if r.system { "[system] " } else { "" },
                crate::run::runlog::one_line(&r.prompt, 44)
            );
        }
    }
    if !recent.is_empty() {
        println!(
            "\n{:<12}  {:<10}  {:<12}  {:<17}  PROMPT (failed: the error)",
            "RECENT", "WORKER", "OUTCOME", "FINISHED (UTC)"
        );
        for (ts, t) in &recent {
            let note = match (&t.state[..], &t.error) {
                ("failed", Some(error)) => crate::run::runlog::one_line(error, 60),
                _ => crate::run::runlog::one_line(&t.prompt, 60),
            };
            println!(
                "{:<12}  {:<10}  {:<12}  {:<17}  {}",
                t.id,
                t.worker,
                t.state,
                short_time(ts),
                note
            );
        }
    }
    println!("\nfull history: roster server runs ls   one task: roster worker task show <id>");
    Ok(())
}

fn short_time(value: &str) -> String {
    if value.len() >= 16 {
        format!("{} {}", &value[..10], &value[11..16])
    } else {
        value.to_string()
    }
}
