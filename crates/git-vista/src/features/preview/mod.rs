//! The graph-preview panel (M10.08 A6, #594).
//!
//! #576 landed the engine: `/api/preview` computes what a revert, cherry-pick
//! or merge *would* do, using real git against a scratch object store that can
//! read the repository and cannot write to it. Nothing in the app called it —
//! found on 2026-09-01 by driving the app, after three independent audits had
//! all checked the engine against its own contract and none had re-read the
//! acceptance list. This module is A6, the missing half.
//!
//! [`core`] is the framework-free decision layer: one `/api/preview` answer in,
//! one [`core::PreviewView`] out, with the four arms kept distinct and the
//! per-row marks derived. Rendering and the request live beside it.

pub mod core;
pub mod scene;
// The animated before→after transition (#591): a tween between the two real
// layouts `core`/`scene` already compute. Framework-free for the same reason
// they are — see `tween`'s own doc for the honesty rule this split protects.
pub mod tween;
// The reactive half. Gated because it imports Leptos and `crate::api`, which is
// itself wasm-gated; `core` and `scene` above are not, so every rule and every
// pixel of geometry is decided somewhere `cargo test` can reach.
#[cfg(target_arch = "wasm32")]
pub mod signals;
