//! The task management system (TMS) — docs/work.md.
//!
//! One partition document per worker (`data/workers/<name>/tasks/tasks.json`)
//! holds the live tasks and recurring templates; terminal tasks move to an
//! append-only journal. All scheduling state and logic live here: lifecycle,
//! DAG dependencies, scheduled times, cron recurrence, and the due view the
//! supervisor consumes. The supervisor holds no plan and no timer; the agent
//! curates its partition through a mounted read view plus one optimistically-
//! concurrent write (`set_tasks`).

use crate::paths;
use crate::util::{now_ms, now_rfc3339};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

pub type BErr = Box<dyn std::error::Error + Send + Sync>;

// ── model ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Tags {
    /// "discord" | "slack" | "term" — with `channel`, the reply route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    /// Empty on agent-authored new entries; the host mints one.
    #[serde(default)]
    pub id: String,
    /// Short worker name; the partition implies it, kept for self-description.
    #[serde(default, alias = "imp")]
    pub worker: String,
    pub prompt: String,
    /// user | agent | recurrence | relay | gate
    #[serde(default = "default_created_by")]
    pub created_by: String,
    /// owner (never budget-gated) | proactive (paced by the envelope)
    #[serde(default = "default_standing")]
    pub standing: String,
    /// pending | claimed | needs-review | completed | failed
    #[serde(default = "default_state")]
    pub state: String,
    #[serde(default, skip_serializing_if = "tags_empty")]
    pub tags: Tags,
    /// Reply routing etc. (e.g. the Discord channel a task answers to).
    #[serde(default)]
    pub context: Value,
    /// RFC3339 UTC ("…Z"). Absent = eligible now (pacing comes from standing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<String>,
    /// DAG edges. An edge to an id not live in the document blocks (a failed
    /// or canceled dependency); completed dependencies are pruned from here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default = "default_ceiling")]
    pub ceiling_min: f64,
    /// Set on children spawned from a recurring template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurring_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// Code task: repo to branch a worktree from + base ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// append | reorganization
    #[serde(default = "default_knowledge_mode")]
    pub knowledge_mode: String,
    /// Why a failed task failed — attested with the outcome, journaled with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Window {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recurring {
    pub id: String,
    #[serde(default)]
    pub worker: String,
    pub prompt: String,
    /// 5-field cron (host-local time) or an interval ("every 30m").
    pub schedule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Window>,
    /// skip-if-previous-open | always
    #[serde(default = "default_spawn_policy")]
    pub spawn_policy: String,
    #[serde(default = "default_standing_proactive")]
    pub standing: String,
    #[serde(default = "default_recurring_ceiling")]
    pub ceiling_min: f64,
    #[serde(default, skip_serializing_if = "tags_empty")]
    pub tags: Tags,
    #[serde(default)]
    pub context: Value,
    /// Host-owned (the heartbeat); `set_tasks` cannot touch it.
    #[serde(default)]
    pub system: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Partition {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub recurring: Vec<Recurring>,
}

fn default_created_by() -> String {
    "user".into()
}
fn default_state() -> String {
    "pending".into()
}
fn default_standing() -> String {
    "owner".into()
}
fn default_standing_proactive() -> String {
    "proactive".into()
}
fn default_spawn_policy() -> String {
    "skip-if-previous-open".into()
}
fn default_ceiling() -> f64 {
    30.0
}
fn default_recurring_ceiling() -> f64 {
    20.0
}
fn default_knowledge_mode() -> String {
    "append".into()
}
fn tags_empty(t: &Tags) -> bool {
    t.provider.is_none() && t.channel.is_none() && t.user.is_none()
}

impl Task {
    pub fn subject(&self) -> String {
        format!("org/{}", self.worker)
    }
    pub fn proactive(&self) -> bool {
        self.standing == "proactive"
    }
    pub fn live(&self) -> bool {
        matches!(self.state.as_str(), "pending" | "claimed" | "needs-review")
    }
}

