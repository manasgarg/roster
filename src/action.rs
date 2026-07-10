//! Actions — the box proposes a consequential action by POSTing an envelope to
//! the gateway's action host (`actions.roster.internal`). The gateway attributes
//! it to the worker (identity token on the CONNECT, un-spoofable), checks the
//! owner's action grants + the trust ladder, and either executes it now (auto)
//! or files a durable gate. Executors run trusted-side and hold the real
//! credentials the box never sees. See docs/supervisor-spec.md.

use crate::gate::{self, Gate};
use crate::journal;
use crate::proxy::Body;
use crate::trust::{self, TrustRule};
use crate::util::{now_rfc3339, root};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// The sentinel host the box POSTs action envelopes to. It never leaves the
/// gateway: all box HTTPS is proxied, so this arrives as CONNECT + a
/// TLS-terminated POST the gateway handles internally instead of forwarding.
pub const ACTION_HOST: &str = "actions.roster.internal";

// ── the envelope + owner policy ──────────────────────────────────────────────

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

/// An owner-declared action a worker may propose (compiled from `[[action]]`).
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
    let path = root().join("runs").join("compiled").join("actions.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<ActionPolicy>(&s).ok())
        .unwrap_or_default()
}

fn grant_for<'a>(policy: &'a ActionPolicy, worker: &str, intent: &str) -> Option<&'a ActionGrant> {
    policy.actions.iter().find(|a| crate::scope::applies(&a.scope, worker) && a.name == intent)
}

// ── the gateway's action decision ────────────────────────────────────────────

fn reply(status: StatusCode, v: Value) -> Response<Body> {
    let mut resp = Response::new(Full::new(Bytes::from(v.to_string())).map_err(|n| match n {}).boxed());
    *resp.status_mut() = status;
    resp.headers_mut().insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
    resp
}

/// Route a request to the action host. `worker` is resolved from the identity
/// token. POST /submit proposes an action; GET /gates and /journal are the
/// worker's read-only view of its own state (the box's cross-run awareness).
pub async fn handle_action(worker: &str, method: &str, path: &str, body: &[u8]) -> Response<Body> {
    match (method, path) {
        ("POST", "/submit") => submit(worker, body).await,
        ("GET", "/gates") => {
            let gates: Vec<Value> = crate::gate::for_worker(worker).iter().map(|g| g.summary()).collect();
            reply(StatusCode::OK, json!({ "gates": gates }))
        }
        ("GET", "/journal") => reply(StatusCode::OK, json!({ "events": journal::tail(worker, 30) })),
        _ => reply(StatusCode::NOT_FOUND, json!({ "status": "error", "error": "unknown action endpoint" })),
    }
}

