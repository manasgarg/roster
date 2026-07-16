//! Gates — durable records of a proposed action awaiting a decision, and the
//! authoritative enforcement state for what may execute. A gate is a timestamped
//! state machine: `pending → approved → executing → executed | failed`, or
//! `pending → denied`. Trusted-side and un-writable by the box: the executor
//! acts only on a real gate here, never on the journal. See
//! docs/actions-and-trust.md.

use crate::paths;
use crate::util::now_rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub type BErr = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gate {
    pub id: String,
    #[serde(alias = "imp")]
    pub worker: String,
    /// The owner-named action (intent = rule/action name, D15).
    pub intent: String,
    /// Which trusted-side executor performs it once approved.
    pub executor: String,
    /// The frozen action content the human reviews and the executor runs.
    pub payload: Value,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub task_id: String,
    /// pending | approved | executing | executed | failed | denied
    pub state: String,
    pub filed_at: String,
    #[serde(default)]
    pub decided_by: Option<String>,
    #[serde(default)]
    pub decided_at: Option<String>,
    #[serde(default)]
    pub decision_note: Option<String>,
    #[serde(default)]
    pub executed_at: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Gate {
    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "executed" | "failed" | "denied")
    }
    /// A compact view for `gates ls` and the worker briefing.
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "id": self.id, "worker": self.worker, "intent": self.intent,
            "state": self.state, "filed_at": self.filed_at,
            "decided_by": self.decided_by, "decided_at": self.decided_at,
            "executed_at": self.executed_at,
        })
    }
}

// Gates live under their worker's subtree; scans walk every worker. A worker
// handle may be a bare name or a subject — paths normalizes.
fn worker_dirs() -> Vec<PathBuf> {
    std::fs::read_dir(paths::workers_data_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

pub fn new_id() -> String {
    format!("g-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

pub fn now() -> String {
    now_rfc3339()
}

/// The exclusive lock every gate transition takes. Approvals are rare and
/// human-paced, so one global lock (rather than per-worker) is simplest, and it
/// is only ever held across the file writes — never across the executor — so a
/// slow SMTP send can't block other approvals. Holding this across
/// load→check→transition is what stops two approvers from both executing a gate.
pub fn lock() -> std::io::Result<crate::statefile::FileLock> {
    crate::statefile::FileLock::acquire("gates")
}

/// Persist a gate, filing it under pending/ or resolved/ by its state and
/// removing any stale copy in the other directory. The write is atomic + fsynced
/// (a reader sees the whole old or whole new file); callers mutating state hold
/// `lock()` so the two-directory move is never observed half-done by a writer.
pub fn save(g: &Gate) -> Result<(), BErr> {
    let (dir, other) = if g.is_terminal() {
        (
            paths::worker_gates_resolved_dir(&g.worker),
            paths::worker_gates_pending_dir(&g.worker),
        )
    } else {
        (
            paths::worker_gates_pending_dir(&g.worker),
            paths::worker_gates_resolved_dir(&g.worker),
        )
    };
    let text = format!("{}\n", serde_json::to_string_pretty(g)?);
    crate::statefile::write_atomic(&dir.join(format!("{}.json", g.id)), text.as_bytes())?;
    let stale = other.join(format!("{}.json", g.id));
    if stale.exists() {
        let _ = std::fs::remove_file(stale);
    }
    Ok(())
}

pub fn load(id: &str) -> Option<Gate> {
    for worker in worker_dirs() {
        // Resolved is the terminal truth: prefer it over a pending copy that a
        // crashed transition may have left behind, so an executed gate is never
        // re-read as still-executing.
        for sub in ["resolved", "pending"] {
            if let Ok(s) =
                std::fs::read_to_string(worker.join("gates").join(sub).join(format!("{id}.json")))
            {
                if let Ok(g) = serde_json::from_str::<Gate>(&s) {
                    return Some(g);
                }
            }
        }
    }
    None
}

fn read_dir(dir: PathBuf) -> Vec<Gate> {
    let mut out: Vec<Gate> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<Gate>(&s).ok())
        .collect();
    out.sort_by(|a, b| a.filed_at.cmp(&b.filed_at));
    out
}

pub fn list_pending() -> Vec<Gate> {
    let mut out: Vec<Gate> = worker_dirs()
        .into_iter()
        .flat_map(|w| read_dir(w.join("gates").join("pending")))
        .collect();
    out.sort_by(|a, b| a.filed_at.cmp(&b.filed_at));
    out
}

pub fn list_all() -> Vec<Gate> {
    let mut all: Vec<Gate> = worker_dirs()
        .into_iter()
        .flat_map(|w| {
            let mut gates = read_dir(w.join("gates").join("pending"));
            gates.extend(read_dir(w.join("gates").join("resolved")));
            gates
        })
        .collect();
    all.sort_by(|a, b| a.filed_at.cmp(&b.filed_at));
    all
}

/// A worker's own gates (for the run-start briefing and the box's read tool).
/// Accepts a bare name or a subject — reads that worker's subtree directly.
pub fn for_worker(worker: &str) -> Vec<Gate> {
    let mut out = read_dir(paths::worker_gates_pending_dir(worker));
    out.extend(read_dir(paths::worker_gates_resolved_dir(worker)));
    out.sort_by(|a, b| a.filed_at.cmp(&b.filed_at));
    out
}

/// Still-pending gates filed by a given task's run (the supervisor uses this to
/// decide whether a finished task needs review or is done).
pub fn pending_for_task(task_id: &str) -> Vec<Gate> {
    list_pending()
        .into_iter()
        .filter(|g| g.task_id == task_id)
        .collect()
}

/// A (worker, intent)'s gate history as (executed, denied) — the numbers the
/// earned-trust ladder reads. A denial is a reversal signal.
pub fn history(worker: &str, intent: &str) -> (u32, u32) {
    let mut executed = 0;
    let mut denied = 0;
    for g in list_all()
        .into_iter()
        .filter(|g| g.worker == worker && g.intent == intent)
    {
        match g.state.as_str() {
            "executed" => executed += 1,
            "denied" => denied += 1,
            _ => {}
        }
    }
    (executed, denied)
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
        (guard, dir)
    }

    fn gate(id: &str, state: &str) -> Gate {
        Gate {
            id: id.into(),
            worker: "w".into(),
            intent: "email".into(),
            executor: "email".into(),
            payload: Value::Null,
            rationale: String::new(),
            run_id: "r".into(),
            task_id: "t".into(),
            state: state.into(),
            filed_at: now(),
            decided_by: None,
            decided_at: None,
            decision_note: None,
            executed_at: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn load_prefers_resolved_over_a_stale_pending_leftover() {
        let (_g, _dir) = sandbox();
        // Simulate a crashed transition: the terminal copy landed in resolved/
        // but the pending/ copy was never removed.
        let resolved = gate("g-1", "executed");
        save(&resolved).unwrap();
        let stale_pending = gate("g-1", "executing");
        let pdir = paths::worker_gates_pending_dir("w");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("g-1.json"),
            serde_json::to_string_pretty(&stale_pending).unwrap(),
        )
        .unwrap();
        // load must report the executed (resolved) truth, not the stale copy.
        assert_eq!(load("g-1").unwrap().state, "executed");
    }
}
