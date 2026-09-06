//! Feature boundaries (M1.11, #64).
//!
//! One module per area named in the issue. Each owns its state; nothing here writes
//! another feature's state directly (design spec D2). `core.rs` files are framework-free
//! and host-tested; `signals.rs` files are the wasm-only reactive wrappers.
//!
//! # The per-seam enforcement rule (#612)
//!
//! `core.rs` being framework-free and host-tested does not, by itself, prove the
//! wasm-only `signals.rs`/view code actually asks `core` the question `core` answers —
//! `dialogs/confirm.rs`'s `preview_subject`/`previewable` composition shipped exactly
//! that gap (#612's own origin: a mutation proof that read "both caught" while the
//! line that *composed* the two host-tested halves lived in wasm-only code no runner
//! executed). `crate::wasm_module_census` catches the coarser failure — a wasm-only
//! module nobody's host test reads at all — but it is coverage bookkeeping, not a
//! verdict on any one seam (its own module doc says so).
//!
//! What actually closes a seam is not a mechanism installed once; it is a habit,
//! applied at the moment a decision moves out of wasm-only code and into a `core`
//! module: write a source-level census, alongside the moved decision, that binds the
//! *specific* wasm-only caller to the *specific* `core` function it must ask — the way
//! `features::preview::core`'s `the_confirm_dialog_does_not_have_the_two_arms_the_wrong_way_round`
//! pairs each `dialogs/confirm.rs` arm with the `preview.` call that follows it, or the
//! way `features::history::core`'s census pins `app/mod.rs`'s `HistoryPhase` effects to
//! the three rules that decide them. A purity lint was considered and rejected for this
//! (#645's PR body has the argument: a wasm-only module is full of functions pure by
//! signature — every `#[component]`, every markup helper — so a lint keyed on purity
//! drowns in false positives). This is deliberately not mechanised further than that:
//! it is a review question asked of every slice that moves a decision out of a
//! `signals.rs`/view file, not a check `cargo test` can run unattended.

pub mod core_traits;

pub mod a11y;
pub mod activity;
// No `conflicts` module here any more. M4.31's four-pane view model and its
// marker-file block editor moved to the `git-vista-conflicts` crate for
// M10.07 (#462; ADR 0105), so the terminal client resolves conflicts through
// the same implementation this one does rather than a second copy of it. They
// were always framework-free and host-tested, which is what made the move a
// `git mv`; `api::conflicts` and `viewer.rs` now name that crate directly.
pub mod dialogs;
pub mod diff;
pub mod explain;
// M12.05 (#555): is the plan on screen still true? The decision and its
// sentences are host-tested in `core`; `signals` is the one EventSource.
pub mod freshness;
pub mod graph;
// #612: the graph panel's load phase. The signal lives in the `App` shell
// (wasm-only); the three rules that move it are here, where a host test runs
// them and a source census pins the shell to their answers.
pub mod history;
pub mod operations;
pub mod preview;
// #387: the full-screen viewer's readiness predicate — derived from the same
// staleness check `viewer.rs`'s body match already makes, not a new signal.
pub mod readiness;
pub mod session;
pub mod shell;
pub mod status;
// M3.24 (#77): the stash drawer — rows, action offers, push preview,
// and the client-composed pop.
pub mod stash;
pub mod tags;
// M11.03 (#548): the worktree drawer — every desk this repository has, git's
// flags and this app's fence kept as separate statements, and the switch.
pub mod worktrees;
