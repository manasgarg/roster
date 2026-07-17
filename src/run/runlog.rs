//! Durable run manifests plus discovery of legacy run directories.

use crate::paths;
use crate::util::now_rfc3339;
use crate::work::tms;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeRunRecord {
    pub base_commit: String,
    /// "write" | "read" (older records: "append" | "reorganization" | "read").
    pub mode: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    #[serde(alias = "imp")]
    pub worker: String,
    pub kind: String,
    pub state: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why an error-ended run ended — the message the daemon logged, kept
    /// where `runs show` can find it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<KnowledgeRunRecord>,
}

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub id: String,
    pub worker: String,
    pub kind: String,
    pub state: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub task_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
    pub run_dir: PathBuf,
    pub record: Option<RunRecord>,
}

fn record_path(run_id: &str) -> PathBuf {
    paths::run_dir(run_id).join("run.json")
}

pub fn start(run_id: &str, worker: &str, kind: &str, task_id: Option<&str>) -> Result<(), String> {
    let record = RunRecord {
        id: run_id.to_string(),
        worker: worker.to_string(),
        kind: kind.to_string(),
        state: "running".into(),
        started_at: now_rfc3339(),
        ended_at: None,
        task_id: task_id.filter(|s| !s.is_empty()).map(String::from),
        ended_by: None,
        exit_code: None,
        error: None,
        knowledge: None,
    };
    save(&record)
}

pub fn finish(run_id: &str, ended_by: &str, exit_code: Option<i32>) -> Result<(), String> {
    let Some(mut record) = load(run_id) else {
        return Ok(());
    };
    record.state = if ended_by == "ceiling" || exit_code != Some(0) {
        "failed".into()
    } else {
        "done".into()
    };
    record.ended_at = Some(now_rfc3339());
    record.ended_by = Some(ended_by.to_string());
    record.exit_code = exit_code;
    save(&record)
}

pub fn fail(run_id: &str, error: Option<&str>) {
    if let Some(mut record) = load(run_id) {
        record.state = "failed".into();
        record.ended_at = Some(now_rfc3339());
        record.ended_by = Some("error".into());
        record.error = error.map(String::from);
        let _ = save(&record);
    }
}

/// The worker's own claim about how its task went — evidence for the host's
/// attestation, never the attestation itself. Last report wins.
pub fn record_outcome_report(run_id: &str, status: &str, note: Option<&str>) -> Result<(), String> {
    let path = paths::run_dir(run_id).join("outcome.json");
    let dir = path.parent().ok_or("bad run dir")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let body = serde_json::json!({ "status": status, "note": note, "ts": now_rfc3339() });
    std::fs::write(&path, format!("{body}\n")).map_err(|e| e.to_string())
}

/// The reported outcome for a run, if the worker filed one: (status, note).
pub fn outcome_report(run_id: &str) -> Option<(String, Option<String>)> {
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(paths::run_dir(run_id).join("outcome.json")).ok()?,
    )
    .ok()?;
    Some((
        v.get("status")?.as_str()?.to_string(),
        v.get("note").and_then(Value::as_str).map(String::from),
    ))
}

pub fn attach_storage(run_id: &str, knowledge: Option<KnowledgeRunRecord>) -> Result<(), String> {
    let mut record = load(run_id).ok_or_else(|| format!("no run record for {run_id}"))?;
    record.knowledge = knowledge;
    save(&record)
}

pub fn update_knowledge(
    run_id: &str,
    state: &str,
    produced_commit: Option<&str>,
    error: Option<&str>,
) -> Result<(), String> {
    let mut record = load(run_id).ok_or_else(|| format!("no run record for {run_id}"))?;
    if let Some(knowledge) = record.knowledge.as_mut() {
        knowledge.state = state.into();
        knowledge.produced_commit = produced_commit.map(String::from);
        knowledge.error = error.map(String::from);
    }
    save(&record)
}

