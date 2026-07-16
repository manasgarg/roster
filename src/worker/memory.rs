//! Scoped, append-only worker memory.
//!
//! The box can propose memory actions, but the trusted host derives the active
//! worker/channel/user from the run context and owns the JSONL event log. Memories
//! are advisory prompt data, never authorization inputs.

use crate::paths;
use crate::util::now_rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const DEFAULT_NOTE_CHARS: usize = 2_000;
const DEFAULT_SCOPE_NOTES: usize = 100;
const DEFAULT_RECALL_NOTES: usize = 20;
const DEFAULT_RECALL_CHARS: usize = 6_000;
pub const SUPPORTED_MEMORY_KINDS: &[&str] = &["preference", "fact", "decision", "interaction"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    #[serde(alias = "imp")]
    Worker,
    Channel,
    User,
}

impl MemoryScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Channel => "channel",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBasis {
    Explicit,
    #[default]
    Inferred,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemorySource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNote {
    pub id: String,
    pub ts: String,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    pub kind: String,
    pub note: String,
    #[serde(default)]
    pub basis: MemoryBasis,
    #[serde(default)]
    pub source: MemorySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub forgotten: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected_at: Option<String>,
}

impl MemoryNote {
    pub fn status(&self) -> &'static str {
        if self.forgotten {
            "forgotten"
        } else if self.disabled {
            "disabled"
        } else if expired(self.expires_at.as_deref()) {
            "expired"
        } else {
            "active"
        }
    }

    fn active(&self) -> bool {
        self.status() == "active"
    }
}

/// Trusted context for the current turn. The host writes this before delivering
/// a message to the box; model-supplied action fields never replace it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunContext {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub is_dm: bool,
    /// The run's prompt embeds inbound third-party content (a relay task) —
    /// person-tainted even without channel/user identifiers.
    #[serde(default)]
    pub inbound: bool,
}

impl RunContext {
    /// Did interaction content or context enter this run? Tainted runs get
    /// knowledge read-only; clean runs get no memory recall — the two halves
    /// of the memory/knowledge boundary (docs/knowledge.md). One
    /// predicate, shared by provisioning and context compilation.
    pub fn tainted(&self) -> bool {
        self.channel_id.is_some() || self.user_id.is_some() || self.inbound
    }

