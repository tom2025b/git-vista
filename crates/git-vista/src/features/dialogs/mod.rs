//! Modal overlays: which one is up, and the iOS ghost-click guard (M1.11, #64).

pub mod core;
// M2.19c (#224): the commit dialog's own decisions — the three commit modes,
// the staged-scope review, the amend request/response contract and the guided
// re-check after a stale tip. Pure and host-tested for the reason `core` is:
// `crate::dialogs::commit` (the view) never compiles under `cargo test`.
pub mod commit;

#[cfg(target_arch = "wasm32")]
pub mod signals;
