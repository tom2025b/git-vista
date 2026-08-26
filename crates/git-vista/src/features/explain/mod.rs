//! Explain Mode's client half (M6.39b, #545) — turning a plan's typed
//! explanation into words.
//!
//! The protocol crate decides what a plan *means*
//! ([`git_vista_protocol::explain`], ADR 0091); this slice decides how that
//! meaning reads. The split is deliberate and load-bearing: an
//! [`ExplanationFact`](git_vista_protocol::ExplanationFact) carries the plan's
//! own typed value and no English at all, so replacing this module is what
//! translation would mean — not rewriting the explanation.
//!
//! [`core`] is framework-free and host-tested, matching this crate's `core.rs`
//! convention and, more to the point, `features/conflicts/core.rs`'s stated
//! reason for existing: **`cargo test` never compiles the wasm viewer.** #545's
//! acceptance criteria are facts about rendering — that every fact kind
//! produces a real sentence, that a ref in the explanation names the ref the
//! graph draws — and a mapping table living inside a Leptos component is
//! untested by construction. Here they are ordinary host tests.

pub mod core;
