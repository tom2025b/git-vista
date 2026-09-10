//! Working-tree status — SEAM ONLY.
//!
//! M1.11 (#64) deliberately ships this module empty. The behaviour belongs to M2.15 (#68),
//! whose requirements are not yet fixed; writing speculative state here would be worse than
//! leaving a shaped hole (design spec D2).
//!
//! When #68 fills this in, it inherits ONE owner for the status read. Task 7 made that
//! true: [`signals::create`] is now the only place `fetch_status()` is called for the
//! topbar chip and the Activity panel, which until then held two independently-fetched
//! copies of the same data. No state machine came with it — that is still #68's to design.
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
