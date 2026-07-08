//! The provider registry (providers.json), shared with the CLI's `connect`.
//! The gateway reads the fields it needs — refresh constants + the inject spec.
//! (login-flow fields are the CLI's; serde ignores them here.) See
//! docs/injection-spec.md.

use crate::util::root;
use serde::Deserialize;
use std::collections::HashMap;

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

#[derive(Debug, Clone, Deserialize)]
pub struct InjectHeader {
    pub header: String,
    pub value: String,
}

/// Look up a provider by name. Read fresh so owner edits are live.
pub fn provider(name: &str) -> Option<Provider> {
    let text = std::fs::read_to_string(root().join("providers.json")).ok()?;
    let map: HashMap<String, Provider> = serde_json::from_str(&text).ok()?;
    map.get(name).cloned()
}
