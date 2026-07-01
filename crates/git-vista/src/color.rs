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

// Used only by the wasm-only `app` view.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
/// The colour for branch slot `slot`, but guaranteed to *differ* from the colour
/// of `avoid`. Because the palette collapses many slots onto few colours, a slot
/// can otherwise land on the same colour as an unrelated branch; this steps to the
/// next slot until the colour differs. Used so a freshly-created branch never
/// shares the colour of the branch it forked off (Issue #30).
pub fn branch_color_distinct_from(slot: usize, avoid: usize) -> &'static str {
    let avoid = branch_color(avoid);
    // Non-trunk slots (>= 1) cycle through BRANCH_COLORS, so stepping at most
    // `len` times always reaches a different colour. `slot` for a stub is always
    // >= 1, so we never step onto (or off) the reserved trunk blue.
    (slot..)
        .map(branch_color)
        .find(|&c| c != avoid)
        .unwrap_or_else(|| branch_color(slot))
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

    #[test]
    fn distinct_from_never_matches_the_avoided_colour() {
        // A stub whose raw slot collides with its parent's colour gets bumped to a
        // different colour; a non-colliding slot is returned unchanged.
        for slot in 1..30 {
            for avoid in 0..30 {
                let c = branch_color_distinct_from(slot, avoid);
                assert_ne!(c, branch_color(avoid), "slot {slot} vs avoid {avoid}");
                assert_ne!(c, TRUNK_COLOR, "a stub colour is never the trunk blue");
            }
        }
        // No collision => unchanged (slot 1 is green, avoiding blue leaves it green).
        assert_eq!(branch_color_distinct_from(1, 0), branch_color(1));
    }

    /// End-to-end (real layout + the exact colour call the view makes): a branch
    /// created off an existing coloured branch renders in a *different* colour
    /// from that parent branch. This is the Issue #30 report — a new branch
    /// reusing its parent's colour — pinned so it can't regress.
    #[test]
    fn a_new_branch_renders_a_different_colour_from_its_parent() {
        use git_vista_core::layout::layout_with_refs;
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
        // main line with a feature side branch (its own colour); `fork` is a fresh
        // branch created at feature's tip F, so it's a stub anchored on feature.
        //   M  merge[C, F]
        //   C  F   (F = feature tip, `fork` also points here)
        //   B
        //   A
        let g = layout_with_refs(
            vec![
                c("M", &["C", "F"]),
                c("C", &["B"]),
                c("F", &["B"]),
                c("B", &["A"]),
                c("A", &[]),
            ],
            vec![
                r("HEAD", RefKind::Head, "M"),
                r("main", RefKind::Branch, "M"),
                r("feature", RefKind::Branch, "F"),
                r("fork", RefKind::Branch, "F"),
            ],
            Some("main"),
        );

        let stub = g.stubs.iter().find(|s| s.name == "fork").expect("fork is a stub");
        // Exactly the computation the view performs for a stub's colour.
        let anchor_slot = g.rows[stub.anchor_row].color;
        let rendered = branch_color_distinct_from(stub.color, anchor_slot);
        assert_ne!(
            rendered,
            branch_color(anchor_slot),
            "the new branch must not share the colour of the branch it forked off"
        );
        // The parent branch here is a real coloured (non-trunk) line, so this is a
        // meaningful check, not a trivial trunk-vs-branch difference.
        assert_ne!(branch_color(anchor_slot), TRUNK_COLOR);
    }
}
