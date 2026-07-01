//! Colouring for the commit graph.
//!
//! A small, stable palette indexed by **branch** (Phase 7), kept separate from
//! the spatial [`crate::geometry`] so colour decisions can evolve on their own.
//! The backend gives every commit a `color` slot that is stable per branch across
//! the whole graph (see `git_vista_core::layout`); we just map that slot onto a
//! palette entry, so a branch keeps one colour wherever it appears — independent
//! of the lane it happens to sit in.

/// The trunk colour — reserved exclusively for `main` (colour slot 0). No other
/// branch is ever painted blue, so blue always means "the mainline".
const TRUNK_COLOR: &str = "#2f81f7"; // blue

/// Colours for every *non*-trunk branch. Slots 1, 2, 3, … cycle through these in
/// order, so a side branch is always one of these five and never the trunk blue.
const BRANCH_COLORS: [&str; 5] = [
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

/// Colour for the given branch slot. Slot 0 is always the trunk blue (`main`);
/// every other slot cycles through the non-trunk palette, so blue is unique to
/// the mainline and no side branch is ever painted blue. The backend's per-branch
/// `color` index feeds straight in, so the same branch always lands on the same
/// colour.
pub fn branch_color(slot: usize) -> &'static str {
    match slot {
        0 => TRUNK_COLOR,
        // Map slots 1, 2, 3, … onto the non-trunk palette (0, 1, 2, … of it),
        // wrapping once it's exhausted — but never back onto the trunk blue.
        n => BRANCH_COLORS[(n - 1) % BRANCH_COLORS.len()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_zero_is_the_trunk_blue() {
        assert_eq!(branch_color(0), TRUNK_COLOR);
        assert_eq!(branch_color(0), "#2f81f7");
    }

    #[test]
    fn non_trunk_slots_are_never_blue() {
        // Whatever the slot, only the trunk (slot 0) is ever blue — side branches
        // cycle through the non-trunk palette and never wrap back onto blue.
        for slot in 1..100 {
            assert_ne!(
                branch_color(slot),
                TRUNK_COLOR,
                "slot {slot} must not be the trunk blue"
            );
        }
    }

    #[test]
    fn non_trunk_slots_cycle_through_the_palette() {
        // Slot 1 is the first non-trunk colour; the cycle wraps after five, still
        // skipping blue.
        assert_eq!(branch_color(1), BRANCH_COLORS[0]);
        assert_eq!(branch_color(BRANCH_COLORS.len()), BRANCH_COLORS[BRANCH_COLORS.len() - 1]);
        assert_eq!(branch_color(1), branch_color(1 + BRANCH_COLORS.len()));
    }
}
