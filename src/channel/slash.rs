//! The slash-command surface — the same admin commands on every conversation
//! channel. Discord parses an interaction into a `SlashCall`; the terminal
//! parses a typed `/…` line. Execution and wording live here once, so the
//! surfaces cannot drift. Discord's registration (`discord::command_defs`)
//! mirrors the grammar table below.

use super::discord::{
    purpose_path, set_channel_memory, set_channel_memory_allowed_kinds,
    set_channel_memory_inferred_auto, set_channel_memory_retention_days, set_channel_mode,
    set_channel_trust,
};
use crate::worker::memory::RunContext;
use std::collections::HashMap;

pub struct SlashCall {
    pub cmd: String,
    pub sub: String,
    pub args: HashMap<String, String>,
}

impl SlashCall {
    fn arg(&self, name: &str) -> String {
        self.args.get(name).cloned().unwrap_or_default()
    }
}

/// The grammar: command, subcommand, named args in positional order (the last
/// one swallows the rest of the line), and what it does. `/help` and the
/// terminal parser both derive from this table.
const GRAMMAR: &[(&str, &str, &[&str], &str)] = &[
    ("help", "", &[], "these commands"),
    ("approvals", "ls", &[], "what is pending your approval"),
    ("approvals", "show", &["id"], "the exact action that would run"),
    ("approvals", "approve", &["id"], "approve and execute"),
    ("approvals", "deny", &["id"], "record the refusal"),
    ("task", "ls", &[], "tasks, newest first"),
    ("task", "add", &["prompt"], "file a task for this worker"),
    ("task", "show", &["id"], "one task: state, gates, prompt"),
    ("task", "requeue", &["id"], "put a stuck task back to waiting"),
    ("runs", "ls", &[], "this worker's recent sessions"),
    ("runs", "show", &["run"], "one session's record"),
    ("channel", "show", &[], "this channel's settings"),
    ("channel", "trust", &[], "participants here may administer"),
    ("channel", "untrust", &[], "participants here are content-only"),
    ("channel", "mode", &["mode"], "all = every message, mention = only when @mentioned"),
    ("channel", "memory", &["state"], "memory in this channel: on or off"),
    ("channel", "memory-inferred", &["state"], "inferred channel notes: auto or review"),
    ("channel", "memory-kinds", &["kinds"], "default, or comma-separated kinds"),
    ("channel", "memory-retention", &["days"], "default, or a number of days"),
    ("memory", "show", &[], "memories visible here"),
    ("memory", "ls", &["scope"], "notes by scope: worker | channel | user"),
    ("memory", "forget", &["id"], "forget a memory"),
    ("memory", "correct", &["id", "text"], "replace a memory's content"),
    ("purpose", "show", &[], "this channel's purpose"),
    ("purpose", "set", &["text"], "set this channel's purpose"),
    ("worker", "show", &[], "this worker: tasks, approvals, memory"),
    ("worker", "trust", &[], "per-action trust and earned history"),
    ("identity", "show", &[], "the worker's fixed identity"),
];

fn usage(cmd: &str, sub: &str, args: &[&str]) -> String {
    let mut s = format!("/{cmd}");
    if !sub.is_empty() {
        s.push(' ');
        s.push_str(sub);
    }
    for a in args {
        s.push_str(&format!(" <{a}>"));
    }
    s
}