/// Handle one action envelope: attribute, authorize, and either execute now or
/// file a gate.
async fn submit(worker: &str, body: &[u8]) -> Response<Body> {
    let env: Envelope = match serde_json::from_slice(body) {
        Ok(e) => e,
        Err(e) => return reply(StatusCode::BAD_REQUEST, json!({ "status": "error", "error": format!("bad envelope: {e}") })),
    };
    let policy = load_action_policy();
    let Some(grant) = grant_for(&policy, worker, &env.intent) else {
        journal::append(worker, "action-refused", json!({ "intent": env.intent, "reason": "no action grant in scope" }));
        audit(worker, &env.intent, "refused", None, None);
        return reply(StatusCode::FORBIDDEN, json!({ "status": "denied", "reason": format!("no action grant for \"{}\"", env.intent) }));
    };
    let grant = grant.clone();

    journal::append(worker, "action-proposed", json!({ "intent": env.intent, "rationale": env.rationale, "run_id": env.run_id }));

    let (executed, denied) = gate::history(worker, &env.intent);
    let level = trust::evaluate(worker, &env.intent, &env.payload, &grant.trust, &policy.trust, executed, denied);
    if level == "auto" {
        match run_executor(&grant.executor, worker, &env.intent, &env.payload, &env.run_id).await {
            Ok(result) => {
                journal::append(worker, "executed", json!({ "intent": env.intent, "auto": true, "result": result }));
                audit(worker, &env.intent, "auto-executed", None, Some(&result));
                reply(StatusCode::OK, json!({ "status": "done", "result": result }))
            }
            Err(e) => {
                journal::append(worker, "failed", json!({ "intent": env.intent, "auto": true, "error": e }));
                audit(worker, &env.intent, "failed", None, None);
                reply(StatusCode::OK, json!({ "status": "error", "error": e }))
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
            run_id: env.run_id.clone(),
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
            return reply(StatusCode::INTERNAL_SERVER_ERROR, json!({ "status": "error", "error": format!("could not file gate: {e}") }));
        }
        journal::append(worker, "gate-filed", json!({ "gate_id": g.id, "intent": env.intent, "rationale": env.rationale }));
        audit(worker, &env.intent, "gated", Some(&g.id), None);
        reply(StatusCode::ACCEPTED, json!({ "status": "pending", "gate_id": g.id, "message": "held for human approval" }))
    }
}

// ── approval-side execution (shared with `roster gates approve`) ─────────────

/// Execute an approved gate, idempotently. `pending`/`approved` → run; `executed`
/// is terminal (never re-runs); a crash between approve and execute resumes.
pub async fn execute_gate(id: &str, decided_by: &str, note: Option<&str>) -> Result<Gate, String> {
    let mut g = gate::load(id).ok_or_else(|| format!("no such gate {id}"))?;
    if g.state == "executed" {
        return Err(format!("gate {id} already executed"));
    }
    if g.state == "denied" {
        return Err(format!("gate {id} was denied"));
    }
    if g.state == "pending" {
        g.state = "approved".into();
        g.decided_by = Some(decided_by.to_string());
        g.decided_at = Some(now_rfc3339());
        g.decision_note = note.map(String::from);
        gate::save(&g).map_err(|e| e.to_string())?;
        journal::append(&g.worker, "approved", json!({ "gate_id": g.id, "by": decided_by, "note": note }));
    }
    g.state = "executing".into();
    gate::save(&g).map_err(|e| e.to_string())?;
    match run_executor(&g.executor, &g.worker, &g.intent, &g.payload, &g.run_id).await {
        Ok(result) => {
            g.state = "executed".into();
            g.executed_at = Some(now_rfc3339());
            g.result = Some(result.clone());
            gate::save(&g).map_err(|e| e.to_string())?;
            journal::append(&g.worker, "executed", json!({ "gate_id": g.id, "intent": g.intent, "result": result }));
            audit(&g.worker, &g.intent, "executed", Some(&g.id), Some(&result));
            resolve_followup(&g);
            Ok(g)
        }
        Err(e) => {
            g.state = "failed".into();
            g.error = Some(e.clone());
            gate::save(&g).map_err(|e| e.to_string())?;
            journal::append(&g.worker, "failed", json!({ "gate_id": g.id, "intent": g.intent, "error": e }));
            audit(&g.worker, &g.intent, "failed", Some(&g.id), None);
            Err(e)
        }
    }
}

pub fn deny_gate(id: &str, decided_by: &str, note: Option<&str>) -> Result<Gate, String> {
    let mut g = gate::load(id).ok_or_else(|| format!("no such gate {id}"))?;
    if g.is_terminal() {
        return Err(format!("gate {id} is already {}", g.state));
    }
    g.state = "denied".into();
    g.decided_by = Some(decided_by.to_string());
    g.decided_at = Some(now_rfc3339());
    g.decision_note = note.map(String::from);
    gate::save(&g).map_err(|e| e.to_string())?;
    journal::append(&g.worker, "denied", json!({ "gate_id": g.id, "by": decided_by, "note": note }));
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
    if let Some(mut task) = crate::queue::find(&g.task_id) {
        if task.state == "needs-review" && gate::pending_for_task(&task.id).is_empty() {
            let _ = crate::queue::set_state(&mut task, "done");
        }
    }

    let policy = load_action_policy();
    let wake = grant_for(&policy, &g.worker, &g.intent).map(|a| a.wake_on_resolve).unwrap_or(false);
    if !wake {
        return;
    }
    let short = g.worker.strip_prefix("org/").unwrap_or(&g.worker).to_string();
    let outcome = if g.state == "executed" {
        format!("was approved and executed. Result: {}", g.result.clone().unwrap_or(json!(null)))
    } else {
        format!("was denied{}", g.decision_note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default())
    };
    let prompt = format!(
        "A previous action you proposed — {} (gate {}) — {}. Decide whether any follow-up is needed; if not, you are done.",
        g.intent, g.id, outcome
    );
    let context = json!({ "resolved_gate": { "id": g.id, "intent": g.intent, "state": g.state, "result": g.result, "decided_by": g.decided_by, "note": g.decision_note } });
    let _ = crate::queue::create(&short, &prompt, "continuation", false, 15.0, context, None, None);
    journal::append(&g.worker, "continuation-filed", json!({ "gate_id": g.id, "intent": g.intent }));
}

// ── executors (trusted-side; hold real credentials the box never sees) ───────

/// Dispatch to the executor that performs an intent. New capabilities register
/// here. Executors that egress route through the gateway as the privileged
/// subject (uniform judge/inject/meter/audit); local ones act directly.
pub async fn run_executor(executor: &str, worker: &str, intent: &str, payload: &Value, run_id: &str) -> Result<Value, String> {
    match executor {
        "message-user" => exec_message_user(worker, payload),
        "email" => exec_email(worker, payload),
        "git-pr" => exec_git_pr(worker, run_id, payload),
        other => Err(format!("no executor \"{other}\" for intent \"{intent}\"")),
    }
}

/// Deliver a note from the worker to its owner. Non-egress: appends to the
/// owner's inbox and logs. (A later phase points this at a Discord DM.)
fn exec_message_user(worker: &str, payload: &Value) -> Result<Value, String> {
    let text = payload.get("text").and_then(|v| v.as_str()).ok_or("message-user needs a \"text\" field")?;
    let path = root().join("runs").join("messages.jsonl");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", json!({ "ts": now_rfc3339(), "worker": worker, "text": text }));
    }
    eprintln!("message-user [{worker}]: {text}");
    Ok(json!({ "delivered": true }))
}

