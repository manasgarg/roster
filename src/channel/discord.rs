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
            let wait = body
                .get("retry_after")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0);
            if attempt < 4 {
                tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.0, 60.0) + 0.05)).await;
                continue;
            }
            return Err(format!(
                "discord rate limited; gave up after retries (retry_after {wait}s)"
            ));
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
    stop_typing(token, channel_id);
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

/// Enough to RESUME a dropped session (op 6) rather than re-IDENTIFY: Discord
/// only replays messages missed during the gap on a resume, so without this
/// every message sent during a reconnect is lost.
#[derive(Clone)]
struct ResumeState {
    session_id: String,
    seq: i64,
    url: String,
}

struct GwError {
    fatal: bool,
    msg: String,
    /// Present when the drop is resumable — the next dial should RESUME, not
    /// re-IDENTIFY. Absent (op 9 / fatal) means start a fresh session.
    resume: Option<ResumeState>,
}

/// Run the gateway for one worker: dial out, identify, and dispatch events.
/// Reconnects on transient errors, resuming the session when possible so no
/// messages are dropped in the gap; stops on fatal ones (bad token / intent).
/// This worker's grant edge on the connection this listener runs under,
/// looked up live so a grant edit applies without a listener restart.
/// DMs are admitted by default (1:1, sought-out, dynamically created ids
/// that could never be pre-listed) — a scope that names classes and leaves
/// "dm" out is the one way to refuse them. Broken config fails closed,
/// like dispatch does — and so does a connection file that grants this
/// worker nothing.
fn out_of_scope(worker: &str, credential: &str, guild_id: Option<&str>, channel_id: &str) -> bool {
    let cfg = match crate::config::snapshot() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("discord: config invalid — dropping traffic until it parses");
            return true;
        }
    };
    match cfg.connections.iter().find(|c| c.name == credential) {
        Some(c) => {
            let class = match guild_id {
                None => crate::config::SurfaceClass::Dm,
                Some(_) => channel_class(channel_id),
            };
            !c.allows_surface(worker, guild_id, channel_id, class)
        }
        // No connection file for this credential (legacy [channels]-only
        // binding) = unrestricted, exactly as before scoping existed.
        None => false,
    }
}

/// The listener's classification of a guild channel, from the meta recorded
/// at GUILD_CREATE. A channel never classified is Unknown — it never
/// matches a class entry in a scope (fail closed), though ids and servers
/// still admit it.
fn channel_class(channel_id: &str) -> crate::config::SurfaceClass {
    channel_meta(channel_id)
        .and_then(|m| {
            m.get("class")
                .and_then(|v| v.as_str())
                .and_then(crate::config::SurfaceClass::parse)
        })
        .unwrap_or(crate::config::SurfaceClass::Unknown)
}

/// Durable per-channel replay cursor: the newest message id this listener
/// has seen, plus the channel's guild (for the scope check at catch-up).
/// A fresh IDENTIFY replays nothing — Discord only replays into a RESUME,
/// and a server restart can't resume — so this cursor is what lets a
/// starting listener fetch what arrived while the server was down.
fn cursor_path(channel_id: &str) -> PathBuf {
    crate::paths::channel_dir(channel_id).join("discord-cursor.json")
}

fn read_cursor(channel_id: &str) -> Option<(String, Option<String>)> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(cursor_path(channel_id)).ok()?).ok()?;
    Some((
        v.get("last_message_id")?.as_str()?.to_string(),
        v.get("guild_id").and_then(Value::as_str).map(String::from),
    ))
}

fn write_cursor(channel_id: &str, message_id: &str, guild_id: Option<&str>) {
    let path = cursor_path(channel_id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        &path,
        json!({ "last_message_id": message_id, "guild_id": guild_id }).to_string(),
    );
}

