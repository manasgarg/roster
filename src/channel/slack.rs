//! Slack — the Web API client for outbound messages and the Socket Mode
//! (WebSocket) client for inbound (docs/channels.md). Trusted-side code
//! holds both tokens (from the vault); the box never does. Socket Mode keeps
//! roster's posture: dial out, never listen on the internet. Base URL is
//! overridable via SLACK_API_BASE so the executor can be tested against a mock.
//!
//! Channel machinery (trust, mode, memory settings, history) is shared with
//! Discord — it is channel-id keyed and platform-agnostic; Slack ids (C…, D…)
//! drop straight in.

use crate::channel::discord::{channel_mode, channel_trusted, persist_message, set_channel_trust};
use crate::util::now_rfc3339;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as Ws;

fn base() -> String {
    std::env::var("SLACK_API_BASE").unwrap_or_else(|_| "https://slack.com/api".into())
}

/// One Web API POST with the bot token. Slack signals failure inside a 200
/// body (`ok: false`), so both layers are checked. Honors a 429 (Retry-After)
/// so a long, chunked reply isn't abandoned half-delivered under rate limiting.
async fn api(token: &str, method: &str, body: Value) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let url = format!("{}/{method}", base());
    for attempt in 0..5 {
        let res = client
            .post(&url)
            .header("authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status();
        if status.as_u16() == 429 {
            let wait = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(1.0);
            if attempt < 4 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(wait.clamp(0.0, 60.0) + 0.05))
                    .await;
                continue;
            }
            return Err(format!("slack {method} rate limited; gave up after retries"));
        }
        let payload: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() || !payload["ok"].as_bool().unwrap_or(false) {
            return Err(format!(
                "slack {method} {status}: {}",
                payload["error"].as_str().unwrap_or("?")
            ));
        }
        return Ok(payload);
    }
    Err(format!("slack {method}: exhausted rate-limit retries"))
}

/// Post a message. Returns the message ts (Slack's message id). `thread_ts`
/// replies inside a thread.
/// Post a message of any length: split at Slack's practical 4000-char
/// display limit on natural boundaries and send the chunks in order.
pub async fn post_chunked(
    token: &str,
    channel_id: &str,
    text: &str,
    thread_ts: Option<&str>,
) -> Result<String, String> {
    let mut last = String::new();
    for chunk in crate::util::chunk_message(text, 4000) {
        last = post_message(token, channel_id, &chunk, thread_ts).await?;
    }
    Ok(last)
}

pub async fn post_message(
    token: &str,
    channel_id: &str,
    text: &str,
    thread_ts: Option<&str>,
) -> Result<String, String> {
    let mut body = json!({ "channel": channel_id, "text": text });
    if let Some(ts) = thread_ts {
        body["thread_ts"] = json!(ts);
    }
    let res = api(token, "chat.postMessage", body).await?;
    Ok(res["ts"].as_str().unwrap_or("").to_string())
}

