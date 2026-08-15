//! File and hunk diff views.
//!
//! M1.11 (#64) shipped this module as an empty seam; the full behaviour still
//! belongs to M2.16 (#69). M2.16e (#210) filled in the first real piece:
//! `core` holds the pure hunk-navigation decisions the flat diff rendering's
//! accessibility wiring consumes. M2.17d (#215) added `selection`: the
//! finger/Pencil hunk-selection state for staging, pure and host-tested the
//! same way, and `staging_view` — the wasm-only view wiring it to the
//! staging endpoints.

pub mod core;
pub mod rows;
pub mod selection;
#[cfg(target_arch = "wasm32")]
pub mod staging_view;

/// Placeholder; carries no state by design.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiffSeam;
