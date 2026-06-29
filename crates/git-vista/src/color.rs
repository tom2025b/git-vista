//! Colouring for the commit graph.
//!
//! A small, stable palette indexed by **branch** (Phase 7), kept separate from
//! the spatial [`crate::geometry`] so colour decisions can evolve on their own.
//! The backend gives every commit a `color` slot that is stable per branch across
//! the whole graph (see `git_vista_core::layout`); we just map that slot onto a
//! palette entry, so a branch keeps one colour wherever it appears — independent
//! of the lane it happens to sit in.

const BRANCH_COLORS: [&str; 6] = [
    "#2f81f7", // blue   (trunk / HEAD's branch)
    "#3fb950", // green
    "#d29922", // amber
    "#db61a2", // pink
    "#a371f7", // purple
    "#f78166", // coral
];

// Used only by the wasm-only `app` view, so it reads as dead on host/test builds.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
/// Background fill for hollow merge nodes — the canvas colour, so a merge reads
/// as a ring rather than a filled dot. Also the text colour on filled badges,
/// where it gives dark-on-bright, GitHub-label-style contrast.
pub const BADGE_DARK: &str = "#0d1117";

// Used only by the wasm-only `app` view.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
/// Alias kept for the merge-node fill (same value as [`BADGE_DARK`]).
pub const MERGE_FILL: &str = BADGE_DARK;

// Used only by the wasm-only `app` view.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
/// Fill for the `HEAD` badge — a bright neutral so "you are here" stands apart
/// from any branch colour.
pub const HEAD_BADGE: &str = "#e6edf3";

// Used only by the wasm-only `app` view.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
/// Fill for tag badges — a consistent tag colour, regardless of branch.
pub const TAG_BADGE: &str = "#d29922";

/// Colour for the given branch slot, wrapping around the palette. The backend's
/// per-branch `color` index feeds straight in, so the same branch always lands on
/// the same colour.
pub fn branch_color(slot: usize) -> &'static str {
    BRANCH_COLORS[slot % BRANCH_COLORS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_color_wraps_around_the_palette() {
        assert_eq!(branch_color(0), branch_color(BRANCH_COLORS.len()));
        assert_eq!(branch_color(1), branch_color(BRANCH_COLORS.len() + 1));
    }

    #[test]
    fn slot_zero_is_the_trunk_blue() {
        assert_eq!(branch_color(0), "#2f81f7");
    }
}
