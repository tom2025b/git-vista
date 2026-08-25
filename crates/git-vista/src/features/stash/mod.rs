//! The stash drawer (M3.24, #77).
//!
//! `core.rs` holds every decision — what a row says, which actions an entry
//! offers, what a push will and will not capture, and how a composed pop is
//! reported. It is framework-free and host-tested.
//!
//! There is no `signals.rs` yet: the drawer's listing is a
//! `create_local_resource` keyed on the panel's visibility and the graph epoch,
//! exactly like the tag list and the event feed beside it, so a stash written
//! from the app refreshes with everything else and there is no second copy of
//! "is the panel open" to fall out of sync.

pub mod core;
#[cfg(target_arch = "wasm32")]
pub mod signals;
#[cfg(target_arch = "wasm32")]
pub mod view;
