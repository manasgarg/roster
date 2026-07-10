//! Discord — the REST client for outbound messages and the gateway (WebSocket)
//! client for inbound. The trusted-side code holds the bot token (from the
//! vault); the box never does. Base URL is overridable via DISCORD_API_BASE so
//! the outbound executor can be tested against a mock.

use crate::util::{now_rfc3339, root};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as Ws;

fn base() -> String {
    std::env::var("DISCORD_API_BASE").unwrap_or_else(|_| "https://discord.com/api/v10".into())
}

/// Post a message to a channel. Returns the created message id.
pub async fn post_message(token: &str, channel_id: &str, text: &str) -> Result<String, String> {
    let res = reqwest::Client::new()
        .post(format!("{}/channels/{channel_id}/messages", base()))
        .header("authorization", format!("Bot {token}"))
        .json(&json!({ "content": text }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("discord POST message {status}: {body}"));
    }
    Ok(body.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string())
}

/// Open (or fetch) a DM channel with a user. Returns the DM channel id.
pub async fn open_dm(token: &str, user_id: &str) -> Result<String, String> {
    let res = reqwest::Client::new()
        .post(format!("{}/users/@me/channels", base()))
        .header("authorization", format!("Bot {token}"))
        .json(&json!({ "recipient_id": user_id }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("discord open DM {status}: {body}"));
    }
    body.get("id").and_then(|v| v.as_str()).map(String::from).ok_or_else(|| "DM channel had no id".to_string())
}

// ── inbound: the gateway (WebSocket) client ───────────────────────────────────

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
// Intents: GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT.
const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);
const PERM_ADMINISTRATOR: u64 = 0x8;

/// A guild's data we cache from GUILD_CREATE, to resolve a message author's role.
#[derive(Default, Clone)]
struct Guild {
    name: String,
    owner_id: String,
    everyone_perms: u64,
    role_perms: HashMap<String, u64>,
    channels: usize,
}

struct GwError {
    fatal: bool,
    msg: String,
}