pub fn new_task_id() -> String {
    format!("t-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}
pub fn new_recurring_id() -> String {
    format!("r-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

// ── persistence: partition, journal, view ─────────────────────────────────────

/// Serialize access per process: the daemon's tick and its executors share this.
/// Cross-process writers (the CLI) are rare and human-paced; last write wins
/// there, which the version counter makes visible.
static LOCK: Mutex<()> = Mutex::new(());

fn tasks_file(worker: &str) -> PathBuf {
    paths::worker_tasks_file(worker)
}

pub fn load(worker: &str) -> Partition {
    migrate_legacy_queue(worker);
    read_partition(worker)
}

fn read_partition(worker: &str) -> Partition {
    std::fs::read_to_string(tasks_file(worker))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(worker: &str, p: &mut Partition) -> Result<(), BErr> {
    p.version += 1;
    let file = tasks_file(worker);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = format!("{}\n", serde_json::to_string_pretty(p)?);
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, &file)?;
    render_view(worker, p);
    Ok(())
}

/// Rewrite the box-facing view IN PLACE (truncate + write, same inode) so a
/// live bind mount in a warm session sees fresh state. Edits to the view are
/// scratch — the partition file is the only truth.
pub fn render_view(worker: &str, p: &Partition) {
    use std::io::Write;
    let path = paths::tms_view_file(worker);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(p) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            let _ = f.write_all(text.as_bytes());
            let _ = f.write_all(b"\n");
        }
    }
}

/// Make sure the view exists and is current before a box mounts it.
pub fn ensure_view(worker: &str) -> PathBuf {
    let p = load(worker);
    render_view(worker, &p);
    paths::tms_view_file(worker)
}

fn journal_append(worker: &str, event: &str, record: Value) {
    use std::io::Write;
    let path = paths::worker_tasks_journal(worker);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let line = json!({ "ts": now_rfc3339(), "event": event, "record": record });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// The latest journaled outcomes across every worker — (event timestamp,
/// record) pairs, newest first, one per task id. What `task ls` shows so a
/// finished or failed task doesn't simply vanish from view.
pub fn journal_recent(limit: usize) -> Vec<(String, Task)> {
    let mut out: Vec<(String, Task)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for w in worker_names() {
        let Ok(text) = std::fs::read_to_string(paths::worker_tasks_journal(&w)) else {
            continue;
        };
        for v in text
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            if !matches!(
                v.get("event").and_then(|e| e.as_str()),
                Some("completed" | "failed" | "canceled")
            ) {
                continue;
            }
            let Some(t) = v
                .get("record")
                .and_then(|r| serde_json::from_value::<Task>(r.clone()).ok())
            else {
                continue;
            };
            if !seen.insert(t.id.clone()) {
                continue; // a requeued-and-refinished task: newest record wins
            }
            let ts = v
                .get("ts")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            out.push((ts, t));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(limit);
    out
}

/// Search a worker's journal for a task by id (completed/failed tasks live
/// there, pruned from the document).
pub fn journal_find(worker: &str, task_id: &str) -> Option<Task> {
    let text = std::fs::read_to_string(paths::worker_tasks_journal(worker)).ok()?;
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| {
            let t: Task = serde_json::from_value(v.get("record")?.clone()).ok()?;
            (t.id == task_id).then_some(t)
        })
}

// ── legacy migration (flat queue → partition) ─────────────────────────────────

/// Import pre-TMS per-task queue files into the partition. `waiting`/
/// `deferred` become `pending`, `running` becomes `claimed` (boot reclaim
/// sorts it out), terminal states go straight to the journal. Runs whenever
/// a queue dir exists — a stale CLI binary can recreate one at any time —
/// merging into the current partition and shelving the files under
/// `queue.migrated/` so nothing imports twice and nothing is deleted.
fn migrate_legacy_queue(worker: &str) {
    let qdir = paths::worker_queue_dir(worker);
    if !qdir.is_dir() {
        return;
    }
    #[derive(Deserialize)]
    struct Legacy {
        id: String,
        #[serde(alias = "imp")]
        worker: String,
        prompt: String,
        #[serde(default)]
        origin: String,
        #[serde(default)]
        proactive: bool,
        state: String,
        created_at: String,
        updated_at: String,
        #[serde(default = "default_ceiling")]
        ceiling_min: f64,
        #[serde(default)]
        run_id: Option<String>,
        #[serde(default)]
        context: Value,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        base: Option<String>,
        #[serde(default = "default_knowledge_mode")]
        knowledge_mode: String,
    }
    let mut p = read_partition(worker);
    let shelf = qdir.with_extension("migrated");
    let _ = std::fs::create_dir_all(&shelf);
    let mut imported = 0usize;
    for entry in std::fs::read_dir(&qdir).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let _ = std::fs::rename(entry.path(), shelf.join(entry.file_name()));
        let Ok(l) = serde_json::from_str::<Legacy>(&text) else { continue };
        if p.tasks.iter().any(|t| t.id == l.id) || journal_find(worker, &l.id).is_some() {
            continue; // already imported
        }
        imported += 1;
        let created_by = match l.origin.as_str() {
            "schedule" => "recurrence",
            "continuation" => "gate",
            "event" => "relay",
            "worker" => "agent",
            _ => "user",
        };
        let state = match l.state.as_str() {
            "waiting" | "deferred" => "pending",
            "running" => "claimed",
            "done" => "completed",
            other => other, // needs-review | failed
        };
        let t = Task {
            id: l.id,
            worker: l.worker,
            prompt: l.prompt,
            created_by: created_by.into(),
            standing: if l.proactive { "proactive" } else { "owner" }.into(),
            state: state.into(),
            tags: Tags::default(),
            context: l.context,
            scheduled_at: None,
            depends_on: Vec::new(),
            ceiling_min: l.ceiling_min,
            recurring_id: None,
            run_id: l.run_id,
            created_at: l.created_at,
            updated_at: l.updated_at,
            repo: l.repo,
            base: l.base,
            knowledge_mode: l.knowledge_mode,
            error: None,
        };
        if t.live() {
            p.tasks.push(t);
        } else if let Ok(record) = serde_json::to_value(&t) {
            journal_append(worker, &t.state.clone(), record);
        }
    }
    let _ = std::fs::remove_dir(&qdir); // only removes it when empty
    if imported > 0 {
        let _ = save(worker, &mut p);
        eprintln!("tms: migrated {imported} legacy queue file(s) for {worker}");
    }
}

// ── add ───────────────────────────────────────────────────────────────────────

pub struct Draft {
    pub prompt: String,
    pub created_by: String,
    pub standing: String,
    pub ceiling_min: f64,
    pub tags: Tags,
    pub context: Value,
    pub scheduled_at: Option<String>,
    pub depends_on: Vec<String>,
    pub repo: Option<String>,
    pub base: Option<String>,
    pub knowledge_mode: String,
    pub recurring_id: Option<String>,
}

impl Default for Draft {
    fn default() -> Self {
        Draft {
            prompt: String::new(),
            created_by: "user".into(),
            standing: "owner".into(),
            ceiling_min: default_ceiling(),
            tags: Tags::default(),
            context: Value::Null,
            scheduled_at: None,
            depends_on: Vec::new(),
            repo: None,
            base: None,
            knowledge_mode: default_knowledge_mode(),
            recurring_id: None,
        }
    }
}

fn check_scheduled_at(s: &Option<String>) -> Result<(), BErr> {
    if let Some(t) = s {
        if !t.ends_with('Z') || t.len() < 20 {
            return Err(format!(
                "scheduled_at must be RFC3339 UTC ending in Z (got \"{t}\")"
            )
            .into());
        }
    }
    Ok(())
}

pub fn add(worker: &str, draft: Draft) -> Result<Task, BErr> {
    if draft.prompt.trim().is_empty() {
        return Err("a task needs a prompt".into());
    }
    if !matches!(draft.knowledge_mode.as_str(), "append" | "reorganization") {
        return Err(format!("unknown knowledge mode \"{}\"", draft.knowledge_mode).into());
    }
    if draft.knowledge_mode == "reorganization" && draft.repo.is_some() {
        return Err("a knowledge reorganization cannot also be a code task".into());
    }
    check_scheduled_at(&draft.scheduled_at)?;
    let _guard = LOCK.lock().unwrap();
    let mut p = load(worker);
    if draft.knowledge_mode == "reorganization" {
        if let Some(active) = p
            .tasks
            .iter()
            .find(|t| t.knowledge_mode == "reorganization" && t.live())
        {
            return Err(format!(
                "worker {worker} already has reorganization task {} in state {}",
                active.id, active.state
            )
            .into());
        }
    }
    let now = now_rfc3339();
    let t = Task {
        id: new_task_id(),
        worker: worker.to_string(),
        prompt: draft.prompt,
        created_by: draft.created_by,
        standing: draft.standing,
        state: "pending".into(),
        tags: draft.tags,
        context: draft.context,
        scheduled_at: draft.scheduled_at,
        depends_on: draft.depends_on,
        ceiling_min: draft.ceiling_min,
        recurring_id: draft.recurring_id,
        run_id: None,
        created_at: now.clone(),
        updated_at: now,
        repo: draft.repo,
        base: draft.base,
        knowledge_mode: draft.knowledge_mode,
        error: None,
    };
    p.tasks.push(t.clone());
    check_acyclic(&p.tasks).map_err(|e| -> BErr { e.into() })?;
    save(worker, &mut p)?;
    Ok(t)
}

// ── set_tasks (the agent's curation write; OCC) ───────────────────────────────

#[derive(Debug)]
pub struct SetRejection {
    pub reason: String,
    pub current: Partition,
}

/// Validate and apply the agent's reshaped partition. See the spec for the
/// rules; the shape of every rejection is (reason, current document) so the
/// agent can re-read and retry.
pub fn set_tasks(
    worker: &str,
    base_version: u64,
    mut tasks: Vec<Task>,
    mut recurring: Vec<Recurring>,
    ctx: &crate::worker::memory::RunContext,
) -> Result<Partition, SetRejection> {
    let _guard = LOCK.lock().unwrap();
    let current = load(worker);
    let reject = |reason: String, current: &Partition| SetRejection {
        reason,
        current: current.clone(),
    };
    if base_version != current.version {
        return Err(reject(
            format!(
                "version conflict: base_version {base_version} but the document is at {}",
                current.version
            ),
            &current,
        ));
    }
    let trusted_room = matches!(ctx.role.as_str(), "host-op" | "admin" | "trusted");
    let now = now_rfc3339();

    // Mint ids for new entries; force the partition's worker onto everything.
    for t in tasks.iter_mut() {
        if t.id.trim().is_empty() {
            t.id = new_task_id();
        }
        t.worker = worker.to_string();
    }
    for r in recurring.iter_mut() {
        if r.id.trim().is_empty() {
            r.id = new_recurring_id();
        }
        r.worker = worker.to_string();
    }
    let mut seen = HashSet::new();
    for id in tasks.iter().map(|t| &t.id).chain(recurring.iter().map(|r| &r.id)) {
        if !seen.insert(id.clone()) {
            return Err(reject(format!("duplicate id {id}"), &current));
        }
    }

    let old_tasks: HashMap<String, Task> = current
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.clone()))
        .collect();
    let new_task_ids: HashSet<String> = tasks.iter().map(|t| t.id.clone()).collect();

    // In-flight and host-attested state is untouchable.
    for old in current.tasks.iter().filter(|t| t.state != "pending") {
        match tasks.iter().find(|t| t.id == old.id) {
            None => {
                return Err(reject(
                    format!("task {} is {} — it cannot be removed", old.id, old.state),
                    &current,
                ))
            }
            Some(new) if new != old => {
                return Err(reject(
                    format!("task {} is {} — it cannot be edited", old.id, old.state),
                    &current,
                ))
            }
            _ => {}
        }
    }
    for t in tasks.iter() {
        let existed = old_tasks.get(t.id.as_str());
        if existed.is_none() && t.state != "pending" {
            return Err(reject(
                format!(
                    "task {} is new and must be pending (completion is host-attested)",
                    t.id
                ),
                &current,
            ));
        }
        if let Some(old) = existed {
            if old.state == "pending" && t.state != "pending" {
                return Err(reject(
                    format!("task {} may not change state (host-attested)", t.id),
                    &current,
                ));
            }
        }
        if let Err(e) = check_scheduled_at(&t.scheduled_at) {
            return Err(reject(format!("task {}: {e}", t.id), &current));
        }
        if !matches!(t.standing.as_str(), "owner" | "proactive") {
            return Err(reject(
                format!("task {}: standing must be owner or proactive", t.id),
                &current,
            ));
        }
    }

    // System templates (the heartbeat) are host-owned, byte for byte.
    let new_rec_ids: HashSet<String> = recurring.iter().map(|r| r.id.clone()).collect();
    for old in current.recurring.iter().filter(|r| r.system) {
        match recurring.iter().find(|r| r.id == old.id) {
            Some(new) if new == old => {}
            _ => {
                return Err(reject(
                    format!("recurring {} is system-owned and cannot change", old.id),
                    &current,
                ))
            }
        }
    }
    for r in recurring.iter() {
        if r.system && !current.recurring.iter().any(|o| o.id == r.id && o.system) {
            return Err(reject(
                format!("recurring {}: system entries are host-owned", r.id),
                &current,
            ));
        }
        if parse_schedule(&r.schedule).is_none() {
            return Err(reject(
                format!(
                    "recurring {}: unparseable schedule \"{}\" (5-field cron or \"every 30m\")",
                    r.id, r.schedule
                ),
                &current,
            ));
        }
    }

    // Standing follows the room: owner standing on a new or upgraded entry
    // needs a trusted room; otherwise it is forced to proactive.
    let old_rec: HashMap<String, Recurring> = current
        .recurring
        .iter()
        .map(|r| (r.id.clone(), r.clone()))
        .collect();
    if !trusted_room {
        for t in tasks.iter_mut() {
            let was_owner = old_tasks
                .get(t.id.as_str())
                .map(|o| o.standing == "owner")
                .unwrap_or(false);
            if t.standing == "owner" && !was_owner {
                t.standing = "proactive".into();
            }
        }
        for r in recurring.iter_mut() {
            let was_owner = old_rec
                .get(r.id.as_str())
                .map(|o| o.standing == "owner")
                .unwrap_or(false);
            if r.standing == "owner" && !was_owner {
                r.standing = "proactive".into();
            }
        }
    }

    if let Err(e) = check_acyclic(&tasks) {
        return Err(reject(e, &current));
    }

    // The participant scan guards the choke point: new or changed prompts
    // written from a tainted room must not name the people in it.
    if ctx.tainted() {
        for (id, prompt, old_prompt) in tasks
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    t.prompt.clone(),
                    old_tasks.get(t.id.as_str()).map(|o| o.prompt.clone()),
                )
            })
            .chain(recurring.iter().map(|r| {
                (
                    r.id.clone(),
                    r.prompt.clone(),
                    old_rec.get(r.id.as_str()).map(|o| o.prompt.clone()),
                )
            }))
        {
            if old_prompt.as_deref() != Some(prompt.as_str()) {
                if let Err(reason) = crate::worker::boundary::check_task_prompt(ctx, &prompt) {
                    return Err(reject(format!("{id}: {reason}"), &current));
                }
            }
        }
    }

    // Stamp bookkeeping; journal what the curation dropped.
    for t in tasks.iter_mut() {
        match old_tasks.get(t.id.as_str()) {
            Some(old) => {
                t.created_at = old.created_at.clone();
                t.created_by = old.created_by.clone();
                if *old != *t {
                    t.updated_at = now.clone();
                }
            }
            None => {
                t.created_at = now.clone();
                t.updated_at = now.clone();
                t.created_by = "agent".into();
            }
        }
    }
    for old in current.tasks.iter().filter(|t| t.state == "pending") {
        if !new_task_ids.contains(old.id.as_str()) {
            if let Ok(record) = serde_json::to_value(old) {
                journal_append(worker, "canceled", record);
            }
        }
    }
    for old in current.recurring.iter().filter(|r| !r.system) {
        if !new_rec_ids.contains(old.id.as_str()) {
            if let Ok(record) = serde_json::to_value(old) {
                journal_append(worker, "retired", record);
            }
        }
    }

    let mut next = Partition {
        version: current.version,
        tasks,
        recurring,
    };
    if let Err(e) = save(worker, &mut next) {
        return Err(reject(format!("could not persist: {e}"), &current));
    }
    Ok(next)
}

