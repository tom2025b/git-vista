//! Inspecting a conflict (M4.31a, #428) — the client half.
//!
//! [`core`] is the whole of this slice: the pure mapping from a
//! [`ConflictedFile`](git_vista_protocol::conflict::ConflictedFile)'s three
//! stages, plus the working tree's own copy, onto the four panes a viewer
//! renders. Framework-free and host-tested, matching this crate's `core.rs`
//! convention (`features/status/core.rs` states it explicitly): no Leptos, no
//! signals, no `#[cfg(target_arch = "wasm32")]` gate.
//!
//! That placement is the point, not an accident of tidiness. Two of #428's
//! four acceptance criteria — *"a stage that is `Absent` reads as absent, not
//! as empty"* and *"a stage that is `Unreadable` says so, and is never
//! silently rendered as empty"* — are facts about **rendering**. Put that
//! mapping in a wasm-only module and `cargo test` cannot see it, so those two
//! criteria would be pinned by nothing: the corollary the 2026-08-22 handoff
//! records is that `mod menu` and `mod prefs` are `#[cfg(target_arch =
//! "wasm32")]` and therefore invisible to the test runner. Here they are
//! ordinary host tests.
//!
//! No `signals.rs` yet: this slice adds no live resource of its own.

pub mod core;
/// M4.31c (#432): reading git's marker file into choosable blocks, and
/// composing the resolved file back out of them. Framework-free and
/// host-tested for the same reason [`core`] is — #432's criteria are facts
/// about what content a choice produces, and `cargo test` never compiles the
/// wasm viewer.
pub mod markers;