pub fn help() -> String {
    let rows: Vec<(String, &str)> = GRAMMAR
        .iter()
        .map(|(cmd, sub, args, what)| (usage(cmd, sub, args), *what))
        .collect();
    let width = rows.iter().map(|(u, _)| u.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(u, what)| format!("{u:<width$}  {what}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Old command names that keep working (muscle memory, saved snippets). The
/// mechanism is still the gate; "approvals" is the same desk from the human
/// seat, so the alias maps one to the other.
fn canonical(cmd: &str) -> &str {
    match cmd {
        "gates" => "approvals",
        "queue" => "task",
        other => other,
    }
}

/// Parse a typed `/command …` line against the grammar.
pub fn parse(line: &str) -> Result<SlashCall, String> {
    let mut tokens = line.trim().trim_start_matches('/').split_whitespace();
    let Some(cmd) = tokens.next() else {
        return Err("Empty command — /help lists them.".into());
    };
    let cmd = canonical(cmd);
    let rows: Vec<&(&str, &str, &[&str], &str)> = GRAMMAR.iter().filter(|r| r.0 == cmd).collect();
    if rows.is_empty() {
        return Err(format!("Unknown command /{cmd} — /help lists them."));
    }
    let sub = if rows[0].1.is_empty() {
        String::new()
    } else {
        tokens.next().unwrap_or_default().to_string()
    };
    let Some((_, _, names, _)) = rows.iter().find(|r| r.1 == sub) else {
        let subs: Vec<&str> = rows.iter().map(|r| r.1).collect();
        return Err(format!("Usage: /{cmd} {}", subs.join("|")));
    };
    let mut args = HashMap::new();
    for (i, name) in names.iter().enumerate() {
        let value = if i + 1 == names.len() {
            tokens.by_ref().collect::<Vec<_>>().join(" ")
        } else {
            tokens.next().unwrap_or_default().to_string()
        };
        if value.is_empty() {
            return Err(format!("Usage: {}", usage(cmd, &sub, names)));
        }
        args.insert(name.to_string(), value);
    }
    Ok(SlashCall {
        cmd: cmd.to_string(),
        sub,
        args,
    })
}

/// Tab completion for a partial `/…` line with the cursor at `pos`. The slot
/// under the cursor decides what completes: slot 0 the command, slot 1 the
/// subcommand, later slots the argument's values — live ids read from the
/// same stores the commands answer from. Returns (replace-from, candidates).
pub fn complete(line: &str, pos: usize, worker: &str) -> (usize, Vec<String>) {
    let head = &line[..pos.min(line.len())];
    let indent = head.len() - head.trim_start().len();
    if !head[indent..].starts_with('/') {
        return (pos, Vec::new());
    }

    // Token starts, as byte offsets into `head`.
    let bytes = head.as_bytes();
    let mut tokens: Vec<(usize, &str)> = Vec::new();
    let mut i = indent;
    while i < head.len() {
        while i < head.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < head.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if start < i {
            tokens.push((start, &head[start..i]));
        }
    }

    // At a space the operator starts the next slot; otherwise the last token
    // is the partial word being completed.
    let (slot, prefix, from) = if head.ends_with(|c: char| c.is_ascii_whitespace()) {
        (tokens.len(), "", pos)
    } else {
        let (at, tok) = *tokens.last().expect("head starts with /");
        (tokens.len() - 1, tok, at)
    };

    let finish = |mut v: Vec<String>| -> (usize, Vec<String>) {
        v.retain(|c| c.starts_with(prefix));
        v.sort();
        v.dedup();
        (from, v)
    };

    match slot {
        0 => finish(GRAMMAR.iter().map(|r| format!("/{}", r.0)).collect()),
        1 => {
            let cmd = canonical(tokens[0].1.trim_start_matches('/'));
            finish(
                GRAMMAR
                    .iter()
                    .filter(|r| r.0 == cmd && !r.1.is_empty())
                    .map(|r| r.1.to_string())
                    .collect(),
            )
        }
        n => {
            let cmd = canonical(tokens[0].1.trim_start_matches('/'));
            let sub = tokens.get(1).map(|t| t.1).unwrap_or("");
            let Some(row) = GRAMMAR.iter().find(|r| r.0 == cmd && r.1 == sub) else {
                return (pos, Vec::new());
            };
            let Some(arg) = row.2.get(n - 2) else {
                return (pos, Vec::new());
            };
            finish(arg_values(cmd, sub, arg, worker))
        }
    }
}

/// Live values for an argument slot. Reading the stores on every TAB is fine
/// at interactive cadence.
fn arg_values(cmd: &str, sub: &str, arg: &str, worker: &str) -> Vec<String> {
    let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    match (cmd, sub, arg) {
        ("approvals", "show", "id") => crate::action::gate::list_all()
            .into_iter()
            .map(|g| g.id)
            .collect(),
        ("approvals", _, "id") => crate::action::gate::list_pending()
            .into_iter()
            .map(|g| g.id)
            .collect(),
        ("task", _, "id") => {
            let mut tasks = crate::work::tms::list_all();
            tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            tasks.into_iter().take(25).map(|t| t.id).collect()
        }
        ("runs", "show", "run") => crate::run::runlog::list()
            .into_iter()
            .take(25)
            .map(|r| r.id)
            .collect(),
        ("memory", "ls", "scope") => strs(&["worker", "channel", "user"]),
        ("memory", _, "id") => crate::worker::memory::list(worker)
            .into_iter()
            .map(|n| n.id)
            .collect(),
        ("channel", "mode", _) => strs(&["all", "mention"]),
        ("channel", "memory", _) => strs(&["on", "off"]),
        ("channel", "memory-inferred", _) => strs(&["auto", "review"]),
        ("channel", "memory-kinds", _) | ("channel", "memory-retention", _) => strs(&["default"]),
        _ => Vec::new(),
    }
}

fn role_rank(role: &str) -> u8 {
    match role {
        "host-op" | "admin" => 2,
        "trusted" => 1,
        _ => 0,
    }
}

/// Execute one command. `caller` is the attribution recorded on decisions,
/// already provider-qualified ("discord:jane", "term:manas").
pub async fn run(
    worker: &str,
    call: &SlashCall,
    channel_id: &str,
    memory_context: &RunContext,
    role: &str,
    caller: &str,
) -> String {
    let rank = role_rank(role);
    let denied = |need: &str| format!("Not permitted — {need} only (you are {role}).");

    match (canonical(call.cmd.as_str()), call.sub.as_str()) {
        ("help", _) => format!("Commands:\n{}", help()),
        ("approvals", "ls") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let g = crate::action::gate::list_pending();
            if g.is_empty() {
                "Nothing pending your approval.".into()
            } else {
                let lines: Vec<String> = g
                    .iter()
                    .map(|x| format!("• `{}` {} ({})", x.id, x.intent, x.worker))
                    .collect();
                format!("Pending your approval:\n{}", lines.join("\n"))
            }
        }
        ("approvals", "show") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let id = call.arg("id");
            match crate::cli::approvals::render_show(&id) {
                Ok(text) => text,
                Err(e) => format!("Could not show `{id}`: {e}"),
            }
        }
        ("approvals", "approve") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let id = call.arg("id");
            match crate::action::execute_gate(&id, caller, None).await {
                Ok(g) => format!("Approved `{}` ({}).", g.id, g.intent),
                Err(e) => format!("Could not approve `{id}`: {e}"),
            }
        }
        ("approvals", "deny") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let id = call.arg("id");
            match crate::action::deny_gate(&id, caller, None) {
                Ok(g) => format!("Denied `{}` ({}).", g.id, g.intent),
                Err(e) => format!("Could not deny `{id}`: {e}"),
            }
        }
        ("task", "ls") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let mut tasks = crate::work::tms::list_all();
            tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            let recurring = crate::work::tms::list_recurring();
            if tasks.is_empty() && recurring.is_empty() {
                "No tasks.".into()
            } else {
                let mut lines: Vec<String> = tasks
                    .iter()
                    .map(|t| {
                        let when = t
                            .scheduled_at
                            .as_deref()
                            .map(|at| format!(" (at {at})"))
                            .unwrap_or_default();
                        let deps = if t.depends_on.is_empty() { "" } else { " ⛓" };
                        format!(
                            "• `{}` [{}{deps}] {}{when}",
                            t.id,
                            t.state,
                            first_words(&t.prompt)
                        )
                    })
                    .collect();
                for r in &recurring {
                    lines.push(format!(
                        "↻ `{}` [{}] {}{}",
                        r.id,
                        r.schedule,
                        if r.system { "[system] " } else { "" },
                        first_words(&r.prompt)
                    ));
                }
                format!("Tasks:\n{}", lines.join("\n"))
            }
        }
        ("task", "add") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let prompt = call.arg("prompt");
            match crate::work::tms::add(
                worker,
                crate::work::tms::Draft {
                    prompt,
                    created_by: "user".into(),
                    standing: "owner".into(),
                    tags: crate::work::tms::Tags {
                        provider: Some(memory_context.provider.clone())
                            .filter(|p| !p.is_empty()),
                        channel: memory_context.channel_id.clone(),
                        user: memory_context.user_id.clone(),
                    },
                    ..Default::default()
                },
            ) {
                Ok(t) => format!(
                    "Queued `{}` for {} — it runs when the dispatcher picks it up.",
                    t.id, t.worker
                ),
                Err(e) => format!("Could not queue the task: {e}"),
            }
        }
        ("task", "show") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let id = call.arg("id");
            let tasks = crate::work::tms::list_all();
            let resolved = match crate::util::resolve_prefix(
                "task",
                &id,
                tasks.iter().map(|t| t.id.as_str()),
            ) {
                Ok(full) => Some(tasks.into_iter().find(|t| t.id == full).expect("resolved")),
                Err(_) => crate::work::tms::find(&id),
            };
            match resolved {
                None => format!("Could not show `{id}`: no such task"),
                Some(t) => {
                    let mut lines = vec![
                        format!("Task `{}` — {} [{}]", t.id, t.worker, t.state),
                        format!(
                            "filed by {} ({}) · ceiling {} min · updated {}",
                            t.created_by, t.standing, t.ceiling_min, t.updated_at
                        ),
                    ];
                    if let Some(at) = &t.scheduled_at {
                        lines.push(format!("scheduled {at}"));
                    }
                    if !t.depends_on.is_empty() {
                        lines.push(format!("depends on {}", t.depends_on.join(", ")));
                    }
                    if let Some(run) = &t.run_id {
                        lines.push(format!("run {run}"));
                    }
                    let gates = crate::action::gate::pending_for_task(&t.id);
                    if !gates.is_empty() {
                        lines.push(format!(
                            "pending approval: {}",
                            gates.iter().map(|g| g.id.as_str()).collect::<Vec<_>>().join(", ")
                        ));
                    }
                    lines.push(format!(
                        "prompt: {}",
                        crate::run::runlog::one_line(&t.prompt, 200)
                    ));
                    lines.join("\n")
                }
            }
        }
        ("task", "requeue") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let id = call.arg("id");
            match crate::work::tms::requeue(&id) {
                Ok(msg) => msg,
                Err(e) => format!("Could not requeue `{id}`: {e}"),
            }
        }
        ("runs", "ls") => {
            if rank < 2 {
                return denied("server admins");
            }
            let runs: Vec<String> = crate::run::runlog::list()
                .into_iter()
                .filter(|r| r.worker == worker)
                .take(10)
                .map(|r| format!("• `{}` [{}] {} started {}", r.id, r.state, r.kind, r.started_at))
                .collect();
            if runs.is_empty() {
                format!("No runs for {worker} yet.")
            } else {
                format!("{worker}'s recent sessions:\n{}", runs.join("\n"))
            }
        }
        ("runs", "show") => {
            if rank < 2 {
                return denied("server admins");
            }
            let id = call.arg("run");
            match crate::run::runlog::resolve(&id) {
                Err(e) => format!("Could not show `{id}`: {e}"),
                Ok(run) => {
                    let mut lines = vec![
                        format!("Run `{}` — {} [{}]", run.id, run.worker, run.state),
                        format!("kind {} · started {}", run.kind, run.started_at),
                    ];
                    if let Some(ended) = &run.ended_at {
                        lines.push(format!("ended {ended}"));
                    }
                    if let Some(task) = &run.task_id {
                        lines.push(format!("task {task}"));
                    }
                    if let Some(channel) = &run.channel_id {
                        lines.push(format!("channel {channel}"));
                    }
                    lines.push(format!(
                        "full detail: roster server runs show {}",
                        run.id
                    ));
                    lines.join("\n")
                }
            }
        }
        ("channel", "show") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let s = super::discord::channel_settings_all()
                .get(channel_id)
                .cloned()
                .unwrap_or_default();
            let purpose = std::fs::read_to_string(purpose_path(channel_id))
                .ok()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
            let described = crate::cli::channel::describe(channel_id);
            let place = if described == "-" {
                String::new()
            } else {
                format!(" — {described}")
            };
            format!(
                "This channel ({channel_id}{place}):\n\
                 trust: **{}** · mode: **{}**\n\
                 memory: **{}** (inferred {}, kinds {}, retention {})\n\
                 purpose: {}",
                if s.trusted { "trusted" } else { "untrusted" },
                s.mode,
                if s.memory_enabled { "on" } else { "off" },
                if s.memory_inferred_auto { "auto" } else { "review" },
                s.memory_allowed_kinds
                    .as_ref()
                    .map(|v| v.join(","))
                    .unwrap_or_else(|| "default".into()),
                s.memory_retention_days
                    .map(|n| format!("{n} days"))
                    .unwrap_or_else(|| "default".into()),
                purpose.as_deref().unwrap_or("(none — /purpose set)")
            )
        }
        ("channel", "trust") => {
            if rank < 2 {
                return denied("server admins");
            }
            if let Err(e) = set_channel_trust(channel_id, true) {
                return format!("Couldn't update this channel: {e}");
            }
            "This channel's participants are now **trusted** — they can administer, and I'll reply here without a gate.".into()
        }
        ("channel", "untrust") => {
            if rank < 2 {
                return denied("server admins");
            }
            if let Err(e) = set_channel_trust(channel_id, false) {
                return format!("Couldn't update this channel: {e}");
            }
            "This channel's participants are now **untrusted** — they can talk to me, but not administer, and my replies here will be gated.".into()
        }
        ("channel", "mode") => {
            if rank < 2 {
                return denied("server admins");
            }
            let mode = call.arg("mode");
            if mode != "all" && mode != "mention" {
                return "Mode must be `all` or `mention`.".into();
            }
            if let Err(e) = set_channel_mode(channel_id, &mode) {
                return format!("Couldn't update this channel: {e}");
            }
            if mode == "all" {
                "I'll now read **every** message here and decide whether to respond.".into()
            } else {
                "I'll now respond here **only when @mentioned**.".into()
            }
        }
        ("channel", "memory") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let state = call.arg("state");
            let enabled = match state.as_str() {
                "on" => true,
                "off" => false,
                _ => return "Memory state must be `on` or `off`.".into(),
            };
            if let Err(e) = set_channel_memory(channel_id, enabled) {
                return format!("Couldn't update this channel: {e}");
            }
            format!(
                "Memory is now **{}** in this channel.",
                if enabled { "on" } else { "off" }
            )
        }
        ("channel", "memory-inferred") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let state = call.arg("state");
            let enabled = match state.as_str() {
                "auto" => true,
                "review" => false,
                _ => return "Inferred memory must be `auto` or `review`.".into(),
            };
            if let Err(e) = set_channel_memory_inferred_auto(channel_id, enabled) {
                return format!("Couldn't update this channel: {e}");
            }
            format!(
                "Inferred channel memories now require **{}**.",
                if enabled { "no review" } else { "review" }
            )
        }
        ("channel", "memory-kinds") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let value = call.arg("kinds");
            match crate::cli::channel::parse_memory_kinds(&value) {
                Ok(kinds) => {
                    if let Err(e) = set_channel_memory_allowed_kinds(channel_id, kinds.clone()) {
                        return format!("Couldn't update this channel: {e}");
                    }
                    format!(
                        "Channel memory kinds: **{}**.",
                        kinds
                            .map(|v| v.join(", "))
                            .unwrap_or_else(|| "default".into())
                    )
                }
                Err(e) => format!("Could not set memory kinds: {e}"),
            }
        }
        ("channel", "memory-retention") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let value = call.arg("days");
            let days = if value == "default" {
                None
            } else {
                match value.parse::<u64>().ok().filter(|n| *n > 0) {
                    Some(days) => Some(days),
                    None => {
                        return "Retention must be `default` or a positive number of days.".into()
                    }
                }
            };
            if let Err(e) = set_channel_memory_retention_days(channel_id, days) {
                return format!("Couldn't update this channel: {e}");
            }
            format!(
                "Channel memory retention: **{}**.",
                days.map(|n| format!("{n} days"))
                    .unwrap_or_else(|| "default".into())
            )
        }
        ("memory", "show") => {
            let notes = crate::worker::memory::visible_in_context(worker, memory_context);
            if notes.is_empty() {
                "No visible memories about you or this channel.".into()
            } else {
                let lines: Vec<String> = notes
                    .iter()
                    .take(20)
                    .map(|n| {
                        format!(
                            "• `{}` [{} / {}] {}",
                            n.id,
                            n.scope.as_str(),
                            n.status(),
                            n.note
                        )
                    })
                    .collect();
                format!("Visible memories:\n{}", lines.join("\n"))
            }
        }
        ("memory", "ls") => {
            if rank < 2 {
                return denied("server admins");
            }
            let scope = call.arg("scope");
            if !matches!(scope.as_str(), "worker" | "channel" | "user") {
                return "Scope must be `worker`, `channel`, or `user`.".into();
            }
            // Contextual: channel = this channel's notes, user = yours.
            let scope_id = match scope.as_str() {
                "channel" => memory_context.channel_scope_id(),
                "user" => memory_context.user_scope_id(),
                _ => None,
            };
            let notes: Vec<_> = crate::worker::memory::list(worker)
                .into_iter()
                .filter(|n| n.scope.as_str() == scope)
                .filter(|n| {
                    scope_id
                        .as_deref()
                        .map(|id| n.scope_id.as_deref() == Some(id))
                        .unwrap_or(true)
                })
                .collect();
            if notes.is_empty() {
                format!("No {scope}-scope memories here.")
            } else {
                let total = notes.len();
                let lines: Vec<String> = notes
                    .iter()
                    .take(20)
                    .map(|n| format!("• `{}` [{}] {}", n.id, n.status(), n.note))
                    .collect();
                let more = if total > 20 {
                    format!("\n… {} more: roster worker memory ls {worker}", total - 20)
                } else {
                    String::new()
                };
                format!("{total} {scope}-scope note(s):\n{}{more}", lines.join("\n"))
            }
        }
        ("memory", "forget") => {
            let id = call.arg("id");
            match crate::worker::memory::participant_mutate(worker, "forget", &id, None, memory_context)
            {
                Ok(()) => format!("Forgot `{id}`."),
                Err(e) => format!("Could not forget `{id}`: {e}"),
            }
        }
        ("memory", "correct") => {
            let id = call.arg("id");
            let text = call.arg("text");
            match crate::worker::memory::participant_mutate(
                worker,
                "correct",
                &id,
                Some(&text),
                memory_context,
            ) {
                Ok(()) => format!("Corrected `{id}`."),
                Err(e) => format!("Could not correct `{id}`: {e}"),
            }
        }
        ("purpose", "show") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            match std::fs::read_to_string(purpose_path(channel_id)) {
                Ok(p) if !p.trim().is_empty() => {
                    format!("This channel's purpose:\n```\n{}\n```", p.trim())
                }
                _ => "This channel has no purpose set yet. Set one with `/purpose set`.".into(),
            }
        }
        ("purpose", "set") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let text = call.arg("text");
            let path = purpose_path(channel_id);
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            match std::fs::write(&path, format!("{}\n", text.trim())) {
                Ok(()) => "This channel's purpose is updated. It'll shape how I act here from your next message.".into(),
                Err(e) => format!("Could not set purpose: {e}"),
            }
        }
        ("worker", "show") => {
            if rank < 2 {
                return denied("server admins");
            }
            let mut by_state: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for t in crate::work::tms::list_all().into_iter().filter(|t| t.worker == worker) {
                *by_state.entry(t.state).or_insert(0) += 1;
            }
            let queue_line = if by_state.is_empty() {
                "empty".to_string()
            } else {
                by_state
                    .iter()
                    .map(|(state, n)| format!("{n} {state}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let gates = crate::action::gate::for_worker(worker)
                .iter()
                .filter(|g| g.state == "pending")
                .count();
            format!(
                "{worker}\ntasks: {queue_line}\npending approval: {gates}\nmemory: {} note(s)\nfull detail: roster worker show {worker}",
                crate::worker::memory::list(worker).len()
            )
        }
        ("worker", "trust") => {
            if rank < 2 {
                return denied("server admins");
            }
            let subject = format!("org/{worker}");
            let policy = crate::action::load_action_policy();
            let lines: Vec<String> = policy
                .actions
                .iter()
                .filter(|g| crate::gateway::scope::applies(&g.scope, &subject))
                .map(|grant| {
                    let (executed, denied_n) = crate::action::gate::history(worker, &grant.name);
                    format!(
                        "• {} (default {}) — {} executed, {} denied",
                        grant.name, grant.trust, executed, denied_n
                    )
                })
                .collect();
            if lines.is_empty() {
                format!("{worker} has no action grants — it can propose nothing.")
            } else {
                format!(
                    "{worker}'s action trust:\n{}\nrules and promotion: roster worker trust {worker}",
                    lines.join("\n")
                )
            }
        }
        ("identity", "show") => {
            if rank < 2 {
                return denied("server admins");
            }
            match std::fs::read_to_string(crate::run::boxed::identity_path(worker)) {
                Ok(p) if !p.trim().is_empty() => {
                    format!("{worker}'s identity:\n```\n{}\n```", p.trim())
                }
                _ => format!("{worker} has no identity.md set."),
            }
        }
        _ => "Unknown command — /help lists them.".into(),
    }
}

fn first_words(s: &str) -> String {
    let s = s.replace('\n', " ");
    // Char-safe truncation: a byte slice at 60 panics on multibyte input and
    // takes down the listener task.
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(60).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_command_names_still_parse() {
        assert_eq!(canonical("gates"), "approvals");
        assert!(parse("/gates ls").is_ok());
        assert!(parse("/queue ls").is_ok());
    }

    #[test]
    fn completes_commands_subcommands_and_values() {
        // slot 0: the command, '/' included in the replaced range
        let (from, c) = complete("/ap", 3, "w");
        assert_eq!((from, c), (0, vec!["/approvals".to_string()]));
        // slot 1: subcommands of the typed command
        let (_, c) = complete("/purpose s", 10, "w");
        assert_eq!(c, vec!["set".to_string(), "show".to_string()]);
        // a space starts the next slot
        let (from, c) = complete("/memory ls ", 11, "w");
        assert_eq!(from, 11);
        assert_eq!(c, vec!["channel".to_string(), "user".to_string(), "worker".to_string()]);
        // slot 2: argument values, prefix-filtered
        let (from, c) = complete("/channel mode a", 15, "w");
        assert_eq!((from, c), (14, vec!["all".to_string()]));
        // not a slash line → nothing
        assert!(complete("hello /wor", 10, "w").1.is_empty());
        // beyond the last argument → nothing
        assert!(complete("/gates ls extra ", 16, "w").1.is_empty());
    }

    #[test]
    fn first_words_is_char_safe_on_multibyte() {
        // A multibyte char straddling byte 60 must not panic.
        let s = "é".repeat(80); // 160 bytes, 80 chars; byte 60 splits a char
        let out = first_words(&s);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 61); // 60 chars + ellipsis
        // Short strings pass through untouched.
        assert_eq!(first_words("hello"), "hello");
    }
}
