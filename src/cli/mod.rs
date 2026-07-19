//! The CLI surface of the `roster` binary — thin typed handlers only: parse
//! arguments, call a functional block, print. The clap grammar lives in
//! main.rs; the machinery lives in the blocks (gateway, credential, action,
//! work, run, worker, channel).

pub mod approvals;
pub mod channel;
pub mod connections;
pub mod create;
pub mod init;
pub mod knowledge;
pub mod runs;
pub mod server;
pub mod task;
pub mod worker;
