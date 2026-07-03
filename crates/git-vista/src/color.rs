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

/// Colours for every *non*-trunk branch. The backend hashes a branch's *name*
/// onto slots `1..=BRANCH_PALETTE` (see `git_vista_core::layout`), so the same
/// branch keeps the same colour across every operation; this array must stay
/// exactly `BRANCH_PALETTE` long (a test pins it). None of these is ever the
/// trunk blue.
const BRANCH_COLORS: [&str; git_vista_core::layout::BRANCH_PALETTE] = [
    "#3fb950", // green
    "#d29922", // amber
    "#db61a2", // pink
    "#a371f7", // purple
    "#f78166", // coral
    "#39c5cf", // cyan
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
        // Slot 1 is the first non-trunk colour; the cycle wraps after the last,
        // still skipping blue.
        assert_eq!(branch_color(1), BRANCH_COLORS[0]);
        assert_eq!(branch_color(BRANCH_COLORS.len()), BRANCH_COLORS[BRANCH_COLORS.len() - 1]);
        assert_eq!(branch_color(1), branch_color(1 + BRANCH_COLORS.len()));
    }

    /// The palette must be exactly as long as the backend's hash modulus, and
    /// all-distinct — otherwise different stable slots would silently collapse
    /// onto one colour and the "same branch, same colour" guarantee would skew.
    #[test]
    fn the_palette_matches_the_backends_slot_space() {
        assert_eq!(BRANCH_COLORS.len(), git_vista_core::layout::BRANCH_PALETTE);
        for (i, a) in BRANCH_COLORS.iter().enumerate() {
            assert_ne!(*a, TRUNK_COLOR, "no branch colour may be the trunk blue");
            for b in &BRANCH_COLORS[i + 1..] {
                assert_ne!(a, b, "palette colours must be distinct");
            }
        }
    }

    /// End-to-end (real layout + the exact colour call the view makes): a stub
    /// renders in exactly the colour its branch's line will have once it owns a
    /// commit — colour is a pure function of the branch name. This is the July
    /// test round's issue #6 ("my stub's commit landed on another line") pinned
    /// at the colour level.
    #[test]
    fn a_stub_renders_the_same_colour_its_line_will_have() {
        use git_vista_core::layout::{layout_with_refs, stable_color_slot};
        use git_vista_core::model::{CommitSummary, GitRef, Oid, RefKind};

        let c = |id: &str, parents: &[&str]| CommitSummary {
            id: Oid(id.into()),
            parents: parents.iter().map(|p| Oid((*p).into())).collect(),
            summary: id.into(),
            author: "t".into(),
            time: 0,
        };
        let r = |name: &str, kind: RefKind, target: &str| GitRef {
            name: name.into(),
            kind,
            target: Oid(target.into()),
        };
        // `fork` freshly created at feature's tip F — a stub.
        let g = layout_with_refs(
            vec![c("F", &["B"]), c("B", &[])],
            vec![
                r("HEAD", RefKind::Head, "B"),
                r("main", RefKind::Branch, "B"),
                r("feature", RefKind::Branch, "F"),
                r("fork", RefKind::Branch, "F"),
            ],
            Some("main"),
        );
        let stub = g.stubs.iter().find(|s| s.name == "fork").expect("fork is a stub");
        // The view colours a stub with plain `branch_color(stub.color)`; that
        // must equal the colour of a future line owned by the same branch name.
        assert_eq!(branch_color(stub.color), branch_color(stable_color_slot("fork")));
        assert_ne!(branch_color(stub.color), TRUNK_COLOR);
    }
}
