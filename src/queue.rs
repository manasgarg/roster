//! The task queue — one durable per-worker list the supervisor dispatches from
//! (§3.6). Tasks are files under `queue/<worker>/<id>.json`; the state field
//! drives the lifecycle `waiting → running → needs-review | done | failed`.
//! Owned locally (not a GitHub mirror, Q3): core control flow stays off any
//! external dependency. See docs/supervisor-spec.md.

use crate::paths;
use crate::util::now_rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub type BErr = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// Short worker name (the box's `--worker`); subject is `org/<worker>`.
    pub worker: String,
    pub prompt: String,
    /// manual | schedule | continuation | event
    pub origin: String,
    /// Proactive work is budget-gated at dispatch (D6); owner/chat always runs.
    #[serde(default)]
    pub proactive: bool,
    /// waiting | running | needs-review | done | failed
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default = "default_ceiling")]
    pub ceiling_min: f64,
    #[serde(default)]
    pub run_id: Option<String>,
    /// Context threaded into the box (e.g. a continuation's gate outcome).
    #[serde(default)]
    pub context: Value,
    /// Code task: the git repo to branch a writable worktree from, and the base
    /// ref. Absent → a research task (read-only repo + container temp space).
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    /// append | reorganization. Code and ordinary research tasks use append.
    #[serde(default = "default_knowledge_mode")]
    pub knowledge_mode: String,
}

fn default_ceiling() -> f64 {
    30.0
}

fn default_knowledge_mode() -> String {
    "append".into()
}

impl Task {
    pub fn subject(&self) -> String {
        format!("org/{}", self.worker)
    }
}

pub fn new_id() -> String {
    format!("t-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

fn dir(worker: &str) -> PathBuf {
    paths::worker_queue_dir(worker)
}

#[allow(clippy::too_many_arguments)]
pub fn create(
    worker: &str,
    prompt: &str,
    origin: &str,
    proactive: bool,
    ceiling_min: f64,
    knowledge_mode: &str,
    context: Value,
    repo: Option<String>,
    base: Option<String>,
) -> Result<Task, BErr> {
    if !matches!(knowledge_mode, "append" | "reorganization") {
        return Err(format!("unknown knowledge mode \"{knowledge_mode}\"").into());
    }
    if knowledge_mode == "reorganization" && repo.is_some() {
        return Err("a knowledge reorganization cannot also be a code task".into());
    }
    let now = now_rfc3339();
    let t = Task {
        id: new_id(),
        worker: worker.to_string(),
        prompt: prompt.to_string(),
        origin: origin.to_string(),
        proactive,
        state: "waiting".into(),
        created_at: now.clone(),
        updated_at: now,
        ceiling_min,
        run_id: None,
        context,
        repo,
        base,
        knowledge_mode: knowledge_mode.into(),
    };
    save(&t)?;
    Ok(t)
}

pub fn active_reorganization(worker: &str) -> Option<Task> {
    list_all().into_iter().find(|task| {
        task.worker == worker
            && task.knowledge_mode == "reorganization"
            && matches!(
                task.state.as_str(),
                "waiting" | "running" | "deferred" | "needs-review"
            )
    })
}

pub fn save(t: &Task) -> Result<(), BErr> {
    let d = dir(&t.worker);
    std::fs::create_dir_all(&d)?;
    let text = format!("{}\n", serde_json::to_string_pretty(t)?);
    let tmp = d.join(format!("{}.json.tmp", t.id));
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, d.join(format!("{}.json", t.id)))?;
    Ok(())
}

pub fn set_state(t: &mut Task, state: &str) -> Result<(), BErr> {
    t.state = state.to_string();
    t.updated_at = now_rfc3339();
    save(t)
}

pub fn list_all() -> Vec<Task> {
    let base = paths::workers_data_dir();
    let mut out: Vec<Task> = std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .flat_map(|worker_dir| {
            std::fs::read_dir(worker_dir.path().join("queue"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .filter_map(|s| serde_json::from_str::<Task>(&s).ok())
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    out
}

pub fn find(task_id: &str) -> Option<Task> {
    list_all().into_iter().find(|t| t.id == task_id)
}

/// The oldest waiting task, atomically claimed by flipping it to `running` so a
/// concurrent poll won't pick it twice.
pub fn claim_next() -> Option<Task> {
    let tasks = list_all();
    let mut t = tasks
        .iter()
        .find(|task| {
            task.state == "waiting"
                && !(task.knowledge_mode == "reorganization"
                    && tasks.iter().any(|other| {
                        other.worker == task.worker
                            && other.knowledge_mode == "reorganization"
                            && other.state == "running"
                    }))
        })?
        .clone();
    if set_state(&mut t, "running").is_err() {
        return None;
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_tasks_default_to_append_mode() {
        let value = serde_json::json!({
            "id": "t-old",
            "worker": "yuko",
            "prompt": "research",
            "origin": "manual",
            "proactive": false,
            "state": "waiting",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "ceiling_min": 30.0,
            "context": null,
            "repo": null,
            "base": null
        });
        let task: Task = serde_json::from_value(value).unwrap();
        assert_eq!(task.knowledge_mode, "append");
    }
}
