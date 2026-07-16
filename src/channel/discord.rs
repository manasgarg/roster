//! Discord — the REST client for outbound messages and the gateway (WebSocket)
//! client for inbound. The trusted-side code holds the bot token (from the
//! vault); the box never does. Base URL is overridable via DISCORD_API_BASE so
//! the outbound executor can be tested against a mock.

use crate::paths;
use crate::util::now_rfc3339;
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

/// Post a message to a channel. Returns the created message id. Honors Discord's
/// 429 rate limit (retry after the advised delay) so a long, chunked reply isn't
/// abandoned half-delivered when it trips the per-channel bucket.
pub async fn post_message(token: &str, channel_id: &str, text: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let url = format!("{}/channels/{channel_id}/messages", base());
    for attempt in 0..5 {
        let res = client
            .post(&url)
            .header("authorization", format!("Bot {token}"))
            .json(&json!({ "content": text }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status();
        if status.as_u16() == 429 {
            let body: Value = res.json().await.unwrap_or(Value::Null);
            let wait = body.get("retry_after").and_then(|v| v.as_f64()).unwrap_or(1.0);
            if attempt < 4 {
                tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.0, 60.0) + 0.05)).await;
                continue;
            }
            return Err(format!("discord rate limited; gave up after retries (retry_after {wait}s)"));
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(format!("discord POST message {status}: {body}"));
        }
        return Ok(body
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string());
    }
    Err("discord POST message: exhausted rate-limit retries".into())
}

