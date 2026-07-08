//! Owned OAuth refresh (ports `src/providers.ts`). Per-provider public
//! constants + a standard `refresh_token` grant; no dependency on pi's code.
//! See docs/rust-port.md (P3).

use crate::proxy::BErr;
use crate::util::now_ms;
use serde_json::Value;

#[derive(Clone, Copy)]
pub enum Encoding {
    Form,
    Json,
}

pub struct ProviderConfig {
    pub token_url: &'static str,
    pub client_id: &'static str,
    pub encoding: Encoding,
    pub skew_ms: i64,
}

pub fn provider(name: &str) -> Option<ProviderConfig> {
    match name {
        "openai-codex" => Some(ProviderConfig {
            token_url: "https://auth.openai.com/oauth/token",
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
            encoding: Encoding::Form,
            skew_ms: 0,
        }),
        "anthropic" => Some(ProviderConfig {
            token_url: "https://platform.claude.com/v1/oauth/token",
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            encoding: Encoding::Json,
            skew_ms: 5 * 60 * 1000,
        }),
        _ => None,
    }
}

pub struct RefreshedTokens {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
}

/// Map a provider token-endpoint response to our credential fields. Pure aside
/// from `now_ms`. Errors on a malformed response so the caller fails closed.
pub fn map_token_response(cfg: &ProviderConfig, json: &Value) -> Result<RefreshedTokens, BErr> {
    let access = json.get("access_token").and_then(|v| v.as_str());
    let refresh = json.get("refresh_token").and_then(|v| v.as_str());
    let expires_in = json.get("expires_in").and_then(|v| v.as_f64());
    match (access, refresh, expires_in) {
        (Some(a), Some(r), Some(e)) => Ok(RefreshedTokens {
            access: a.to_string(),
            refresh: r.to_string(),
            expires: now_ms() + (e as i64) * 1000 - cfg.skew_ms,
        }),
        _ => Err(format!("token response missing fields: {json}").into()),
    }
}

/// Perform the `refresh_token` grant. A direct host-side call (the gateway's own
/// trusted action). Throws on any failure — the caller must fail closed.
pub async fn refresh(name: &str, refresh_token: &str) -> Result<RefreshedTokens, BErr> {
    let cfg = provider(name).ok_or_else(|| format!("no refresh config for provider \"{name}\""))?;
    let client = reqwest::Client::new();
    let req = match cfg.encoding {
        Encoding::Form => client.post(cfg.token_url).form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", cfg.client_id),
        ]),
        Encoding::Json => client.post(cfg.token_url).json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": cfg.client_id,
        })),
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("refresh request to {} failed: {e}", cfg.token_url))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet = &text[..text.len().min(200)];
        return Err(format!("refresh for \"{name}\" returned {status}: {snippet}").into());
    }
    let json: Value = serde_json::from_str(&text).map_err(|_| format!("refresh for \"{name}\" returned non-JSON"))?;
    map_token_response(&cfg, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_fields_and_applies_skew() {
        let cfg = provider("openai-codex").unwrap();
        let before = now_ms();
        let out = map_token_response(
            &cfg,
            &serde_json::json!({"access_token":"a","refresh_token":"r","expires_in":3600}),
        )
        .unwrap();
        assert_eq!(out.access, "a");
        assert_eq!(out.refresh, "r");
        assert!(out.expires >= before + 3_600_000 && out.expires <= now_ms() + 3_600_000);
    }

    #[test]
    fn anthropic_subtracts_its_skew() {
        let cfg = provider("anthropic").unwrap();
        let out = map_token_response(
            &cfg,
            &serde_json::json!({"access_token":"a","refresh_token":"r","expires_in":3600}),
        )
        .unwrap();
        assert!(out.expires <= now_ms() + 3_600_000 - 5 * 60 * 1000);
    }

    #[test]
    fn malformed_response_fails_closed() {
        let cfg = provider("openai-codex").unwrap();
        assert!(map_token_response(&cfg, &serde_json::json!({"access_token":"a"})).is_err());
        assert!(map_token_response(&cfg, &serde_json::json!({"error":"invalid_grant"})).is_err());
    }
}