    /// A deliberately-tainted context for when a run's real context can't be
    /// read. The participant scan then runs (fail closed) instead of being
    /// silently skipped, so an unreadable context file cannot disable the
    /// memory/knowledge boundary. `inbound` is the only field `tainted()` reads.
    pub fn tainted_unknown() -> Self {
        RunContext {
            inbound: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UserMemorySettings {
    pub allow_inferred: bool,
    pub cross_channel_recall: bool,
}

impl Default for UserMemorySettings {
    fn default() -> Self {
        Self {
            allow_inferred: true,
            cross_channel_recall: false,
        }
    }
}

impl RunContext {
    fn qualified(&self, raw: &str) -> String {
        if self.provider.is_empty() || raw.contains(':') {
            raw.to_string()
        } else {
            format!("{}:{raw}", self.provider)
        }
    }

    pub fn channel_scope_id(&self) -> Option<String> {
        self.channel_id.as_deref().map(|id| self.qualified(id))
    }

    pub fn user_scope_id(&self) -> Option<String> {
        self.user_id.as_deref().map(|id| self.qualified(id))
    }

    fn trusted_participant(&self) -> bool {
        matches!(self.role.as_str(), "trusted" | "admin" | "host-op") || self.is_dm
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryPolicy {
    pub enabled: bool,
    pub allowed_kinds: Vec<String>,
    pub max_note_chars: usize,
    pub max_notes_per_scope: usize,
    pub recall_max_notes: usize,
    pub recall_char_budget: usize,
    pub max_retention_days: Option<u64>,
    pub allow_inferred_user_auto: bool,
    pub allow_worker_auto: bool,
    pub cross_channel_user_recall: bool,
    pub user_memory_in_groups: bool,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_kinds: SUPPORTED_MEMORY_KINDS
                .iter()
                .copied()
                .map(String::from)
                .collect(),
            max_note_chars: DEFAULT_NOTE_CHARS,
            max_notes_per_scope: DEFAULT_SCOPE_NOTES,
            recall_max_notes: DEFAULT_RECALL_NOTES,
            recall_char_budget: DEFAULT_RECALL_CHARS,
            max_retention_days: None,
            allow_inferred_user_auto: false,
            allow_worker_auto: false,
            cross_channel_user_recall: false,
            user_memory_in_groups: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompiledMemoryPolicy {
    #[serde(default)]
    pub default: MemoryPolicy,
    #[serde(default)]
    pub workers: HashMap<String, MemoryPolicy>,
}

pub fn load_policy(worker: &str) -> MemoryPolicy {
    let compiled = crate::config::snapshot()
        .map(|c| c.memory.clone())
        .unwrap_or_default();
    compiled
        .workers
        .get(short_worker(worker))
        .cloned()
        .unwrap_or(compiled.default)
}

fn contextual_policy(worker: &str, context: &RunContext) -> MemoryPolicy {
    let mut policy = load_policy(worker);
    if let Some(channel) = context.channel_id.as_deref() {
        if !crate::channel::discord::channel_memory_enabled(channel) {
            policy.enabled = false;
        }
        if let Some(limit) = crate::channel::discord::channel_memory_recall_max_notes(channel) {
            policy.recall_max_notes = policy.recall_max_notes.min(limit);
        }
        if let Some(limit) = crate::channel::discord::channel_memory_recall_char_budget(channel) {
            policy.recall_char_budget = policy.recall_char_budget.min(limit);
        }
        if let Some(kinds) = crate::channel::discord::channel_memory_allowed_kinds(channel) {
            policy.allowed_kinds.retain(|kind| kinds.contains(kind));
        }
    }
    if let Some(user) = context.user_scope_id() {
        let settings = user_settings(worker, &user);
        policy.cross_channel_user_recall &= settings.cross_channel_recall;
    }
    policy
}

fn short_worker(worker: &str) -> &str {
    worker.strip_prefix("org/").unwrap_or(worker)
}

fn memory_path(worker: &str) -> PathBuf {
    paths::worker_memory_file(worker)
}

/// Memory used to live under `notes/`. Read the old event log as well so an
/// upgrade does not lose conversational continuity. All new writes go to
/// `memory/`; an admin-requested compaction finishes the physical migration.
fn legacy_notes_path(worker: &str) -> PathBuf {
    paths::worker_notes_legacy_file(worker)
}

fn run_context_path(run_id: &str) -> PathBuf {
    paths::run_dir(run_id).join("memory-context.json")
}

fn recall_trace_path(run_id: &str) -> PathBuf {
    paths::run_dir(run_id).join("memory-recall.jsonl")
}

fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Holds both the in-process memory mutex and the per-worker cross-process file
/// lock, so a `compact` in one process can't clobber a `remember`/`forget` that
/// another process (the daemon's executor, a `gates approve`) is appending — the
/// erased or resurrected note bug. Taken mutex-first, one worker at a time.
struct MemoryGuard {
    _proc: std::sync::MutexGuard<'static, ()>,
    _file: crate::statefile::FileLock,
}

fn lock_memory(worker: &str) -> Result<MemoryGuard, String> {
    let proc = write_lock()
        .lock()
        .map_err(|_| "memory write lock poisoned".to_string())?;
    let file = crate::statefile::FileLock::acquire(&format!("memory-{}", short_worker(worker)))
        .map_err(|e| format!("memory lock: {e}"))?;
    Ok(MemoryGuard {
        _proc: proc,
        _file: file,
    })
}

pub fn save_run_context(run_id: &str, context: &RunContext) -> Result<(), String> {
    if run_id.is_empty() {
        return Ok(());
    }
    let path = run_context_path(run_id);
    let dir = path.parent().ok_or("bad run context path")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        format!(
            "{}\n",
            serde_json::to_string_pretty(context).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

pub fn load_run_context(run_id: &str) -> RunContext {
    if run_id.is_empty() {
        // No run identity to key a context on (host-op / CLI paths). This is a
        // legitimate absence, not a failure, and carries no interaction content.
        return RunContext::default();
    }
    match crate::statefile::read_if_present(&run_context_path(run_id)) {
        Ok(Some(s)) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("memory: run context for {run_id} is corrupt ({e}); treating run as tainted");
            RunContext::tainted_unknown()
        }),
        // A dispatched run always writes its context; a missing or unreadable
        // one means that write was lost. Fail closed so the participant scan
        // still runs, rather than silently disabling the boundary.
        Ok(None) => {
            eprintln!("memory: no run context for {run_id}; treating run as tainted");
            RunContext::tainted_unknown()
        }
        Err(e) => {
            eprintln!("memory: could not read run context for {run_id} ({e}); treating run as tainted");
            RunContext::tainted_unknown()
        }
    }
}

fn append_event(worker: &str, event: &Value) -> Result<(), String> {
    let _guard = lock_memory(worker)?;
    // append_line fsyncs before returning, so a "remembered"/"forgot"
    // acknowledgement is never given for a write the OS could still lose.
    crate::statefile::append_line(&memory_path(worker), &event.to_string())
        .map_err(|e| e.to_string())
}

fn read_events(worker: &str) -> Vec<Value> {
    [legacy_notes_path(worker), memory_path(worker)]
        .into_iter()
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect::<Vec<Value>>()
        })
        .collect()
}

fn fold_events(events: &[Value]) -> Vec<MemoryNote> {
    let mut notes: HashMap<String, MemoryNote> = HashMap::new();
    for event in events {
        let op = event.get("op").and_then(Value::as_str).unwrap_or("");
        let id = event.get("id").and_then(Value::as_str).unwrap_or("");
        match op {
            "remember" => {
                if let Ok(note) = serde_json::from_value::<MemoryNote>(event.clone()) {
                    notes.insert(note.id.clone(), note);
                }
            }
            "correct" => {
                if let (Some(note), Some(replacement)) =
                    (notes.get_mut(id), event.get("note").and_then(Value::as_str))
                {
                    note.note = replacement.to_string();
                    note.corrected_at = event.get("ts").and_then(Value::as_str).map(String::from);
                }
            }
            "disable" => {
                if let Some(note) = notes.get_mut(id) {
                    note.disabled = true;
                }
            }
            "enable" => {
                if let Some(note) = notes.get_mut(id) {
                    note.disabled = false;
                }
            }
            "pin" => {
                if let Some(note) = notes.get_mut(id) {
                    note.pinned = true;
                }
            }
            "unpin" => {
                if let Some(note) = notes.get_mut(id) {
                    note.pinned = false;
                }
            }
            "forget" => {
                if let Some(note) = notes.get_mut(id) {
                    note.forgotten = true;
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<MemoryNote> = notes.into_values().collect();
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    out
}

pub fn user_settings(worker: &str, user_scope_id: &str) -> UserMemorySettings {
    let mut settings = UserMemorySettings::default();
    for event in read_events(worker) {
        if event.get("op").and_then(Value::as_str) != Some("user-settings")
            || event.get("scope_id").and_then(Value::as_str) != Some(user_scope_id)
        {
            continue;
        }
        if let Some(value) = event.get("allow_inferred").and_then(Value::as_bool) {
            settings.allow_inferred = value;
        }
        if let Some(value) = event.get("cross_channel_recall").and_then(Value::as_bool) {
            settings.cross_channel_recall = value;
        }
    }
    settings
}

pub fn list(worker: &str) -> Vec<MemoryNote> {
    fold_events(&read_events(worker))
}

pub fn find(worker: &str, id: &str) -> Option<MemoryNote> {
    list(worker).into_iter().find(|n| n.id == id)
}

fn new_id() -> String {
    format!("note_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

fn parse_scope(
    payload: &Value,
    context: &RunContext,
) -> Result<(MemoryScope, Option<String>), String> {
    let requested = payload.get("scope").and_then(Value::as_str);
    let scope = match requested {
        Some("worker") => MemoryScope::Worker,
        Some("channel") => MemoryScope::Channel,
        Some("user") => MemoryScope::User,
        Some(other) => return Err(format!("unknown memory scope \"{other}\"")),
        None if context.user_id.is_some() => MemoryScope::User,
        None if context.channel_id.is_some() => MemoryScope::Channel,
        None => return Err("remember needs an explicit scope outside a conversation".into()),
    };
    let trusted_id = match scope {
        MemoryScope::Worker => None,
        MemoryScope::Channel => Some(
            context
                .channel_scope_id()
                .ok_or("no current channel for channel memory")?,
        ),
        MemoryScope::User => Some(
            context
                .user_scope_id()
                .ok_or("no current user for user memory")?,
        ),
    };
    if let Some(supplied) = payload.get("scope_id").and_then(Value::as_str) {
        if trusted_id.as_deref() != Some(supplied) {
            return Err("the requested memory subject is not the active channel or user".into());
        }
    }
    Ok((scope, trusted_id))
}

fn parse_basis(payload: &Value) -> Result<MemoryBasis, String> {
    match payload
        .get("basis")
        .and_then(Value::as_str)
        .unwrap_or("inferred")
    {
        "explicit" => Ok(MemoryBasis::Explicit),
        "inferred" => Ok(MemoryBasis::Inferred),
        other => Err(format!("unknown memory basis \"{other}\"")),
    }
}

fn obvious_secret(note: &str) -> bool {
    let n = note.to_ascii_lowercase();
    [
        "-----begin private key",
        "authorization: bearer",
        "password:",
        "password is ",
        "api_key:",
        "api key:",
        "api key is ",
        "access token:",
        "access token is ",
        "ghp_",
        "xoxb-",
    ]
    .iter()
    .any(|needle| n.contains(needle))
}

/// Decide the minimum trust level for a note action and reject subject
/// mismatches before the normal action trust ladder runs.
pub fn action_trust(
    worker: &str,
    intent: &str,
    payload: &Value,
    context: &RunContext,
) -> Result<&'static str, String> {
    let policy = contextual_policy(worker, context);
    if !policy.enabled {
        return Err("memory is disabled for this worker".into());
    }
    if intent == "memory-preferences" {
        context
            .user_scope_id()
            .ok_or("memory preferences require an active user")?;
        return Ok("auto");
    }
    let creating = intent == "remember";
    let (scope, basis) = if creating {
        let (scope, _) = parse_scope(payload, context)?;
        (scope, parse_basis(payload)?)
    } else {
        let id = payload
            .get("note_id")
            .and_then(Value::as_str)
            .ok_or("memory action needs note_id")?;
        let note = find(worker, id).ok_or_else(|| format!("no such memory {id}"))?;
        authorize_existing(&note, context)?;
        (note.scope, note.basis)
    };
    if creating && scope == MemoryScope::User && basis == MemoryBasis::Inferred {
        let user = context
            .user_scope_id()
            .ok_or("inferred user memory requires an active user")?;
        if !user_settings(worker, &user).allow_inferred {
            return Err("the current user has opted out of inferred memory".into());
        }
    }
    Ok(match scope {
        MemoryScope::Worker if policy.allow_worker_auto => "auto",
        MemoryScope::Worker => "gate",
        MemoryScope::Channel
            if context.trusted_participant()
                && (basis == MemoryBasis::Explicit
                    || context
                        .channel_id
                        .as_deref()
                        .map(crate::channel::discord::channel_memory_inferred_auto)
                        .unwrap_or(false)) =>
        {
            "auto"
        }
        MemoryScope::Channel => "gate",
        MemoryScope::User if !creating => "auto",
        MemoryScope::User if basis == MemoryBasis::Explicit => "auto",
        MemoryScope::User if policy.allow_inferred_user_auto => "auto",
        MemoryScope::User => "gate",
    })
}

fn authorize_existing(note: &MemoryNote, context: &RunContext) -> Result<(), String> {
    match note.scope {
        MemoryScope::Worker => Ok(()), // normal action trust still hard-gates it
        MemoryScope::Channel => {
            if note.scope_id == context.channel_scope_id() {
                Ok(())
            } else {
                Err("memory does not belong to the active channel".into())
            }
        }
        MemoryScope::User => {
            if note.scope_id == context.user_scope_id() {
                Ok(())
            } else {
                Err("memory is not about the active user".into())
            }
        }
    }
}

pub fn execute(worker: &str, intent: &str, payload: &Value, run_id: &str) -> Result<Value, String> {
    let context = load_run_context(run_id);
    match intent {
        "remember" => remember(worker, payload, run_id, &context),
        "memory-preferences" => set_user_preferences(worker, payload, &context),
        "forget" | "memory-correct" | "memory-disable" | "memory-enable" | "memory-pin"
        | "memory-unpin" => mutate(worker, intent, payload, &context),
        _ => Err(format!("unknown memory intent \"{intent}\"")),
    }
}

fn remember(
    worker: &str,
    payload: &Value,
    run_id: &str,
    context: &RunContext,
) -> Result<Value, String> {
    let policy = contextual_policy(worker, context);
    let text = payload
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("remember needs a non-empty note")?;
    if text.chars().count() > policy.max_note_chars {
        return Err(format!(
            "memory exceeds the {} character limit",
            policy.max_note_chars
        ));
    }
    if obvious_secret(text) {
        return Err("memory appears to contain a secret or credential".into());
    }
    let kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("fact");
    if !SUPPORTED_MEMORY_KINDS.contains(&kind) || !policy.allowed_kinds.iter().any(|k| k == kind) {
        return Err(format!("memory kind \"{kind}\" is not allowed"));
    }
    let basis = parse_basis(payload)?;
    let (scope, scope_id) = parse_scope(payload, context)?;
    if let Some(value) = payload.get("expires_at").and_then(Value::as_str) {
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
            .map_err(|_| "expires_at must be an RFC 3339 timestamp")?;
    }
    let existing = list(worker);
    if let Some(duplicate) = existing.iter().find(|n| {
        n.active()
            && n.scope == scope
            && n.scope_id == scope_id
            && n.kind == kind
            && n.note.trim().eq_ignore_ascii_case(text)
    }) {
        return Ok(
            json!({ "id": duplicate.id, "scope": duplicate.scope, "scope_id": duplicate.scope_id, "status": "already-remembered" }),
        );
    }
    let active_in_scope = existing
        .iter()
        .filter(|n| n.active() && n.scope == scope && n.scope_id == scope_id)
        .count();
    if active_in_scope >= policy.max_notes_per_scope {
        return Err(format!(
            "memory scope already has the maximum {} active notes",
            policy.max_notes_per_scope
        ));
    }
    let now = now_rfc3339();
    let source = MemorySource {
        channel_id: context.channel_scope_id(),
        message_id: context
            .message_id
            .as_deref()
            .map(|id| context.qualified(id)),
        author_id: context.user_scope_id(),
        run_id: Some(run_id.to_string()).filter(|s| !s.is_empty()),
        artifact: payload
            .get("artifact")
            .and_then(Value::as_str)
            .map(String::from),
    };
    let expires_at = effective_expiry(payload, &scope, context, &policy);
    let note = MemoryNote {
        id: new_id(),
        ts: now,
        scope,
        scope_id,
        kind: kind.to_string(),
        note: text.to_string(),
        basis,
        source,
        expires_at,
        pinned: false,
        disabled: false,
        forgotten: false,
        corrected_at: None,
    };
    let mut event = serde_json::to_value(&note).map_err(|e| e.to_string())?;
    event["op"] = json!("remember");
    append_event(worker, &event)?;
    Ok(
        json!({ "id": note.id, "scope": note.scope, "scope_id": note.scope_id, "status": "remembered" }),
    )
}

fn retention_expiry(days: u64) -> Option<String> {
    let days = i64::try_from(days).ok()?;
    time::OffsetDateTime::now_utc()
        .checked_add(time::Duration::days(days))?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

fn effective_expiry(
    payload: &Value,
    scope: &MemoryScope,
    context: &RunContext,
    policy: &MemoryPolicy,
) -> Option<String> {
    let format = &time::format_description::well_known::Rfc3339;
    let mut candidates: Vec<time::OffsetDateTime> = payload
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|s| time::OffsetDateTime::parse(s, format).ok())
        .into_iter()
        .collect();
    if let Some(value) = policy
        .max_retention_days
        .and_then(retention_expiry)
        .and_then(|s| time::OffsetDateTime::parse(&s, format).ok())
    {
        candidates.push(value);
    }
    if *scope == MemoryScope::Channel {
        if let Some(value) = context
            .channel_id
            .as_deref()
            .and_then(crate::channel::discord::channel_memory_retention_days)
            .and_then(retention_expiry)
            .and_then(|s| time::OffsetDateTime::parse(&s, format).ok())
        {
            candidates.push(value);
        }
    }
    candidates
        .into_iter()
        .min()
        .and_then(|at| at.format(format).ok())
}

fn set_user_preferences(worker: &str, payload: &Value, context: &RunContext) -> Result<Value, String> {
    let scope_id = context
        .user_scope_id()
        .ok_or("memory preferences require an active user")?;
    if payload
        .get("allow_inferred")
        .and_then(Value::as_bool)
        .is_none()
        && payload
            .get("cross_channel_recall")
            .and_then(Value::as_bool)
            .is_none()
    {
        return Err("memory preferences need allow_inferred or cross_channel_recall".into());
    }
    let current = user_settings(worker, &scope_id);
    let allow_inferred = payload
        .get("allow_inferred")
        .and_then(Value::as_bool)
        .unwrap_or(current.allow_inferred);
    let cross_channel_recall = payload
        .get("cross_channel_recall")
        .and_then(Value::as_bool)
        .unwrap_or(current.cross_channel_recall);
    let event = json!({
        "op": "user-settings",
        "ts": now_rfc3339(),
        "scope_id": scope_id,
        "allow_inferred": allow_inferred,
        "cross_channel_recall": cross_channel_recall,
    });
    append_event(worker, &event)?;
    Ok(json!({
        "status": "updated",
        "allow_inferred": allow_inferred,
        "cross_channel_recall": cross_channel_recall,
    }))
}

fn operation_name(intent: &str) -> Result<&'static str, String> {
    match intent {
        "forget" => Ok("forget"),
        "memory-correct" => Ok("correct"),
        "memory-disable" => Ok("disable"),
        "memory-enable" => Ok("enable"),
        "memory-pin" => Ok("pin"),
        "memory-unpin" => Ok("unpin"),
        _ => Err(format!("unknown memory mutation \"{intent}\"")),
    }
}

fn mutate(worker: &str, intent: &str, payload: &Value, context: &RunContext) -> Result<Value, String> {
    let id = payload
        .get("note_id")
        .and_then(Value::as_str)
        .ok_or("memory action needs note_id")?;
    let note = find(worker, id).ok_or_else(|| format!("no such memory {id}"))?;
    authorize_existing(&note, context)?;
    let op = operation_name(intent)?;
    let mut event = json!({ "op": op, "id": id, "ts": now_rfc3339() });
    if op == "correct" {
        let replacement = payload
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("memory-correct needs a non-empty note")?;
        let policy = load_policy(worker);
        if replacement.chars().count() > policy.max_note_chars || obvious_secret(replacement) {
            return Err("replacement memory is too long or appears to contain a secret".into());
        }
        event["note"] = json!(replacement);
    }
    append_event(worker, &event)?;
    Ok(json!({ "id": id, "status": op }))
}

/// Owner CLI mutation. Host access is already the admin boundary.
pub fn admin_mutate(
    worker: &str,
    op: &str,
    id: &str,
    replacement: Option<&str>,
) -> Result<(), String> {
    if find(worker, id).is_none() {
        return Err(format!("no such memory {id}"));
    }
    if !matches!(
        op,
        "forget" | "correct" | "disable" | "enable" | "pin" | "unpin"
    ) {
        return Err(format!("unknown memory operation \"{op}\""));
    }
    let mut event = json!({ "op": op, "id": id, "ts": now_rfc3339() });
    if op == "correct" {
        let text = replacement
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("correct needs replacement text")?;
        event["note"] = json!(text);
    }
    append_event(worker, &event)
}

/// Rewrite the event log to its current live state. This is deliberately an
/// owner-only operation used for retention/privacy erasure, not normal upkeep.
pub fn compact(worker: &str) -> Result<usize, String> {
    let _guard = lock_memory(worker)?;
    let path = memory_path(worker);
    let legacy = legacy_notes_path(worker);

    // Fold the legacy file into the primary log FIRST — durably — and only then
    // remove it. Dropping a legacy note's `forget` tombstone (below) while its
    // `remember` still lived in a not-yet-removed legacy file would resurrect a
    // forgotten note on the next read; merging first closes that window. A crash
    // mid-merge is safe: the events are idempotent under fold, and the tombstones
    // are still present until the rewrite completes.
    if legacy.exists() {
        if let Ok(text) = std::fs::read_to_string(&legacy) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                crate::statefile::append_line(&path, line).map_err(|e| e.to_string())?;
            }
        }
        std::fs::remove_file(&legacy).map_err(|e| e.to_string())?;
    }

    // Now the primary log is the whole truth; fold it and rewrite atomically.
    let notes: Vec<MemoryNote> = fold_events(&read_events(worker))
        .into_iter()
        .filter(|n| !n.forgotten)
        .collect();
    let settings: Vec<Value> = read_events(worker)
        .into_iter()
        .filter(|e| e.get("op").and_then(Value::as_str) == Some("user-settings"))
        .collect();

    let mut buf = String::new();
    for note in &notes {
        let mut event = serde_json::to_value(note).map_err(|e| e.to_string())?;
        event["op"] = json!("remember");
        buf.push_str(&event.to_string());
        buf.push('\n');
    }
    // Preserve only the latest effective settings for each user.
    let mut latest: HashMap<String, Value> = HashMap::new();
    for event in settings {
        if let Some(id) = event.get("scope_id").and_then(Value::as_str) {
            latest.insert(id.to_string(), event);
        }
    }
    for event in latest.into_values() {
        buf.push_str(&event.to_string());
        buf.push('\n');
    }
    // Atomic + fsynced: power loss can't leave a truncated or empty memory log.
    crate::statefile::write_atomic(&path, buf.as_bytes()).map_err(|e| e.to_string())?;
    Ok(notes.len())
}

/// Direct participant operation from a trusted channel adapter. Users may
/// manage their own notes; trusted channel participants may manage shared notes.
pub fn participant_mutate(
    worker: &str,
    op: &str,
    id: &str,
    replacement: Option<&str>,
    context: &RunContext,
) -> Result<(), String> {
    let note = find(worker, id).ok_or_else(|| format!("no such memory {id}"))?;
    authorize_existing(&note, context)?;
    match note.scope {
        MemoryScope::Worker => return Err("worker memory is admin-controlled".into()),
        MemoryScope::Channel if !context.trusted_participant() => {
            return Err("shared channel memory requires a trusted participant".into())
        }
        _ => {}
    }
    admin_mutate(worker, op, id, replacement)
}

fn expired(value: Option<&str>) -> bool {
    let Some(value) = value else { return false };
    let Ok(at) = time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    else {
        return false;
    };
    at <= time::OffsetDateTime::now_utc()
}

fn eligible(note: &MemoryNote, context: &RunContext, policy: &MemoryPolicy) -> bool {
    if !note.active() {
        return false;
    }
    if !SUPPORTED_MEMORY_KINDS.contains(&note.kind.as_str())
        || !policy.allowed_kinds.contains(&note.kind)
    {
        return false;
    }
    match note.scope {
        MemoryScope::Worker => true,
        MemoryScope::Channel => note.scope_id == context.channel_scope_id(),
        MemoryScope::User => {
            if !context.is_dm && !policy.user_memory_in_groups {
                return false;
            }
            if note.scope_id != context.user_scope_id() {
                return false;
            }
            policy.cross_channel_user_recall || note.source.channel_id == context.channel_scope_id()
        }
    }
}

#[cfg(test)]
fn select_for_recall(
    notes: &[MemoryNote],
    context: &RunContext,
    policy: &MemoryPolicy,
) -> Vec<MemoryNote> {
    let mut eligible: Vec<MemoryNote> = notes
        .iter()
        .filter(|n| eligible(n, context, policy))
        .cloned()
        .collect();
    eligible.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| {
                (b.basis == MemoryBasis::Explicit).cmp(&(a.basis == MemoryBasis::Explicit))
            })
            .then_with(|| b.ts.cmp(&a.ts))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut chars = 0usize;
    eligible
        .into_iter()
        .filter(|n| {
            let next = n.note.chars().count();
            if chars + next > policy.recall_char_budget {
                false
            } else {
                chars += next;
                true
            }
        })
        .take(policy.recall_max_notes)
        .collect()
}

/// Ranked notes eligible for the context compiler. The compiler owns final
/// rendering and may select fewer notes when the complete injected prompt (JSON
/// envelopes and all) reaches its budget.
#[derive(Debug, Clone)]
pub struct RecallCandidates {
    pub all: Vec<MemoryNote>,
    pub ranked: Vec<MemoryNote>,
    pub max_notes: usize,
    pub note_char_budget: usize,
    policy: MemoryPolicy,
}

pub fn recall_candidates(worker: &str, context: &RunContext) -> RecallCandidates {
    let policy = contextual_policy(worker, context);
    let all = list(worker);
    let mut ranked: Vec<MemoryNote> = if policy.enabled {
        all.iter()
            .filter(|note| eligible(note, context, &policy))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    ranked.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| {
                (b.basis == MemoryBasis::Explicit).cmp(&(a.basis == MemoryBasis::Explicit))
            })
            .then_with(|| b.ts.cmp(&a.ts))
            .then_with(|| a.id.cmp(&b.id))
    });
    RecallCandidates {
        all,
        ranked,
        max_notes: policy.recall_max_notes,
        note_char_budget: policy.recall_char_budget,
        policy,
    }
}

pub fn trace_compiled_recall(
    worker: &str,
    run_id: &str,
    context: &RunContext,
    candidates: &RecallCandidates,
    selected: &[MemoryNote],
) {
    trace_recall(
        worker,
        run_id,
        &candidates.all,
        selected,
        context,
        &candidates.policy,
    );
}

fn trace_recall(
    worker: &str,
    run_id: &str,
    all: &[MemoryNote],
    selected: &[MemoryNote],
    context: &RunContext,
    policy: &MemoryPolicy,
) {
    if run_id.is_empty() {
        return;
    }
    let selected_ids: Vec<&str> = selected.iter().map(|n| n.id.as_str()).collect();
    let candidates: Vec<Value> = all
        .iter()
        .map(|n| {
            let reason = if selected_ids.contains(&n.id.as_str()) {
                "selected"
            } else if !n.active() {
                n.status()
            } else if !eligible(n, context, policy) {
                "out-of-scope-or-private"
            } else {
                "rank-or-budget"
            };
            json!({ "id": n.id, "reason": reason })
        })
        .collect();
    let event = json!({
        "ts": now_rfc3339(),
        "worker": short_worker(worker),
        "context": context,
        "candidates": candidates,
        "selected": selected_ids,
    });
    let path = recall_trace_path(run_id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{event}");
    }
}

pub fn recall_trace(run_id: &str) -> Vec<Value> {
    std::fs::read_to_string(recall_trace_path(run_id))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Notes the current participant is allowed to ask about conversationally.
pub fn visible_to_current_actor(worker: &str, run_id: &str) -> Vec<MemoryNote> {
    let context = load_run_context(run_id);
    visible_in_context(worker, &context)
}

pub fn current_user_settings(worker: &str, run_id: &str) -> Option<UserMemorySettings> {
    let context = load_run_context(run_id);
    context.user_scope_id().map(|id| user_settings(worker, &id))
}

pub fn visible_in_context(worker: &str, context: &RunContext) -> Vec<MemoryNote> {
    list(worker)
        .into_iter()
        .filter(|n| {
            !n.forgotten
                && match n.scope {
                    MemoryScope::Worker => false,
                    MemoryScope::Channel => n.scope_id == context.channel_scope_id(),
                    MemoryScope::User => context.is_dm && n.scope_id == context.user_scope_id(),
                }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(
        id: &str,
        scope: MemoryScope,
        scope_id: Option<&str>,
        basis: MemoryBasis,
        text: &str,
    ) -> MemoryNote {
        MemoryNote {
            id: id.into(),
            ts: format!("2026-01-0{id}T00:00:00Z"),
            scope,
            scope_id: scope_id.map(String::from),
            kind: "fact".into(),
            note: text.into(),
            basis,
            source: MemorySource {
                channel_id: Some("discord:c1".into()),
                ..Default::default()
            },
            expires_at: None,
            pinned: false,
            disabled: false,
            forgotten: false,
            corrected_at: None,
        }
    }

    fn dm() -> RunContext {
        RunContext {
            provider: "discord".into(),
            channel_id: Some("c1".into()),
            user_id: Some("u1".into()),
            role: "trusted".into(),
            is_dm: true,
            ..Default::default()
        }
    }

    #[test]
    fn folds_corrections_and_tombstones() {
        let events = vec![
            json!({"op":"remember","id":"1","ts":"2026-01-01T00:00:00Z","scope":"user","scope_id":"discord:u1","kind":"fact","note":"old","basis":"explicit","source":{}}),
            json!({"op":"correct","id":"1","ts":"2026-01-02T00:00:00Z","note":"new"}),
            json!({"op":"pin","id":"1","ts":"2026-01-03T00:00:00Z"}),
            json!({"op":"forget","id":"1","ts":"2026-01-04T00:00:00Z"}),
        ];
        let out = fold_events(&events);
        assert_eq!(out[0].note, "new");
        assert!(out[0].pinned);
        assert!(out[0].forgotten);
    }

    #[test]
    fn dm_recall_combines_worker_channel_and_user() {
        let notes = vec![
            note("1", MemoryScope::Worker, None, MemoryBasis::Inferred, "worker"),
            note(
                "2",
                MemoryScope::Channel,
                Some("discord:c1"),
                MemoryBasis::Explicit,
                "channel",
            ),
            note(
                "3",
                MemoryScope::User,
                Some("discord:u1"),
                MemoryBasis::Explicit,
                "user",
            ),
            note(
                "4",
                MemoryScope::User,
                Some("discord:u2"),
                MemoryBasis::Explicit,
                "other",
            ),
        ];
        let selected = select_for_recall(&notes, &dm(), &MemoryPolicy::default());
        let texts: Vec<&str> = selected.iter().map(|n| n.note.as_str()).collect();
        assert!(texts.contains(&"worker"));
        assert!(texts.contains(&"channel"));
        assert!(texts.contains(&"user"));
        assert!(!texts.contains(&"other"));
    }

    #[test]
    fn group_context_excludes_user_memory_by_default() {
        let mut context = dm();
        context.is_dm = false;
        let notes = vec![note(
            "1",
            MemoryScope::User,
            Some("discord:u1"),
            MemoryBasis::Explicit,
            "private",
        )];
        assert!(select_for_recall(&notes, &context, &MemoryPolicy::default()).is_empty());
    }

    #[test]
    fn explicit_and_pinned_notes_rank_first() {
        let mut inferred = note(
            "1",
            MemoryScope::Worker,
            None,
            MemoryBasis::Inferred,
            "inferred",
        );
        inferred.ts = "2026-03-01T00:00:00Z".into();
        let explicit = note(
            "2",
            MemoryScope::Worker,
            None,
            MemoryBasis::Explicit,
            "explicit",
        );
        let mut pinned = note("3", MemoryScope::Worker, None, MemoryBasis::Inferred, "pinned");
        pinned.pinned = true;
        let selected = select_for_recall(
            &[inferred, explicit, pinned],
            &dm(),
            &MemoryPolicy::default(),
        );
        assert_eq!(selected[0].note, "pinned");
        assert_eq!(selected[1].note, "explicit");
    }

    #[test]
    fn supplied_subject_cannot_escape_active_context() {
        let payload = json!({"scope":"user","scope_id":"discord:other"});
        assert!(parse_scope(&payload, &dm()).is_err());
    }

    #[test]
    fn retention_expiry_is_valid_and_in_the_future() {
        let value = retention_expiry(7).unwrap();
        let parsed =
            time::OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339)
                .unwrap();
        assert!(parsed > time::OffsetDateTime::now_utc());
    }
}
