//! Actions — the box proposes a consequential action by POSTing an envelope to
//! the gateway's action host (`actions.roster.internal`). The gateway attributes
//! it to the worker (identity token on the CONNECT, un-spoofable), checks the
//! admin's action grants + the trust ladder, and either executes it now (auto)
//! or files a durable gate. Executors run trusted-side and hold the real
//! credentials the box never sees. See docs/actions-and-trust.md.

pub mod gate;
pub mod smtp;
pub mod trust;

use crate::action::gate::Gate;
use crate::action::trust::TrustRule;
use crate::gateway::proxy::Body;
use crate::paths;
use crate::util::now_rfc3339;
use crate::worker::journal;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;

/// The sentinel host the box POSTs action envelopes to. It never leaves the
/// gateway: all box HTTPS is proxied, so this arrives as CONNECT + a
/// TLS-terminated POST the gateway handles internally instead of forwarding.
pub const ACTION_HOST: &str = "actions.roster.internal";

// ── the envelope + admin policy ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Envelope {
    pub intent: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub task_id: String,
}

fn org_scope() -> String {
    "org".to_string()
}
fn gate_default() -> String {
    "gate".to_string()
}

/// An admin-declared action a worker may propose (compiled from `[[action]]`).
#[derive(Debug, Clone, Deserialize)]
pub struct ActionGrant {
    #[serde(default = "org_scope")]
    pub scope: String,
    pub name: String,
    pub executor: String,
    /// Default trust level (T0 = "gate"); the trust ladder can override per payload.
    #[serde(default = "gate_default")]
    pub trust: String,
    /// When true, resolving a gate for this action files a continuation task so
    /// the worker can react to the outcome (§3.5). Default: just close the task.
    #[serde(default)]
    pub wake_on_resolve: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionPolicy {
    #[serde(default)]
    pub actions: Vec<ActionGrant>,
    #[serde(default)]
    pub trust: Vec<TrustRule>,
}

pub fn load_action_policy() -> ActionPolicy {
    crate::config::snapshot()
        .map(|c| c.actions.clone())
        .unwrap_or_default()
}

fn grant_for<'a>(policy: &'a ActionPolicy, worker: &str, intent: &str) -> Option<&'a ActionGrant> {
    policy
        .actions
        .iter()
        .find(|a| crate::gateway::scope::applies(&a.scope, worker) && a.name == intent)
}

/// Is a channel-send payload targeting a channel an admin marked trusted?
/// (The trust store is channel-id keyed and platform-agnostic.)
fn channel_payload_trusted(payload: &Value) -> bool {
    payload
        .get("channel_id")
        .and_then(|v| v.as_str())
        .map(crate::channel::discord::channel_trusted)
        .unwrap_or(false)
}

// ── the gateway's action decision ────────────────────────────────────────────

fn reply(status: StatusCode, v: Value) -> Response<Body> {
    let mut resp = Response::new(
        Full::new(Bytes::from(v.to_string()))
            .map_err(|n| match n {})
            .boxed(),
    );
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    resp
}

/// Route a request to the action host. `worker` is resolved from the identity
/// token. POST /submit proposes an action; GET /gates and /journal are the
/// worker's read-only view of its own state (the box's cross-run awareness).
pub async fn handle_action(
    worker: &str,
    trusted_run_id: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Response<Body> {
    match (method, path) {
        ("POST", "/submit") => submit(worker, trusted_run_id, body).await,
        ("POST", "/outcome") => outcome(worker, trusted_run_id, body),
        ("GET", "/gates") => {
            let gates: Vec<Value> = crate::action::gate::for_worker(worker)
                .iter()
                .map(|g| g.summary())
                .collect();
            reply(StatusCode::OK, json!({ "gates": gates }))
        }
        ("GET", "/journal") => reply(
            StatusCode::OK,
            json!({ "events": journal::tail(worker, 30) }),
        ),
        _ => reply(
            StatusCode::NOT_FOUND,
            json!({ "status": "error", "error": "unknown action endpoint" }),
        ),
    }
}

/// The worker's outcome report for its task run — part of the task protocol,
/// not a granted action. The claim is recorded as evidence (journal +
/// run manifest); the host still attests the task's state when the box ends.
/// A run that ends silently after refused calls is attested failed.
fn outcome(worker: &str, trusted_run_id: &str, body: &[u8]) -> Response<Body> {
    if trusted_run_id.is_empty() {
        return reply(
            StatusCode::FORBIDDEN,
            json!({ "status": "error", "error": "an outcome report needs a trusted run context" }),
        );
    }
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({ "status": "error", "error": format!("bad outcome body: {e}") }),
            )
        }
    };
    let status = v.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(status, "completed" | "failed") {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({ "status": "error", "error": "outcome status must be \"completed\" or \"failed\"" }),
        );
    }
    let note = v
        .get("note")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty());
    if let Err(e) = crate::run::runlog::record_outcome_report(trusted_run_id, status, note) {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "status": "error", "error": format!("could not record the report: {e}") }),
        );
    }
    journal::append(
        worker,
        trusted_run_id,
        "outcome-reported",
        json!({ "status": status, "note": note }),
    );
    reply(StatusCode::OK, json!({ "status": "done" }))
}

