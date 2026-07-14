//! The CLI surface of the `impyard` binary — thin typed handlers only: parse
//! arguments, call a functional block, print. The clap grammar lives in
//! main.rs; the machinery lives in the blocks (gateway, credential, action,
//! work, run, imp, channel).

pub mod channel;
pub mod connections;
pub mod create;
pub mod gates;
pub mod imp;
pub mod init;
pub mod knowledge;
pub mod memory;
pub mod runs;
pub mod server;
pub mod task;
pub mod vault;