/// Run the gateway for one worker: dial out, identify, and dispatch events.
/// Reconnects on transient errors; stops on fatal ones (bad token / disallowed
/// intent).
pub async fn run_gateway(worker: &str, token: &str) {
    loop {
        match connect_once(worker, token).await {
            Ok(()) => {}
            Err(e) if e.fatal => {
                eprintln!("discord gateway: {} — stopping.", e.msg);
                return;
            }
            Err(e) => {
                eprintln!("discord gateway: {} — reconnecting in 5s", e.msg);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn connect_once(worker: &str, token: &str) -> Result<(), GwError> {
    let transient = |m: String| GwError { fatal: false, msg: m };
    let (ws, _) = tokio_tungstenite::connect_async(GATEWAY_URL).await.map_err(|e| transient(format!("connect: {e}")))?;
    let (mut sink, mut stream) = ws.split();

    // One writer task owns the sink; heartbeat + identify send through this channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ws>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    let seq: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let mut heartbeat: Option<tokio::task::JoinHandle<()>> = None;
    let mut bot_id = String::new();
    let mut app_id = String::new();
    let mut guilds: HashMap<String, Guild> = HashMap::new();

    let result = loop {
        let Some(msg) = stream.next().await else { break Err(transient("stream ended".into())) };
        let msg = match msg {
            Ok(m) => m,
            Err(e) => break Err(transient(format!("read: {e}"))),
        };
        let text = match msg {
            Ws::Text(t) => t,
            Ws::Ping(p) => {
                let _ = tx.send(Ws::Pong(p));
                continue;
            }
            Ws::Close(frame) => {
                let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                // 4004 auth failed, 4013/4014 (dis)allowed intents — don't spin.
                if code == 4014 || code == 4013 {
                    break Err(GwError { fatal: true, msg: "a privileged intent (MESSAGE CONTENT) isn't enabled — Developer Portal → Bot → Privileged Gateway Intents".into() });
                }
                if code == 4004 {
                    break Err(GwError { fatal: true, msg: "authentication failed — bad bot token".into() });
                }
                break Err(transient(format!("closed ({code})")));
            }
            _ => continue,
        };
        let v: Value = match serde_json::from_str(text.as_str()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(s) = v["s"].as_i64() {
            *seq.lock().unwrap() = Some(s);
        }
        match v["op"].as_i64().unwrap_or(-1) {
            10 => {
                // HELLO → start heartbeat, then IDENTIFY.
                let interval = v["d"]["heartbeat_interval"].as_u64().unwrap_or(45_000);
                let (tx2, seq2) = (tx.clone(), seq.clone());
                heartbeat = Some(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(interval / 2)).await; // jitter
                    loop {
                        let s = *seq2.lock().unwrap();
                        if tx2.send(Ws::text(json!({ "op": 1, "d": s }).to_string())).is_err() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(interval)).await;
                    }
                }));
                let identify = json!({ "op": 2, "d": {
                    "token": token, "intents": INTENTS,
                    "properties": { "os": "linux", "browser": "roster", "device": "roster" },
                }});
                let _ = tx.send(Ws::text(identify.to_string()));
            }
            1 => {
                let s = *seq.lock().unwrap();
                let _ = tx.send(Ws::text(json!({ "op": 1, "d": s }).to_string()));
            }
            7 | 9 => break Err(transient(format!("gateway asked to reconnect (op {})", v["op"]))),
            0 => {
                let d = &v["d"];
                match v["t"].as_str().unwrap_or("") {
                    "READY" => {
                        bot_id = d["user"]["id"].as_str().unwrap_or("").to_string();
                        app_id = d["application"]["id"].as_str().unwrap_or("").to_string();
                        eprintln!("discord: connected as {} ({bot_id})", d["user"]["username"].as_str().unwrap_or("?"));
                    }
                    "GUILD_CREATE" => {
                        let g = ingest_guild(d);
                        eprintln!("discord: guild \"{}\" — {} channels visible", g.name, g.channels);
                        let guild_id = d["id"].as_str().unwrap_or("").to_string();
                        register_commands(&app_id, &guild_id, token).await;
                        guilds.insert(guild_id, g);
                    }
                    "MESSAGE_CREATE" => handle_message(worker, d, &bot_id, &guilds, token).await,
                    "INTERACTION_CREATE" => handle_interaction(worker, d, &guilds, &app_id).await,
                    _ => {}
                }
            }
            _ => {}
        }
    };

    if let Some(h) = heartbeat {
        h.abort();
    }
    writer.abort();
    result
}

fn ingest_guild(d: &Value) -> Guild {
    let mut role_perms = HashMap::new();
    let mut everyone_perms = 0;
    let guild_id = d["id"].as_str().unwrap_or("");
    if let Some(roles) = d["roles"].as_array() {
        for r in roles {
            let id = r["id"].as_str().unwrap_or("");
            let perms = r["permissions"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            if id == guild_id {
                everyone_perms = perms; // @everyone role id == guild id
            }
            role_perms.insert(id.to_string(), perms);
        }
    }
    Guild {
        name: d["name"].as_str().unwrap_or("").to_string(),
        owner_id: d["owner_id"].as_str().unwrap_or("").to_string(),
        everyone_perms,
        role_perms,
        channels: d["channels"].as_array().map(|c| c.len()).unwrap_or(0),
    }
}

/// A message author's authority in this context. Channel trust (trusted vs
/// untrusted participants) lands with the slash-command increment; until then a
/// non-admin guild member is untrusted, a DM party is trusted.
fn resolve_role(d: &Value, guilds: &HashMap<String, Guild>) -> &'static str {
    let Some(guild_id) = d["guild_id"].as_str() else {
        return "trusted"; // DM
    };
    let author_id = d["author"]["id"].as_str().unwrap_or("");
    if let Some(g) = guilds.get(guild_id) {
        if author_id == g.owner_id {
            return "admin";
        }
        let mut perms = g.everyone_perms;
        if let Some(roles) = d["member"]["roles"].as_array() {
            for r in roles {
                if let Some(rid) = r.as_str() {
                    perms |= g.role_perms.get(rid).copied().unwrap_or(0);
                }
            }
        }
        if perms & PERM_ADMINISTRATOR != 0 {
            return "admin";
        }
    }
    // Non-admin in a guild channel: trusted only if an admin marked the channel so.
    if channel_trusted(d["channel_id"].as_str().unwrap_or("")) {
        "trusted"
    } else {
        "untrusted"
    }
}

async fn handle_message(worker: &str, d: &Value, bot_id: &str, guilds: &HashMap<String, Guild>, token: &str) {
    // Never react to bots (including ourselves) — avoids reply loops.
    if d["author"]["bot"].as_bool().unwrap_or(false) {
        return;
    }
    let channel_id = d["channel_id"].as_str().unwrap_or("");
    if channel_id.is_empty() {
        return;
    }
    let is_dm = d["guild_id"].as_str().is_none();
    if is_dm {
        set_channel_trust(channel_id, true); // DMs are always trusted (1:1, sought-out)
    }
    let role = resolve_role(d, guilds);
    let author = d["author"]["username"].as_str().unwrap_or("?");
    let content = d["content"].as_str().unwrap_or("");

    // Persist to the channel's history and download any attachments.
    let record = json!({
        "ts": now_rfc3339(),
        "author_id": d["author"]["id"].as_str().unwrap_or(""),
        "author": author, "role": role, "content": content,
        "attachments": d["attachments"].as_array().map(|a| a.iter().filter_map(|x| x["filename"].as_str()).collect::<Vec<_>>()).unwrap_or_default(),
    });
    persist_message(channel_id, &record);
    download_attachments(channel_id, &d["attachments"]).await;

    // Wake rule: a DM, an @mention, or a channel in "all" mode. In "mention"
    // mode ambient messages are persisted but don't spawn a run.
    let mentioned = d["mentions"].as_array().map(|m| m.iter().any(|u| u["id"].as_str() == Some(bot_id))).unwrap_or(false);
    if !(is_dm || mentioned || channel_mode(channel_id) == "all") {
        return;
    }

    // Deliver to the channel's warm session (or start one). Governance is
    // unchanged — the session box's actions route through the gateway. A brief
    // hint tells the worker whether it was directly addressed.
    let hint = if is_dm || mentioned {
        ""
    } else if distinct_human_authors(channel_id) <= 1 {
        " [you're the only other person here — reply]"
    } else {
        " [group chat; you were not directly addressed — reply only if useful]"
    };
    let text = format!("{content}{hint}");
    let context = crate::memory::RunContext {
        provider: "discord".into(),
        channel_id: Some(channel_id.to_string()),
        user_id: d["author"]["id"].as_str().map(String::from),
        message_id: d["id"].as_str().map(String::from),
        role: role.to_string(),
        is_dm,
    };
    eprintln!("discord: {author} ({role}) in {channel_id} → session");
    route_to_session(
        worker,
        channel_id,
        author.to_string(),
        text,
        context,
        token,
    )
    .await;
}

// ── conversation sessions: one warm box per active channel ────────────────────

fn sessions() -> &'static Mutex<HashMap<String, tokio::sync::mpsc::Sender<crate::cmd::run_box::SessionMessage>>> {
    static S: OnceLock<Mutex<HashMap<String, tokio::sync::mpsc::Sender<crate::cmd::run_box::SessionMessage>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

const SESSION_IDLE_SECS: u64 = 90;

/// Deliver a message to the channel's live session, or start a new one. A live
/// session keeps the box warm across messages; it exits on idle (its sender then
/// reads closed, and the next message starts a fresh one).
async fn route_to_session(
    worker: &str,
    channel_id: &str,
    author_label: String,
    text: String,
    context: crate::memory::RunContext,
    token: &str,
) {
    let start_context = context.clone();
    let message = crate::cmd::run_box::SessionMessage {
        text,
        author_label,
        context,
    };
    let delivered = {
        let map = sessions().lock().unwrap();
        match map.get(channel_id) {
            Some(tx) => tx.try_send(crate::cmd::run_box::SessionMessage {
                text: message.text.clone(),
                author_label: message.author_label.clone(),
                context: message.context.clone(),
            }).is_ok(),
            None => false,
        }
    };
    spawn_typing(channel_id, token);
    if delivered {
        return;
    }
    // Start a new session (clears any stale closed sender).
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::cmd::run_box::SessionMessage>(64);
    let _ = tx.try_send(message);
    sessions().lock().unwrap().insert(channel_id.to_string(), tx);
    let (w, run_id) = (worker.to_string(), crate::cmd::run_box::new_run_id());
    tokio::spawn(async move {
        if let Err(e) = crate::cmd::run_box::run_session(
            &w,
            &run_id,
            crate::context::RunSurface::DiscordSession,
            start_context,
            rx,
            SESSION_IDLE_SECS,
        )
        .await
        {
            eprintln!("discord session error: {e}");
        }
        // On exit, the map's sender is now closed; the next message replaces it.
    });
}

/// Show the typing indicator for a while after a message (the reply clears it).
fn spawn_typing(channel_id: &str, token: &str) {
    let (ch, tok) = (channel_id.to_string(), token.to_string());
    tokio::spawn(async move {
        for _ in 0..4 {
            trigger_typing(&ch, &tok).await;
            tokio::time::sleep(Duration::from_secs(8)).await;
        }
    });
}

async fn trigger_typing(channel_id: &str, token: &str) {
    let _ = reqwest::Client::new()
        .post(format!("{}/channels/{channel_id}/typing", base()))
        .header("authorization", format!("Bot {token}"))
        .send()
        .await;
}

// ── slash commands (the admin surface) ────────────────────────────────────────

/// Command definitions registered per guild (instant, unlike global). Scoped to
/// what's safe: the approval desk, the queue, and channel trust.
fn command_defs() -> Value {
    json!([
        { "name": "gates", "description": "Roster approval desk", "options": [
            { "type": 1, "name": "ls", "description": "List pending gates" },
            { "type": 1, "name": "approve", "description": "Approve a gate", "options": [{ "type": 3, "name": "id", "description": "Gate id", "required": true }] },
            { "type": 1, "name": "deny", "description": "Deny a gate", "options": [{ "type": 3, "name": "id", "description": "Gate id", "required": true }] }
        ]},
        { "name": "queue", "description": "Roster task queue", "options": [
            { "type": 1, "name": "ls", "description": "List tasks" }
        ]},
        { "name": "channel", "description": "Channel settings", "options": [
            { "type": 1, "name": "trust", "description": "Mark this channel's participants trusted" },
            { "type": 1, "name": "untrust", "description": "Mark this channel's participants untrusted" },
            { "type": 1, "name": "mode", "description": "How the worker wakes here", "options": [
                { "type": 3, "name": "mode", "description": "all = every message, mention = only when @mentioned", "required": true,
                  "choices": [{ "name": "all", "value": "all" }, { "name": "mention", "value": "mention" }] }
            ]},
            { "type": 1, "name": "memory", "description": "Enable or disable memory in this channel", "options": [
                { "type": 3, "name": "state", "description": "on or off", "required": true,
                  "choices": [{ "name": "on", "value": "on" }, { "name": "off", "value": "off" }] }
            ]},
            { "type": 1, "name": "memory-inferred", "description": "Choose whether inferred channel notes need review", "options": [
                { "type": 3, "name": "state", "description": "auto or review", "required": true,
                  "choices": [{ "name": "auto", "value": "auto" }, { "name": "review", "value": "review" }] }
            ]},
            { "type": 1, "name": "memory-kinds", "description": "Limit memory kinds in this channel", "options": [
                { "type": 3, "name": "kinds", "description": "default or comma-separated kinds", "required": true }
            ]},
            { "type": 1, "name": "memory-retention", "description": "Shorten channel memory retention", "options": [
                { "type": 3, "name": "days", "description": "default or a positive number of days", "required": true }
            ]}
        ]},
        { "name": "memory", "description": "Inspect or correct scoped memory", "options": [
            { "type": 1, "name": "show", "description": "Show your and this channel's visible memories" },
            { "type": 1, "name": "forget", "description": "Forget a memory", "options": [
                { "type": 3, "name": "id", "description": "Memory id", "required": true }
            ]},
            { "type": 1, "name": "correct", "description": "Correct a memory", "options": [
                { "type": 3, "name": "id", "description": "Memory id", "required": true },
                { "type": 3, "name": "text", "description": "Complete corrected note", "required": true }
            ]}
        ]},
        { "name": "purpose", "description": "This channel's purpose for the worker", "options": [
            { "type": 1, "name": "show", "description": "Show this channel's purpose" },
            { "type": 1, "name": "set", "description": "Set this channel's purpose", "options": [{ "type": 3, "name": "text", "description": "The purpose", "required": true }] }
        ]},
        { "name": "identity", "description": "The worker's fixed identity", "options": [
            { "type": 1, "name": "show", "description": "Show the worker's identity" }
        ]}
    ])
}

async fn register_commands(app_id: &str, guild_id: &str, token: &str) {
    if app_id.is_empty() {
        return;
    }
    let res = reqwest::Client::new()
        .put(format!("{}/applications/{app_id}/guilds/{guild_id}/commands", base()))
        .header("authorization", format!("Bot {token}"))
        .json(&command_defs())
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("discord: register commands → {} (guild {guild_id})", r.status()),
        Err(e) => eprintln!("discord: register commands failed: {e}"),
    }
}