/// Handle one action envelope: attribute, authorize, and either execute now or
/// file a gate.
async fn submit(worker: &str, trusted_run_id: &str, body: &[u8]) -> Response<Body> {
    let env: Envelope = match serde_json::from_slice(body) {
        Ok(e) => e,
        Err(e) => {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({ "status": "error", "error": format!("bad envelope: {e}") }),
            )
        }
    };
    let run_id = if trusted_run_id.is_empty() {
        env.run_id.clone()
    } else {
        trusted_run_id.to_string()
    };
    let policy = load_action_policy();
    let Some(grant) = grant_for(&policy, worker, &env.intent) else {
        journal::append(
            worker,
            &run_id,
            "action-refused",
            json!({ "intent": env.intent, "reason": "no action grant in scope" }),
        );
        audit(worker, &env.intent, "refused", None, None);
        return reply(
            StatusCode::FORBIDDEN,
            json!({ "status": "denied", "reason": format!("no action grant for \"{}\"", env.intent) }),
        );
    };
    let grant = grant.clone();
    journal::append(
        worker,
        &run_id,
        "action-proposed",
        json!({ "intent": env.intent, "rationale": env.rationale, "run_id": run_id }),
    );

    let (executed, denied) = gate::history(worker, &env.intent);
    let level = if grant.executor == "identity" {
        // Identity is worker-wide — always hard-gated (D10).
        "gate".to_string()
    } else if (grant.executor == "discord"
        || grant.executor == "slack"
        || grant.executor == "purpose")
        && channel_payload_trusted(&env.payload)
    {
        // Replies AND channel-purpose refinements flow without a gate in a trusted
        // channel — its participants are authorized to set the purpose (they could
        // `/purpose set` directly). Untrusted channels still gate for review.
        "auto".to_string()
    } else {
        trust::evaluate(
            worker,
            &env.intent,
            &env.payload,
            &grant.trust,
            &policy.trust,
            executed,
            denied,
        )
    };
    if level == "auto" {
        match run_executor(&grant.executor, worker, &env.intent, &env.payload, &run_id).await {
            Ok(result) => {
                journal::append(
                    worker,
                    &run_id,
                    "executed",
                    json!({ "intent": env.intent, "auto": true, "result": result }),
                );
                audit(worker, &env.intent, "auto-executed", None, Some(&result));
                reply(
                    StatusCode::OK,
                    json!({ "status": "done", "result": result }),
                )
            }
            Err(e) => {
                journal::append(
                    worker,
                    &run_id,
                    "failed",
                    json!({ "intent": env.intent, "auto": true, "error": e }),
                );
                audit(worker, &env.intent, "failed", None, None);
                // A failed side effect must not read as HTTP success: a caller
                // keying off the status code would report an unsent email as sent.
                reply(
                    StatusCode::BAD_GATEWAY,
                    json!({ "status": "error", "error": e }),
                )
            }
        }
    } else {
        let g = Gate {
            id: gate::new_id(),
            worker: worker.to_string(),
            intent: env.intent.clone(),
            executor: grant.executor.clone(),
            payload: env.payload.clone(),
            rationale: env.rationale.clone(),
            run_id: run_id.clone(),
            task_id: env.task_id.clone(),
            state: "pending".into(),
            filed_at: gate::now(),
            decided_by: None,
            decided_at: None,
            decision_note: None,
            executed_at: None,
            result: None,
            error: None,
        };
        if let Err(e) = gate::save(&g) {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "status": "error", "error": format!("could not file gate: {e}") }),
            );
        }
        journal::append(
            worker,
            &run_id,
            "gate-filed",
            json!({ "gate_id": g.id, "intent": env.intent, "rationale": env.rationale }),
        );
        audit(worker, &env.intent, "gated", Some(&g.id), None);
        reply(
            StatusCode::ACCEPTED,
            json!({ "status": "pending", "gate_id": g.id, "message": "held for human approval" }),
        )
    }
}

// ── approval-side execution (shared with `roster gates approve`) ─────────────

