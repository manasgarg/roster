//! The worker journal — the shared, append-only timeline every actor writes to
//! (box, gateway, approval desk, executors). One file per worker,
//! `state/workers/<name>/journal/events.jsonl`. It is the worker's *view* of its own
//! history and gate state (see docs/supervisor-spec.md, "Visibility"); it is
//! never an enforcement input — the authoritative state is the gates/ store.

use crate::paths;
use crate::util::now_rfc3339;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::collections::HashMap;

fn path(worker: &str) -> PathBuf {
    paths::worker_journal_file(worker)
}

/// Append one event to a worker's timeline, tagged with the run it belongs to
/// (empty for events not tied to a run). Best-effort (a journal write must never
/// fail an action); the authoritative record lives elsewhere.
pub fn append(worker: &str, run_id: &str, kind: &str, detail: Value) {
    let _ = append_required(worker, run_id, kind, detail);
}

/// Durable content operations use the journal as their recovery index. They
/// call this variant and fail closed instead of claiming success when the
/// pointer could not be recorded.
pub fn append_required(worker: &str, run_id: &str, kind: &str, detail: Value) -> Result<(), String> {
    let p = path(worker);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let ev = json!({ "ts": now_rfc3339(), "worker": worker, "run_id": run_id, "kind": kind, "detail": detail });
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|error| error.to_string())?;
    writeln!(f, "{ev}").map_err(|error| error.to_string())?;
    f.sync_data().map_err(|error| error.to_string())
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

/// Recover run → worker attribution from the append-only journals. This lets
/// `roster runs` describe executions created before run manifests existed.
pub fn run_workers() -> HashMap<String, String> {
    fn files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files(&path, out);
            } else if path.file_name().and_then(|n| n.to_str()) == Some("events.jsonl") {
                out.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    files(&crate::paths::workers_data_dir(), &mut paths);
    let mut out = HashMap::new();
    for path in paths {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        for event in text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()) {
            let run_id = event.get("run_id").and_then(Value::as_str).unwrap_or("");
            let worker = event.get("worker").and_then(Value::as_str).unwrap_or("");
            if !run_id.is_empty() && !worker.is_empty() {
                out.insert(run_id.to_string(), worker.strip_prefix("org/").unwrap_or(worker).to_string());
            }
        }
    }
    out
}