fn role_rank(role: &str) -> u8 {
    match role {
        "host-op" | "admin" => 2,
        "trusted" => 1,
        _ => 0,
    }
}

/// The caller's role for an interaction. Discord supplies the member's computed
/// permissions directly, so we don't recompute from roles here.
fn interaction_role(d: &Value, guilds: &HashMap<String, Guild>) -> &'static str {
    let Some(guild_id) = d["guild_id"].as_str() else {
        return "trusted"; // DM
    };
    let perms = d["member"]["permissions"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    if perms & PERM_ADMINISTRATOR != 0 {
        return "admin";
    }
    if let Some(g) = guilds.get(guild_id) {
        if d["member"]["user"]["id"].as_str() == Some(g.owner_id.as_str()) {
            return "admin";
        }
    }
    if channel_trusted(d["channel_id"].as_str().unwrap_or("")) {
        "trusted"
    } else {
        "untrusted"
    }
}

async fn handle_interaction(worker: &str, d: &Value, guilds: &HashMap<String, Guild>, app_id: &str) {
    if d["type"].as_i64().unwrap_or(0) != 2 {
        return; // only APPLICATION_COMMAND
    }
    let interaction_id = d["id"].as_str().unwrap_or("");
    let itoken = d["token"].as_str().unwrap_or("");
    // Ack immediately (deferred, ephemeral) — the interaction token, not bot auth.
    let _ = reqwest::Client::new()
        .post(format!("{}/interactions/{interaction_id}/{itoken}/callback", base()))
        .json(&json!({ "type": 5, "data": { "flags": 64 } }))
        .send()
        .await;

    let role = interaction_role(d, guilds);
    let caller = d["member"]["user"]["username"].as_str().or_else(|| d["user"]["username"].as_str()).unwrap_or("someone");
    let text = run_command(worker, d, role, caller).await;

    // Fill in the deferred response (webhook uses the interaction token).
    let _ = reqwest::Client::new()
        .patch(format!("{}/webhooks/{app_id}/{itoken}/messages/@original", base()))
        .json(&json!({ "content": text }))
        .send()
        .await;
}