/// Execute an approved gate exactly once. Two phases: under the gate lock we
/// atomically claim a `pending`/`approved`/`failed` gate by moving it to
/// `executing` (a second approver then sees `executing` and is refused); the
/// executor — the real, non-idempotent side effect — runs with the lock
/// released; then, under the lock again, we record the terminal state.
///
/// `executing` is not retryable through this path: a gate stuck there means a
/// prior run died mid-execute, and we cannot know whether the email/PR/message
/// already went out, so we refuse rather than risk a double send.
pub async fn execute_gate(id: &str, decided_by: &str, note: Option<&str>) -> Result<Gate, String> {
    // Phase 1 — claim the gate for execution, exclusively.
    let mut g = {
        let _lock = gate::lock().map_err(|e| format!("gate lock: {e}"))?;
        let mut g = gate::load(id).ok_or_else(|| format!("no such gate {id}"))?;
        match g.state.as_str() {
            "executed" => return Err(format!("gate {id} already executed")),
            "denied" => return Err(format!("gate {id} was denied")),
            "executing" => {
                return Err(format!(
                    "gate {id} is mid-execution (or a prior run crashed while executing it) — \
                     inspect whether the action already happened before retrying"
                ))
            }
            // pending | approved | failed (a failed executor may be retried)
            _ => {}
        }
        if g.state == "pending" {
            g.state = "approved".into();
            g.decided_by = Some(decided_by.to_string());
            g.decided_at = Some(now_rfc3339());
            g.decision_note = note.map(String::from);
            gate::save(&g).map_err(|e| e.to_string())?;
            journal::append(
                &g.worker,
                &g.run_id,
                "approved",
                json!({ "gate_id": g.id, "by": decided_by, "note": note }),
            );
        }
        g.state = "executing".into();
        g.error = None;
        gate::save(&g).map_err(|e| e.to_string())?;
        g
    };

    // Phase 2 — the side effect runs without the lock, so a slow send can't
    // block other approvals; the `executing` marker guards against re-entry.
    let outcome = run_executor(&g.executor, &g.worker, &g.intent, &g.payload, &g.run_id).await;

    // Phase 3 — record the terminal state under the lock.
    let _lock = gate::lock().map_err(|e| format!("gate lock: {e}"))?;
    match outcome {
        Ok(result) => {
            g.state = "executed".into();
            g.executed_at = Some(now_rfc3339());
            g.result = Some(result.clone());
            gate::save(&g).map_err(|e| e.to_string())?;
            journal::append(
                &g.worker,
                &g.run_id,
                "executed",
                json!({ "gate_id": g.id, "intent": g.intent, "result": result }),
            );
            audit(&g.worker, &g.intent, "executed", Some(&g.id), Some(&result));
            resolve_followup(&g);
            Ok(g)
        }
        Err(e) => {
            g.state = "failed".into();
            g.error = Some(e.clone());
            gate::save(&g).map_err(|e| e.to_string())?;
            journal::append(
                &g.worker,
                &g.run_id,
                "failed",
                json!({ "gate_id": g.id, "intent": g.intent, "error": e }),
            );
            audit(&g.worker, &g.intent, "failed", Some(&g.id), None);
            // Close the loop even on failure, so the task doesn't wedge in
            // needs-review and the worker learns the action didn't happen.
            resolve_followup(&g);
            Err(e)
        }
    }
}

pub fn deny_gate(id: &str, decided_by: &str, note: Option<&str>) -> Result<Gate, String> {
    let _lock = gate::lock().map_err(|e| format!("gate lock: {e}"))?;
    let mut g = gate::load(id).ok_or_else(|| format!("no such gate {id}"))?;
    if g.is_terminal() {
        return Err(format!("gate {id} is already {}", g.state));
    }
    if g.state == "executing" {
        return Err(format!(
            "gate {id} is being executed — it can no longer be denied"
        ));
    }
    g.state = "denied".into();
    g.decided_by = Some(decided_by.to_string());
    g.decided_at = Some(now_rfc3339());
    g.decision_note = note.map(String::from);
    gate::save(&g).map_err(|e| e.to_string())?;
    journal::append(
        &g.worker,
        &g.run_id,
        "denied",
        json!({ "gate_id": g.id, "by": decided_by, "note": note }),
    );
    audit(&g.worker, &g.intent, "denied", Some(&g.id), None);
    resolve_followup(&g);
    Ok(g)
}

/// After a gate reaches a terminal state, close the loop for its originating
/// task: if all the task's gates are resolved, mark it done, and — when the
/// action opts in with `wake_on_resolve` — file a continuation task so a fresh
/// box can react to the outcome (§3.5: ephemeral boxes + async gates meet here).
fn resolve_followup(g: &Gate) {
    if g.task_id.is_empty() {
        return;
    }
    if let Some(task) = crate::work::tms::find(&g.task_id) {
        if task.state == "needs-review" && gate::pending_for_task(&task.id).is_empty() {
            let _ = crate::work::tms::finish(&task.worker, &task.id, "completed");
        }
    }

    let policy = load_action_policy();
    let wake = grant_for(&policy, &g.worker, &g.intent)
        .map(|a| a.wake_on_resolve)
        .unwrap_or(false);
    if !wake {
        return;
    }
    let short = g
        .worker
        .strip_prefix("org/")
        .unwrap_or(&g.worker)
        .to_string();
    let outcome = if g.state == "executed" {
        format!(
            "was approved and executed. Result: {}",
            g.result.clone().unwrap_or(json!(null))
        )
    } else {
        format!(
            "was denied{}",
            g.decision_note
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        )
    };
    let prompt = format!(
        "A previous action you proposed — {} (gate {}) — {}. Decide whether any follow-up is needed; if not, you are done.",
        g.intent, g.id, outcome
    );
    let context = json!({ "resolved_gate": { "id": g.id, "intent": g.intent, "state": g.state, "result": g.result, "decided_by": g.decided_by, "note": g.decision_note } });
    let _ = crate::work::tms::add(
        &short,
        crate::work::tms::Draft {
            prompt,
            created_by: "gate".into(),
            standing: "owner".into(),
            ceiling_min: 15.0,
            context,
            ..Default::default()
        },
    );
    journal::append(
        &g.worker,
        &g.run_id,
        "continuation-filed",
        json!({ "gate_id": g.id, "intent": g.intent }),
    );
}

