//! `roster worker task` — file, list, and inspect tasks: the human window
//! into the TMS partition (docs/specs/task-management.md).

use crate::util::BErr;
use crate::work::tms;

#[allow(clippy::too_many_arguments)]
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
    let kind = if reorganize {
        "reorganization"
    } else if repo.is_some() {
        "code"
    } else if proactive {
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
            repo,
            base,
            knowledge_mode: if reorganize {
                "reorganization"
            } else {
                "append"
            }
            .into(),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    println!("queued {} for {} ({kind})", t.id, t.worker);
    Ok(())
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
    println!(
        "filed by {}  (standing: {})",
        t.created_by, t.standing
    );
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
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "tasks": tasks,
                "recurring": recurring,
            }))?
        );
        return Ok(());
    }
    if tasks.is_empty() && recurring.is_empty() {
        println!("no tasks");
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
    println!("\ncompleted and failed tasks live in each worker's journal: data/workers/<name>/tasks/journal.jsonl");
    Ok(())
}

fn short_time(value: &str) -> String {
    if value.len() >= 16 {
        format!("{} {}", &value[..10], &value[11..16])
    } else {
        value.to_string()
    }
}
