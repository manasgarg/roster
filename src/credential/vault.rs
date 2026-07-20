//! The vault + injection. Credentials live at
//! `<data>/vault/<name>.json` (override `ROSTER_VAULT_DIR`), never mounted.
//! `get_fresh_credential` refreshes an expired OAuth token before returning it
//! (single-flight per credential, atomic write, audit to audit/credentials.jsonl,
//! fail-closed). See docs/gateway.md.

use crate::credential::providers;
use crate::gateway::proxy::BErr;
use crate::paths;
use crate::util::now_ms;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// A stored credential, kept as its raw JSON object so a refresh merge preserves
/// unknown fields (accountId, type, …).
pub type Credential = Map<String, Value>;

const REFRESH_SKEW_MS: i64 = 60_000;

pub fn vault_dir() -> PathBuf {
    crate::paths::vault_dir()
}

pub fn get_credential(name: &str) -> Option<Credential> {
    let path = vault_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .as_object()
        .cloned()
}

fn expires(cred: &Credential) -> Option<i64> {
    cred.get("expires").and_then(|v| v.as_i64())
}

/// Is an OAuth credential expired (or within the refresh skew window)?
pub fn needs_refresh(cred: &Credential, now: i64) -> bool {
    match expires(cred) {
        Some(exp) => now >= exp - REFRESH_SKEW_MS,
        None => false,
    }
}

fn is_oauth(cred: &Credential) -> bool {
    cred.get("type").and_then(|v| v.as_str()) == Some("oauth")
}

/// The auth headers to inject, per the provider's registry `inject` spec —
/// each value is a template (e.g. "Bearer {access}") filled from the
/// credential. Generalizes over OAuth and api-key providers. A header whose
/// template references a missing field is skipped. `host` is the request's
/// destination: entries listing hosts apply only there, and for one header
/// name the last matching entry wins.
pub fn render_injection(
    cred: &Credential,
    provider_name: &str,
    host: &str,
) -> Vec<(String, String)> {
    let Some(p) = crate::credential::registry::provider(provider_name) else {
        return Vec::new();
    };
    render_headers(cred, &p.inject, host)
}

pub fn render_headers(
    cred: &Credential,
    headers: &[crate::credential::registry::InjectHeader],
    host: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for h in headers {
        if !h.hosts.is_empty() && !h.hosts.iter().any(|x| x == host) {
            continue;
        }
        if let Some(value) = substitute(&h.value, cred) {
            match out.iter_mut().find(|(k, _)| *k == h.header) {
                Some(slot) => slot.1 = value,
                None => out.push((h.header.clone(), value)),
            }
        }
    }
    out
}

/// Fill `{field}` placeholders from the credential. Returns None if any
/// referenced field is missing (so we never inject a half-built value).
/// `{b64:…}` base64-encodes its (recursively substituted) body — how a
/// template builds Basic auth, e.g. "Basic {b64:x-access-token:{key}}".
fn substitute(template: &str, cred: &Credential) -> Option<String> {
    use base64::Engine as _;
    let mut result = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        // Placeholders may nest inside {b64:…} — take the matching brace.
        let mut depth = 0usize;
        let mut close = None;
        for (i, c) in rest[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?;
        let inner = &rest[open + 1..close];
        match inner.strip_prefix("b64:") {
            Some(body) => result.push_str(
                &base64::engine::general_purpose::STANDARD.encode(substitute(body, cred)?),
            ),
            None => result.push_str(cred.get(inner).and_then(|v| v.as_str())?),
        }
        rest = &rest[close + 1..];
    }
    result.push_str(rest);
    Some(result)
}

/// Atomic vault write: temp + rename, 0600. A half-written rotation would lock
/// us out (the old refresh token is already spent).
fn write_credential(name: &str, cred: &Credential) -> Result<(), BErr> {
    let dir = vault_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.json"));
    let tmp = dir.join(format!("{name}.json.tmp"));
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(cred)?))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn log_refresh(event: Value) {
    let path = paths::credentials_log();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{event}");
    }
}

