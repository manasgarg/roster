//! Subcommands of the `roster` binary — the trusted host-side control plane
//! (D20). Each is a thin orchestration entry point; the heavy lifting reuses
//! the gateway's own modules (schema, budget, vault, registry) so there is one
//! set of types, not two.

pub mod connect;
pub mod create;
pub mod deploy;
pub mod run_box;
pub mod vault_sync;
