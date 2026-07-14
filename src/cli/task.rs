//! `roster worker task` — file, list, and inspect tasks for dispatch.

use crate::util::BErr;
use crate::work::queue;

pub fn add(
    worker: &str,
    ceiling: f64,
    proactive: bool,
    reorganize: bool,
    repo: Option<String>,
    base: &str,
    prompt: String,
) -> Result<(), BErr> {
    crate::worker::require_worker(worker)?;
    if prompt.trim().is_empty() {
        return Err("task add needs a prompt".into());
    }
    // Absolute path so the worktree resolves from the daemon's cwd.
    let repo = repo.map(|p| {
        std::fs::canonicalize(&p)
            .map(|c| c.display().to_string())
            .unwrap_or(p)
    });
    let base = repo.as_ref().map(|_| base.to_string());
    if reorganize {
        if let Some(active) = queue::active_reorganization(worker) {
            return Err(format!(
                "worker {worker} already has reorganization task {} in state {}",
                active.id, active.state
            )
            .into());
        }
    }
    let knowledge_mode = if reorganize {
        "reorganization"
    } else {
        "append"
    };
    let kind = if reorganize {
        "reorganization"
    } else if repo.is_some() {
        "code"
    } else if proactive {
        "proactive"
    } else {
        "owner-filed"
    };
    let t = queue::create(
        worker,
        &prompt,
        "manual",
        proactive,
        ceiling,
        knowledge_mode,
        serde_json::Value::Null,
        repo,
        base,
    )
    .map_err(|e| e.to_string())?;
    println!("queued {} for {} ({kind})", t.id, t.worker);
    Ok(())
}

fn resolve(id_or_prefix: &str) -> Result<queue::Task, BErr> {
    let tasks = queue::list_all();
    let id =
        crate::util::resolve_prefix("task", id_or_prefix, tasks.iter().map(|t| t.id.as_str()))?;
    Ok(tasks.into_iter().find(|t| t.id == id).unwrap())
}

/// Put a stuck or finished task back to `waiting` so the daemon runs it again.
/// Refuses if a box for it is still live (would double-run).
pub fn requeue(id: &str) -> Result<(), BErr> {
    let mut t = resolve(id)?;
    if t.state == "waiting" {
        println!("task {} is already waiting", t.id);
        return Ok(());
    }
    if let Some(run) = &t.run_id {
        if crate::run::boxed::box_alive(run) {
            return Err(format!(
                "task {} still has a live box ({run}) — let it finish, or `docker kill {}` first",
                t.id,
                crate::run::boxed::container_name(run)
            )
            .into());
        }
    }
    if t.knowledge_mode == "reorganization" {
        if let Some(active) =
            queue::active_reorganization(&t.worker).filter(|active| active.id != t.id)
        {
            return Err(format!(
                "worker {} already has reorganization task {} in state {}",
                t.worker, active.id, active.state
            )
            .into());
        }
    }
    let prev = t.state.clone();
    t.run_id = None;
    queue::set_state(&mut t, "waiting").map_err(|e| e.to_string())?;
    println!("requeued {}: {prev} → waiting", t.id);
    Ok(())
}

pub fn show(id: &str) -> Result<(), BErr> {
    let t = resolve(id)?;
    println!("task     {}", t.id);
    println!("worker   {}", t.worker);
    println!("state    {}", t.state);
    println!(
        "origin   {}{}",
        t.origin,
        if t.proactive { " (proactive)" } else { "" }
    );
    println!("created  {}", t.created_at);
    println!("updated  {}", t.updated_at);
    println!("ceiling  {} min", t.ceiling_min);
    println!("knowledge {}", t.knowledge_mode);
    if let Some(run) = &t.run_id {
        println!("run      {run}   (transcript: state/runs/{run}/stdout.jsonl)");
    }
    if let Some(repo) = &t.repo {
        println!("repo     {repo} @ {}", t.base.as_deref().unwrap_or("main"));
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

    // The full timeline from this task's run — the worker's own (run-tagged) journal.
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
    let mut tasks = queue::list_all();
    tasks.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&tasks)?);
        return Ok(());
    }
    if tasks.is_empty() {
        println!("no tasks");
        return Ok(());
    }
    println!(
        "{:<12}  {:<10}  {:<12}  {:<17}  {:<29}  {:<12}  PROMPT",
        "TASK", "WORKER", "STATE", "UPDATED (UTC)", "RUN", "ORIGIN"
    );
    for t in tasks {
        let prompt = crate::run::runlog::one_line(&t.prompt, 48);
        let updated = if t.updated_at.len() >= 16 {
            format!("{} {}", &t.updated_at[..10], &t.updated_at[11..16])
        } else {
            t.updated_at.clone()
        };
        println!(
            "{:<12}  {:<10}  {:<12}  {:<17}  {:<29}  {:<12}  {}",
            t.id,
            t.worker,
            t.state,
            updated,
            t.run_id.as_deref().unwrap_or("-"),
            t.origin,
            prompt
        );
    }
    Ok(())
}
