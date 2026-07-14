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