// ── executors (trusted-side; hold real credentials the box never sees) ───────

/// Dispatch to the executor that performs an intent. New capabilities register
/// here. Executors that egress route through the gateway as the privileged
/// subject (uniform judge/inject/meter/audit); local ones act directly.
pub async fn run_executor(
    executor: &str,
    worker: &str,
    intent: &str,
    payload: &Value,
    run_id: &str,
) -> Result<Value, String> {
    match executor {
        "message-user" => exec_message_user(worker, payload).await,
        "email" => exec_email(worker, payload).await,
        "identity" => exec_identity(worker, payload),
        "purpose" => exec_purpose(payload),
        "discord" => exec_discord(worker, payload).await,
        "slack" => exec_slack(worker, payload).await,
        "term" => exec_term_send(worker, payload),
        "task" => match intent {
            "set-tasks" => exec_set_tasks(worker, payload, run_id),
            _ => exec_file_task(worker, payload, run_id),
        },
        "knowledge" => exec_knowledge_push(worker, run_id, payload),
        "self" => exec_file_update(worker, payload),
        other => Err(format!("no executor \"{other}\" for intent \"{intent}\"")),
    }
}

/// `repo_push` — land the run's committed branch on a gated repo's main
/// (docs/plans/worker-environment.md). The heavy lifting — bundle
/// quarantine, validation, ff-only advance in the integration lane — lives in
/// `worker::knowledge::push`; this maps the envelope and demands a trusted
/// run. A stale or refused push comes back as the action error, in-run, so
/// the agent can rebase and try again. `connection` defaults to "knowledge"
/// so the legacy `knowledge-push` grant keeps its exact meaning.
fn exec_knowledge_push(worker: &str, run_id: &str, payload: &Value) -> Result<Value, String> {
    if run_id.is_empty() {
        return Err("repo_push needs a trusted run context".into());
    }
    let connection = payload
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("knowledge");
    let head = payload
        .get("head")
        .and_then(Value::as_str)
        .ok_or("repo_push needs \"head\" (the commit sha to land)")?;
    let confirmed = payload.get("confirm_bulk_delete").and_then(Value::as_str) == Some("yes");
    let outcome = crate::worker::knowledge::push(worker, run_id, connection, head, confirmed)?;
    Ok(json!({
        "landed": outcome.commit,
        "files": outcome.files,
        "deletions": outcome.deletions,
    }))
}

