//! Working-tree status — SEAM ONLY.
//!
//! M1.11 (#64) deliberately shipped this module empty; the behaviour was slated
//! for M2.15 (#68), whose requirements were not yet fixed at the time, and
//! writing speculative state here would have been worse than leaving a shaped
//! hole (design spec D2).
//!
//! #783: corrected the two paragraphs below, which used to describe #68 as
//! still open. **#68 closed 2026-08-07.** It gave the status read ONE owner
//! without ever filling this seam in: Task 7 made [`signals::create`] the
//! only place `fetch_status()` is called for the topbar chip and the Activity
//! panel, which until then held two independently-fetched copies of the same
//! data. No state machine came with it, and none is coming from #68 — that
//! milestone is done. This seam (`StatusSeam` below) and the invalidation
//! scope it used to justify (`InvalidateScope::Status`, deleted by #783; see
//! `StatusSeam`'s own doc) remain exactly where M1.11 left them. A state
//! machine here is a fresh design question for whoever picks it up next, not
//! an #68 deliverable that merely hasn't landed yet.
//!
//! [`core`] supplies M2.15/#68d's framework-free grouping, sorting, counting,
//! and accessible-label data. The Activity overlay renders that data as
//! touch-card sections from its own v2 `WorktreeStatus` resource; this module
//! remains separate from `signals`'s live v1 `RepoStatus` fetch.

pub mod core;
pub mod detail;

#[cfg(target_arch = "wasm32")]
pub mod signals;

/// Placeholder so the module has a public surface. Carries no state by design.
///
/// #783: this used to also justify itself as giving `InvalidateScope::Status`
/// "a documented destination" — that variant is deleted now
/// (`features/core_traits.rs`). #68 (M2.15) shipped without ever
/// constructing or matching it: the status feature reads through
/// [`signals::create`]/`fetch_status()` directly, never through an
/// invalidation scope. This struct's own reason for existing (a public
/// surface placeholder) still holds independent of that; only the specific
/// claim about `InvalidateScope::Status` was removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatusSeam;
