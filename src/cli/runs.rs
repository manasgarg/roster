//! `roster server runs` inspection — every execution, including warm Discord
//! sessions that intentionally bypass the durable task queue.

use crate::worker::context as context_compiler;
use crate::worker::journal;
use crate::worker::memory;
use crate::run::runlog;
use crate::util::BErr;
use crate::work::tms;

pub fn ls(worker: Option<&str>, limit: usize, json: bool) -> Result<(), BErr> {
    if limit == 0 {
        return Err("--limit wants a positive integer".into());
    }
    let runs: Vec<_> = runlog::list()
        .into_iter()
        .filter(|run| worker.map(|w| run.worker == w).unwrap_or(true))
        .take(limit)
        .collect();
    if json {
        let rows: Vec<serde_json::Value> = runs
            .iter()
            .map(|run| {
                serde_json::json!({
                    "id": run.id, "worker": run.worker, "kind": run.kind,
                    "state": run.state, "started_at": run.started_at,
                    "ended_at": run.ended_at, "task_id": run.task_id,
                    "channel_id": run.channel_id,
                    "error": run.record.as_ref().and_then(|r| r.error.clone()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if runs.is_empty() {
        println!(
            "no runs yet — every session lands here once a worker runs \
             (roster talk <worker>, roster worker run <worker> \"<prompt>\", or a dispatched task)"
        );
        return Ok(());
    }
    println!(
        "{:<29}  {:<10}  {:<9}  {:<10}  {:<17}  SCOPE/TASK",
        "RUN", "WORKER", "KIND", "STATE", "STARTED (UTC)"
    );
    for run in runs {
        let scope = run
            .task_id
            .as_deref()
            .map(|id| format!("task:{id}"))
            .or_else(|| run.channel_id.as_deref().map(|id| format!("channel:{id}")))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<29}  {:<10}  {:<9}  {:<10}  {:<17}  {}",
            run.id,
            run.worker,
            run.kind,
            run.state,
            short_time(&run.started_at),
            scope
        );
    }
    Ok(())
}

pub fn show(id: &str) -> Result<(), BErr> {
    let run = runlog::resolve(id).map_err(|e| -> BErr { e.into() })?;
    println!("run       {}", run.id);
    println!("worker    {}", run.worker);
    println!("kind      {}", run.kind);
    println!("state     {}", run.state);
    println!("started   {}", run.started_at);
    if let Some(ended) = &run.ended_at {
        println!("ended     {ended}");
    }
    if let Some(task) = &run.task_id {
        println!("task      {task}");
    }
    if let Some(channel) = &run.channel_id {
        println!("channel   {channel}");
    }
    if let Some(user) = &run.user_id {
        println!("user      {user}");
    }
    if let Some(record) = &run.record {
        if let Some(ended_by) = &record.ended_by {
            println!("ended by  {ended_by}");
        }
        if let Some(exit) = record.exit_code {
            println!("exit      {exit}");
        }
        if let Some(error) = &record.error {
            println!("error     {}", runlog::one_line(error, 300));
        }
        if let Some(knowledge) = &record.knowledge {
            println!("knowledge {}", knowledge.state);
            println!("  mode    {}", knowledge.mode);
            println!("  base    {}", knowledge.base_commit);
            println!("  records {}", knowledge.record_namespace);
            if let Some(commit) = &knowledge.produced_commit {
                println!("  commit  {commit}");
            }
            if let Some(error) = &knowledge.error {
                println!("  error   {}", runlog::one_line(error, 200));
            }
        }
    }
    println!("path      {}", run.run_dir.display());

    if let Some(task_id) = &run.task_id {
        if let Some(task) = tms::find(task_id) {
            println!("\ntask prompt:\n{}", task.prompt);
        }
    }

    let conversation = runlog::conversation(&run.run_dir);
    if !conversation.is_empty() {
        let omitted = conversation.len().saturating_sub(50);
        println!(
            "\nconversation ({} message{}):",
            conversation.len(),
            if conversation.len() == 1 { "" } else { "s" }
        );
        if omitted > 0 {
            println!("  … {omitted} earlier messages omitted");
        }
        for line in conversation.iter().skip(omitted) {
            println!("  {line}");
        }
    }

    let subject = if run.worker == "?" {
        None
    } else {
        Some(format!("org/{}", run.worker))
    };
    let events = subject
        .as_deref()
        .map(|s| journal::for_run(s, &run.id))
        .unwrap_or_default();
    if !events.is_empty() {
        println!(
            "\njournal ({} event{}):",
            events.len(),
            if events.len() == 1 { "" } else { "s" }
        );
        for event in events {
            println!(
                "  {}  {:<18}  {}",
                event.get("ts").and_then(|v| v.as_str()).unwrap_or(""),
                event.get("kind").and_then(|v| v.as_str()).unwrap_or("?"),
                runlog::one_line(
                    &event
                        .get("detail")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                        .to_string(),
                    200
                )
            );
        }
    }

    let recalls = memory::recall_trace(&run.id);
    if !recalls.is_empty() {
        println!(
            "\nmemory recall ({} turn{}):",
            recalls.len(),
            if recalls.len() == 1 { "" } else { "s" }
        );
        for recall in recalls {
            let selected = recall
                .get("selected")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!(
                "  {}  selected: {}",
                recall.get("ts").and_then(|v| v.as_str()).unwrap_or(""),
                if selected.is_empty() { "-" } else { &selected }
            );
        }
    }

    let contexts = context_compiler::trace_events(&run.id);
    if !contexts.is_empty() {
        println!(
            "\ncompiled context ({} event{}):",
            contexts.len(),
            if contexts.len() == 1 { "" } else { "s" }
        );
        for event in contexts {
            let status = event
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let phase = event
                .get("phase")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let used = event
                .pointer("/budget/used_chars")
                .and_then(|value| value.as_u64())
                .map(|value| format!("{value} chars"))
                .unwrap_or_else(|| "-".into());
            let blocks = event
                .get("blocks")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("kind").and_then(|value| value.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!(
                "  {:<5} {:<8} {:>10}  {}",
                phase,
                status,
                used,
                if blocks.is_empty() { "-" } else { &blocks }
            );
        }
        println!("  exact prompts: roster server runs context {}", run.id);
    }

    let files = runlog::files(&run.run_dir);
    if !files.is_empty() {
        println!("\nfiles:");
        for (path, bytes) in files {
            println!("  {:>10}  {path}", human_bytes(bytes));
        }
    }
    Ok(())
}

pub fn context(id: &str, all: bool) -> Result<(), BErr> {
    let run = runlog::resolve(id).map_err(|error| -> BErr { error.into() })?;
    let mut events: Vec<_> = context_compiler::trace_events(&run.id)
        .into_iter()
        .filter(|event| event.get("status").and_then(|value| value.as_str()) == Some("compiled"))
        .collect();
    if events.is_empty() {
        return Err(format!("run {} has no compiled context", run.id).into());
    }
    let session_system = events
        .iter()
        .rev()
        .find_map(|event| {
            event
                .get("system_prompt")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(String::from)
        })
        .unwrap_or_default();
    if !all {
        events = vec![events.pop().unwrap()];
    }
    for (index, event) in events.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!(
            "=== {} {}{} ===",
            event
                .get("phase")
                .and_then(|value| value.as_str())
                .unwrap_or("?"),
            event
                .get("ts")
                .and_then(|value| value.as_str())
                .unwrap_or(""),
            event
                .get("turn_id")
                .and_then(|value| value.as_str())
                .map(|id| format!(" turn {id}"))
                .unwrap_or_default()
        );
        let event_system = event
            .get("system_prompt")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let system = if !all && event_system.is_empty() {
            session_system.as_str()
        } else {
            event_system
        };
        let input = event
            .get("input_prompt")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !system.is_empty() {
            println!("\n--- system ---\n{system}");
        }
        if !input.is_empty() {
            println!("\n--- input ---\n{input}");
        }
    }
    Ok(())
}

/// The memory recall trace for one session (was: `roster memory explain`).
pub fn recall(id: &str) -> Result<(), BErr> {
    let run = runlog::resolve(id).map_err(|e| -> BErr { e.into() })?;
    let trace = memory::recall_trace(&run.id);
    if trace.is_empty() {
        println!("no memory recall trace for run {}", run.id);
        return Ok(());
    }
    for event in trace {
        println!("{}", serde_json::to_string_pretty(&event)?);
    }
    Ok(())
}

fn short_time(value: &str) -> String {
    if value.len() >= 16 {
        format!("{} {}", &value[..10], &value[11..16])
    } else {
        value.to_string()
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_time_and_sizes() {
        assert_eq!(short_time("2026-07-10T21:51:18Z"), "2026-07-10 21:51");
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
    }
}
