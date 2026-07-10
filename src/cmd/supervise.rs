//! `roster supervise` — the trusted orchestration loop. It dispatches waiting
//! tasks to the box (bounded concurrency), and when a run ends decides whether
//! the task is done or needs review (it filed a gate). Sibling to `serve`: both
//! are trusted Rust sharing the same on-disk state. See docs/supervisor-spec.md.
//!
//!   roster supervise [--cap <n>] [--once]

use crate::cmd::run_box;
use crate::{gate, ledger, queue, trigger};
use std::time::Duration;
use tokio::task::JoinSet;

type BErr = Box<dyn std::error::Error>;

pub async fn run(args: &[String]) -> Result<(), BErr> {
    let mut cap = 3usize;
    let mut once = false;
    let mut fire_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cap" => {
                cap = args.get(i + 1).and_then(|s| s.parse().ok()).filter(|&n| n >= 1).ok_or("--cap wants a positive integer")?;
                i += 2;
            }
            "--once" => {
                once = true;
                i += 1;
            }
            // Fire due schedule triggers (file their tasks) and exit — for a cron
            // driver, or to test triggers without dispatching.
            "--fire-only" => {
                fire_only = true;
                i += 1;
            }
            other => return Err(format!("unknown supervise flag \"{other}\"").into()),
        }
    }

    if fire_only {
        let n = trigger::fire();
        println!("fired {n} trigger(s)");
        return Ok(());
    }

    eprintln!("roster supervise — dispatching tasks (cap {cap}{})", if once { ", once" } else { "" });
    let mut set: JoinSet<(queue::Task, Result<run_box::Outcome, String>)> = JoinSet::new();

    loop {
        // Fire any due schedule triggers, which file proactive tasks (§3.5).
        trigger::fire();

        // Fill idle slots with the next waiting tasks (each atomically claimed).
        while set.len() < cap {
            let Some(task) = queue::claim_next() else { break };
            // D6: proactive work is soft-stopped when the worker is over budget;
            // owner-filed/continuation work always runs.
            if task.proactive {
                let limits = crate::budget::load_budget().limits;
                if let Some(reason) = ledger::over_any_limit(&task.subject(), &limits, crate::util::now_ms()) {
                    let mut t = task;
                    eprintln!("defer {} (proactive, {reason})", t.id);
                    let _ = queue::set_state(&mut t, "deferred");
                    continue;
                }
            }
            eprintln!("dispatch {} [{}] {}", task.id, task.worker, first_line(&task.prompt));
            let t = task.clone();
            let prompt = effective_prompt(&task);
            set.spawn(async move {
                let out = run_box::dispatch(&t.worker, &prompt, t.ceiling_min, &t.id).await.map_err(|e| e.to_string());
                (t, out)
            });
        }

        if set.is_empty() {
            if once {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        if let Some(joined) = set.join_next().await {
            match joined {
                Ok((task, outcome)) => finalize(task, outcome),
                Err(e) => eprintln!("supervise: a run panicked: {e}"),
            }
        }
    }
    Ok(())
}

/// The prompt handed to the box, prefixed with a run-start briefing so the
/// worker starts with awareness of its own state: any outcome it's reacting to
/// (a continuation), and its open gates (so it doesn't re-propose them). During
/// the run it can also query live state via the check_gates tool.
fn effective_prompt(task: &queue::Task) -> String {
    let subject = task.subject();
    let mut brief: Vec<String> = Vec::new();

    if let Some(rg) = task.context.get("resolved_gate") {
        brief.push(format!(
            "You are continuing after an earlier action resolved: {} — state {}.",
            rg.get("intent").and_then(|v| v.as_str()).unwrap_or("?"),
            rg.get("state").and_then(|v| v.as_str()).unwrap_or("?"),
        ));
    }

    let open: Vec<String> = gate::for_worker(&subject)
        .into_iter()
        .filter(|g| !g.is_terminal())
        .map(|g| format!("{} ({})", g.intent, g.id))
        .collect();
    if !open.is_empty() {
        brief.push(format!("You have actions already awaiting approval — do NOT re-propose these: {}.", open.join(", ")));
    }

    if brief.is_empty() {
        task.prompt.clone()
    } else {
        format!("[Briefing]\n{}\n\n[Task]\n{}", brief.join("\n"), task.prompt)
    }
}

/// Decide the task's terminal state from how the box ended and whether it left a
/// gate open. A filed gate → needs-review (a human, then a continuation).
fn finalize(mut task: queue::Task, outcome: Result<run_box::Outcome, String>) {
    let next = match outcome {
        Err(e) => {
            eprintln!("task {} failed to run: {e}", task.id);
            "failed"
        }
        Ok(o) => {
            task.run_id = Some(o.run_id.clone());
            if o.ended_by == "ceiling" {
                "failed"
            } else if o.exit_code != Some(0) {
                eprintln!("task {} box exited {:?}", task.id, o.exit_code);
                "failed"
            } else if !gate::pending_for_task(&task.id).is_empty() {
                "needs-review"
            } else {
                "done"
            }
        }
    };
    if let Err(e) = queue::set_state(&mut task, next) {
        eprintln!("supervise: could not update task {}: {e}", task.id);
    } else {
        eprintln!("task {} → {next}", task.id);
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.len() > 60 {
        format!("{}…", &line[..60])
    } else {
        line.to_string()
    }
}