/// Deliver a note from the worker to its lead: a Discord DM when a bot token +
/// lead id are configured, else the local inbox. The bot token stays in the
/// vault; the box never holds it.
/// `file_task` — the bridge across the memory/knowledge boundary: a run that
/// carries interaction content queues durable work instead of writing
/// records. The filed task is worker-only by construction (context: null —
/// no channel, no participants), so it runs clean-room eligible with a
/// writable knowledge mount. The prompt is the entire crossing, and the
/// participant scan polices it (docs/repos.md).
fn exec_file_task(worker: &str, payload: &Value, run_id: &str) -> Result<Value, String> {
    if run_id.is_empty() {
        return Err("file-task needs a trusted run context".into());
    }
    let prompt = payload
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("file-task needs a non-empty \"prompt\"")?;
    let ceiling = payload
        .get("ceiling_min")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0)
        .clamp(1.0, 240.0);

    let context = crate::worker::memory::load_run_context(run_id);
    if let Err(reason) = crate::worker::boundary::check_task_prompt(&context, prompt) {
        journal::append(
            worker,
            run_id,
            "boundary-denied",
            json!({ "intent": "file-task", "reason": reason }),
        );
        return Err(reason);
    }

    // Standing inherits the room: a trusted operator's ask is owner work;
    // untrusted contexts and autonomous runs are proactive (budget-paced).
    let standing = if matches!(context.role.as_str(), "host-op" | "admin" | "trusted") {
        "owner"
    } else {
        "proactive"
    };
    let scheduled_at = payload.get("at").and_then(|v| v.as_str()).map(String::from);
    let task = crate::work::tms::add(
        crate::paths::short_worker(worker),
        crate::work::tms::Draft {
            prompt: prompt.to_string(),
            created_by: "agent".into(),
            standing: standing.into(),
            ceiling_min: ceiling,
            tags: crate::work::tms::Tags {
                provider: Some(context.provider.clone()).filter(|p| !p.is_empty()),
                channel: context.channel_id.clone(),
                user: context.user_id.clone(),
            },
            scheduled_at,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    journal::append(
        worker,
        run_id,
        "task-filed",
        json!({ "task_id": task.id, "prompt": prompt }),
    );
    eprintln!("file-task [{worker}] → {} ({} min)", task.id, ceiling);
    Ok(json!({ "task_id": task.id, "state": task.state }))
}

/// `term_send` — deliver results to a terminal channel: append to the
/// channel's recorded history; `roster talk` shows what arrived while the
/// operator was away. The terminal is a real reply target, not inbound-only.
fn exec_term_send(worker: &str, payload: &Value) -> Result<Value, String> {
    let channel = payload
        .get("channel_id")
        .and_then(|v| v.as_str())
        .ok_or("term-send needs a \"channel_id\"")?;
    if !channel.starts_with("term-") {
        return Err(format!(
            "term-send is for terminal channels (term-…), not \"{channel}\" — use the platform's send tool"
        ));
    }
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("term-send needs non-empty \"text\"")?;
    let short = crate::paths::short_worker(worker);
    crate::channel::discord::persist_message(
        channel,
        &json!({
            "ts": crate::util::now_rfc3339(),
            "author_id": short, "author": short, "role": "worker",
            "content": text, "attachments": [],
        }),
    );
    eprintln!("term-send [{worker}] → channel {channel}");
    Ok(json!({ "delivered": "terminal", "channel_id": channel }))
}

/// `file_update` — the generic check-and-set write over the worker's own
/// editable files (docs/plans/worker-environment.md). The token is a content
/// hash, so an out-of-band hand edit on the host invalidates any stale token
/// a worker holds. Allowlist is server-side and deliberately short:
/// `config/worker.toml` to start. The schedule has set_tasks (structured,
/// versioned) and identity.md has the hard-gated identity action — this
/// path refuses both rather than offering a weaker write route.
///
/// After the write the whole config must still parse, or the edit is
/// reverted and rejected — a worker cannot break the deployment, only
/// change it (fail closed, exactly like a hand edit).
fn exec_file_update(worker: &str, payload: &Value) -> Result<Value, String> {
    use sha2::{Digest, Sha256};
    let hash = |bytes: &[u8]| -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    };
    let short = crate::paths::short_worker(worker);
    let rel = payload
        .get("path")
        .and_then(Value::as_str)
        .ok_or("file_update needs \"path\" (as listed under $HOME/self/)")?;
    let base_hash = payload
        .get("base_hash")
        .and_then(Value::as_str)
        .ok_or("file_update needs \"base_hash\" (sha256 of the content you read)")?;
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .ok_or("file_update needs \"content\" (the full new file)")?;
    let real =
        match rel {
            "config/worker.toml" => crate::paths::worker_dir(short).join("worker.toml"),
            "config/identity.md" => return Err(
                "identity.md is not written this way — propose the identity action; it waits for \
                 your lead's approval"
                    .into(),
            ),
            "schedule.json" => {
                return Err("the schedule is written with set_tasks, not file_update".into())
            }
            other => {
                return Err(format!(
                    "\"{other}\" is not worker-editable — editable: config/worker.toml"
                ))
            }
        };
    let _lock = crate::statefile::FileLock::acquire(&format!("selfedit-{short}"))
        .map_err(|e| format!("lock: {e}"))?;
    let current = crate::statefile::read_if_present(&real)
        .map_err(|e| format!("read {rel}: {e}"))?
        .unwrap_or_default();
    let current_hash = hash(current.as_bytes());
    if current_hash != base_hash {
        return Err(format!(
            "stale base_hash — the file changed since you read it (current sha256 {current_hash}); \
             re-read $HOME/self/{rel} and retry"
        ));
    }
    if content == current {
        return Ok(json!({ "status": "unchanged", "sha256": current_hash }));
    }
    crate::statefile::write_atomic(&real, content.as_bytes())
        .map_err(|e| format!("write {rel}: {e}"))?;
    if let Err(errors) = crate::config::load() {
        let revert = crate::statefile::write_atomic(&real, current.as_bytes());
        let reverted = if revert.is_ok() {
            "reverted"
        } else {
            "REVERT FAILED — tell your lead"
        };
        return Err(format!(
            "rejected — the edit breaks config validation ({reverted}):\n{}",
            errors.join("\n")
        ));
    }
    eprintln!("file-update [{worker}] {rel} ({} bytes)", content.len());
    Ok(json!({ "status": "ok", "sha256": hash(content.as_bytes()) }))
}

/// `set_tasks` — the agent's curation write over its own TMS partition
/// (docs/work.md): one optimistically-concurrent document
/// swap, validated host-side; a rejection tells the agent to re-read the
/// mounted view and retry.
fn exec_set_tasks(worker: &str, payload: &Value, run_id: &str) -> Result<Value, String> {
    if run_id.is_empty() {
        return Err("set-tasks needs a trusted run context".into());
    }
    let base_version = payload
        .get("base_version")
        .and_then(|v| v.as_u64())
        .ok_or("set-tasks needs \"base_version\" (read it from $HOME/self/schedule.json)")?;
    let tasks: Vec<crate::work::tms::Task> =
        serde_json::from_value(payload.get("tasks").cloned().unwrap_or_else(|| json!([])))
            .map_err(|e| format!("tasks: {e}"))?;
    let recurring: Vec<crate::work::tms::Recurring> = serde_json::from_value(
        payload
            .get("recurring")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|e| format!("recurring: {e}"))?;
    let context = crate::worker::memory::load_run_context(run_id);
    let short = crate::paths::short_worker(worker);
    match crate::work::tms::set_tasks(short, base_version, tasks, recurring, &context) {
        Ok(p) => {
            journal::append(
                worker,
                run_id,
                "tasks-set",
                json!({ "version": p.version, "tasks": p.tasks.len(), "recurring": p.recurring.len() }),
            );
            Ok(json!({ "status": "ok", "version": p.version }))
        }
        Err(rej) => Err(format!(
            "set_tasks rejected: {} — re-read $HOME/self/schedule.json (now version {}) and retry",
            rej.reason, rej.current.version
        )),
    }
}