/// Fetch and handle everything that arrived while no listener was connected:
/// for every channel with a cursor, page through `GET /channels/<id>/messages
/// ?after=<cursor>` oldest-first and run each message down the exact same
/// path as a live event. Only channels seen at least once before can be
/// caught up — a first-ever message in a brand-new channel during downtime
/// stays missed until someone speaks there again.
async fn catch_up(worker: &str, token: &str, credential: &str, bot_id: &str) {
    let client = reqwest::Client::new();
    let channels: Vec<String> = std::fs::read_dir(crate::paths::channels_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    for channel in channels {
        let cursor = match read_cursor(&channel) {
            Some(c) => Some(c),
            // No cursor yet (a channel from before cursors existed): baseline
            // it from the channel object — guild_id is load-bearing (a guild
            // message replayed without it would read as a DM, and DMs
            // auto-trust), and last_message_id marks "caught up to now"
            // without replaying pre-cursor history. Discord ids are pure
            // digits; term/slack channel dirs are not.
            None if !channel.is_empty() && channel.bytes().all(|b| b.is_ascii_digit()) => {
                let Ok(res) = client
                    .get(format!("{}/channels/{channel}", base()))
                    .header("authorization", format!("Bot {token}"))
                    .send()
                    .await
                else {
                    continue;
                };
                if !res.status().is_success() {
                    continue;
                }
                let Ok(v) = res.json::<Value>().await else {
                    continue;
                };
                let guild = v.get("guild_id").and_then(Value::as_str).map(String::from);
                match v.get("last_message_id").and_then(Value::as_str) {
                    Some(last) => {
                        write_cursor(&channel, last, guild.as_deref());
                        None // baselined; nothing to replay this round
                    }
                    None => None,
                }
            }
            None => None,
        };
        let Some((mut after, guild)) = cursor else {
            continue;
        };
        if out_of_scope(worker, credential, guild.as_deref(), &channel) {
            continue;
        }
        // Bounded pages per channel: a week of backlog lands; an unbounded
        // flood doesn't hold the whole catch-up hostage.
        for _page in 0..3 {
            let res = client
                .get(format!(
                    "{}/channels/{channel}/messages?after={after}&limit=100",
                    base()
                ))
                .header("authorization", format!("Bot {token}"))
                .send()
                .await;
            // Lost access (403/404) or a hiccup: skip this channel, never
            // the whole pass.
            let Ok(res) = res else { break };
            if !res.status().is_success() {
                break;
            }
            let Ok(mut msgs) = res.json::<Vec<Value>>().await else {
                break;
            };
            if msgs.is_empty() {
                break;
            }
            let full_page = msgs.len() == 100;
            msgs.reverse(); // REST returns newest first; handle oldest first
            for mut m in msgs {
                if let Some(g) = &guild {
                    // REST message objects carry no guild_id (only gateway
                    // events do); the scope check and DM detection need it.
                    m["guild_id"] = json!(g);
                }
                if let Some(id) = m["id"].as_str() {
                    after = id.to_string();
                    write_cursor(&channel, id, guild.as_deref());
                }
                // Roles can't be resolved from REST payloads (no member
                // data); the empty guild map resolves conservatively.
                handle_message(worker, &m, bot_id, &HashMap::new(), token, credential).await;
            }
            if !full_page {
                break;
            }
        }
    }
}

pub async fn run_gateway(worker: &str, token: &str, credential: &str) {
    let mut resume: Option<ResumeState> = None;
    loop {
        match connect_once(worker, token, credential, resume.take()).await {
            Ok(()) => {}
            Err(e) if e.fatal => {
                eprintln!("discord gateway: {} — stopping.", e.msg);
                return;
            }
            Err(e) => {
                resume = e.resume;
                // A resume can retry promptly; a fresh reconnect waits longer.
                let delay = if resume.is_some() { 1 } else { 5 };
                eprintln!("discord gateway: {} — reconnecting in {delay}s", e.msg);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
}

async fn connect_once(
    worker: &str,
    token: &str,
    credential: &str,
    resume: Option<ResumeState>,
) -> Result<(), GwError> {
    let transient = |m: String| GwError {
        fatal: false,
        msg: m,
        resume: None,
    };
    // Resume against the session's own gateway url; a fresh session uses the
    // well-known one.
    let dial = resume
        .as_ref()
        .map(|r| format!("{}/?v=10&encoding=json", r.url.trim_end_matches('/')))
        .unwrap_or_else(|| GATEWAY_URL.to_string());
    let (ws, _) = tokio_tungstenite::connect_async(&dial)
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

    let seq: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(resume.as_ref().map(|r| r.seq)));
    let mut heartbeat: Option<tokio::task::JoinHandle<()>> = None;
    let mut bot_id = String::new();
    let mut app_id = String::new();
    let mut guilds: HashMap<String, Guild> = HashMap::new();
    // Carried across a resume so a resumable drop can re-supply them; a fresh
    // READY overwrites them.
    let mut session_id = resume
        .as_ref()
        .map(|r| r.session_id.clone())
        .unwrap_or_default();
    let mut resume_url = resume.as_ref().map(|r| r.url.clone()).unwrap_or_default();
    let mut can_resume = true;
    // A live gateway always sends traffic (at least heartbeat ACKs) within a
    // heartbeat interval; prolonged silence means a half-open connection that
    // TCP alone wouldn't notice for ~15 min. Tightened once HELLO gives us the
    // real interval.
    let mut read_timeout = Duration::from_secs(90);

    let result: Result<(), GwError> = loop {
        let msg = match tokio::time::timeout(read_timeout, stream.next()).await {
            Ok(Some(m)) => m,
            Ok(None) => break Err(transient("stream ended".into())),
            Err(_) => {
                break Err(transient(
                    "no gateway traffic within the heartbeat window — connection is half-open"
                        .into(),
                ))
            }
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
                    break Err(GwError { fatal: true, resume: None, msg: "a privileged intent (MESSAGE CONTENT) isn't enabled — Developer Portal → Bot → Privileged Gateway Intents".into() });
                }
                if code == 4004 {
                    break Err(GwError {
                        fatal: true,
                        resume: None,
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
                // HELLO → start heartbeat, then RESUME (op 6) if we're
                // reconnecting a live session, else IDENTIFY (op 2) fresh.
                let interval = v["d"]["heartbeat_interval"].as_u64().unwrap_or(45_000);
                // Expect traffic within ~2 heartbeats; else the link is dead.
                read_timeout = Duration::from_millis(interval * 2 + 5_000);
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
                let frame = match &resume {
                    Some(r) => json!({ "op": 6, "d": {
                        "token": token, "session_id": r.session_id, "seq": r.seq,
                    }}),
                    None => json!({ "op": 2, "d": {
                        "token": token, "intents": INTENTS,
                        "properties": { "os": "linux", "browser": "roster", "device": "roster" },
                    }}),
                };
                let _ = tx.send(Ws::text(frame.to_string()));
            }
            1 => {
                let s = *seq.lock().unwrap();
                let _ = tx.send(Ws::text(json!({ "op": 1, "d": s }).to_string()));
            }
            // op 7: reconnect and resume. op 9: session invalidated — a resume
            // can't be replayed, so the next dial must start fresh.
            7 => break Err(transient("gateway asked to reconnect (op 7)".into())),
            9 => {
                can_resume = false;
                break Err(transient("session invalidated (op 9)".into()));
            }
            0 => {
                let d = &v["d"];
                match v["t"].as_str().unwrap_or("") {
                    "READY" => {
                        bot_id = d["user"]["id"].as_str().unwrap_or("").to_string();
                        app_id = d["application"]["id"].as_str().unwrap_or("").to_string();
                        // Capture what a later reconnect needs to RESUME.
                        session_id = d["session_id"].as_str().unwrap_or("").to_string();
                        resume_url = d["resume_gateway_url"].as_str().unwrap_or("").to_string();
                        eprintln!(
                            "discord: connected as {} ({bot_id})",
                            d["user"]["username"].as_str().unwrap_or("?")
                        );
                        register_dm_commands(&app_id, token).await;
                        // A fresh session replays nothing — go fetch what
                        // arrived while no listener was connected. Off the
                        // read loop: catch-up REST calls and session wakes
                        // must not stall heartbeats.
                        let (w, t, c, b) = (
                            worker.to_string(),
                            token.to_string(),
                            credential.to_string(),
                            bot_id.clone(),
                        );
                        tokio::spawn(async move {
                            catch_up(&w, &t, &c, &b).await;
                        });
                    }
                    "RESUMED" => eprintln!("discord: resumed session (missed events replayed)"),
                    "GUILD_CREATE" => {
                        let g = ingest_guild(d);
                        eprintln!(
                            "discord: guild \"{}\" — {} channels visible",
                            g.name, g.channels
                        );
                        let guild_id = d["id"].as_str().unwrap_or("").to_string();
                        // A scoped listener doesn't advertise commands in
                        // guilds it won't act in: the guild must be admitted
                        // by scope, or contain a scoped channel (the payload
                        // lists the guild's channels). Ingest either way, for
                        // name resolution in logs.
                        let guild_admitted = match crate::config::snapshot() {
                            Err(_) => false,
                            Ok(cfg) => {
                                match cfg.connections.iter().find(|c| c.name == credential) {
                                    None => true,
                                    Some(c) => match c.grant_for(worker) {
                                        // A file that grants this worker nothing
                                        // admits nothing.
                                        None => false,
                                        Some(r) if r.is_empty() => true,
                                        Some(_) => {
                                            c.allows_surface(
                                                worker,
                                                Some(&guild_id),
                                                "",
                                                crate::config::SurfaceClass::Unknown,
                                            ) || d["channels"]
                                                .as_array()
                                                .map(|chs| {
                                                    chs.iter()
                                                        .filter_map(|ch| ch["id"].as_str())
                                                        .any(|id| {
                                                            c.allows_surface(
                                                                worker,
                                                                None,
                                                                id,
                                                                channel_class(id),
                                                            )
                                                        })
                                                })
                                                .unwrap_or(false)
                                        }
                                    },
                                }
                            }
                        };
                        if guild_admitted {
                            register_commands(&app_id, &guild_id, token).await;
                        }
                        guilds.insert(guild_id, g);
                    }
                    "MESSAGE_CREATE" => {
                        // The replay cursor advances on every in-scope message
                        // seen live — bot-authored ones included, so catch-up
                        // never refetches the bot's own replies.
                        let ch = d["channel_id"].as_str().unwrap_or("");
                        let gid = d["guild_id"].as_str();
                        if !ch.is_empty() && !out_of_scope(worker, credential, gid, ch) {
                            if let Some(id) = d["id"].as_str() {
                                write_cursor(ch, id, gid);
                            }
                        }
                        handle_message(worker, d, &bot_id, &guilds, token, credential).await
                    }
                    "INTERACTION_CREATE" => {
                        // The same attachment rule as messages: an interaction
                        // from an out-of-scope surface doesn't exist for us.
                        if out_of_scope(
                            worker,
                            credential,
                            d["guild_id"].as_str(),
                            d["channel_id"].as_str().unwrap_or(""),
                        ) {
                            continue;
                        }
                        // Handle interactions off the read loop: a slow command
                        // (e.g. an approval that sends email) must not delay the
                        // NEXT interaction's 3-second deferral deadline.
                        let (w, aid) = (worker.to_string(), app_id.clone());
                        let (gs, dd) = (guilds.clone(), d.clone());
                        tokio::spawn(async move {
                            handle_interaction(&w, &dd, &gs, &aid).await;
                        });
                    }
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
    // On a resumable drop, hand the session details forward so the next dial
    // RESUMEs and Discord replays whatever we missed. op 9 / a fresh start
    // clears can_resume, forcing a clean IDENTIFY.
    result.map_err(|mut e| {
        if !e.fatal && can_resume && !session_id.is_empty() && !resume_url.is_empty() {
            let seq = seq.lock().unwrap().unwrap_or(0);
            e.resume = Some(ResumeState {
                session_id,
                seq,
                url: resume_url,
            });
        }
        e
    })
}

/// The bot's own username (`GET /users/@me`) — the connections wizard
/// derives the connection's name from it ("discord-looper").
pub async fn bot_username(token: &str) -> Result<String, String> {
    let res = reqwest::Client::new()
        .get(format!("{}/users/@me", base()))
        .header("authorization", format!("Bot {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("users/@me → {}", res.status()));
    }
    let v: Value = res.json().await.map_err(|e| e.to_string())?;
    match v["username"].as_str() {
        Some(name) if !name.is_empty() => Ok(name.to_string()),
        _ => Err("users/@me returned no username".into()),
    }
}

/// The app's owner (user id, username) via the bot token
/// (`GET /oauth2/applications/@me`) — the connections wizard's default
/// recipient for message_user DMs. A team-owned app names the team's
/// owner instead (its own owner field is a team pseudo-user).
pub async fn app_owner(token: &str) -> Result<(String, String), String> {
    let res = reqwest::Client::new()
        .get(format!("{}/oauth2/applications/@me", base()))
        .header("authorization", format!("Bot {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("applications/@me → {}", res.status()));
    }
    let v: Value = res.json().await.map_err(|e| e.to_string())?;
    if let Some(team) = v.get("team").filter(|t| !t.is_null()) {
        let id = team["owner_user_id"].as_str().unwrap_or("");
        if id.is_empty() {
            return Err("team-owned app names no owner_user_id".into());
        }
        let name = team["members"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["user"]["id"].as_str() == Some(id))
            .and_then(|m| m["user"]["username"].as_str())
            .unwrap_or("")
            .to_string();
        return Ok((id.to_string(), name));
    }
    match v["owner"]["id"].as_str() {
        Some(id) if !id.is_empty() => Ok((
            id.to_string(),
            v["owner"]["username"].as_str().unwrap_or("").to_string(),
        )),
        _ => Err("application object names no owner".into()),
    }
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
    // ls/show` can say "#general @ rototo" instead of a snowflake id — and
    // its permission overwrites, from which the listener classifies the
    // surface: a channel whose overwrites deny VIEW_CHANNEL to @everyone
    // (role id == guild id) is "private", the rest are "public". Scope
    // evaluation consults this classification.
    let guild_name = d["name"].as_str().unwrap_or("");
    if let Some(channels) = d["channels"].as_array() {
        for c in channels {
            let (Some(id), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else {
                continue;
            };
            const VIEW_CHANNEL: u64 = 1 << 10;
            let hidden_from_everyone = c["permission_overwrites"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|o| {
                    o["id"].as_str() == Some(guild_id)
                        && o["deny"]
                            .as_str()
                            .and_then(|s| s.parse::<u64>().ok())
                            .is_some_and(|deny| deny & VIEW_CHANNEL != 0)
                });
            write_channel_meta(
                id,
                &serde_json::json!({
                    "platform": "discord",
                    "server": guild_name,
                    "name": name,
                    "class": if hidden_from_everyone { "private" } else { "public" },
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
    credential: &str,
) {
    // Never react to bots (including ourselves) — avoids reply loops.
    if d["author"]["bot"].as_bool().unwrap_or(false) {
        return;
    }
    // The surface is where the message physically arrived; the channel is
    // the conversation it belongs to — identical until an operator links.
    let surface_id = d["channel_id"].as_str().unwrap_or("");
    if surface_id.is_empty() {
        return;
    }
    let channel_id = crate::channel::links::logical_of(surface_id);
    let channel_id = channel_id.as_str();
    // Attachment rule: an out-of-scope surface doesn't exist for this
    // listener — not persisted, not answered, no attachments fetched.
    if out_of_scope(worker, credential, d["guild_id"].as_str(), surface_id) {
        return;
    }
    let is_dm = d["guild_id"].as_str().is_none();
    if is_dm {
        if let Err(e) = set_channel_trust(surface_id, true) {
            // DMs are always trusted (1:1, sought-out); a failure here isn't
            // user-facing, but it must not pass unnoticed.
            eprintln!("discord: could not mark DM channel {surface_id} trusted: {e}");
        }
        write_channel_meta(
            surface_id,
            &json!({
                "platform": "discord",
                "name": format!("DM with {}", d["author"]["username"].as_str().unwrap_or("?")),
                "class": "dm",
            }),
        );
    }
    let role = resolve_role(d, guilds);
    let author = d["author"]["username"].as_str().unwrap_or("?");
    let content = d["content"].as_str().unwrap_or("");

    // Wake rule: a DM, an @mention, or a channel in "all" mode. In "mention"
    // mode ambient messages are persisted but don't spawn a run. Evaluated
    // before persisting so a waking message can snapshot the history it is
    // about to join: the listener handles its events one at a time, so
    // everything on file right now is exactly "the channel before this
    // message" — no id matching, no race.
    let mentioned = d["mentions"]
        .as_array()
        .map(|m| m.iter().any(|u| u["id"].as_str() == Some(bot_id)))
        .unwrap_or(false);
    let wakes = is_dm || mentioned || channel_mode(channel_id) == "all";
    let history = if wakes {
        recent_messages(channel_id, HISTORY_SNAPSHOT_MAX)
    } else {
        Vec::new()
    };

    // Persist to the channel's history and download any attachments.
    let record = json!({
        // Discord's own send time, so caught-up messages land in history
        // with when they were SAID, not when the listener finally saw them.
        "ts": d["timestamp"].as_str().map(String::from).unwrap_or_else(now_rfc3339),
        "author_id": d["author"]["id"].as_str().unwrap_or(""),
        "author": author, "role": role, "content": content,
        "attachments": d["attachments"].as_array().map(|a| a.iter().filter_map(|x| x["filename"].as_str()).collect::<Vec<_>>()).unwrap_or_default(),
    });
    persist_message(channel_id, &record);
    download_attachments(channel_id, &d["attachments"]).await;

    if !wakes {
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
    // The host names the reply surface on EVERY turn — in a linked channel
    // because the reply goes where the person spoke THIS time, and in a
    // singleton channel because a model that only saw the instruction in the
    // system block sometimes answers in plain text, which a chat session
    // silently drops (reply_tx is None; the reply must be the send action).
    let routing = if channel_id != surface_id {
        format!(" [arrived via Discord — reply with discord_send to channel id {surface_id}]")
    } else {
        format!(" [reply via discord_send to channel id {surface_id}]")
    };
    let text = format!("{content}{hint}{routing}");
    let context = crate::worker::memory::RunContext {
        provider: "discord".into(),
        channel_id: Some(channel_id.to_string()),
        surface_id: Some(surface_id.to_string()),
        user_id: d["author"]["id"].as_str().map(String::from),
        message_id: d["id"].as_str().map(String::from),
        thread_ts: None, // Discord has no Slack-style thread ts
        role: role.to_string(),
        is_dm,
        inbound: false, // live channel context carries ids; inbound marks relay tasks
    };
    eprintln!("discord: {author} ({role}) in {channel_id} → session");
    route_to_session(
        worker,
        channel_id,
        surface_id,
        author.to_string(),
        text,
        context,
        history,
        token,
    )
    .await;
}

/// How many records a waking message snapshots for a fresh session's first
/// turn. Generous on purpose — the context policy's `history_max_messages` /
/// `history_max_chars` do the real trimming at compile time.
pub(crate) const HISTORY_SNAPSHOT_MAX: usize = 50;

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
    surface_id: &str,
    author_label: String,
    text: String,
    context: crate::worker::memory::RunContext,
    history: Vec<Value>,
    token: &str,
) {
    let start_context = context.clone();
    // Key sessions by (worker, LOGICAL channel): two workers can share a
    // channel, and a channel-only key would route one worker's traffic into
    // the other's box. Linked same-provider surfaces land in one session.
    // Typing indicators and failure notes go to the SURFACE the person is
    // actually looking at.
    let key = session_key(worker, channel_id);
    let message = crate::run::boxed::SessionMessage {
        text,
        author_label,
        context,
        history,
    };
    let delivered = {
        let map = sessions().lock().unwrap();
        match map.get(&key) {
            Some(tx) => tx
                .try_send(crate::run::boxed::SessionMessage {
                    text: message.text.clone(),
                    author_label: message.author_label.clone(),
                    context: message.context.clone(),
                    // A live session already holds its own turns.
                    history: Vec::new(),
                })
                .is_ok(),
            None => false,
        }
    };
    spawn_typing(surface_id, token);
    if delivered {
        return;
    }
    // Start a new session (clears any stale closed sender).
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::run::boxed::SessionMessage>(64);
    let _ = tx.try_send(message);
    sessions().lock().unwrap().insert(key.clone(), tx);
    let (w, run_id) = (worker.to_string(), crate::run::boxed::new_run_id());
    let (channel_owned, token_owned) = (surface_id.to_string(), token.to_string());
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
            if map
                .get(&session_map_key)
                .map(|tx| tx.is_closed())
                .unwrap_or(false)
            {
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

/// The in-flight typing loop per (bot, channel), so posting a reply can abort
/// it instead of letting it re-light the indicator after the message landed.
/// Keyed by token as well as channel because two workers (two bots) can share
/// a channel, and one bot's reply must not stop the other's indicator.
fn typing_tasks() -> &'static Mutex<HashMap<String, tokio::task::AbortHandle>> {
    static T: OnceLock<Mutex<HashMap<String, tokio::task::AbortHandle>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn typing_key(token: &str, channel_id: &str) -> String {
    format!("{token}\u{1f}{channel_id}")
}

/// Show the typing indicator while the worker thinks (each trigger lasts ~10s,
/// so the loop keeps it lit for ~30s). Posting a reply aborts the loop via
/// `stop_typing`; without that, a fast reply would be followed by the next
/// trigger re-lighting the indicator on an already-answered channel.
fn spawn_typing(channel_id: &str, token: &str) {
    let (ch, tok) = (channel_id.to_string(), token.to_string());
    let handle = tokio::spawn(async move {
        for _ in 0..4 {
            trigger_typing(&ch, &tok).await;
            tokio::time::sleep(Duration::from_secs(8)).await;
        }
    });
    let mut map = typing_tasks().lock().unwrap();
    map.retain(|_, h| !h.is_finished());
    if let Some(prev) = map.insert(typing_key(token, channel_id), handle.abort_handle()) {
        prev.abort();
    }
}

/// Abort the channel's typing loop, if one is running.
fn stop_typing(token: &str, channel_id: &str) {
    if let Some(h) = typing_tasks()
        .lock()
        .unwrap()
        .remove(&typing_key(token, channel_id))
    {
        h.abort();
    }
}

async fn trigger_typing(channel_id: &str, token: &str) {
    let _ = reqwest::Client::new()
        .post(format!("{}/channels/{channel_id}/typing", base()))
        .header("authorization", format!("Bot {token}"))
        .send()
        .await;
}

// ── slash commands (the admin surface) ────────────────────────────────────────

/// Command definitions, registered per guild (instant, unlike global) and
/// globally for the bot's DMs, where guild commands never appear. Scoped to
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
            { "type": 1, "name": "show", "description": "Queue and gates at a glance" },
            { "type": 1, "name": "trust", "description": "Per-action trust and earned history" }
        ]},
        { "name": "channel", "description": "Channel settings", "options": [
            { "type": 1, "name": "show", "description": "This channel's settings" },
            { "type": 1, "name": "trust", "description": "Mark this channel's participants trusted" },
            { "type": 1, "name": "untrust", "description": "Mark this channel's participants untrusted" },
            { "type": 1, "name": "mode", "description": "How the worker wakes here", "options": [
                { "type": 3, "name": "mode", "description": "all = every message, mention = only when @mentioned", "required": true,
                  "choices": [{ "name": "all", "value": "all" }, { "name": "mention", "value": "mention" }] }
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

/// The same commands, registered globally but limited to the bot's DMs
/// (contexts: [1] = BOT_DM). Guild channels are covered by the per-guild
/// registration above; without the context limit the global copies would
/// show up there too, as duplicates.
async fn register_dm_commands(app_id: &str, token: &str) {
    if app_id.is_empty() {
        return;
    }
    let mut defs = command_defs();
    if let Some(list) = defs.as_array_mut() {
        for cmd in list {
            cmd["contexts"] = json!([1]);
            cmd["integration_types"] = json!([0]);
        }
    }
    let res = reqwest::Client::new()
        .put(format!("{}/applications/{app_id}/commands", base()))
        .header("authorization", format!("Bot {token}"))
        .json(&defs)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("discord: register DM commands → {}", r.status()),
        Err(e) => eprintln!("discord: register DM commands failed: {e}"),
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

async fn handle_interaction(
    worker: &str,
    d: &Value,
    guilds: &HashMap<String, Guild>,
    app_id: &str,
) {
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
        channel_id: Some(crate::channel::links::logical_of(channel_id)).filter(|s| !s.is_empty()),
        surface_id: Some(channel_id.to_string()).filter(|s| !s.is_empty()),
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
        sub: data["options"][0]["name"]
            .as_str()
            .unwrap_or("")
            .to_string(),
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
}

fn default_mode() -> String {
    "all".to_string()
}

impl Default for ChannelSettings {
    fn default() -> Self {
        Self {
            trusted: false,
            mode: default_mode(),
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
fn mutate_settings(f: impl FnOnce(&mut HashMap<String, ChannelSettings>)) -> Result<(), String> {
    let _lock = crate::statefile::FileLock::acquire("channels")
        .map_err(|e| format!("channel settings lock: {e}"))?;
    let mut m = load_settings();
    f(&mut m);
    save_settings(&m)
}

/// Is this channel marked trusted? (DMs are marked trusted when first seen.)
/// Settings key on the LOGICAL channel: a surface id resolves to its linked
/// channel first, so linked surfaces share one designation.
pub fn channel_trusted(channel_id: &str) -> bool {
    let channel_id = crate::channel::links::logical_of(channel_id);
    load_settings()
        .get(&channel_id)
        .map(|s| s.trusted)
        .unwrap_or(false)
}

pub fn set_channel_trust(channel_id: &str, trusted: bool) -> Result<(), String> {
    let channel_id = crate::channel::links::logical_of(channel_id);
    // Hot path: DMs re-assert trust on every message. Skip the locked write when
    // nothing changes (the read sees a whole file thanks to atomic writes).
    if channel_trusted(&channel_id) == trusted {
        return Ok(());
    }
    mutate_settings(|m| {
        m.entry(channel_id).or_default().trusted = trusted;
    })
}

/// The channel's response mode: "all" (default) or "mention".
pub fn channel_mode(channel_id: &str) -> String {
    let channel_id = crate::channel::links::logical_of(channel_id);
    load_settings()
        .get(&channel_id)
        .map(|s| s.mode.clone())
        .unwrap_or_else(default_mode)
}

pub fn set_channel_mode(channel_id: &str, mode: &str) -> Result<(), String> {
    let channel_id = crate::channel::links::logical_of(channel_id);
    let mode = mode.to_string();
    mutate_settings(|m| {
        m.entry(channel_id).or_default().mode = mode;
    })
}

pub fn channel_settings_all() -> HashMap<String, ChannelSettings> {
    load_settings()
}

/// Drop a channel's settings entry (`channel rm`) — the id reverts to the
/// defaults (untrusted, mode=all). Returns whether an entry existed.
pub fn remove_channel_settings(channel_id: &str) -> Result<bool, String> {
    let mut existed = false;
    mutate_settings(|m| {
        existed = m.remove(channel_id).is_some();
    })?;
    Ok(existed)
}

/// Fold newly linked surfaces' settings entries into their logical
/// channel's (channel link): member entries are removed — reads resolve to
/// the channel from here on — the strictest wake mode survives, and the
/// channel is trusted (v1 links 1:1 surfaces only, which are trusted by
/// definition; docs/channels.md).
pub fn absorb_settings(logical: &str, members: &[String]) -> Result<(), String> {
    mutate_settings(|m| {
        let mut mention = m.get(logical).map(|s| s.mode == "mention").unwrap_or(false);
        for sid in members {
            if let Some(s) = m.remove(sid) {
                mention = mention || s.mode == "mention";
            }
        }
        let entry = m.entry(logical.to_string()).or_default();
        entry.trusted = true;
        if mention {
            entry.mode = "mention".into();
        }
    })
}

// ── channel store (history + uploads), under the read-only repo mount ─────────

/// The LOGICAL channel's directory: conversation material (history, files,
/// purpose) follows the conversation, so a linked surface's material lands
/// in its channel's dir. Surface-keyed artifacts (meta, replay cursors) call
/// `paths::channel_dir` directly and never resolve.
fn channel_dir(channel_id: &str) -> PathBuf {
    paths::channel_dir(&crate::channel::links::logical_of(channel_id))
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

/// Distinct human authors seen in a channel's history, to tell a 1:1
/// conversation from a group one. The worker's own replies are on file too
/// (role "worker" — the send executors record them); they don't make a
/// conversation a group.
pub(crate) fn distinct_human_authors(channel_id: &str) -> usize {
    use std::collections::HashSet;
    recent_messages(channel_id, 500)
        .iter()
        .filter(|m| m["role"].as_str() != Some("worker"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn replay_cursor_roundtrips_with_and_without_guild() {
        let _guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());

        // A guild channel keeps its guild id — the field that stops a
        // caught-up guild message from reading as an auto-trusted DM.
        write_cursor("111", "9001", Some("g1"));
        assert_eq!(
            read_cursor("111"),
            Some(("9001".to_string(), Some("g1".to_string())))
        );
        // A DM has none, and stays that way.
        write_cursor("222", "9002", None);
        assert_eq!(read_cursor("222"), Some(("9002".to_string(), None)));
        // Advancing overwrites in place.
        write_cursor("111", "9010", Some("g1"));
        assert_eq!(read_cursor("111").unwrap().0, "9010");
        // No cursor file → no catch-up claim.
        assert_eq!(read_cursor("333"), None);
    }

    /// A minimal Discord REST stand-in: counts typing triggers, answers message
    /// posts with an id. One connection per request (the client is per-call).
    async fn mock_discord(typing_posts: Arc<AtomicUsize>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let counter = typing_posts.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    // Read to the header terminator; these requests are small
                    // enough that any body arrives in the same segment.
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let reply = if head.contains("/typing") {
                        counter.fetch_add(1, Ordering::SeqCst);
                        "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n".to_string()
                    } else {
                        let body = r#"{"id":"123"}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn reply_stops_the_typing_loop() {
        let typing_posts = Arc::new(AtomicUsize::new(0));
        let base = mock_discord(typing_posts.clone()).await;
        std::env::set_var("DISCORD_API_BASE", &base);

        spawn_typing("chan1", "tok");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while typing_posts.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "typing never triggered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let handle = typing_tasks()
            .lock()
            .unwrap()
            .get(&typing_key("tok", "chan1"))
            .expect("typing task registered")
            .clone();

        let id = post_chunked("tok", "chan1", "hello").await.unwrap();
        assert_eq!(id, "123");
        assert!(
            typing_tasks()
                .lock()
                .unwrap()
                .get(&typing_key("tok", "chan1"))
                .is_none(),
            "reply should clear the typing task entry"
        );
        // Without the abort the loop runs ~30s more; finishing well inside the
        // 8s to its next trigger proves the reply stopped it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "typing loop survived the reply"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(typing_posts.load(Ordering::SeqCst), 1);
    }
}
