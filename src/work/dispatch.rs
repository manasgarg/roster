//! The task-dispatch half of `roster server start`: the trusted orchestration
//! loop, a dumb executor over the TMS (docs/work.md). Each
//! tick it asks the TMS who is due, applies its own envelope — the concurrency
//! cap, and budgets for proactive standing — runs one governed box per task,
//! and attests claim/complete/fail back. It holds no plan and no timer.

use crate::action::gate;
use crate::gateway::ledger;
use crate::run::boxed;
use crate::work::tms;
use std::collections::HashSet;
use std::time::Duration;
use tokio::task::JoinSet;

type BErr = Box<dyn std::error::Error>;

/// How often the loop re-polls the TMS whether idle or running boxes — the
/// ceiling on dispatch latency.
const POLL_SECS: u64 = 2;

pub async fn dispatch_loop(cap: usize, once: bool) -> Result<(), BErr> {
    if cap == 0 {
        return Err("--cap wants a positive integer".into());
    }
    let mut set: JoinSet<(tms::Task, Result<boxed::Outcome, String>)> = JoinSet::new();
    // Tasks handed to boxes this tick-cycle but not yet joined, so a slow
    // journal write can't double-dispatch.
    let mut in_flight: HashSet<String> = HashSet::new();
    reclaim(&mut set, &mut in_flight);
    let mut credential_warned = false;

    loop {
        // Broken config = no governance guarantees; pause dispatch until it
        // parses again. The gateway is failing closed in parallel.
        let config = match crate::config::snapshot() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("dispatch paused — config invalid:\n{e}");
                if once {
                    return Err("config invalid".into());
                }
                tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
                continue;
            }
        };

        // No model credential = every box would fail on arrival. Hold the
        // queue (tasks stay pending) instead of burning each one to a failed
        // run; resume the moment a credential appears.
        let creds_ok = boxed::model_credentials_available();
        if !creds_ok && !credential_warned {
            eprintln!(
                "dispatch holding — no model credential; tasks stay queued. \
                 Connect one: roster connection add anthropic  (or openai-codex)"
            );
            credential_warned = true;
        }
        if creds_ok && credential_warned {
            eprintln!("dispatch resuming — a model credential is available");
            credential_warned = false;
        }

        if !creds_ok && once && set.is_empty() {
            return Err(
                "no model credential — tasks cannot run. Connect one: \
                 roster connection add anthropic  (or openai-codex)"
                    .into(),
            );
        }

        // The TMS keeps heartbeats honest and spawns due recurrences inside
        // due(); what comes back is eligibility, owner standing first. While
        // holding, skip claiming only — in-flight boxes still join below.
        let mut idle = true;
        'workers: for due in tms::due(&config.heartbeats) {
            if !creds_ok {
                break;
            }
            for task in due.claimable {
                if set.len() >= cap {
                    break 'workers;
                }
                if in_flight.contains(&task.id) {
                    continue;
                }
                // D6: proactive work is paced by the envelope; owner work
                // always runs. An unaffordable task is simply late — it stays
                // claimable and runs when the window clears.
                if task.proactive() {
                    let limits = crate::gateway::budget::load_budget().limits;
                    if ledger::over_any_limit(&task.subject(), &limits, crate::util::now_ms())
                        .is_some()
                    {
                        continue;
                    }
                }
                let run_id = boxed::new_run_id();
                let task = match tms::claim(&due.worker, &task.id, &run_id) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("dispatch: could not claim {}: {e}", task.id);
                        continue;
                    }
                };
                idle = false;
                in_flight.insert(task.id.clone());
                let memory_context = task_memory_context(&task);
                let _ = crate::worker::memory::save_run_context(&run_id, &memory_context);
                eprintln!(
                    "dispatch {} [{}] run {run_id} — {}",
                    task.id,
                    task.worker,
                    first_line(&task.prompt)
                );
                // Routing, not provenance: the origin channel reaches the
                // box through the briefing while the run context stays clean
                // (the tainting context.discord path is only for relays).
                let reply_to = if memory_context.channel_id.is_none() {
                    task.tags
                        .provider
                        .clone()
                        .zip(task.tags.channel.clone())
                        .map(|(provider, channel)| crate::worker::context::ReplyTo {
                            provider,
                            channel,
                        })
                } else {
                    None
                };
                let context_task = crate::worker::context::TaskInput {
                    task_id: Some(task.id.clone()),
                    origin: task.created_by.clone(),
                    text: task.prompt.clone(),
                    continuation: task.context.get("resolved_gate").cloned(),
                    reply_to,
                };
                let t = task.clone();
                set.spawn(async move {
                    let code = t.repo.as_ref().map(|r| boxed::CodeSpec {
                        repo: r.clone(),
                        base: t.base.clone().unwrap_or_else(|| "main".into()),
                    });
                    let spec = boxed::RunSpec {
                        worker: &t.worker,
                        run_id: &run_id,
                        task_id: &t.id,
                        ceiling_min: t.ceiling_min,
                        code: code.as_ref(),
                        run_context: &memory_context,
                        knowledge_mode: &t.knowledge_mode,
                    };
                    let out = boxed::dispatch(spec, context_task)
                        .await
                        .map_err(|e| e.to_string());
                    (t, out)
                });
            }
        }

        if set.is_empty() {
            if once {
                if idle {
                    break;
                }
                continue;
            }
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
            continue;
        }

        // Wait for a run to finish, but wake at least every POLL_SECS so a
        // newly due task fills a free slot promptly.
        let joined =
            match tokio::time::timeout(Duration::from_secs(POLL_SECS), set.join_next()).await {
                Ok(j) => j,
                Err(_) => continue, // timed out — loop back and re-poll
            };
        if let Some(joined) = joined {
            match joined {
                Ok((task, outcome)) => {
                    in_flight.remove(&task.id);
                    finalize(task, outcome);
                }
                Err(e) => eprintln!("supervise: a run panicked: {e}"),
            }
        }
    }
    Ok(())
}

