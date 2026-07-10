//! `roster supervise` — the trusted orchestration loop. It dispatches waiting
//! tasks to the box (bounded concurrency), and when a run ends decides whether
//! the task is done or needs review (it filed a gate). Sibling to `serve`: both
//! are trusted Rust sharing the same on-disk state. See docs/supervisor-spec.md.
//!
//!   roster supervise [--cap <n>] [--once]

use crate::cmd::run_box;
use crate::{gate, queue};
use std::time::Duration;
use tokio::task::JoinSet;

type BErr = Box<dyn std::error::Error>;

pub async fn run(args: &[String]) -> Result<(), BErr> {
    let mut cap = 3usize;
    let mut once = false;
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
            other => return Err(format!("unknown supervise flag \"{other}\"").into()),
        }
    }

    eprintln!("roster supervise — dispatching tasks (cap {cap}{})", if once { ", once" } else { "" });
    let mut set: JoinSet<(queue::Task, Result<run_box::Outcome, String>)> = JoinSet::new();

    loop {
        // Fill idle slots with the next waiting tasks (each atomically claimed).
        while set.len() < cap {
            let Some(task) = queue::claim_next() else { break };
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

/// The prompt handed to the box. Phase 5 prepends the run-start briefing (open
/// gates + journal); for now it's the task prompt as filed.
fn effective_prompt(task: &queue::Task) -> String {
    task.prompt.clone()
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