fn save(record: &RunRecord) -> Result<(), String> {
    let path = record_path(&record.id);
    let dir = path.parent().ok_or("bad run manifest path")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(record).map_err(|e| e.to_string())?
    );
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

pub fn load(run_id: &str) -> Option<RunRecord> {
    std::fs::read_to_string(record_path(run_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn list() -> Vec<RunSummary> {
    let tasks = tms::list_all();
    let by_run: HashMap<String, tms::Task> = tasks
        .iter()
        .filter_map(|task| task.run_id.as_ref().map(|id| (id.clone(), task.clone())))
        .collect();
    let journal_workers = crate::worker::journal::run_workers();
    let base = paths::runs_dir();
    let mut runs: Vec<RunSummary> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            path.is_dir()
                && (path.join("run.json").exists()
                    || path.join("stdout.jsonl").exists()
                    || path.join("session").is_dir()
                    || path.join("workspace").is_dir())
        })
        .filter_map(|entry| {
            let id = entry.file_name().to_string_lossy().to_string();
            summarize(&entry.path(), by_run.get(&id), journal_workers.get(&id))
        })
        .collect();
    runs.sort_by(
        |a, b| match (a.started_at == "unknown", b.started_at == "unknown") {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => b
                .started_at
                .cmp(&a.started_at)
                .then_with(|| b.id.cmp(&a.id)),
        },
    );
    runs
}

fn summarize(
    path: &Path,
    task: Option<&tms::Task>,
    journal_worker: Option<&String>,
) -> Option<RunSummary> {
    let id = path.file_name()?.to_string_lossy().to_string();
    let record = load(&id);
    let context = read_json(path.join("memory-context.json"));
    let channel_id = context
        .as_ref()
        .and_then(|v| v.get("channel_id"))
        .and_then(Value::as_str)
        .map(String::from);
    let user_id = context
        .as_ref()
        .and_then(|v| v.get("user_id"))
        .and_then(Value::as_str)
        .map(String::from);
    let session_start = session_start(path);
    let started_at = record
        .as_ref()
        .map(|r| r.started_at.clone())
        .or_else(|| session_start.clone())
        .or_else(|| timestamp_from_id(&id))
        .or_else(|| modified_at(path))
        .unwrap_or_else(|| "unknown".into());
    let worker = record
        .as_ref()
        .map(|r| r.worker.clone())
        .or_else(|| task.map(|t| t.worker.clone()))
        .or_else(|| journal_worker.cloned())
        .unwrap_or_else(|| "?".into());
    let kind = record.as_ref().map(|r| r.kind.clone()).unwrap_or_else(|| {
        if task.and_then(|t| t.repo.as_ref()).is_some() || path.join("worktree").exists() {
            "code".into()
        } else if task.is_some() {
            "task".into()
        } else if channel_id.is_some() {
            "session".into()
        } else {
            "box".into()
        }
    });
    let mut state = record
        .as_ref()
        .map(|r| r.state.clone())
        .or_else(|| task.map(|t| t.state.clone()))
        .unwrap_or_else(|| {
            if path.join("stdout.jsonl").exists() {
                "finished".into()
            } else {
                "unknown".into()
            }
        });
    if state == "running" && !crate::run::boxed::box_alive(&id) {
        state = "orphaned".into();
    }
    let ended_at = record
        .as_ref()
        .and_then(|r| r.ended_at.clone())
        .or_else(|| modified_at(&path.join("stdout.jsonl")));
    let task_id = record
        .as_ref()
        .and_then(|r| r.task_id.clone())
        .or_else(|| task.map(|t| t.id.clone()));
    Some(RunSummary {
        id,
        worker,
        kind,
        state,
        started_at,
        ended_at,
        task_id,
        channel_id,
        user_id,
        run_dir: path.to_path_buf(),
        record,
    })
}

fn read_json(path: PathBuf) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn session_files(path: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(path.join("session"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
}

fn session_start(path: &Path) -> Option<String> {
    let file = session_files(path).into_iter().next()?;
    let first = std::fs::read_to_string(file)
        .ok()?
        .lines()
        .next()?
        .to_string();
    serde_json::from_str::<Value>(&first)
        .ok()?
        .get("timestamp")?
        .as_str()
        .map(String::from)
}

fn timestamp_from_id(id: &str) -> Option<String> {
    let prefix = id.get(..19)?;
    if !prefix.bytes().enumerate().all(|(i, b)| match i {
        4 | 7 => b == b'-',
        10 | 13 | 16 => b == b'-',
        _ => b.is_ascii_digit(),
    }) {
        return None;
    }
    Some(format!(
        "{}T{}:{}:{}Z",
        &prefix[..10],
        &prefix[11..13],
        &prefix[14..16],
        &prefix[17..19]
    ))
}

fn modified_at(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    time::OffsetDateTime::from(modified)
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

pub fn resolve(id_or_prefix: &str) -> Result<RunSummary, String> {
    let matches: Vec<RunSummary> = list()
        .into_iter()
        .filter(|r| r.id == id_or_prefix || r.id.starts_with(id_or_prefix))
        .collect();
    match matches.len() {
        0 => Err(format!("no such run {id_or_prefix}")),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => Err(format!("run prefix {id_or_prefix} is ambiguous")),
    }
}

pub fn conversation(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for file in session_files(path) {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        for value in text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if value.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let Some(message) = value.get("message") else {
                continue;
            };
            let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
            let content = render_content(message.get("content").unwrap_or(&Value::Null));
            if !content.is_empty() {
                out.push(format!("{role}: {content}"));
            }
        }
    }
    out
}

fn render_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return one_line(text, 240);
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let rendered: Vec<String> = items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
            Some("text") => item
                .get("text")
                .and_then(Value::as_str)
                .map(|s| one_line(s, 240)),
            Some("toolCall") => Some(format!(
                "tool {} {}",
                item.get("name").and_then(Value::as_str).unwrap_or("?"),
                one_line(
                    &item
                        .get("arguments")
                        .cloned()
                        .unwrap_or(Value::Null)
                        .to_string(),
                    160
                )
            )),
            _ => None,
        })
        .collect();
    rendered.join(" | ")
}