/// On startup, any task still `claimed` is orphaned from a previous supervisor.
/// If its box is gone, put it back to pending so it runs again. If its box is
/// still running, ADOPT it — wait for it and finalize the task on exit — rather
/// than leaving it wedged in `claimed` forever (never attested, then requeued
/// and re-run on the next restart, repeating its side effects).
fn reclaim(
    set: &mut JoinSet<(tms::Task, Result<boxed::Outcome, String>)>,
    in_flight: &mut HashSet<String>,
) {
    for t in tms::list_all().into_iter().filter(|t| t.state == "claimed") {
        let run_id = t.run_id.clone().unwrap_or_default();
        if !run_id.is_empty() && boxed::box_alive(&run_id) {
            eprintln!("reclaim: adopting live box {run_id} for {}", t.id);
            in_flight.insert(t.id.clone());
            let task = t.clone();
            set.spawn(async move {
                let out = boxed::adopt(&run_id).await;
                (task, out)
            });
        } else if tms::finish(&t.worker, &t.id, "pending").is_ok() {
            eprintln!("reclaim: {} → pending (no live box)", t.id);
        }
    }
}

fn task_memory_context(task: &tms::Task) -> crate::worker::memory::RunContext {
    let d = task
        .context
        .get("discord")
        .unwrap_or(&serde_json::Value::Null);
    crate::worker::memory::RunContext {
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
        thread_ts: d
            .get("thread_ts")
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

/// Attest the outcome. Host evidence rules first — a crash, a nonzero exit,
/// or the ceiling is failure no matter what anyone claims; a filed gate is
/// needs-review. Then the worker's own outcome report (task_complete /
/// task_fail — a claim, not authority) decides, and a run that ended
/// silently after refused gateway calls is failure: the worker was blocked
/// (a spent budget, a revoked credential, a denied host) and never said it
/// finished. The reason rides along so `task show` can answer "why".
fn finalize(task: tms::Task, outcome: Result<boxed::Outcome, String>) {
    let (next, error) = match outcome {
        Err(e) => {
            eprintln!("task {} failed to run: {e}", task.id);
            ("failed", Some(e))
        }
        Ok(o) => {
            let report = task
                .run_id
                .as_deref()
                .and_then(crate::run::runlog::outcome_report);
            let refusals = task
                .run_id
                .as_deref()
                .map(crate::gateway::proxy::take_refusals)
                .unwrap_or(0);
            if o.ended_by == "ceiling" {
                (
                    "failed",
                    Some(format!(
                        "hit the {:.0}-minute wall-clock ceiling",
                        task.ceiling_min
                    )),
                )
            } else if o.exit_code != Some(0) {
                eprintln!("task {} box exited {:?}", task.id, o.exit_code);
                (
                    "failed",
                    Some(match o.exit_code {
                        Some(code) => format!("the box exited with code {code}"),
                        None => "the box was killed before it exited".to_string(),
                    }),
                )
            } else if !gate::pending_for_task(&task.id).is_empty() {
                ("needs-review", None)
            } else {
                match report {
                    Some((status, note)) if status == "failed" => (
                        "failed",
                        Some(note.unwrap_or_else(|| "the worker reported failure".into())),
                    ),
                    Some(_) => ("completed", None),
                    None if refusals > 0 => (
                        "failed",
                        Some(format!(
                            "ended without reporting an outcome after {refusals} refused gateway call(s) — transcript: roster server runs show {}",
                            task.run_id.as_deref().unwrap_or("?")
                        )),
                    ),
                    None => ("completed", None),
                }
            }
        }
    };
    if let Err(e) = tms::finish_with(&task.worker, &task.id, next, error) {
        eprintln!("supervise: could not update task {}: {e}", task.id);
    } else {
        eprintln!("task {} → {next}", task.id);
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    // Truncate by character, not byte: a multibyte char straddling byte 60
    // would panic here, and this runs after claim — a poison-pill crash loop.
    if line.chars().count() > 60 {
        format!("{}…", line.chars().take(60).collect::<String>())
    } else {
        line.to_string()
    }
}