async fn exec_message_user(worker: &str, payload: &Value) -> Result<Value, String> {
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or("message-user needs a \"text\" field")?;

    if let Some(cred) = crate::credential::vault::get_credential("discord") {
        let token = cred.get("token").and_then(|v| v.as_str());
        let owner = cred
            .get("owner_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let (Some(token), Some(owner)) = (token, owner) {
            match crate::channel::discord::open_dm(token, owner).await {
                Ok(dm) => match crate::channel::discord::post_chunked(token, &dm, text).await {
                    Ok(_) => {
                        eprintln!("message-user [{worker}] → lead DM");
                        return Ok(json!({ "delivered": "discord-dm" }));
                    }
                    Err(e) => {
                        eprintln!("message-user: DM post failed ({e}); trying other channels")
                    }
                },
                Err(e) => eprintln!("message-user: open DM failed ({e}); trying other channels"),
            }
        }
    }

    if let Some(cred) = crate::credential::vault::get_credential(&slack_credential_name(worker)) {
        let token = cred.get("bot_token").and_then(|v| v.as_str());
        let owner = cred
            .get("owner_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let (Some(token), Some(owner)) = (token, owner) {
            match crate::channel::slack::open_dm(token, owner).await {
                Ok(dm) => match crate::channel::slack::post_chunked(token, &dm, text, None).await {
                    Ok(_) => {
                        eprintln!("message-user [{worker}] → lead Slack DM");
                        return Ok(json!({ "delivered": "slack-dm" }));
                    }
                    Err(e) => eprintln!("message-user: Slack DM post failed ({e}); using inbox"),
                },
                Err(e) => eprintln!("message-user: Slack open DM failed ({e}); using inbox"),
            }
        }
    }

    let path = paths::messages_log();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(
            f,
            "{}",
            json!({ "ts": now_rfc3339(), "worker": worker, "text": text })
        );
    }
    eprintln!("message-user [{worker}]: {text}");
    Ok(json!({ "delivered": "inbox" }))
}

/// Record the worker's own outbound message in the channel's history, so a
/// later session's first-turn history block (and $HOME/channel) carries both
/// sides of the conversation. The listener never persists bot-authored
/// events, so this is the single writer for the worker's side — no
/// duplication, live or via catch-up.
fn persist_worker_message(channel: &str, worker: &str, text: &str) {
    let short = crate::paths::short_worker(worker);
    crate::channel::discord::persist_message(
        channel,
        &json!({
            "ts": now_rfc3339(),
            "author_id": short, "author": short, "role": "worker",
            "content": text, "attachments": [],
        }),
    );
}

/// Post a message to a Discord channel (the worker's reply). Trusted-side; the
/// bot token comes from the vault, never the box.
/// Post a system courtesy notice (e.g. "your task failed") to a chat channel
/// using the worker's own bot credential — the trusted side speaking directly,
/// not a gated action. Best-effort: a missing credential or send error is
/// logged, since the notice is itself a courtesy and must never wedge dispatch.
pub async fn deliver_notice(provider: &str, channel: &str, worker: &str, text: &str) {
    let result = match provider {
        "discord" => match crate::credential::vault::get_credential("discord")
            .and_then(|c| c.get("token").and_then(|v| v.as_str()).map(String::from))
        {
            Some(token) => crate::channel::discord::post_chunked(&token, channel, text)
                .await
                .map(|_| ()),
            None => Err("no discord credential".into()),
        },
        "slack" => {
            let name = slack_credential_name(worker);
            match crate::credential::vault::get_credential(&name).and_then(|c| {
                c.get("bot_token")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            }) {
                Some(token) => crate::channel::slack::post_chunked(&token, channel, text, None)
                    .await
                    .map(|_| ()),
                None => Err(format!("no \"{name}\" credential")),
            }
        }
        // term/talk surfaces failures in the session itself; nothing to post.
        _ => return,
    };
    match result {
        Ok(()) => persist_worker_message(channel, worker, text),
        Err(e) => eprintln!("notice to {provider} channel {channel} not delivered: {e}"),
    }
}

async fn exec_discord(worker: &str, payload: &Value) -> Result<Value, String> {
    let channel = payload
        .get("channel_id")
        .and_then(|v| v.as_str())
        .ok_or("discord-send needs a \"channel_id\"")?;
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("discord-send needs non-empty \"text\"")?;
    let cred = crate::credential::vault::get_credential("discord")
        .ok_or("no discord credential — run: roster connection add discord")?;
    let token = cred
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or("discord credential has no token")?;
    let id = crate::channel::discord::post_chunked(token, channel, text).await?;
    persist_worker_message(channel, worker, text);
    eprintln!("discord [{worker}] → channel {channel}");
    Ok(json!({ "sent": true, "channel_id": channel, "message_id": id }))
}

