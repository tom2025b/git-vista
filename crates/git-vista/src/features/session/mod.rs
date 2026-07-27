//! The repository session: the CSRF credential, the transport we arrived on, and the mode
//! the open repository is in (M1.11, #64, decision D6).

pub mod core;
// The process-wide holder. Gated because only the running app has one; the core above is
// not, so its rules are tested on the host (M1.11 D1).
#[cfg(target_arch = "wasm32")]
pub mod signals;