/// Cycle check over the live task graph. Edges to ids not present are allowed
/// (they block; a failed or canceled dependency looks like this).
fn check_acyclic(tasks: &[Task]) -> Result<(), String> {
    let index: HashMap<&str, usize> = tasks.iter().enumerate().map(|(i, t)| (t.id.as_str(), i)).collect();
    // 0 = unvisited, 1 = in stack, 2 = done
    fn visit(
        i: usize,
        tasks: &[Task],
        index: &HashMap<&str, usize>,
        state: &mut Vec<u8>,
    ) -> Result<(), String> {
        match state[i] {
            1 => return Err(format!("dependency cycle through {}", tasks[i].id)),
            2 => return Ok(()),
            _ => {}
        }
        state[i] = 1;
        for dep in &tasks[i].depends_on {
            if let Some(&j) = index.get(dep.as_str()) {
                visit(j, tasks, index, state)?;
            }
        }
        state[i] = 2;
        Ok(())
    }
    let mut state = vec![0u8; tasks.len()];
    for i in 0..tasks.len() {
        visit(i, tasks, &index, &mut state)?;
    }
    Ok(())
}

// ── attestation (supervisor-only transitions) ─────────────────────────────────

fn mutate<F: FnOnce(&mut Partition) -> Result<Option<Task>, String>>(
    worker: &str,
    f: F,
) -> Result<Option<Task>, BErr> {
    let _guard = LOCK.lock().unwrap();
    let mut p = load(worker);
    let out = f(&mut p).map_err(|e| -> BErr { e.into() })?;
    save(worker, &mut p)?;
    Ok(out)
}

