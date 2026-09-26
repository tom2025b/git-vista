//! Plan freshness (M12.05, #555): whether the plan on screen still describes
//! the repository, and what to say when it does not.
//!
//! `core` is pure and host-tested — every decision and every sentence.
//! `signals` is the wasm-only half: one `EventSource` on
//! `GET /api/repository/events`, the log the decision reads, and the live
//! graph follow-up (#852) that remounts only when a history-v1 Frame moved.

pub mod core;
#[cfg(target_arch = "wasm32")]
pub mod signals;
