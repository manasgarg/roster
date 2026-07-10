//! Discord REST client for outbound messages. The trusted-side executor holds
//! the bot token (from the vault); the box never does. The inbound gateway
//! (WebSocket) client will join this module later. Base URL is overridable via
//! DISCORD_API_BASE so the executor can be tested against a mock.

use serde_json::{json, Value};

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