/// pending → claimed, run id stamped before the box starts.
pub fn claim(worker: &str, task_id: &str, run_id: &str) -> Result<Task, BErr> {
    mutate(worker, |p| {
        let t = p
            .tasks
            .iter_mut()
            .find(|t| t.id == task_id)
            .ok_or(format!("no task {task_id}"))?;
        if t.state != "pending" {
            return Err(format!("task {task_id} is {} — cannot claim", t.state));
        }
        t.state = "claimed".into();
        t.run_id = Some(run_id.to_string());
        t.updated_at = now_rfc3339();
        Ok(Some(t.clone()))
    })
    .map(|t| t.unwrap())
}

/// claimed → completed | failed | needs-review. Terminal states journal and
/// prune; completion also releases the task's dependents (edges pruned).
pub fn finish(worker: &str, task_id: &str, state: &str) -> Result<(), BErr> {
    finish_with(worker, task_id, state, None)
}

/// `finish`, carrying the failure reason into the journaled record.
pub fn finish_with(
    worker: &str,
    task_id: &str,
    state: &str,
    error: Option<String>,
) -> Result<(), BErr> {
    if !matches!(state, "completed" | "failed" | "needs-review" | "pending") {
        return Err(format!("not an outcome state: {state}").into());
    }
    mutate(worker, |p| {
        let Some(pos) = p.tasks.iter().position(|t| t.id == task_id) else {
            return Err(format!("no task {task_id}"));
        };
        p.tasks[pos].state = state.to_string();
        p.tasks[pos].updated_at = now_rfc3339();
        p.tasks[pos].error = error;
        if state == "pending" {
            p.tasks[pos].run_id = None;
            p.tasks[pos].error = None;
            return Ok(None);
        }
        if state == "needs-review" {
            return Ok(None);
        }
        let t = p.tasks.remove(pos);
        if state == "completed" {
            for other in p.tasks.iter_mut() {
                other.depends_on.retain(|d| d != &t.id);
            }
        }
        if let Ok(record) = serde_json::to_value(&t) {
            journal_append(worker, state, record);
        }
        Ok(None)
    })
    .map(|_| ())
}

// ── reads (compat surface for CLI, slash, runlog, status) ─────────────────────

