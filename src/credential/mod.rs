//! Credentials: stored (vault), acquired (connect), refreshed (providers,
//! registry) — and injected in transit by the gateway. Workers never see keys.

pub mod connect;
pub mod providers;
pub mod registry;
pub mod vault;

/// The providers whose credential lets a box call a model — the one
/// connection a deployment cannot work without.
pub const LLM_PROVIDERS: [&str; 2] = ["anthropic", "openai-codex"];
