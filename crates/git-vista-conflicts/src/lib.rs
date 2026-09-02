//! `git-vista-conflicts` — the conflict view model, shared by every client
//! that inspects or resolves a conflict (M10.07, #462; ADR 0104).
//!
//! Two modules, and between them they are the whole of the client-side
//! conflict slice M4.31 shipped:
//!
//! - [`core`] — the four panes of a conflict view, the state each one is in,
//!   and the [`ResolutionSurface`](core::ResolutionSurface) saying which
//!   whole-file resolutions may be offered and why the others may not.
//! - [`markers`] — reading git's marker file into choosable blocks, and
//!   composing the resolved file back out of them.
//!
//! # Framework-free and host-tested, deliberately — the reason this crate
//! could exist at all
//!
//! Both modules were written that way from the start, and it was load-bearing
//! rather than tidy. `features/status/core.rs` states the convention; ADR 0066
//! states the argument. Two of #428's four acceptance criteria — *"a stage
//! that is `Absent` reads as absent, not as empty"* and *"a stage that is
//! `Unreadable` says so, and is never silently rendered as empty"* — are facts
//! about **rendering**. Put that mapping behind `#[cfg(target_arch =
//! "wasm32")]` and `cargo test` cannot see it, so those two criteria would be
//! pinned by nothing at all. #612 is the receipt: a dozen frontend modules sit
//! behind that gate and are executed by no test runner, and the first pure
//! decision moved out of one and properly proved turned out to be wrong.
//!
//! The dividend arrived here. Because these files never touched Leptos, moving
//! them was a `git mv` and a manifest — no logic was rewritten, and every test
//! that pinned M4.31's behaviour came along unchanged and still runs.
//!
//! # Why a crate of its own
//!
//! [`core`] reads `git_vista_core::diff` (the blob and worktree payloads) and
//! `git_vista_protocol::conflict` (the stages). Folding it into either of
//! those crates would break a dependency invariant each of them writes down:
//! `git-vista-protocol`'s lib doc says it depends on neither, and that
//! "`git-vista-core` does *not* depend on it, keeping the domain model free of
//! transport concerns". A crate downstream of both breaks neither claim.
//!
//! The precedent is ADR 0101, one milestone back and the same shape: when
//! `gv-tui` needed the session logic `git-vista-mcp` already had, the answer
//! was to extract `git-vista-session` rather than to reach across or to write
//! it twice.
//!
//! # What this crate deliberately does not hold
//!
//! Transport. Fetching the four panes' content, and posting a resolution, stay
//! with each client — `git-vista`'s `api::conflicts` for the browser,
//! `gv-tui`'s `data` for the terminal. What every client shares is the
//! *judgement*: which pane says what, which control may be offered, what
//! content a block choice produces. That is what moved.

pub mod core;
pub mod markers;
