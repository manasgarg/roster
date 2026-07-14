//! Credentials: stored (vault), acquired (connect), refreshed (providers,
//! registry) — and injected in transit by the gateway. Imps never see keys.

pub mod connect;
pub mod providers;
pub mod registry;
pub mod vault;