pub fn files(path: &Path) -> Vec<(String, u64)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            // Never follow a symlink: the box can write into this tree, and a
            // planted `ln -s /` (host enumeration) or `ln -s .` (infinite
            // recursion) would otherwise be walked. file_type() does not follow.
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                walk(base, &path, out);
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    let relative = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    out.push((relative, meta.len()));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(path, path, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn one_line(value: &str, max_chars: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= max_chars {
        value
    } else {
        value
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_run_id_timestamp() {
        assert_eq!(
            timestamp_from_id("2026-07-10-21-51-17-a3b3").as_deref(),
            Some("2026-07-10T21:51:17Z")
        );
        assert!(timestamp_from_id("compiled").is_none());
    }

    #[test]
    fn renders_text_and_tool_calls() {
        let content = serde_json::json!([
            {"type":"text","text":"hello\nworld"},
            {"type":"toolCall","name":"remember","arguments":{"scope":"user"}}
        ]);
        assert_eq!(
            render_content(&content),
            "hello world | tool remember {\"scope\":\"user\"}"
        );
    }

    #[test]
    fn unicode_truncation_is_safe() {
        assert_eq!(one_line("éééé", 3), "éé…");
    }

    #[test]
    fn legacy_run_records_with_removed_artifact_fields_still_parse() {
        let record: RunRecord = serde_json::from_value(serde_json::json!({
            "id": "run-1",
            "worker": "yuko",
            "kind": "task",
            "state": "done",
            "started_at": "2026-01-01T00:00:00Z",
            "scratch": { "state": "cleaned" },
            "fetch_receipts": ["fetch_old"],
            "published_blobs": ["blob_old"]
        }))
        .unwrap();
        assert_eq!(record.id, "run-1");
    }
}
