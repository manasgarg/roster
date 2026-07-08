//! OAuth refresh, driven by the shared provider registry (providers.json) — no
//! dependency on pi's code. See docs/injection-spec.md, docs/rust-port.md.

use crate::proxy::BErr;
use crate::registry;
use crate::util::now_ms;
use serde_json::{json, Value};

pub struct RefreshedTokens {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
}

/// Map a token-endpoint response to our credential fields (applying the
/// provider's early-expiry skew). Pure aside from `now_ms`. Errors on a
/// malformed response so the caller fails closed.
pub fn map_token_response(skew_ms: i64, json: &Value) -> Result<RefreshedTokens, BErr> {
    let access = json.get("access_token").and_then(|v| v.as_str());
    let refresh = json.get("refresh_token").and_then(|v| v.as_str());
    let expires_in = json.get("expires_in").and_then(|v| v.as_f64());
    match (access, refresh, expires_in) {
        (Some(a), Some(r), Some(e)) => Ok(RefreshedTokens {
            access: a.to_string(),
            refresh: r.to_string(),
            expires: now_ms() + (e as i64) * 1000 - skew_ms,
        }),
        _ => Err(format!("token response missing fields: {json}").into()),
    }
}

/// Perform the `refresh_token` grant for a provider (constants from the
/// registry). A direct host-side call; throws on failure so the caller fails
/// closed and never injects a stale token.
pub async fn refresh(name: &str, refresh_token: &str) -> Result<RefreshedTokens, BErr> {
    let p = registry::provider(name).ok_or_else(|| format!("no provider config for \"{name}\""))?;
    if p.token_url.is_empty() {
        return Err(format!("provider \"{name}\" has no token_url for refresh").into());
    }
    let client = reqwest::Client::new();
    let req = if p.token_encoding == "json" {
        client.post(&p.token_url).json(&json!({
            "grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": p.client_id,
        }))
    } else {
        client.post(&p.token_url).form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", p.client_id.as_str()),
        ])
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("refresh request to {} failed: {e}", p.token_url))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("refresh for \"{name}\" returned {status}: {}", &text[..text.len().min(200)]).into());
    }
    let json: Value = serde_json::from_str(&text).map_err(|_| format!("refresh for \"{name}\" returned non-JSON"))?;
    map_token_response(p.skew_ms, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_fields_and_applies_skew() {
        let before = now_ms();
        let out = map_token_response(0, &json!({"access_token":"a","refresh_token":"r","expires_in":3600})).unwrap();
        assert_eq!(out.access, "a");
        assert_eq!(out.refresh, "r");
        assert!(out.expires >= before + 3_600_000 && out.expires <= now_ms() + 3_600_000);
    }

    #[test]
    fn applies_nonzero_skew() {
        let out = map_token_response(300_000, &json!({"access_token":"a","refresh_token":"r","expires_in":3600})).unwrap();
        assert!(out.expires <= now_ms() + 3_600_000 - 300_000);
    }

    #[test]
    fn malformed_response_fails_closed() {
        assert!(map_token_response(0, &json!({"access_token":"a"})).is_err());
    }
}
