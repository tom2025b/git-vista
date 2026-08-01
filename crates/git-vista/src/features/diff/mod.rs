//! File and hunk diff views.
//!
//! M1.11 (#64) shipped this module as an empty seam; the full behaviour still
//! belongs to M2.16 (#69). M2.16e (#210) filled in the first real piece:
//! `core` holds the pure hunk-navigation decisions the flat diff rendering's
//! accessibility wiring consumes.

pub mod core;

/// Placeholder; carries no state by design.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiffSeam;