async fn run_command(worker: &str, d: &Value, role: &str, caller: &str) -> String {
    let data = &d["data"];
    let cmd = data["name"].as_str().unwrap_or("");
    let sub = data["options"][0]["name"].as_str().unwrap_or("");
    let channel_id = d["channel_id"].as_str().unwrap_or("");
    let caller_id = d["member"]["user"]["id"].as_str().or_else(|| d["user"]["id"].as_str()).unwrap_or("");
    let memory_context = crate::memory::RunContext {
        provider: "discord".into(),
        channel_id: Some(channel_id.to_string()).filter(|s| !s.is_empty()),
        user_id: Some(caller_id.to_string()).filter(|s| !s.is_empty()),
        message_id: None,
        role: role.to_string(),
        is_dm: d["guild_id"].as_str().is_none(),
    };
    let arg = |name: &str| -> String {
        data["options"][0]["options"]
            .as_array()
            .and_then(|a| a.iter().find(|o| o["name"].as_str() == Some(name)))
            .and_then(|o| o["value"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let rank = role_rank(role);
    let denied = |need: &str| format!("Not permitted — {need} only (you are {role}).");

    match (cmd, sub) {
        ("gates", "ls") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let g = crate::gate::list_pending();
            if g.is_empty() {
                "No pending gates.".into()
            } else {
                let lines: Vec<String> = g.iter().map(|x| format!("• `{}` {} ({})", x.id, x.intent, x.worker)).collect();
                format!("Pending gates:\n{}", lines.join("\n"))
            }
        }
        ("gates", "approve") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let id = arg("id");
            match crate::action::execute_gate(&id, &format!("discord:{caller}"), None).await {
                Ok(g) => format!("Approved `{}` ({}).", g.id, g.intent),
                Err(e) => format!("Could not approve `{id}`: {e}"),
            }
        }
        ("gates", "deny") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let id = arg("id");
            match crate::action::deny_gate(&id, &format!("discord:{caller}"), None) {
                Ok(g) => format!("Denied `{}` ({}).", g.id, g.intent),
                Err(e) => format!("Could not deny `{id}`: {e}"),
            }
        }
        ("queue", "ls") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let tasks = crate::queue::list_all();
            if tasks.is_empty() {
                "Queue is empty.".into()
            } else {
                let lines: Vec<String> = tasks.iter().map(|t| format!("• `{}` [{}] {}", t.id, t.state, first_words(&t.prompt))).collect();
                format!("Tasks:\n{}", lines.join("\n"))
            }
        }
        ("channel", "trust") => {
            if rank < 2 {
                return denied("server admins");
            }
            set_channel_trust(channel_id, true);
            "This channel's participants are now **trusted** — they can administer, and I'll reply here without a gate.".into()
        }
        ("channel", "untrust") => {
            if rank < 2 {
                return denied("server admins");
            }
            set_channel_trust(channel_id, false);
            "This channel's participants are now **untrusted** — they can talk to me, but not administer, and my replies here will be gated.".into()
        }
        ("channel", "mode") => {
            if rank < 2 {
                return denied("server admins");
            }
            let mode = arg("mode");
            if mode != "all" && mode != "mention" {
                return "Mode must be `all` or `mention`.".into();
            }
            set_channel_mode(channel_id, &mode);
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
            let state = arg("state");
            let enabled = match state.as_str() {
                "on" => true,
                "off" => false,
                _ => return "Memory state must be `on` or `off`.".into(),
            };
            set_channel_memory(channel_id, enabled);
            format!("Memory is now **{}** in this channel.", if enabled { "on" } else { "off" })
        }
        ("channel", "memory-inferred") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let state = arg("state");
            let enabled = match state.as_str() {
                "auto" => true,
                "review" => false,
                _ => return "Inferred memory must be `auto` or `review`.".into(),
            };
            set_channel_memory_inferred_auto(channel_id, enabled);
            format!("Inferred channel memories now require **{}**.", if enabled { "no review" } else { "review" })
        }
        ("channel", "memory-kinds") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let value = arg("kinds");
            match crate::cmd::channel::parse_memory_kinds(&value) {
                Ok(kinds) => {
                    set_channel_memory_allowed_kinds(channel_id, kinds.clone());
                    format!("Channel memory kinds: **{}**.", kinds.map(|v| v.join(", ")).unwrap_or_else(|| "default".into()))
                }
                Err(e) => format!("Could not set memory kinds: {e}"),
            }
        }
        ("channel", "memory-retention") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let value = arg("days");
            let days = if value == "default" {
                None
            } else {
                match value.parse::<u64>().ok().filter(|n| *n > 0) {
                    Some(days) => Some(days),
                    None => return "Retention must be `default` or a positive number of days.".into(),
                }
            };
            set_channel_memory_retention_days(channel_id, days);
            format!("Channel memory retention: **{}**.", days.map(|n| format!("{n} days")).unwrap_or_else(|| "default".into()))
        }
        ("memory", "show") => {
            let notes = crate::memory::visible_in_context(worker, &memory_context);
            if notes.is_empty() {
                "No visible memories about you or this channel.".into()
            } else {
                let lines: Vec<String> = notes
                    .iter()
                    .take(20)
                    .map(|n| format!("• `{}` [{} / {}] {}", n.id, n.scope.as_str(), n.status(), n.note))
                    .collect();
                format!("Visible memories:\n{}", lines.join("\n"))
            }
        }
        ("memory", "forget") => {
            let id = arg("id");
            match crate::memory::participant_mutate(worker, "forget", &id, None, &memory_context) {
                Ok(()) => format!("Forgot `{id}`."),
                Err(e) => format!("Could not forget `{id}`: {e}"),
            }
        }
        ("memory", "correct") => {
            let id = arg("id");
            let text = arg("text");
            match crate::memory::participant_mutate(worker, "correct", &id, Some(&text), &memory_context) {
                Ok(()) => format!("Corrected `{id}`."),
                Err(e) => format!("Could not correct `{id}`: {e}"),
            }
        }
        ("purpose", "show") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            match std::fs::read_to_string(purpose_path(channel_id)) {
                Ok(p) if !p.trim().is_empty() => format!("This channel's purpose:\n```\n{}\n```", p.trim()),
                _ => "This channel has no purpose set yet. Set one with `/purpose set`.".into(),
            }
        }
        ("purpose", "set") => {
            if rank < 1 {
                return denied("trusted participants");
            }
            let text = arg("text");
            let path = purpose_path(channel_id);
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            match std::fs::write(&path, format!("{}\n", text.trim())) {
                Ok(()) => "This channel's purpose is updated. It'll shape how I act here from your next message.".into(),
                Err(e) => format!("Could not set purpose: {e}"),
            }
        }
        ("identity", "show") => {
            if rank < 2 {
                return denied("server admins");
            }
            match std::fs::read_to_string(crate::cmd::run_box::identity_path(worker)) {
                Ok(p) if !p.trim().is_empty() => format!("{worker}'s identity:\n```\n{}\n```", p.trim()),
                _ => format!("{worker} has no identity.md set."),
            }
        }
        _ => "Unknown command.".into(),
    }
}

