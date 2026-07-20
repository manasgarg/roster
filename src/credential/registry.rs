//! The provider registry. Defaults for known providers ship inside the binary
//! (src/providers.default.json); the admin can override or add providers in
//! `<config>/providers.toml` — a top-level table per provider, replacing the
//! default entry wholesale. Shared by `credential add` and the gateway
//! (refresh constants + the inject spec). See docs/gateway.md.

use crate::paths;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_REGISTRY: &str = include_str!("providers.default.json");

// The gateway declares only the fields it uses (refresh + inject); serde
// ignores the rest (auth, login), which belong to the CLI's `connect`.
#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub token_url: String,
    #[serde(default)]
    pub token_encoding: String,
    #[serde(default)]
    pub skew_ms: i64,
    /// Which headers to inject, and how to build each value from the credential
    /// (e.g. "Bearer {access}"). Generalizes over OAuth and api-key providers.
    #[serde(default)]
    pub inject: Vec<InjectHeader>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct InjectHeader {
    pub header: String,
    pub value: String,
    /// Hosts this entry applies to (exact match); empty = every host the
    /// grant carries. For one header name the last matching entry wins, so
    /// a host-scoped scheme (git's Basic) overrides the provider default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

/// Defaults overlaid with the admin\'s providers.toml (read fresh, so edits
/// are live). Per-provider replace, not deep merge.
pub fn registry_json() -> serde_json::Map<String, Value> {
    let mut map = serde_json::from_str::<Value>(DEFAULT_REGISTRY)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Ok(text) = std::fs::read_to_string(paths::providers_file()) {
        match toml::from_str::<toml::Value>(&text)
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
        {
            Ok(Value::Object(overlay)) => {
                for (name, provider) in overlay {
                    map.insert(name, provider);
                }
            }
            _ => eprintln!("providers.toml is invalid — using built-in defaults only"),
        }
    }
    map
}

/// Look up a provider by name.
pub fn provider(name: &str) -> Option<Provider> {
    registry_json()
        .get(name)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// The uses a provider supports — "capability" (a box acts on the service
/// through a connection file), "channel" (a host-side listener or executor
/// speaks through it), "model" (grants inject it into model-API calls).
/// An explicit `use` array wins; otherwise inferred, so existing
/// providers.toml overlays keep working (docs/connections.md §1).
pub fn provider_uses(p: &Value) -> Vec<String> {
    if let Some(uses) = p.get("use").and_then(Value::as_array) {
        return uses
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
    }
    if p.get("connection").is_some() {
        return vec!["capability".into()];
    }
    match p.get("auth").and_then(Value::as_str) {
        Some("discord") | Some("slack") | Some("smtp") => vec!["channel".into()],
        _ => vec!["model".into()],
    }
}

/// Hidden entries keep old connection files compiling (e.g. the retired
/// `slack-api` alias) without appearing in the catalog.
pub fn is_hidden(p: &Value) -> bool {
    p.get("hidden").and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_are_explicit_or_inferred() {
        let explicit = json!({ "auth": "slack", "use": ["channel", "capability"] });
        assert_eq!(provider_uses(&explicit), vec!["channel", "capability"]);
        let capability = json!({ "auth": "api_key", "connection": { "hosts": ["x"] } });
        assert_eq!(provider_uses(&capability), vec!["capability"]);
        let channel = json!({ "auth": "discord" });
        assert_eq!(provider_uses(&channel), vec!["channel"]);
        let model = json!({ "auth": "oauth" });
        assert_eq!(provider_uses(&model), vec!["model"]);
    }
}
