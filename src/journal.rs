//! The worker journal — the shared, append-only timeline every actor writes to
//! (box, gateway, approval desk, executors). One file per worker,
//! `journal/<subject>/events.jsonl`. It is the worker's *view* of its own
//! history and gate state (see docs/supervisor-spec.md, "Visibility"); it is
//! never an enforcement input — the authoritative state is the gates/ store.

use crate::util::{now_rfc3339, root};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

fn path(worker: &str) -> PathBuf {
    root().join("journal").join(worker).join("events.jsonl")
}

/// Append one event to a worker's timeline, tagged with the run it belongs to
/// (empty for events not tied to a run). Best-effort (a journal write must never
/// fail an action); the authoritative record lives elsewhere.
pub fn append(worker: &str, run_id: &str, kind: &str, detail: Value) {
    let p = path(worker);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let ev = json!({ "ts": now_rfc3339(), "worker": worker, "run_id": run_id, "kind": kind, "detail": detail });
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{ev}");
    }
}

/// Every event for a given run, oldest first — powers `queue show`'s run status.
pub fn for_run(worker: &str, run_id: &str) -> Vec<Value> {
    std::fs::read_to_string(path(worker))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e.get("run_id").and_then(|v| v.as_str()) == Some(run_id))
        .collect()
}

/// The last `n` events for a worker — the run-start briefing and the box's
/// read-only `journal_read` tool draw on this.
pub fn tail(worker: &str, n: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(path(worker)).unwrap_or_default();
    let mut evs: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let len = evs.len();
    if len > n {
        evs.split_off(len - n)
    } else {
        evs
    }
}