fn first_words(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.len() > 60 {
        format!("{}…", &s[..60])
    } else {
        s
    }
}

// ── channel settings (trust designation + response mode) ──────────────────────

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ChannelSettings {
    #[serde(default)]
    pub trusted: bool,
    /// "all" (wake on every message) | "mention" (wake only on @mention/DM).
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Channel-local memory controls may only make the worker policy stricter.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
    #[serde(default)]
    pub memory_recall_max_notes: Option<usize>,
    #[serde(default)]
    pub memory_recall_char_budget: Option<usize>,
    #[serde(default)]
    pub memory_inferred_auto: bool,
    #[serde(default)]
    pub memory_allowed_kinds: Option<Vec<String>>,
    #[serde(default)]
    pub memory_retention_days: Option<u64>,
}

fn default_mode() -> String {
    "all".to_string()
}

fn default_memory_enabled() -> bool {
    true
}

impl Default for ChannelSettings {
    fn default() -> Self {
        Self {
            trusted: false,
            mode: default_mode(),
            memory_enabled: true,
            memory_recall_max_notes: None,
            memory_recall_char_budget: None,
            memory_inferred_auto: false,
            memory_allowed_kinds: None,
            memory_retention_days: None,
        }
    }
}

fn settings_path() -> PathBuf {
    root().join("channels").join("settings.json")
}

