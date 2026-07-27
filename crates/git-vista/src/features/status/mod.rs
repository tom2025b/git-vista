//! Working-tree status — SEAM ONLY.
//!
//! M1.11 (#64) deliberately ships this module empty. The behaviour belongs to M2.15 (#68),
//! whose requirements are not yet fixed; writing speculative state here would be worse than
//! leaving a shaped hole (design spec D2).
//!
//! When #68 fills this in, it inherits ONE owner for the status read. Today there are two
//! independently-fetched copies of the same `fetch_status()` data — the topbar's
//! (`app/mod.rs:256`) and the activity panel's (`activity.rs:102`). Task 7 of the M1.11 plan
//! collapses them onto this seam.

/// Placeholder so the module has a public surface and `InvalidateScope::Status` has a
/// documented destination. Carries no state by design.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatusSeam;
