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
//! [`core`] starts filling that hole (M2.15, #68d's pure-logic slice): the
//! grouping/sort/count/accessible-label data a future view will render. It
//! is framework-free and does not touch `signals`'s live v1 `RepoStatus`
//! fetch — the rendering half (a resource for the new v2 `WorktreeStatus`,
//! actual touch cards, wiring into a shell) is still to come.

pub mod core;

#[cfg(target_arch = "wasm32")]
pub mod signals;

/// Placeholder so the module has a public surface and `InvalidateScope::Status` has a
/// documented destination. Carries no state by design.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatusSeam;