// One refresh lane per credential: concurrent callers hitting an expired token
// share a lock so the second doesn't fail on the token the first rotated.
fn name_lock(name: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let map = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut m = map.lock().unwrap();
    m.entry(name.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Get a credential guaranteed usable now: refreshes (and persists) if the OAuth
/// token has expired. `Ok(None)` if not in the vault. `Err` if a needed refresh
/// fails — the gateway must then deny, never inject a stale token.
pub async fn get_fresh_credential(name: &str) -> Result<Option<Credential>, BErr> {
    let cred = match get_credential(name) {
        Some(c) => c,
        None => return Ok(None),
    };
    if !is_oauth(&cred)
        || cred.get("refresh").and_then(|v| v.as_str()).is_none()
        || !needs_refresh(&cred, now_ms())
    {
        return Ok(Some(cred));
    }

    // Serialize refreshes per credential; re-check after acquiring the lock in
    // case another task just refreshed.
    let lock = name_lock(name);
    let _guard = lock.lock().await;
    let cred = get_credential(name).unwrap_or(cred);
    if !needs_refresh(&cred, now_ms()) {
        return Ok(Some(cred));
    }
    let refresh_token = cred
        .get("refresh")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    match providers::refresh(name, &refresh_token).await {
        Ok(fresh) => {
            let mut merged = cred.clone();
            merged.insert("access".into(), json!(fresh.access));
            merged.insert("refresh".into(), json!(fresh.refresh));
            merged.insert("expires".into(), json!(fresh.expires));
            write_credential(name, &merged)?;
            log_refresh(
                json!({"ts": crate::util::now_rfc3339(), "event":"refresh", "credential": name, "ok": true, "expires": fresh.expires}),
            );
            Ok(Some(merged))
        }
        Err(e) => {
            log_refresh(
                json!({"ts": crate::util::now_rfc3339(), "event":"refresh", "credential": name, "ok": false, "error": e.to_string()}),
            );
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth(expires: i64) -> Credential {
        serde_json::json!({"type":"oauth","access":"a","refresh":"r","expires":expires})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn needs_refresh_semantics() {
        assert!(needs_refresh(&oauth(now_ms() - 1000), now_ms()));
        assert!(!needs_refresh(&oauth(now_ms() + 3_600_000), now_ms()));
        assert!(needs_refresh(&oauth(now_ms() + 30_000), now_ms())); // within 60s skew
        let no_exp = serde_json::json!({"type":"oauth","access":"a","refresh":"r"})
            .as_object()
            .unwrap()
            .clone();
        assert!(!needs_refresh(&no_exp, now_ms()));
    }

    #[test]
    fn substitute_fills_fields_and_skips_missing() {
        let cred = serde_json::json!({"access":"tok","accountId":"acc","key":"sk-1"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            substitute("Bearer {access}", &cred),
            Some("Bearer tok".to_string())
        );
        assert_eq!(substitute("{accountId}", &cred), Some("acc".to_string()));
        assert_eq!(substitute("{key}", &cred), Some("sk-1".to_string()));
        assert_eq!(substitute("Bearer {missing}", &cred), None); // referenced field absent
    }

    #[test]
    fn substitute_b64_encodes_its_substituted_body() {
        use base64::Engine as _;
        let cred = serde_json::json!({"key":"sk-1"})
            .as_object()
            .unwrap()
            .clone();
        let expect = base64::engine::general_purpose::STANDARD.encode("x-access-token:sk-1");
        assert_eq!(
            substitute("Basic {b64:x-access-token:{key}}", &cred),
            Some(format!("Basic {expect}"))
        );
        assert_eq!(substitute("Basic {b64:{missing}}", &cred), None);
        assert_eq!(substitute("{b64:x-access-token:{key}", &cred), None); // unbalanced braces
    }

    #[test]
    fn render_headers_filters_by_host_and_last_match_wins() {
        let cred = serde_json::json!({"key":"sk-1"})
            .as_object()
            .unwrap()
            .clone();
        let headers: Vec<crate::credential::registry::InjectHeader> =
            serde_json::from_value(serde_json::json!([
                { "header": "authorization", "value": "token {key}" },
                { "header": "authorization", "value": "Basic {key}", "hosts": ["github.com"] },
            ]))
            .unwrap();
        assert_eq!(
            render_headers(&cred, &headers, "api.github.com"),
            vec![("authorization".to_string(), "token sk-1".to_string())]
        );
        assert_eq!(
            render_headers(&cred, &headers, "github.com"),
            vec![("authorization".to_string(), "Basic sk-1".to_string())]
        );
    }
}
