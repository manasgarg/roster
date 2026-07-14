//! Gates — durable records of a proposed action awaiting a decision, and the
//! authoritative enforcement state for what may execute. A gate is a timestamped
//! state machine: `pending → approved → executing → executed | failed`, or
//! `pending → denied`. Trusted-side and un-writable by the box: the executor
//! acts only on a real gate here, never on the journal. See
//! docs/supervisor-spec.md.

use crate::paths;
use crate::util::now_rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub type BErr = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gate {
    pub id: String,
    pub imp: String,
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
    /// A compact view for `gates ls` and the imp briefing.
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "id": self.id, "imp": self.imp, "intent": self.intent,
            "state": self.state, "filed_at": self.filed_at,
            "decided_by": self.decided_by, "decided_at": self.decided_at,
            "executed_at": self.executed_at,
        })
    }
}

// Gates live under their imp's subtree; scans walk every imp. An imp
// handle may be a bare name or a subject — paths normalizes.
fn imp_dirs() -> Vec<PathBuf> {
    std::fs::read_dir(paths::imps_data_dir())
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

/// Persist a gate, filing it under pending/ or resolved/ by its state and
/// removing any stale copy in the other directory (atomic move on transition).
pub fn save(g: &Gate) -> Result<(), BErr> {
    let (dir, other) = if g.is_terminal() {
        (
            paths::imp_gates_resolved_dir(&g.imp),
            paths::imp_gates_pending_dir(&g.imp),
        )
    } else {
        (
            paths::imp_gates_pending_dir(&g.imp),
            paths::imp_gates_resolved_dir(&g.imp),
        )
    };
    std::fs::create_dir_all(&dir)?;
    let text = format!("{}\n", serde_json::to_string_pretty(g)?);
    let tmp = dir.join(format!("{}.json.tmp", g.id));
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, dir.join(format!("{}.json", g.id)))?;
    let stale = other.join(format!("{}.json", g.id));
    if stale.exists() {
        let _ = std::fs::remove_file(stale);
    }
    Ok(())
}

pub fn load(id: &str) -> Option<Gate> {
    for imp in imp_dirs() {
        for sub in ["pending", "resolved"] {
            if let Ok(s) =
                std::fs::read_to_string(imp.join("gates").join(sub).join(format!("{id}.json")))
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
    let mut out: Vec<Gate> = imp_dirs()
        .into_iter()
        .flat_map(|w| read_dir(w.join("gates").join("pending")))
        .collect();
    out.sort_by(|a, b| a.filed_at.cmp(&b.filed_at));
    out
}

pub fn list_all() -> Vec<Gate> {
    let mut all: Vec<Gate> = imp_dirs()
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

/// An imp's own gates (for the run-start briefing and the box's read tool).
/// Accepts a bare name or a subject — reads that imp's subtree directly.
pub fn for_imp(imp: &str) -> Vec<Gate> {
    let mut out = read_dir(paths::imp_gates_pending_dir(imp));
    out.extend(read_dir(paths::imp_gates_resolved_dir(imp)));
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

/// A (imp, intent)'s gate history as (executed, denied) — the numbers the
/// earned-trust ladder reads. A denial is a reversal signal.
pub fn history(imp: &str, intent: &str) -> (u32, u32) {
    let mut executed = 0;
    let mut denied = 0;
    for g in list_all()
        .into_iter()
        .filter(|g| g.imp == imp && g.intent == intent)
    {
        match g.state.as_str() {
            "executed" => executed += 1,
            "denied" => denied += 1,
            _ => {}
        }
    }
    (executed, denied)
}
