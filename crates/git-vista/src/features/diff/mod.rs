//! File and hunk diff views — SEAM ONLY.
//!
//! M1.11 (#64) ships this module empty; the behaviour belongs to M2.16 (#69). See the
//! sibling `status` module for the rationale (design spec D2).

/// Placeholder; carries no state by design.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiffSeam;