fn worker_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(paths::workers_data_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every recurring template across workers.
pub fn list_recurring() -> Vec<Recurring> {
    worker_names()
        .iter()
        .flat_map(|w| {
            let mut rec = load(w).recurring;
            for r in rec.iter_mut() {
                if r.worker.is_empty() {
                    r.worker = w.clone();
                }
            }
            rec
        })
        .collect()
}

/// Every live task across workers, oldest first.
pub fn list_all() -> Vec<Task> {
    let mut out: Vec<Task> = worker_names()
        .iter()
        .flat_map(|w| load(w).tasks)
        .collect();
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    out
}

/// A task by id: live documents first, then the journals.
pub fn find(task_id: &str) -> Option<Task> {
    for w in worker_names() {
        if let Some(t) = load(&w).tasks.into_iter().find(|t| t.id == task_id) {
            return Some(t);
        }
    }
    for w in worker_names() {
        if let Some(t) = journal_find(&w, task_id) {
            return Some(t);
        }
    }
    None
}

/// Admin repair: put a claimed (dead box) or needs-review task back to
/// pending, or re-file a journaled (failed/completed/canceled) task so a
/// fixed environment — a credential connected, a bug patched — can retry it.
pub fn requeue(task_id: &str) -> Result<String, BErr> {
    let t = find(task_id).ok_or_else(|| format!("no task {task_id}"))?;
    if !t.live() {
        let (worker, id, prev) = (t.worker.clone(), t.id.clone(), t.state.clone());
        let _guard = LOCK.lock().unwrap();
        let mut p = load(&worker);
        if p.tasks.iter().any(|x| x.id == id) {
            return Ok(format!("task {id} is already back in the queue"));
        }
        p.tasks.push(Task {
            state: "pending".into(),
            run_id: None,
            error: None,
            updated_at: now_rfc3339(),
            ..t
        });
        save(&worker, &mut p)?;
        return Ok(format!(
            "requeued {id}: {prev} → pending (re-filed from the journal)"
        ));
    }
    if t.state == "pending" {
        return Ok(format!("task {} is already pending", t.id));
    }
    if let Some(run) = &t.run_id {
        if crate::run::boxed::box_alive(run) {
            return Err(format!(
                "task {} still has a live box ({run}) — let it finish, or `docker kill {}` first",
                t.id,
                crate::run::boxed::container_name(run)
            )
            .into());
        }
    }
    let prev = t.state.clone();
    finish(&t.worker, task_id, "pending")?;
    Ok(format!("requeued {}: {prev} → pending", t.id))
}

// ── scheduling: eligibility, recurrence, the due view ─────────────────────────

/// Lexical compare works because every timestamp we mint is RFC3339 UTC and
/// `scheduled_at` is validated to end in Z.
fn time_due(scheduled_at: &Option<String>, now: &str) -> bool {
    match scheduled_at {
        None => true,
        Some(t) => t.as_str() <= now,
    }
}

fn window_state(w: &Option<Window>, now: &str) -> &'static str {
    let Some(w) = w else { return "open" };
    if let Some(until) = &w.until {
        if now > until.as_str() && !now.starts_with(until.as_str()) {
            return "expired";
        }
    }
    if let Some(from) = &w.from {
        if now < from.as_str() && !from.starts_with(&now[..10.min(now.len())]) {
            return "early";
        }
    }
    "open"
}

/// The claimable set of one partition: pending, time due, dependencies met
/// (an edge to a non-live id blocks), at most one reorganization in flight.
fn claimable(p: &Partition, now: &str) -> Vec<Task> {
    let live: HashSet<&str> = p.tasks.iter().map(|t| t.id.as_str()).collect();
    let reorg_open = p
        .tasks
        .iter()
        .any(|t| t.knowledge_mode == "reorganization" && matches!(t.state.as_str(), "claimed" | "needs-review"));
    let mut out: Vec<Task> = Vec::new();
    let mut reorg_offered = false;
    let mut sorted: Vec<&Task> = p.tasks.iter().collect();
    sorted.sort_by(|a, b| {
        (a.standing == "proactive")
            .cmp(&(b.standing == "proactive"))
            .then(a.created_at.cmp(&b.created_at))
    });
    for t in sorted {
        if t.state != "pending" || !time_due(&t.scheduled_at, now) {
            continue;
        }
        if t.depends_on.iter().any(|d| live.contains(d.as_str())) {
            continue; // waits on a live task
        }
        if t.depends_on.iter().any(|d| !live.contains(d.as_str())) {
            // Dangling edge: the dependency failed or was canceled. Blocked
            // until the agent or an admin curates it.
            continue;
        }
        if t.knowledge_mode == "reorganization" {
            if reorg_open || reorg_offered {
                continue;
            }
            reorg_offered = true;
        }
        out.push(t.clone());
    }
    out
}