/// Post a message of any length: split at Discord's 2000-char limit on line
/// boundaries and send the chunks in order. Returns the last message id.
pub async fn post_chunked(token: &str, channel_id: &str, text: &str) -> Result<String, String> {
    let mut last = String::new();
    for chunk in crate::util::chunk_message(text, 2000) {
        last = post_message(token, channel_id, &chunk).await?;
    }
    Ok(last)
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
    body.get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "DM channel had no id".to_string())
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
    let transient = |m: String| GwError {
        fatal: false,
        msg: m,
    };
    let (ws, _) = tokio_tungstenite::connect_async(GATEWAY_URL)
        .await
        .map_err(|e| transient(format!("connect: {e}")))?;
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
        let Some(msg) = stream.next().await else {
            break Err(transient("stream ended".into()));
        };
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
                    break Err(GwError {
                        fatal: true,
                        msg: "authentication failed — bad bot token".into(),
                    });
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
                        if tx2
                            .send(Ws::text(json!({ "op": 1, "d": s }).to_string()))
                            .is_err()
                        {
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
            7 | 9 => {
                break Err(transient(format!(
                    "gateway asked to reconnect (op {})",
                    v["op"]
                )))
            }
            0 => {
                let d = &v["d"];
                match v["t"].as_str().unwrap_or("") {
                    "READY" => {
                        bot_id = d["user"]["id"].as_str().unwrap_or("").to_string();
                        app_id = d["application"]["id"].as_str().unwrap_or("").to_string();
                        eprintln!(
                            "discord: connected as {} ({bot_id})",
                            d["user"]["username"].as_str().unwrap_or("?")
                        );
                    }
                    "GUILD_CREATE" => {
                        let g = ingest_guild(d);
                        eprintln!(
                            "discord: guild \"{}\" — {} channels visible",
                            g.name, g.channels
                        );
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

/// Remember a channel's human identity so the CLI never shows a bare id.
pub(crate) fn write_channel_meta(channel_id: &str, meta: &Value) {
    let path = crate::paths::channel_meta_file(channel_id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{meta}\n"));
}

/// The channel's human identity, if a listener has learned it.
pub fn channel_meta(channel_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(crate::paths::channel_meta_file(channel_id)).ok()?;
    serde_json::from_str(&text).ok()
}

fn ingest_guild(d: &Value) -> Guild {
    let mut role_perms = HashMap::new();
    let mut everyone_perms = 0;
    let guild_id = d["id"].as_str().unwrap_or("");
    if let Some(roles) = d["roles"].as_array() {
        for r in roles {
            let id = r["id"].as_str().unwrap_or("");
            let perms = r["permissions"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if id == guild_id {
                everyone_perms = perms; // @everyone role id == guild id
            }
            role_perms.insert(id.to_string(), perms);
        }
    }
    // GUILD_CREATE carries every channel's name — persist them so `channel
    // ls/show` can say "#general @ rototo" instead of a snowflake id.
    let guild_name = d["name"].as_str().unwrap_or("");
    if let Some(channels) = d["channels"].as_array() {
        for c in channels {
            let (Some(id), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else {
                continue;
            };
            write_channel_meta(
                id,
                &serde_json::json!({
                    "platform": "discord",
                    "server": guild_name,
                    "name": name,
                }),
            );
        }
    }

    Guild {
        name: guild_name.to_string(),
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

async fn handle_message(
    worker: &str,
    d: &Value,
    bot_id: &str,
    guilds: &HashMap<String, Guild>,
    token: &str,
) {
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
        if let Err(e) = set_channel_trust(channel_id, true) {
            // DMs are always trusted (1:1, sought-out); a failure here isn't
            // user-facing, but it must not pass unnoticed.
            eprintln!("discord: could not mark DM channel {channel_id} trusted: {e}");
        }
        write_channel_meta(
            channel_id,
            &json!({
                "platform": "discord",
                "name": format!("DM with {}", d["author"]["username"].as_str().unwrap_or("?")),
            }),
        );
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
    let mentioned = d["mentions"]
        .as_array()
        .map(|m| m.iter().any(|u| u["id"].as_str() == Some(bot_id)))
        .unwrap_or(false);
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
    let context = crate::worker::memory::RunContext {
        provider: "discord".into(),
        channel_id: Some(channel_id.to_string()),
        user_id: d["author"]["id"].as_str().map(String::from),
        message_id: d["id"].as_str().map(String::from),
        thread_ts: None, // Discord has no Slack-style thread ts
        role: role.to_string(),
        is_dm,
        inbound: false, // live channel context carries ids; inbound marks relay tasks
    };
    eprintln!("discord: {author} ({role}) in {channel_id} → session");
    route_to_session(worker, channel_id, author.to_string(), text, context, token).await;
}

// ── conversation sessions: one warm box per active channel ────────────────────

fn sessions(
) -> &'static Mutex<HashMap<String, tokio::sync::mpsc::Sender<crate::run::boxed::SessionMessage>>> {
    static S: OnceLock<
        Mutex<HashMap<String, tokio::sync::mpsc::Sender<crate::run::boxed::SessionMessage>>>,
    > = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The live-session map key: a session belongs to a (worker, channel) pair, not
/// a channel alone, so two workers in the same channel don't share one session.
/// The unit separator can't occur in a worker name or platform channel id.
pub fn session_key(worker: &str, channel_id: &str) -> String {
    format!("{worker}\u{1f}{channel_id}")
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
    context: crate::worker::memory::RunContext,
    token: &str,
) {
    let start_context = context.clone();
    // Key sessions by (worker, channel): two workers can share a channel, and a
    // channel-only key would route one worker's traffic into the other's box.
    let key = session_key(worker, channel_id);
    let message = crate::run::boxed::SessionMessage {
        text,
        author_label,
        context,
    };
    let delivered = {
        let map = sessions().lock().unwrap();
        match map.get(&key) {
            Some(tx) => tx
                .try_send(crate::run::boxed::SessionMessage {
                    text: message.text.clone(),
                    author_label: message.author_label.clone(),
                    context: message.context.clone(),
                })
                .is_ok(),
            None => false,
        }
    };
    spawn_typing(channel_id, token);
    if delivered {
        return;
    }
    // Start a new session (clears any stale closed sender).
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::run::boxed::SessionMessage>(64);
    let _ = tx.try_send(message);
    sessions().lock().unwrap().insert(key.clone(), tx);
    let (w, run_id) = (worker.to_string(), crate::run::boxed::new_run_id());
    let (channel_owned, token_owned) = (channel_id.to_string(), token.to_string());
    let session_map_key = key;
    tokio::spawn(async move {
        // Reduce the outcome to a Send-safe String before any await below.
        let failed = crate::run::boxed::run_session(
            &w,
            &run_id,
            crate::worker::context::RunSurface::DiscordSession,
            start_context,
            rx,
            SESSION_IDLE_SECS,
            None,
        )
        .await
        .err()
        .map(|e| e.to_string());
        // Drop our now-closed sender from the map so the next message starts a
        // fresh session instead of try_send-ing into a dead one.
        {
            let mut map = sessions().lock().unwrap();
            if map.get(&session_map_key).map(|tx| tx.is_closed()).unwrap_or(false) {
                map.remove(&session_map_key);
            }
        }
        if let Some(msg) = failed {
            eprintln!("discord session error: {msg}");
            // Don't leave the user staring at a typing indicator that never
            // resolves — say the run failed.
            let _ = post_chunked(
                &token_owned,
                &channel_owned,
                "⚠️ I couldn't finish that just now — my box failed to start or exited early. Nothing unsaved was kept; please try again in a moment.",
            )
            .await;
        }
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
        { "name": "approvals", "description": "Roster approval desk", "options": [
            { "type": 1, "name": "ls", "description": "What is pending your approval" },
            { "type": 1, "name": "show", "description": "The exact action a gate would run", "options": [{ "type": 3, "name": "id", "description": "Gate id", "required": true }] },
            { "type": 1, "name": "approve", "description": "Approve a gate", "options": [{ "type": 3, "name": "id", "description": "Gate id", "required": true }] },
            { "type": 1, "name": "deny", "description": "Deny a gate", "options": [{ "type": 3, "name": "id", "description": "Gate id", "required": true }] }
        ]},
        { "name": "task", "description": "The worker's tasks", "options": [
            { "type": 1, "name": "ls", "description": "List tasks" },
            { "type": 1, "name": "add", "description": "File a task for this worker", "options": [{ "type": 3, "name": "prompt", "description": "The task prompt", "required": true }] },
            { "type": 1, "name": "show", "description": "One task: state, gates, prompt", "options": [{ "type": 3, "name": "id", "description": "Task id", "required": true }] },
            { "type": 1, "name": "requeue", "description": "Put a stuck task back to waiting", "options": [{ "type": 3, "name": "id", "description": "Task id", "required": true }] }
        ]},
        { "name": "runs", "description": "The worker's session log", "options": [
            { "type": 1, "name": "ls", "description": "Recent sessions" },
            { "type": 1, "name": "show", "description": "One session's record", "options": [{ "type": 3, "name": "run", "description": "Run id", "required": true }] }
        ]},
        { "name": "worker", "description": "The worker itself", "options": [
            { "type": 1, "name": "show", "description": "Queue, gates, memory at a glance" },
            { "type": 1, "name": "trust", "description": "Per-action trust and earned history" }
        ]},
        { "name": "channel", "description": "Channel settings", "options": [
            { "type": 1, "name": "show", "description": "This channel's settings" },
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
            { "type": 1, "name": "ls", "description": "Notes by scope (admin)", "options": [
                { "type": 3, "name": "scope", "description": "worker, channel, or user", "required": true,
                  "choices": [{ "name": "worker", "value": "worker" }, { "name": "channel", "value": "channel" }, { "name": "user", "value": "user" }] }
            ]},
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
        .put(format!(
            "{}/applications/{app_id}/guilds/{guild_id}/commands",
            base()
        ))
        .header("authorization", format!("Bot {token}"))
        .json(&command_defs())
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!(
            "discord: register commands → {} (guild {guild_id})",
            r.status()
        ),
        Err(e) => eprintln!("discord: register commands failed: {e}"),
    }
}

/// The caller's role for an interaction. Discord supplies the member's computed
/// permissions directly, so we don't recompute from roles here.
fn interaction_role(d: &Value, guilds: &HashMap<String, Guild>) -> &'static str {
    let Some(guild_id) = d["guild_id"].as_str() else {
        return "trusted"; // DM
    };
    let perms = d["member"]["permissions"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
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
        .post(format!(
            "{}/interactions/{interaction_id}/{itoken}/callback",
            base()
        ))
        .json(&json!({ "type": 5, "data": { "flags": 64 } }))
        .send()
        .await;

    let role = interaction_role(d, guilds);
    let caller = d["member"]["user"]["username"]
        .as_str()
        .or_else(|| d["user"]["username"].as_str())
        .unwrap_or("someone");
    let text = run_command(worker, d, role, caller).await;

    // Fill in the deferred response, chunked to Discord's 2000-char limit: the
    // first chunk edits the deferred @original, the rest are ephemeral followups.
    // Without chunking a long reply (e.g. `/task ls`) 400s and the user is left
    // on "thinking…"; the failure was previously swallowed too.
    let client = reqwest::Client::new();
    let chunks = crate::util::chunk_message(&text, 2000);
    let first = chunks.first().map(|s| s.as_str()).unwrap_or("(no output)");
    match client
        .patch(format!(
            "{}/webhooks/{app_id}/{itoken}/messages/@original",
            base()
        ))
        .json(&json!({ "content": first }))
        .send()
        .await
    {
        Ok(r) if !r.status().is_success() => {
            eprintln!("discord interaction reply failed: {}", r.status())
        }
        Err(e) => eprintln!("discord interaction reply error: {e}"),
        _ => {}
    }
    for chunk in chunks.iter().skip(1) {
        let _ = client
            .post(format!("{}/webhooks/{app_id}/{itoken}", base()))
            .json(&json!({ "content": chunk, "flags": 64 }))
            .send()
            .await;
    }
}

/// Adapt a Discord interaction into the shared slash surface (channel::slash)
/// and run it there — the command bodies live in one place for every channel.
async fn run_command(worker: &str, d: &Value, role: &str, caller: &str) -> String {
    let data = &d["data"];
    let channel_id = d["channel_id"].as_str().unwrap_or("");
    let caller_id = d["member"]["user"]["id"]
        .as_str()
        .or_else(|| d["user"]["id"].as_str())
        .unwrap_or("");
    let memory_context = crate::worker::memory::RunContext {
        provider: "discord".into(),
        channel_id: Some(channel_id.to_string()).filter(|s| !s.is_empty()),
        user_id: Some(caller_id.to_string()).filter(|s| !s.is_empty()),
        message_id: None,
        thread_ts: None,
        role: role.to_string(),
        is_dm: d["guild_id"].as_str().is_none(),
        inbound: false,
    };
    let mut args = std::collections::HashMap::new();
    if let Some(opts) = data["options"][0]["options"].as_array() {
        for o in opts {
            if let (Some(n), Some(v)) = (o["name"].as_str(), o["value"].as_str()) {
                args.insert(n.to_string(), v.to_string());
            }
        }
    }
    let call = super::slash::SlashCall {
        cmd: data["name"].as_str().unwrap_or("").to_string(),
        sub: data["options"][0]["name"].as_str().unwrap_or("").to_string(),
        args,
    };
    super::slash::run(
        worker,
        &call,
        channel_id,
        &memory_context,
        role,
        &format!("discord:{caller}"),
    )
    .await
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
    paths::channels_dir().join("settings.json")
}

fn load_settings() -> HashMap<String, ChannelSettings> {
    if let Some(s) = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, ChannelSettings>>(&s).ok())
    {
        return s;
    }
    // Migrate a legacy trust.json (bool map), if present.
    std::fs::read_to_string(paths::channels_dir().join("trust.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, bool>>(&s).ok())
        .map(|m| {
            m.into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        ChannelSettings {
                            trusted: v,
                            ..Default::default()
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_settings(map: &HashMap<String, ChannelSettings>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    crate::statefile::write_atomic(&settings_path(), text.as_bytes()).map_err(|e| e.to_string())
}

/// Load, apply `f`, and persist channel settings as one locked, atomic
/// read-modify-write. The lock keeps the daemon (marking a DM trusted), other
/// slash commands, and a separate CLI process from losing each other's updates;
/// the atomic write keeps a concurrent reader from ever seeing a half-written
/// file. The write error is returned so callers report a real failure instead
/// of claiming a change that didn't land.
fn mutate_settings(
    f: impl FnOnce(&mut HashMap<String, ChannelSettings>),
) -> Result<(), String> {
    let _lock = crate::statefile::FileLock::acquire("channels")
        .map_err(|e| format!("channel settings lock: {e}"))?;
    let mut m = load_settings();
    f(&mut m);
    save_settings(&m)
}

/// Is this channel marked trusted? (DMs are marked trusted when first seen.)
pub fn channel_trusted(channel_id: &str) -> bool {
    load_settings()
        .get(channel_id)
        .map(|s| s.trusted)
        .unwrap_or(false)
}

pub fn set_channel_trust(channel_id: &str, trusted: bool) -> Result<(), String> {
    // Hot path: DMs re-assert trust on every message. Skip the locked write when
    // nothing changes (the read sees a whole file thanks to atomic writes).
    if channel_trusted(channel_id) == trusted {
        return Ok(());
    }
    mutate_settings(|m| {
        m.entry(channel_id.to_string()).or_default().trusted = trusted;
    })
}

/// The channel's response mode: "all" (default) or "mention".
pub fn channel_mode(channel_id: &str) -> String {
    load_settings()
        .get(channel_id)
        .map(|s| s.mode.clone())
        .unwrap_or_else(default_mode)
}

pub fn set_channel_mode(channel_id: &str, mode: &str) -> Result<(), String> {
    let mode = mode.to_string();
    mutate_settings(|m| {
        m.entry(channel_id.to_string()).or_default().mode = mode;
    })
}

pub fn channel_memory_enabled(channel_id: &str) -> bool {
    load_settings()
        .get(channel_id)
        .map(|s| s.memory_enabled)
        .unwrap_or(true)
}

pub fn channel_memory_recall_max_notes(channel_id: &str) -> Option<usize> {
    load_settings()
        .get(channel_id)
        .and_then(|s| s.memory_recall_max_notes)
}

pub fn channel_memory_recall_char_budget(channel_id: &str) -> Option<usize> {
    load_settings()
        .get(channel_id)
        .and_then(|s| s.memory_recall_char_budget)
}

pub fn channel_memory_inferred_auto(channel_id: &str) -> bool {
    load_settings()
        .get(channel_id)
        .map(|s| s.memory_inferred_auto)
        .unwrap_or(false)
}

pub fn channel_memory_allowed_kinds(channel_id: &str) -> Option<Vec<String>> {
    load_settings()
        .get(channel_id)
        .and_then(|s| s.memory_allowed_kinds.clone())
}

pub fn channel_memory_retention_days(channel_id: &str) -> Option<u64> {
    load_settings()
        .get(channel_id)
        .and_then(|s| s.memory_retention_days)
}

pub fn set_channel_memory_inferred_auto(channel_id: &str, enabled: bool) -> Result<(), String> {
    mutate_settings(|m| {
        m.entry(channel_id.to_string())
            .or_default()
            .memory_inferred_auto = enabled;
    })
}

pub fn set_channel_memory_allowed_kinds(
    channel_id: &str,
    kinds: Option<Vec<String>>,
) -> Result<(), String> {
    mutate_settings(|m| {
        m.entry(channel_id.to_string())
            .or_default()
            .memory_allowed_kinds = kinds;
    })
}

pub fn set_channel_memory_retention_days(channel_id: &str, days: Option<u64>) -> Result<(), String> {
    mutate_settings(|m| {
        m.entry(channel_id.to_string())
            .or_default()
            .memory_retention_days = days;
    })
}

pub fn set_channel_memory(channel_id: &str, enabled: bool) -> Result<(), String> {
    mutate_settings(|m| {
        m.entry(channel_id.to_string()).or_default().memory_enabled = enabled;
    })
}

pub fn set_channel_memory_budget(
    channel_id: &str,
    notes: Option<usize>,
    chars: Option<usize>,
) -> Result<(), String> {
    mutate_settings(|m| {
        let entry = m.entry(channel_id.to_string()).or_default();
        entry.memory_recall_max_notes = notes;
        entry.memory_recall_char_budget = chars;
    })
}

pub fn channel_settings_all() -> HashMap<String, ChannelSettings> {
    load_settings()
}

// ── channel store (history + uploads), under the read-only repo mount ─────────

fn channel_dir(channel_id: &str) -> PathBuf {
    paths::channel_dir(channel_id)
}

/// A channel's purpose file (channels/<id>/purpose.md) — the worker's role in
/// this channel, composed into runs and editable by trusted participants.
pub fn purpose_path(channel_id: &str) -> PathBuf {
    channel_dir(channel_id).join("purpose.md")
}

pub(crate) fn persist_message(channel_id: &str, record: &Value) {
    let dir = channel_dir(channel_id);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("messages.jsonl"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{record}");
    }
}

/// Distinct human authors seen in a channel's history (bots aren't persisted),
/// to tell a 1:1 conversation from a group one.
pub(crate) fn distinct_human_authors(channel_id: &str) -> usize {
    use std::collections::HashSet;
    recent_messages(channel_id, 500)
        .iter()
        .filter_map(|m| m["author_id"].as_str().map(String::from))
        .collect::<HashSet<_>>()
        .len()
}

pub(crate) fn recent_messages(channel_id: &str, n: usize) -> Vec<Value> {
    let text =
        std::fs::read_to_string(channel_dir(channel_id).join("messages.jsonl")).unwrap_or_default();
    let mut evs: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let len = evs.len();
    if len > n {
        evs.split_off(len - n)
    } else {
        evs
    }
}

async fn download_attachments(channel_id: &str, attachments: &Value) {
    let Some(list) = attachments.as_array() else {
        return;
    };
    if list.is_empty() {
        return;
    }
    let dir = channel_dir(channel_id).join("files");
    let _ = std::fs::create_dir_all(&dir);
    for a in list {
        let (Some(url), Some(name)) = (a["url"].as_str(), a["filename"].as_str()) else {
            continue;
        };
        // Keep the filename safe (no path traversal).
        let safe: String = name
            .chars()
            .map(|c| if c == '/' || c == '\\' { '_' } else { c })
            .collect();
        if let Ok(res) = reqwest::Client::new().get(url).send().await {
            if let Ok(bytes) = res.bytes().await {
                let _ = std::fs::write(dir.join(&safe), &bytes);
            }
        }
    }
}