/// Open (or fetch) a DM with a user. Returns the DM channel id (D…).
pub async fn open_dm(token: &str, user_id: &str) -> Result<String, String> {
    let res = api(token, "conversations.open", json!({ "users": user_id })).await?;
    res["channel"]["id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "DM had no channel id".into())
}

// ── inbound: the Socket Mode client ──────────────────────────────────────────

struct SmError {
    fatal: bool,
    msg: String,
}

/// Run Socket Mode for one worker: dial out, ack envelopes, dispatch events.
/// Reconnects on transient errors (Slack refreshes connections routinely);
/// stops on fatal ones (bad tokens).
pub async fn run_socket_mode(worker: &str, bot_token: &str, app_token: &str) {
    loop {
        match connect_once(worker, bot_token, app_token).await {
            Ok(()) => {} // routine disconnect envelope — reconnect immediately
            Err(e) if e.fatal => {
                eprintln!("slack socket-mode: {} — stopping.", e.msg);
                return;
            }
            Err(e) => {
                eprintln!("slack socket-mode: {} — reconnecting in 5s", e.msg);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn connect_once(worker: &str, bot_token: &str, app_token: &str) -> Result<(), SmError> {
    let transient = |m: String| SmError {
        fatal: false,
        msg: m,
    };
    let fatal = |m: String| SmError {
        fatal: true,
        msg: m,
    };

    // Who are we? (Also validates the bot token.)
    let me = api(bot_token, "auth.test", json!({}))
        .await
        .map_err(|e| fatal(format!("auth.test: {e}")))?;
    let bot_user_id = me["user_id"].as_str().unwrap_or("").to_string();
    let team = me["team"].as_str().unwrap_or("?");

    // A fresh Socket Mode URL each connect (they are single-use).
    let open = api(app_token, "apps.connections.open", json!({}))
        .await
        .map_err(|e| {
            if e.contains("invalid_auth") || e.contains("not_allowed") {
                fatal(format!("apps.connections.open: {e}"))
            } else {
                transient(format!("apps.connections.open: {e}"))
            }
        })?;
    let url = open["url"]
        .as_str()
        .ok_or_else(|| transient("no socket url".into()))?;

    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| transient(format!("connect: {e}")))?;
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ws>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    eprintln!("slack: connected as {bot_user_id} to workspace \"{team}\"");

    let mut admins: HashMap<String, bool> = HashMap::new();
    let result = loop {
        let msg = match stream.next().await {
            Some(Ok(Ws::Text(t))) => t,
            Some(Ok(Ws::Ping(p))) => {
                let _ = tx.send(Ws::Pong(p));
                continue;
            }
            Some(Ok(Ws::Close(_))) | None => break Err(transient("socket closed".into())),
            Some(Ok(_)) => continue,
            Some(Err(e)) => break Err(transient(format!("read: {e}"))),
        };
        let v: Value = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v["type"].as_str().unwrap_or("") {
            // Slack refreshes sockets on a schedule; this is routine, not an error.
            "disconnect" => break Ok(()),
            "hello" => {}
            "events_api" => {
                // Ack FIRST — Slack redelivers unacked envelopes, which would
                // double-file work if handling were slow.
                if let Some(id) = v["envelope_id"].as_str() {
                    let _ = tx.send(Ws::text(json!({ "envelope_id": id }).to_string()));
                }
                let event = &v["payload"]["event"];
                if event["type"].as_str() == Some("message") {
                    // Suppress a redelivered event (ack lost / socket dropped)
                    // so the same message isn't handled — and answered — twice.
                    let event_id = v["payload"]["event_id"].as_str().unwrap_or("");
                    if !event_id.is_empty() && already_seen(event_id) {
                        continue;
                    }
                    handle_message(worker, event, &bot_user_id, bot_token, &mut admins).await;
                }
            }
            _ => {}
        }
    };

    writer.abort();
    result
}

/// Is this Slack user a workspace admin/owner? Cached per connection — the
/// answer changes rarely and users.info costs a round trip.
async fn is_workspace_admin(token: &str, user_id: &str, cache: &mut HashMap<String, bool>) -> bool {
    if let Some(known) = cache.get(user_id) {
        return *known;
    }
    let admin = api(token, "users.info", json!({ "user": user_id }))
        .await
        .map(|r| {
            r["user"]["is_admin"].as_bool().unwrap_or(false)
                || r["user"]["is_owner"].as_bool().unwrap_or(false)
        })
        .unwrap_or(false);
    cache.insert(user_id.to_string(), admin);
    admin
}

async fn handle_message(
    worker: &str,
    event: &Value,
    bot_user_id: &str,
    bot_token: &str,
    admins: &mut HashMap<String, bool>,
) {
    // Never react to bots (including ourselves) — avoids reply loops. Skip
    // subtypes too (edits, joins, thread broadcasts): only fresh plain messages.
    if event["bot_id"].as_str().is_some() || event["subtype"].as_str().is_some() {
        return;
    }
    let user_id = event["user"].as_str().unwrap_or("");
    if user_id.is_empty() || user_id == bot_user_id {
        return;
    }
    let channel_id = event["channel"].as_str().unwrap_or("");
    if channel_id.is_empty() {
        return;
    }
    let is_dm = event["channel_type"].as_str() == Some("im");
    if is_dm {
        // DMs are always trusted (1:1, sought-out).
        if let Err(e) = set_channel_trust(channel_id, true) {
            eprintln!("slack: could not mark DM channel {channel_id} trusted: {e}");
        }
    }
    let role = if is_dm {
        "trusted"
    } else if is_workspace_admin(bot_token, user_id, admins).await {
        "admin"
    } else if channel_trusted(channel_id) {
        "trusted"
    } else {
        "untrusted"
    };
    let text = event["text"].as_str().unwrap_or("");

    persist_message(
        channel_id,
        &json!({
            "ts": now_rfc3339(),
            "slack_ts": event["ts"].as_str().unwrap_or(""),
            "thread_ts": event["thread_ts"].as_str(),
            "author_id": user_id, "author": user_id, "role": role, "content": text,
        }),
    );

    // Wake rule (same as Discord): a DM, an @mention, or a channel in "all"
    // mode. In "mention" mode ambient messages are persisted but don't wake.
    let mentioned = text.contains(&format!("<@{bot_user_id}>"));
    if !(is_dm || mentioned || channel_mode(channel_id) == "all") {
        return;
    }

    let hint = if is_dm || mentioned {
        ""
    } else if crate::channel::discord::distinct_human_authors(channel_id) <= 1 {
        " [you're the only other person here — reply]"
    } else {
        " [group chat; you were not directly addressed — reply only if useful]"
    };
    let context = crate::worker::memory::RunContext {
        provider: "slack".into(),
        channel_id: Some(channel_id.to_string()),
        user_id: Some(user_id.to_string()),
        message_id: event["ts"].as_str().map(String::from),
        // Reply back into the thread the message came from, when it was in one.
        thread_ts: event["thread_ts"].as_str().map(String::from),
        role: role.to_string(),
        is_dm,
        inbound: false, // live channel context carries ids; inbound marks relay tasks
    };
    eprintln!("slack: {user_id} ({role}) in {channel_id} → session");
    route_to_session(
        worker,
        channel_id,
        user_id.to_string(),
        format!("{text}{hint}"),
        context,
        bot_token,
    )
    .await;
}

/// Have we already handled this Slack event id? Bounded and process-global, so it
/// survives the reconnects that cause redelivery. Returns true (and does nothing)
/// on a repeat; records and returns false on a first sighting.
fn already_seen(event_id: &str) -> bool {
    use std::collections::{HashSet, VecDeque};
    static SEEN: OnceLock<Mutex<(HashSet<String>, VecDeque<String>)>> = OnceLock::new();
    let m = SEEN.get_or_init(|| Mutex::new((HashSet::new(), VecDeque::new())));
    let mut g = m.lock().unwrap();
    if g.0.contains(event_id) {
        return true;
    }
    g.0.insert(event_id.to_string());
    g.1.push_back(event_id.to_string());
    if g.1.len() > 2048 {
        if let Some(old) = g.1.pop_front() {
            g.0.remove(&old);
        }
    }
    false
}

// ── conversation sessions: one warm box per active channel ────────────────────

fn sessions(
) -> &'static Mutex<HashMap<String, tokio::sync::mpsc::Sender<crate::run::boxed::SessionMessage>>> {
    static S: OnceLock<
        Mutex<HashMap<String, tokio::sync::mpsc::Sender<crate::run::boxed::SessionMessage>>>,
    > = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

const SESSION_IDLE_SECS: u64 = 90;

/// Deliver a message to the channel's live session, or start a new one —
/// the same warm-box pattern as Discord (idle exit, sender replaced on next
/// message). No typing indicator: Socket Mode has none.
async fn route_to_session(
    worker: &str,
    channel_id: &str,
    author_label: String,
    text: String,
    context: crate::worker::memory::RunContext,
    bot_token: &str,
) {
    let start_context = context.clone();
    // Key by (worker, channel) so two workers sharing a channel don't collide.
    let key = crate::channel::discord::session_key(worker, channel_id);
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
    if delivered {
        return;
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::run::boxed::SessionMessage>(64);
    let _ = tx.try_send(message);
    sessions().lock().unwrap().insert(key.clone(), tx);
    let (w, run_id) = (worker.to_string(), crate::run::boxed::new_run_id());
    let (channel_owned, token_owned) = (channel_id.to_string(), bot_token.to_string());
    let session_map_key = key;
    let thread = start_context.thread_ts.clone();
    tokio::spawn(async move {
        let failed = crate::run::boxed::run_session(
            &w,
            &run_id,
            crate::worker::context::RunSurface::SlackSession,
            start_context,
            rx,
            SESSION_IDLE_SECS,
            None,
        )
        .await
        .err()
        .map(|e| e.to_string());
        {
            let mut map = sessions().lock().unwrap();
            if map.get(&session_map_key).map(|tx| tx.is_closed()).unwrap_or(false) {
                map.remove(&session_map_key);
            }
        }
        if let Some(msg) = failed {
            eprintln!("slack session error: {msg}");
            let _ = post_message(
                &token_owned,
                &channel_owned,
                "⚠️ I couldn't finish that just now — my box failed to start or exited early. Nothing unsaved was kept; please try again in a moment.",
                thread.as_deref(),
            )
            .await;
        }
    });
}