fn load_settings() -> HashMap<String, ChannelSettings> {
    if let Some(s) = std::fs::read_to_string(settings_path()).ok().and_then(|s| serde_json::from_str::<HashMap<String, ChannelSettings>>(&s).ok()) {
        return s;
    }
    // Migrate a legacy trust.json (bool map), if present.
    std::fs::read_to_string(root().join("channels").join("trust.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, bool>>(&s).ok())
        .map(|m| {
            m.into_iter()
                .map(|(k, v)| (k, ChannelSettings { trusted: v, ..Default::default() }))
                .collect()
        })
        .unwrap_or_default()
}

fn save_settings(map: &HashMap<String, ChannelSettings>) {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(path, text);
    }
}

/// Is this channel marked trusted? (DMs are marked trusted when first seen.)
pub fn channel_trusted(channel_id: &str) -> bool {
    load_settings().get(channel_id).map(|s| s.trusted).unwrap_or(false)
}

pub fn set_channel_trust(channel_id: &str, trusted: bool) {
    let mut m = load_settings();
    let e = m.entry(channel_id.to_string()).or_default();
    if e.trusted == trusted {
        return;
    }
    e.trusted = trusted;
    save_settings(&m);
}

/// The channel's response mode: "all" (default) or "mention".
pub fn channel_mode(channel_id: &str) -> String {
    load_settings().get(channel_id).map(|s| s.mode.clone()).unwrap_or_else(default_mode)
}

pub fn set_channel_mode(channel_id: &str, mode: &str) {
    let mut m = load_settings();
    let e = m.entry(channel_id.to_string()).or_default();
    e.mode = mode.to_string();
    save_settings(&m);
}

pub fn channel_memory_enabled(channel_id: &str) -> bool {
    load_settings().get(channel_id).map(|s| s.memory_enabled).unwrap_or(true)
}

pub fn channel_memory_recall_max_notes(channel_id: &str) -> Option<usize> {
    load_settings().get(channel_id).and_then(|s| s.memory_recall_max_notes)
}

pub fn channel_memory_recall_char_budget(channel_id: &str) -> Option<usize> {
    load_settings().get(channel_id).and_then(|s| s.memory_recall_char_budget)
}

pub fn channel_memory_inferred_auto(channel_id: &str) -> bool {
    load_settings().get(channel_id).map(|s| s.memory_inferred_auto).unwrap_or(false)
}

pub fn channel_memory_allowed_kinds(channel_id: &str) -> Option<Vec<String>> {
    load_settings().get(channel_id).and_then(|s| s.memory_allowed_kinds.clone())
}

pub fn channel_memory_retention_days(channel_id: &str) -> Option<u64> {
    load_settings().get(channel_id).and_then(|s| s.memory_retention_days)
}

pub fn set_channel_memory_inferred_auto(channel_id: &str, enabled: bool) {
    let mut m = load_settings();
    m.entry(channel_id.to_string()).or_default().memory_inferred_auto = enabled;
    save_settings(&m);
}

pub fn set_channel_memory_allowed_kinds(channel_id: &str, kinds: Option<Vec<String>>) {
    let mut m = load_settings();
    m.entry(channel_id.to_string()).or_default().memory_allowed_kinds = kinds;
    save_settings(&m);
}

pub fn set_channel_memory_retention_days(channel_id: &str, days: Option<u64>) {
    let mut m = load_settings();
    m.entry(channel_id.to_string()).or_default().memory_retention_days = days;
    save_settings(&m);
}

pub fn set_channel_memory(channel_id: &str, enabled: bool) {
    let mut m = load_settings();
    m.entry(channel_id.to_string()).or_default().memory_enabled = enabled;
    save_settings(&m);
}

pub fn set_channel_memory_budget(channel_id: &str, notes: Option<usize>, chars: Option<usize>) {
    let mut m = load_settings();
    let entry = m.entry(channel_id.to_string()).or_default();
    entry.memory_recall_max_notes = notes;
    entry.memory_recall_char_budget = chars;
    save_settings(&m);
}

pub fn channel_settings_all() -> HashMap<String, ChannelSettings> {
    load_settings()
}

// ── channel store (history + uploads), under the read-only repo mount ─────────

fn channel_dir(channel_id: &str) -> PathBuf {
    root().join("channels").join(channel_id)
}

/// A channel's purpose file (channels/<id>/purpose.md) — the worker's role in
/// this channel, composed into runs and editable by trusted participants.
pub fn purpose_path(channel_id: &str) -> PathBuf {
    channel_dir(channel_id).join("purpose.md")
}

fn persist_message(channel_id: &str, record: &Value) {
    let dir = channel_dir(channel_id);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("messages.jsonl")) {
        use std::io::Write;
        let _ = writeln!(f, "{record}");
    }
}

