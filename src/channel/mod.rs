//! Edges to the outside world: the Discord client (REST out, websocket in),
//! listener supervision, and the inbound relay that turns messages into tasks.

pub mod discord;
pub mod links;
pub mod listen;
pub mod relay;
pub mod slack;
pub mod slash;