// Recurrence cursors: last spawn (or adoption) per template, ms since epoch.
fn load_cursors() -> HashMap<String, i64> {
    std::fs::read_to_string(paths::tms_cursors_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cursors(c: &HashMap<String, i64>) {
    let path = paths::tms_cursors_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(path, text);
    }
}

/// Spawn children for due templates and expire templates whose window passed.
/// A missing cursor adopts at now (a new template fires at its next matching
/// time, never for the past). Catch-up is coalesced: one spawn however many
/// firings were missed while the daemon was down.
fn run_recurrence(worker: &str, p: &mut Partition, now_str: &str, now: i64) -> bool {
    let mut cursors = load_cursors();
    let mut changed = false;
    let mut expired: Vec<usize> = Vec::new();
    for (i, r) in p.recurring.iter().enumerate() {
        let key = format!("{worker}|{}", r.id);
        match window_state(&r.window, now_str) {
            "expired" => {
                expired.push(i);
                continue;
            }
            "early" => continue,
            _ => {}
        }
        let Some(schedule) = parse_schedule(&r.schedule) else { continue };
        let last = cursors.get(&key).copied().unwrap_or(0);
        if last == 0 {
            cursors.insert(key, now);
            continue;
        }
        let due = match &schedule {
            Schedule::Every(interval) => now - last >= *interval,
            Schedule::Cron(spec) => cron_due(spec, last, now),
        };
        if !due {
            continue;
        }
        if r.spawn_policy == "skip-if-previous-open"
            && p.tasks
                .iter()
                .any(|t| t.recurring_id.as_deref() == Some(r.id.as_str()) && t.live())
        {
            cursors.insert(key, now); // the slot passed; don't replay it later
            continue;
        }
        let child = Task {
            id: new_task_id(),
            worker: worker.to_string(),
            prompt: r.prompt.clone(),
            created_by: "recurrence".into(),
            standing: r.standing.clone(),
            state: "pending".into(),
            tags: r.tags.clone(),
            context: r.context.clone(),
            scheduled_at: None,
            depends_on: Vec::new(),
            ceiling_min: r.ceiling_min,
            recurring_id: Some(r.id.clone()),
            run_id: None,
            created_at: now_str.to_string(),
            updated_at: now_str.to_string(),
            repo: None,
            base: None,
            knowledge_mode: "append".into(),
            error: None,
        };
        eprintln!("tms: recurrence {} → task {} for {worker}", r.id, child.id);
        p.tasks.push(child);
        cursors.insert(key, now);
        changed = true;
    }
    for i in expired.into_iter().rev() {
        let r = p.recurring.remove(i);
        if let Ok(record) = serde_json::to_value(&r) {
            journal_append(worker, "expired", record);
        }
        changed = true;
    }
    save_cursors(&cursors);
    changed
}

pub const HEARTBEAT_PROMPT: &str = "Heartbeat: read your tasks file and your recent conversations' asks. \
Reshape the task list if your purpose needs it (set_tasks); do any small due work directly; \
exit immediately if nothing needs doing.";

/// Keep the system heartbeat template in line with config (`heartbeat = \"30m\"`
/// in worker.toml, default 30m, \"off\" disables). Host-owned; set_tasks
/// cannot touch it. A worker absent from config (a data-dir leftover) gets
/// none — it could not run the spawned tasks anyway.
fn ensure_heartbeat(worker: &str, p: &mut Partition, interval: Option<&str>) -> bool {
    let id = "r-heartbeat";
    let want = match interval {
        None | Some("off") => None,
        Some(s) => Some(s.to_string()),
    };
    let existing = p.recurring.iter().position(|r| r.id == id);
    match (want, existing) {
        (None, Some(i)) => {
            p.recurring.remove(i);
            true
        }
        (None, None) => false,
        (Some(schedule), existing) => {
            let tpl = Recurring {
                id: id.into(),
                worker: worker.to_string(),
                prompt: HEARTBEAT_PROMPT.into(),
                schedule,
                window: None,
                spawn_policy: "skip-if-previous-open".into(),
                standing: "proactive".into(),
                ceiling_min: 10.0,
                tags: Tags::default(),
                context: Value::Null,
                system: true,
            };
            match existing {
                Some(i) if p.recurring[i] == tpl => false,
                Some(i) => {
                    p.recurring[i] = tpl;
                    true
                }
                None => {
                    p.recurring.push(tpl);
                    true
                }
            }
        }
    }
}

pub struct DueWorker {
    pub worker: String,
    pub claimable: Vec<Task>,
}

/// The supervisor's whole view of the world, evaluated per tick: keep each
/// worker's heartbeat template honest, spawn due recurrences, and return the
/// claimable sets (owner standing first). Budgets and the cap are the
/// supervisor's business, not ours.
pub fn due(heartbeats: &HashMap<String, String>) -> Vec<DueWorker> {
    let now_str = now_rfc3339();
    let now = now_ms();
    let mut out = Vec::new();
    let mut workers = worker_names();
    for w in heartbeats.keys() {
        if !workers.contains(w) {
            workers.push(w.clone());
        }
    }
    for worker in workers {
        let _guard = LOCK.lock().unwrap();
        let mut p = load(&worker);
        let mut changed = ensure_heartbeat(&worker, &mut p, heartbeats.get(&worker).map(|s| s.as_str()));
        changed |= run_recurrence(&worker, &mut p, &now_str, now);
        if changed {
            let _ = save(&worker, &mut p);
        }
        let claimable = claimable(&p, &now_str);
        if !claimable.is_empty() {
            out.push(DueWorker {
                worker: worker.clone(),
                claimable,
            });
        }
    }
    out
}

// ── schedules: intervals and 5-field cron (host-local) ───────────────────────

/// Parse an interval to milliseconds. "every " prefix optional; unit s/m/h/d.
pub fn parse_interval(s: &str) -> Option<i64> {
    let s = s.trim().strip_prefix("every").unwrap_or(s).trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic())?);
    let n: i64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "s" | "sec" | "secs" => 1_000,
        "m" | "min" | "mins" => 60_000,
        "h" | "hr" | "hrs" => 3_600_000,
        "d" | "day" | "days" => 86_400_000,
        _ => return None,
    };
    Some(n * mult)
}

/// A parsed cron expression, each field a bitset of allowed values.
#[derive(Debug, Clone, PartialEq)]
pub struct CronSpec {
    minute: u64,
    hour: u64,
    dom: u64,
    mon: u64,
    dow: u64,
    dom_star: bool,
    dow_star: bool,
}

fn cron_field(s: &str, base: u8, max: u8) -> Option<u64> {
    let mut mask: u64 = 0;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u8>().ok().filter(|n| *n > 0)?),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (base, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().ok()?, b.parse().ok()?)
        } else {
            let v: u8 = range.parse().ok()?;
            if step > 1 {
                (v, max)
            } else {
                (v, v)
            }
        };
        if lo < base || hi > max || lo > hi {
            return None;
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v += step;
        }
    }
    (mask != 0).then_some(mask)
}

/// "min hour dom mon dow" plus the common @aliases. Day-of-week 0-7, both 0
/// and 7 Sunday; times are the host's local time.
pub fn parse_cron(s: &str) -> Option<CronSpec> {
    let s = match s.trim() {
        "@hourly" => "0 * * * *",
        "@daily" | "@midnight" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        other => other,
    };
    let f: Vec<&str> = s.split_whitespace().collect();
    if f.len() != 5 {
        return None;
    }
    let dow = {
        let m = cron_field(f[4], 0, 7)?;
        (m | (m >> 7)) & 0x7f
    };
    Some(CronSpec {
        minute: cron_field(f[0], 0, 59)?,
        hour: cron_field(f[1], 0, 23)?,
        dom: cron_field(f[2], 1, 31)?,
        mon: cron_field(f[3], 1, 12)?,
        dow,
        dom_star: f[2] == "*",
        dow_star: f[4] == "*",
    })
}