/// Distinct human authors seen in a channel's history (bots aren't persisted),
/// to tell a 1:1 conversation from a group one.
fn distinct_human_authors(channel_id: &str) -> usize {
    use std::collections::HashSet;
    recent_messages(channel_id, 500)
        .iter()
        .filter_map(|m| m["author_id"].as_str().map(String::from))
        .collect::<HashSet<_>>()
        .len()
}

fn recent_messages(channel_id: &str, n: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(channel_dir(channel_id).join("messages.jsonl")).unwrap_or_default();
    let mut evs: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let len = evs.len();
    if len > n {
        evs.split_off(len - n)
    } else {
        evs
    }
}

async fn download_attachments(channel_id: &str, attachments: &Value) {
    let Some(list) = attachments.as_array() else { return };
    if list.is_empty() {
        return;
    }
    let dir = channel_dir(channel_id).join("files");
    let _ = std::fs::create_dir_all(&dir);
    for a in list {
        let (Some(url), Some(name)) = (a["url"].as_str(), a["filename"].as_str()) else { continue };
        // Keep the filename safe (no path traversal).
        let safe: String = name.chars().map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
        if let Ok(res) = reqwest::Client::new().get(url).send().await {
            if let Ok(bytes) = res.bytes().await {
                let _ = std::fs::write(dir.join(&safe), &bytes);
            }
        }
    }
}
