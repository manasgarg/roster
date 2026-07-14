//! The task-dispatch half of `impyard server start`: the trusted orchestration
//! loop. It dispatches waiting tasks to the box (bounded concurrency), and when
//! a run ends decides whether the task is done or needs review (it filed a
//! gate). Runs beside the gateway in the same daemon, sharing the same on-disk
//! state. See docs/supervisor-spec.md.

use crate::run::boxed;
use crate::action::gate;
use crate::gateway::ledger;
use crate::work::queue;
use crate::work::trigger;
use std::time::Duration;
use tokio::task::JoinSet;

type BErr = Box<dyn std::error::Error>;

/// How often the loop re-polls the queue (for newly-filed tasks) whether idle or
/// running boxes — the ceiling on dispatch latency.
const POLL_SECS: u64 = 2;

pub async fn dispatch_loop(cap: usize, once: bool) -> Result<(), BErr> {
    if cap == 0 {
        return Err("--cap wants a positive integer".into());
    }
    reclaim();
    let mut set: JoinSet<(queue::Task, Result<boxed::Outcome, String>)> = JoinSet::new();

    loop {
        // Broken config = no governance guarantees; pause dispatch until it
        // parses again. The gateway is failing closed in parallel.
        if let Err(e) = crate::config::snapshot() {
            eprintln!("dispatch paused — config invalid:\n{e}");
            if once {
                return Err("config invalid".into());
            }
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
            continue;
        }
        // Fire any due schedule triggers, which file proactive tasks (§3.5).
        trigger::fire();

        // Fill idle slots with the next waiting tasks (each atomically claimed).
        while set.len() < cap {
            let Some(mut task) = queue::claim_next() else {
                break;
            };
            // D6: proactive work is soft-stopped when the imp is over budget;
            // owner-filed/continuation work always runs.
            if task.proactive {
                let limits = crate::gateway::budget::load_budget().limits;
                if let Some(reason) =
                    ledger::over_any_limit(&task.subject(), &limits, crate::util::now_ms())
                {
                    eprintln!("defer {} (proactive, {reason})", task.id);
                    let _ = queue::set_state(&mut task, "deferred");
                    continue;
                }
            }
            // Record the run id on the task before the box starts, so a crash
            // leaves a task we can map to its (now dead) container and reclaim.
            let run_id = boxed::new_run_id();
            task.run_id = Some(run_id.clone());
            let _ = queue::save(&task);
            let memory_context = task_memory_context(&task);
            let _ = crate::imp::memory::save_run_context(&run_id, &memory_context);
            eprintln!(
                "dispatch {} [{}] run {run_id} — {}",
                task.id,
                task.imp,
                first_line(&task.prompt)
            );
            let t = task.clone();
            let context_task = crate::imp::context::TaskInput {
                task_id: Some(task.id.clone()),
                origin: task.origin.clone(),
                text: task.prompt.clone(),
                continuation: task.context.get("resolved_gate").cloned(),
            };
            set.spawn(async move {
                let code = t.repo.as_ref().map(|r| boxed::CodeSpec {
                    repo: r.clone(),
                    base: t.base.clone().unwrap_or_else(|| "main".into()),
                });
                let out = boxed::dispatch(
                    &t.imp,
                    context_task,
                    &memory_context,
                    t.ceiling_min,
                    &t.id,
                    &run_id,
                    code.as_ref(),
                    &t.knowledge_mode,
                )
                .await
                .map_err(|e| e.to_string());
                (t, out)
            });
        }

        if set.is_empty() {
            if once {
                break;
            }
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
            continue;
        }

        // Wait for a run to finish, but wake at least every POLL_SECS so a task
        // filed while a box is running fills a free slot promptly, instead of
        // waiting for that box to finish.
        let joined =
            match tokio::time::timeout(Duration::from_secs(POLL_SECS), set.join_next()).await {
                Ok(j) => j,
                Err(_) => continue, // timed out — loop back and re-poll the queue
            };
        if let Some(joined) = joined {
            match joined {
                Ok((task, outcome)) => finalize(task, outcome),
                Err(e) => eprintln!("supervise: a run panicked: {e}"),
            }
        }
    }
    Ok(())
}

/// On startup, any task still marked `running` is orphaned from a previous
/// supervisor. If its box container is gone, put it back to `waiting` so it runs
/// again; if a container is somehow still alive, leave it be (don't double-run).
fn reclaim() {
    for mut t in queue::list_all()
        .into_iter()
        .filter(|t| t.state == "running")
    {
        if t.run_id.as_deref().map(boxed::box_alive).unwrap_or(false) {
            eprintln!(
                "reclaim: {} still has a live box ({}) — leaving it",
                t.id,
                t.run_id.as_deref().unwrap_or("")
            );
        } else if queue::set_state(&mut t, "waiting").is_ok() {
            eprintln!("reclaim: {} → waiting (no live box)", t.id);
        }
    }
}

fn task_memory_context(task: &queue::Task) -> crate::imp::memory::RunContext {
    let d = task
        .context
        .get("discord")
        .unwrap_or(&serde_json::Value::Null);
    crate::imp::memory::RunContext {
        provider: if d.is_null() {
            String::new()
        } else {
            "discord".into()
        },
        channel_id: d
            .get("channel_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        user_id: d
            .get("author_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        message_id: d
            .get("message_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        role: d
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        is_dm: d.get("is_dm").and_then(|v| v.as_bool()).unwrap_or(false),
        inbound: task.context.get("inbound").is_some(),
    }
}

/// Decide the task's terminal state from how the box ended and whether it left a
/// gate open. A filed gate → needs-review (a human, then a continuation).
fn finalize(mut task: queue::Task, outcome: Result<boxed::Outcome, String>) {
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