impl CronSpec {
    fn matches(&self, minute: u8, hour: u8, mday: u8, mon: u8, wday: u8) -> bool {
        let bit = |mask: u64, v: u8| mask & (1u64 << v) != 0;
        if !bit(self.minute, minute) || !bit(self.hour, hour) || !bit(self.mon, mon) {
            return false;
        }
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => bit(self.dom, mday),
            (true, false) => bit(self.dow, wday),
            (false, false) => bit(self.dom, mday) || bit(self.dow, wday),
        }
    }
}

fn local_time(ms: i64) -> (u8, u8, u8, u8, u8) {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    (
        tm.tm_min as u8,
        tm.tm_hour as u8,
        tm.tm_mday as u8,
        (tm.tm_mon + 1) as u8,
        tm.tm_wday as u8,
    )
}

/// Any matching minute in (since, now]? Scans at most the trailing 25 hours —
/// catch-up for a nightly across daemon downtime, coalesced to one firing.
fn cron_due(spec: &CronSpec, since: i64, now: i64) -> bool {
    let minute_floor = |ms: i64| ms - ms.rem_euclid(60_000);
    let mut m = minute_floor(since.max(now - 25 * 3_600_000)) + 60_000;
    while m <= now {
        let (minute, hour, mday, mon, wday) = local_time(m);
        if spec.matches(minute, hour, mday, mon, wday) {
            return true;
        }
        m += 60_000;
    }
    false
}

pub enum Schedule {
    Every(i64),
    Cron(CronSpec),
}