/// Send an email. For now a local sink (writes the rendered message to
/// runs/outbox/) so the gate→approve→execute→audit path is real and testable
/// offline; wiring a provider means routing this through the gateway to an email
/// API (POST + injected key) — the box still never holds the credential.
fn exec_email(worker: &str, payload: &Value) -> Result<Value, String> {
    let to: Vec<String> = payload
        .get("to")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
        .ok_or("email needs a non-empty \"to\" array")?;
    let subject = payload.get("subject").and_then(|v| v.as_str()).unwrap_or("");
    let body = payload.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let dir = root().join("runs").join("outbox");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{}-{}.json", now_rfc3339().replace(':', "-"), worker.replace('/', "_")));
    let rendered = json!({ "from": worker, "to": to, "subject": subject, "body": body });
    std::fs::write(&file, format!("{}\n", serde_json::to_string_pretty(&rendered).unwrap_or_default())).map_err(|e| e.to_string())?;
    eprintln!("email [{worker}] → {to:?}: {subject}");
    Ok(json!({ "delivered": "local-sink", "to": to, "file": file.display().to_string() }))
}

/// Land a code task's worktree as a pushed branch (and a PR where possible). The
/// box edited files in runs/<run_id>/worktree; here — only after approval — we
/// commit, push to the repo's origin, and open a PR. git push is direct (the
/// gateway can't govern git's wire protocol); the box never touches any of it.
fn exec_git_pr(worker: &str, run_id: &str, payload: &Value) -> Result<Value, String> {
    if run_id.is_empty() {
        return Err("code-change has no run_id — cannot find the worktree".into());
    }
    let wt = root().join("runs").join(run_id).join("worktree");
    if !wt.exists() {
        return Err(format!("no worktree at {}", wt.display()));
    }
    let wt = wt.display().to_string();
    let message = payload.get("message").and_then(|v| v.as_str()).unwrap_or("changes proposed by worker");
    let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or(message);
    let body = payload.get("body").and_then(|v| v.as_str()).unwrap_or("");

    git(&["-C", &wt, "add", "-A"])?;
    // Nothing staged → the proposal was empty; surface that rather than a git error.
    let clean = std::process::Command::new("git")
        .args(["-C", &wt, "diff", "--cached", "--quiet"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if clean {
        return Err("no changes in the worktree to commit".into());
    }
    let author = format!("user.name=roster worker {worker}");
    git(&["-C", &wt, "-c", "user.email=worker@roster.local", "-c", &author, "commit", "-m", message])?;
    let branch = git(&["-C", &wt, "rev-parse", "--abbrev-ref", "HEAD"])?;
    let commit = git(&["-C", &wt, "rev-parse", "--short", "HEAD"])?;
    git(&["-C", &wt, "push", "-u", "origin", &branch])?;

    // Open a PR if the GitHub CLI is available and authenticated; otherwise the
    // branch is pushed and the PR is opened out of band.
    let pr = match std::process::Command::new("gh")
        .args(["pr", "create", "--head", &branch, "--title", title, "--body", body])
        .current_dir(&wt)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "branch pushed; open the PR from it".to_string(),
    };
    eprintln!("git-pr [{worker}] pushed {branch} ({commit})");
    Ok(json!({ "branch": branch, "commit": commit, "pushed": true, "pr": pr }))
}

fn git(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git").args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The diff the box produced in a run's worktree (for `gates show` on a code
/// gate — the human reviews the actual change, rendered live, not stored).
pub fn worktree_diff(run_id: &str) -> Option<String> {
    let wt = root().join("runs").join(run_id).join("worktree");
    if !wt.exists() {
        return None;
    }
    let wt = wt.display().to_string();
    // Stage nothing; show working-tree changes against HEAD, including new files.
    let _ = std::process::Command::new("git").args(["-C", &wt, "add", "-A", "-N"]).status();
    git(&["-C", &wt, "diff", "HEAD"]).ok()
}

// ── audit ────────────────────────────────────────────────────────────────────

/// Append an action decision to the shared audit log, alongside egress decisions.
fn audit(worker: &str, intent: &str, disposition: &str, gate_id: Option<&str>, result: Option<&Value>) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let rec = json!({
        "decision_id": format!("act-{n:x}"),
        "ts": now_rfc3339(),
        "kind": "action",
        "worker": worker,
        "intent": intent,
        "disposition": disposition,
        "gate_id": gate_id,
        "result": result,
    });
    let path = root().join("runs").join("decisions.jsonl");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{rec}");
    }
}
