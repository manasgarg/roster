//! The vault + injection (ports `src/vault.ts`). Credentials live at
//! `~/.roster/vault/<name>.json` (override `ROSTER_VAULT_DIR`), off the box.
//! `get_fresh_credential` refreshes an expired OAuth token before returning it
//! (single-flight per credential, atomic write, audit to runs/credentials.jsonl,
//! fail-closed). See docs/rust-port.md (P3).

use crate::providers;
use crate::proxy::BErr;
use crate::util::{now_ms, root};
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

fn vault_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ROSTER_VAULT_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".roster").join("vault")
}

pub fn get_credential(name: &str) -> Option<Credential> {
    let path = vault_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.as_object().cloned()
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
/// template references a missing field is skipped.
pub fn render_injection(cred: &Credential, provider_name: &str) -> Vec<(String, String)> {
    let Some(p) = crate::registry::provider(provider_name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for h in &p.inject {
        if let Some(value) = substitute(&h.value, cred) {
            out.push((h.header.clone(), value));
        }
    }
    out
}

/// Fill `{field}` placeholders from the credential. Returns None if any
/// referenced field is missing (so we never inject a half-built value).
fn substitute(template: &str, cred: &Credential) -> Option<String> {
    let mut result = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let close = rest[open..].find('}')? + open;
        let field = &rest[open + 1..close];
        result.push_str(cred.get(field).and_then(|v| v.as_str())?);
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
    let path = root().join("runs").join("credentials.jsonl");
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
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
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
    if !is_oauth(&cred) || cred.get("refresh").and_then(|v| v.as_str()).is_none() || !needs_refresh(&cred, now_ms()) {
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
    let refresh_token = cred.get("refresh").and_then(|v| v.as_str()).unwrap_or_default().to_string();

    match providers::refresh(name, &refresh_token).await {
        Ok(fresh) => {
            let mut merged = cred.clone();
            merged.insert("access".into(), json!(fresh.access));
            merged.insert("refresh".into(), json!(fresh.refresh));
            merged.insert("expires".into(), json!(fresh.expires));
            write_credential(name, &merged)?;
            log_refresh(json!({"ts": crate::util::now_rfc3339(), "event":"refresh", "credential": name, "ok": true, "expires": fresh.expires}));
            Ok(Some(merged))
        }
        Err(e) => {
            log_refresh(json!({"ts": crate::util::now_rfc3339(), "event":"refresh", "credential": name, "ok": false, "error": e.to_string()}));
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
        let no_exp = serde_json::json!({"type":"oauth","access":"a","refresh":"r"}).as_object().unwrap().clone();
        assert!(!needs_refresh(&no_exp, now_ms()));
    }

    #[test]
    fn substitute_fills_fields_and_skips_missing() {
        let cred = serde_json::json!({"access":"tok","accountId":"acc","key":"sk-1"}).as_object().unwrap().clone();
        assert_eq!(substitute("Bearer {access}", &cred), Some("Bearer tok".to_string()));
        assert_eq!(substitute("{accountId}", &cred), Some("acc".to_string()));
        assert_eq!(substitute("{key}", &cred), Some("sk-1".to_string()));
        assert_eq!(substitute("Bearer {missing}", &cred), None); // referenced field absent
    }
}