/// The worker's Slack credential: its `[channels] slack` binding from live
/// config, falling back to a credential literally named "slack".
fn slack_credential_name(worker: &str) -> String {
    let short = crate::paths::short_worker(worker).to_string();
    crate::config::snapshot()
        .ok()
        .and_then(|c| {
            c.listeners
                .iter()
                .find(|(w, platform, _)| *w == short && platform == "slack")
                .map(|(_, _, credential)| credential.clone())
        })
        .unwrap_or_else(|| "slack".to_string())
}

async fn exec_slack(worker: &str, payload: &Value) -> Result<Value, String> {
    let channel = payload
        .get("channel_id")
        .and_then(|v| v.as_str())
        .ok_or("slack-send needs a \"channel_id\"")?;
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("slack-send needs non-empty \"text\"")?;
    let thread_ts = payload
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let name = slack_credential_name(worker);
    let cred = crate::credential::vault::get_credential(&name)
        .ok_or_else(|| format!("no \"{name}\" credential — run: roster connection add slack"))?;
    let token = cred
        .get("bot_token")
        .and_then(|v| v.as_str())
        .ok_or("slack credential has no bot_token")?;
    let ts = crate::channel::slack::post_chunked(token, channel, text, thread_ts).await?;
    persist_worker_message(channel, worker, text);
    eprintln!("slack [{worker}] → channel {channel}");
    Ok(json!({ "sent": true, "channel_id": channel, "message_ts": ts }))
}

/// Send an email. If an SMTP credential is configured in the vault
/// (`roster connect smtp`), deliver for real over TLS; otherwise fall back to a
/// local sink so the gate→approve→execute→audit path still works offline. Either
/// way the box never holds the credential — this runs trusted-side, post-gate.
async fn exec_email(worker: &str, payload: &Value) -> Result<Value, String> {
    let to: Vec<String> = payload
        .get("to")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .ok_or("email needs a non-empty \"to\" array")?;
    let subject = payload
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let body = payload.get("body").and_then(|v| v.as_str()).unwrap_or("");

    if let Some(cfg) = smtp_config() {
        let status = crate::action::smtp::send(&cfg, &to, subject, body)
            .await
            .map_err(|e| format!("smtp send failed: {e}"))?;
        eprintln!("email [{worker}] → {to:?} via {}: {subject}", cfg.host);
        return Ok(
            json!({ "delivered": "smtp", "provider": cfg.host, "to": to, "status": status }),
        );
    }

    // No SMTP configured: fail loudly so an email is never silently dropped. The
    // local sink (a file, no real send) is opt-in for offline testing only.
    if std::env::var("ROSTER_EMAIL_SINK").is_err() {
        return Err("email not sent: no SMTP configured — run `roster connection add smtp` (e.g. your Mailgun SMTP creds)".into());
    }
    let dir = paths::outbox_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!(
        "{}-{}.json",
        now_rfc3339().replace(':', "-"),
        worker.replace('/', "_")
    ));
    let rendered = json!({ "from": worker, "to": to, "subject": subject, "body": body });
    std::fs::write(
        &file,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&rendered).unwrap_or_default()
        ),
    )
    .map_err(|e| e.to_string())?;
    eprintln!(
        "email [{worker}] → {to:?}: {subject} (ROSTER_EMAIL_SINK — wrote local sink, NOT sent)"
    );
    Ok(json!({ "delivered": "local-sink", "to": to, "file": file.display().to_string() }))
}

/// SMTP settings from the vault (`~/.roster/vault/smtp.json`), if present.
fn smtp_config() -> Option<crate::action::smtp::SmtpConfig> {
    let c = crate::credential::vault::get_credential("smtp")?;
    Some(crate::action::smtp::SmtpConfig {
        host: c.get("host")?.as_str()?.to_string(),
        port: c.get("port").and_then(|v| v.as_u64()).unwrap_or(465) as u16,
        user: c.get("user")?.as_str()?.to_string(),
        pass: c.get("pass")?.as_str()?.to_string(),
        from: c.get("from")?.as_str()?.to_string(),
    })
}