pub fn parse_schedule(s: &str) -> Option<Schedule> {
    if let Some(ms) = parse_interval(s) {
        return Some(Schedule::Every(ms));
    }
    parse_cron(s).map(Schedule::Cron)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(role: &str, channel: Option<&str>) -> crate::worker::memory::RunContext {
        crate::worker::memory::RunContext {
            provider: "term".into(),
            channel_id: channel.map(String::from),
            user_id: channel.map(|_| "u1".to_string()),
            message_id: None,
            role: role.into(),
            is_dm: true,
            inbound: false,
        }
    }

    // Tests share the process env (ROSTER_ROOT), so they serialize on one lock.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct Sandbox {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    fn sandbox() -> Sandbox {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());
        Sandbox {
            _guard: guard,
            _dir: dir,
        }
    }

    #[test]
    fn add_claim_finish_journal_roundtrip() {
        let _dir = sandbox();
        let t = add("w1", Draft { prompt: "do a thing".into(), ..Draft::default() }).unwrap();
        assert_eq!(t.state, "pending");
        let claimed = claim("w1", &t.id, "run-1").unwrap();
        assert_eq!(claimed.state, "claimed");
        finish("w1", &t.id, "completed").unwrap();
        assert!(load("w1").tasks.is_empty());
        let journaled = journal_find("w1", &t.id).unwrap();
        assert_eq!(journaled.state, "completed");
        assert_eq!(find(&t.id).unwrap().state, "completed");
    }

    #[test]
    fn completion_releases_dependents_and_failure_blocks() {
        let _dir = sandbox();
        let a = add("w2", Draft { prompt: "a".into(), ..Draft::default() }).unwrap();
        let b = add(
            "w2",
            Draft { prompt: "b".into(), depends_on: vec![a.id.clone()], ..Draft::default() },
        )
        .unwrap();
        let now = now_rfc3339();
        assert!(claimable(&load("w2"), &now).iter().all(|t| t.id != b.id));
        claim("w2", &a.id, "r").unwrap();
        finish("w2", &a.id, "completed").unwrap();
        assert!(claimable(&load("w2"), &now).iter().any(|t| t.id == b.id));

        let c = add("w2", Draft { prompt: "c".into(), ..Draft::default() }).unwrap();
        let d = add(
            "w2",
            Draft { prompt: "d".into(), depends_on: vec![c.id.clone()], ..Draft::default() },
        )
        .unwrap();
        claim("w2", &c.id, "r2").unwrap();
        finish("w2", &c.id, "failed").unwrap();
        // dangling edge → blocked until curated
        assert!(claimable(&load("w2"), &now).iter().all(|t| t.id != d.id));
    }

    #[test]
    fn scheduled_tasks_wait_for_their_time() {
        let _dir = sandbox();
        add(
            "w3",
            Draft {
                prompt: "later".into(),
                scheduled_at: Some("2099-01-01T00:00:00Z".into()),
                ..Draft::default()
            },
        )
        .unwrap();
        add(
            "w3",
            Draft {
                prompt: "past".into(),
                scheduled_at: Some("2020-01-01T00:00:00Z".into()),
                ..Draft::default()
            },
        )
        .unwrap();
        let c = claimable(&load("w3"), &now_rfc3339());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].prompt, "past");
        assert!(add(
            "w3",
            Draft { prompt: "tz".into(), scheduled_at: Some("2026-01-01T00:00:00+05:30".into()), ..Draft::default() }
        )
        .is_err());
    }

    #[test]
    fn set_tasks_occ_and_protections() {
        let _dir = sandbox();
        let t = add("w4", Draft { prompt: "keep me".into(), ..Draft::default() }).unwrap();
        claim("w4", &t.id, "r").unwrap();
        let p = load("w4");

        // stale version rejected
        assert!(set_tasks("w4", p.version + 1, p.tasks.clone(), vec![], &ctx("host-op", None)).is_err());

        // removing a claimed task rejected
        let err = set_tasks("w4", p.version, vec![], vec![], &ctx("host-op", None)).unwrap_err();
        assert!(err.reason.contains("cannot be removed"), "{}", err.reason);

        // adding a pending task via set works; new entries get ids + agent authorship
        let mut tasks = p.tasks.clone();
        tasks.push(Task {
            id: String::new(),
            worker: "w4".into(),
            prompt: "new plan".into(),
            created_by: "user".into(), // will be forced to agent
            standing: "owner".into(),
            state: "pending".into(),
            tags: Tags::default(),
            context: Value::Null,
            scheduled_at: None,
            depends_on: vec![],
            ceiling_min: 10.0,
            recurring_id: None,
            run_id: None,
            created_at: String::new(),
            updated_at: String::new(),
            repo: None,
            base: None,
            knowledge_mode: "append".into(),
            error: None,
        });
        let next = set_tasks("w4", p.version, tasks, vec![], &ctx("host-op", None)).unwrap();
        let added = next.tasks.iter().find(|t| t.prompt == "new plan").unwrap();
        assert_eq!(added.created_by, "agent");
        assert_eq!(added.standing, "owner"); // trusted room may grant owner

        // untrusted room: owner standing forced to proactive
        let p2 = load("w4");
        let mut tasks2 = p2.tasks.clone();
        tasks2.push(Task {
            prompt: "sneaky".into(),
            standing: "owner".into(),
            ..tasks2.iter().find(|t| t.prompt == "new plan").unwrap().clone()
        });
        tasks2.last_mut().unwrap().id = String::new();
        let next2 = set_tasks("w4", p2.version, tasks2, vec![], &ctx("untrusted", Some("ch"))).unwrap();
        let sneaky = next2.tasks.iter().find(|t| t.prompt == "sneaky").unwrap();
        assert_eq!(sneaky.standing, "proactive");

        // cycles rejected
        let p3 = load("w4");
        let mut tasks3 = p3.tasks.clone();
        let (i, j) = (0usize, 1usize);
        let (id_i, id_j) = (tasks3[i].id.clone(), tasks3[j].id.clone());
        if tasks3[i].state == "pending" && tasks3[j].state == "pending" {
            tasks3[i].depends_on = vec![id_j];
            tasks3[j].depends_on = vec![id_i];
            assert!(set_tasks("w4", p3.version, tasks3, vec![], &ctx("host-op", None)).is_err());
        }
    }

    #[test]
    fn system_heartbeat_is_untouchable_and_spawns() {
        let _dir = sandbox();
        let mut hb = HashMap::new();
        hb.insert("w5".to_string(), "every 30m".to_string());
        // first due(): template adopted, cursor set, nothing claimable yet
        assert!(due(&hb).is_empty());
        let p = load("w5");
        assert!(p.recurring.iter().any(|r| r.id == "r-heartbeat" && r.system));

        // agent cannot remove it
        let err = set_tasks("w5", p.version, vec![], vec![], &ctx("host-op", None)).unwrap_err();
        assert!(err.reason.contains("system-owned"), "{}", err.reason);

        // rewind the cursor → the next due() spawns a heartbeat child
        let mut cursors = load_cursors();
        for v in cursors.values_mut() {
            *v -= 3_600_000;
        }
        save_cursors(&cursors);
        let due_now = due(&hb);
        assert_eq!(due_now.len(), 1);
        assert_eq!(due_now[0].claimable[0].created_by, "recurrence");
        assert_eq!(due_now[0].claimable[0].recurring_id.as_deref(), Some("r-heartbeat"));

        // skip-if-previous-open: rewinding again spawns nothing while the child is live
        let mut cursors = load_cursors();
        for v in cursors.values_mut() {
            *v -= 3_600_000;
        }
        save_cursors(&cursors);
        let again = due(&hb);
        assert_eq!(again[0].claimable.len(), 1);
    }

    #[test]
    fn parses_cron_and_intervals() {
        assert!(matches!(parse_schedule("every 4h"), Some(Schedule::Every(_))));
        assert!(matches!(parse_schedule("0 9 * * 1-5"), Some(Schedule::Cron(_))));
        assert!(parse_schedule("whenever").is_none());

        let daily = parse_cron("0 9 * * *").unwrap();
        assert!(daily.matches(0, 9, 15, 7, 2));
        assert!(!daily.matches(1, 9, 15, 7, 2));
        let weekdays = parse_cron("30 8 * * 1-5").unwrap();
        assert!(weekdays.matches(30, 8, 15, 7, 1));
        assert!(!weekdays.matches(30, 8, 15, 7, 0));
        let sunday7 = parse_cron("0 0 * * 7").unwrap();
        assert!(sunday7.matches(0, 0, 15, 7, 0));
        let vixie = parse_cron("0 0 13 * 5").unwrap(); // the 13th OR a Friday
        assert!(vixie.matches(0, 0, 13, 6, 2));
        assert!(vixie.matches(0, 0, 20, 6, 5));
        assert!(!vixie.matches(0, 0, 20, 6, 2));
        assert!(parse_cron("60 9 * * *").is_none());

        let every_minute = parse_cron("* * * * *").unwrap();
        let now = 1_800_000_000_000i64;
        assert!(cron_due(&every_minute, now - 120_000, now));
        assert!(!cron_due(&every_minute, now, now));
    }

    #[test]
    fn migrates_legacy_queue_files() {
        let _dir = sandbox();
        let qdir = paths::worker_queue_dir("w6");
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(
            qdir.join("t-old1.json"),
            serde_json::json!({
                "id": "t-old1", "imp": "w6", "prompt": "old waiting", "origin": "manual",
                "proactive": false, "state": "waiting",
                "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
                "ceiling_min": 30.0, "context": null, "repo": null, "base": null
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            qdir.join("t-old2.json"),
            serde_json::json!({
                "id": "t-old2", "worker": "w6", "prompt": "old done", "origin": "schedule",
                "proactive": true, "state": "done",
                "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
                "ceiling_min": 30.0, "context": null, "repo": null, "base": null
            })
            .to_string(),
        )
        .unwrap();
        let p = load("w6");
        assert_eq!(p.tasks.len(), 1);
        assert_eq!(p.tasks[0].id, "t-old1");
        assert_eq!(p.tasks[0].state, "pending");
        assert_eq!(p.tasks[0].standing, "owner");
        assert_eq!(journal_find("w6", "t-old2").unwrap().state, "completed");
        assert!(!qdir.exists());
    }
}
