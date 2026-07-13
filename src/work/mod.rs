//! The outer loop: what to do and when. The durable per-worker queue, the
//! schedule triggers that file work, and the dispatch loop that drains it.

pub mod dispatch;
pub mod queue;
pub mod trigger;