/// Overwrite a worker's identity, only after an admin approved the exact text
/// (D10). Trusted-side; the box never writes here (its repo mount is read-only).
fn exec_identity(worker: &str, payload: &Value) -> Result<Value, String> {
    let content = payload
        .get("identity")
        .or_else(|| payload.get("charter"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("identity-edit needs a non-empty \"identity\" field")?;
    let short = worker.strip_prefix("org/").unwrap_or(worker);
    let path = crate::run::boxed::identity_path(short);
    let dir = path.parent().ok_or("bad identity path")?;
    if !dir.exists() {
        return Err(format!(
            "no worker directory {} — is \"{short}\" a real worker?",
            dir.display()
        ));
    }
    write_atomic(&path, content)?;
    eprintln!(
        "identity [{worker}] updated ({} bytes)",
        content.trim().len()
    );
    Ok(json!({ "written": path.display().to_string(), "bytes": content.trim().len() }))
}

/// Overwrite a channel's purpose (channels/<id>/purpose.md), post-approval (D10).
fn exec_purpose(payload: &Value) -> Result<Value, String> {
    let channel = payload
        .get("channel_id")
        .and_then(|v| v.as_str())
        .ok_or("purpose-edit needs a \"channel_id\"")?;
    let content = payload
        .get("purpose")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("purpose-edit needs a non-empty \"purpose\" field")?;
    let path = crate::channel::discord::purpose_path(channel);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    write_atomic(&path, content)?;
    eprintln!(
        "purpose [{channel}] updated ({} bytes)",
        content.trim().len()
    );
    Ok(json!({ "written": path.display().to_string(), "channel_id": channel }))
}

fn write_atomic(path: &std::path::Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, format!("{}\n", content.trim())).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// A current-vs-proposed unified diff of a file (for `gates show` on an
/// identity/purpose gate — the reviewer sees exactly what would change).
pub fn identity_diff(worker: &str, proposed: &str) -> Option<String> {
    let short = worker.strip_prefix("org/").unwrap_or(worker);
    file_diff(&crate::run::boxed::identity_path(short), proposed)
}

pub fn purpose_diff(channel_id: &str, proposed: &str) -> Option<String> {
    file_diff(&crate::channel::discord::purpose_path(channel_id), proposed)
}

fn file_diff(current: &std::path::Path, proposed: &str) -> Option<String> {
    let current_arg = if current.exists() {
        current.to_path_buf()
    } else {
        std::path::PathBuf::from("/dev/null")
    };
    let tmp = std::env::temp_dir().join(format!("roster-charter-{}.md", std::process::id()));
    std::fs::write(&tmp, format!("{}\n", proposed.trim())).ok()?;
    let out = std::process::Command::new("git")
        .args(["diff", "--no-index", "--"])
        .arg(&current_arg)
        .arg(&tmp)
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&tmp);
    let diff = String::from_utf8_lossy(&out.stdout).to_string();
    if diff.trim().is_empty() {
        None
    } else {
        Some(diff)
    }
}

// ── audit ────────────────────────────────────────────────────────────────────

/// Append an action decision to the shared audit log, alongside egress decisions.
fn audit(
    worker: &str,
    intent: &str,
    disposition: &str,
    gate_id: Option<&str>,
    result: Option<&Value>,
) {
    // A random id, not a per-process counter: a counter restarts at 0 every boot
    // and collides with earlier ids in this append-only log.
    let rec = json!({
        "decision_id": format!("act-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        "ts": now_rfc3339(),
        "kind": "action",
        "worker": worker,
        "intent": intent,
        "disposition": disposition,
        "gate_id": gate_id,
        "result": result,
    });
    let path = paths::decisions_log();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{rec}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        let guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());
        // A minimal deployment that config::load() accepts.
        std::fs::create_dir_all(crate::paths::config_root()).unwrap();
        std::fs::write(crate::paths::org_file(), "").unwrap();
        std::fs::create_dir_all(crate::paths::worker_dir("dobby")).unwrap();
        std::fs::write(
            crate::paths::worker_dir("dobby").join("worker.toml"),
            "name = \"dobby\"\n",
        )
        .unwrap();
        (guard, dir)
    }

    fn sha(s: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        format!("{:x}", h.finalize())
    }

    #[test]
    fn file_update_cas_accepts_stales_and_reverts() {
        let _sb = sandbox();
        let path = crate::paths::worker_dir("dobby").join("worker.toml");

        // Correct hash lands the edit.
        let ok = exec_file_update(
            "org/dobby",
            &json!({ "path": "config/worker.toml", "base_hash": sha("name = \"dobby\"\n"),
                     "content": "name = \"dobby\" # edited\n" }),
        )
        .unwrap();
        assert_eq!(ok["status"], "ok");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name = \"dobby\" # edited\n"
        );

        // A stale hash is a clean conflict, not a write.
        let err = exec_file_update(
            "org/dobby",
            &json!({ "path": "config/worker.toml", "base_hash": sha("name = \"dobby\"\n"),
                     "content": "clobber\n" }),
        )
        .unwrap_err();
        assert!(err.contains("stale base_hash"), "{err}");

        // An edit that breaks config validation is reverted.
        let err = exec_file_update(
            "org/dobby",
            &json!({ "path": "config/worker.toml", "base_hash": sha("name = \"dobby\" # edited\n"),
                     "content": "not = valid = toml\n" }),
        )
        .unwrap_err();
        assert!(err.contains("reverted"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name = \"dobby\" # edited\n"
        );

        // Paths with their own write channels are refused by name.
        for (p, hint) in [
            ("config/identity.md", "identity"),
            ("schedule.json", "set_tasks"),
            ("journal/journal.jsonl", "not worker-editable"),
        ] {
            let err = exec_file_update(
                "org/dobby",
                &json!({ "path": p, "base_hash": "x", "content": "y" }),
            )
            .unwrap_err();
            assert!(err.contains(hint), "{p}: {err}");
        }
    }
}
