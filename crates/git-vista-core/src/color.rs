//! The single source of truth for every colour in git-vista — the "Color God".
//!
//! One module decides **everything** about branch colour, for both halves of the
//! app: the pure layout engine (which assigns each commit a colour *slot*) and the
//! browser frontend (which paints those slots). Nothing else defines a palette, a
//! slot, or a hex value — so a dot on the graph and its label on the right are, by
//! construction, the same colour: they read the same slot and run it through the
//! same [`branch_color`].
//!
//! The design in two layers, both here:
//!
//!  1. **Slots** — [`stable_color_slot`] hashes a *branch name* onto a stable slot
//!     (`1..=BRANCH_PALETTE`); the layout reserves slot `0` for the trunk. A slot
//!     is a pure function of the name, so a branch keeps its colour across every
//!     operation, and a stub keeps the colour its line will have once it commits.
//!  2. **Values** — [`branch_color`] maps a slot onto a concrete CSS colour from
//!     the palette below, plus the fixed badge/HEAD/tag/merge colours.
//!
//! Because this crate is pure (no UI, no platform deps) it is shared as-is by the
//! wasm frontend and the native backend, so both literally link the same constants
//! and functions — there is no second copy to drift out of sync.

/// Number of non-trunk branch colours the palette holds. Colour slots are
/// `1 + hash(name) % BRANCH_PALETTE`, so this must equal [`BRANCH_COLORS`]'s
/// length (a test below pins the two together).
pub const BRANCH_PALETTE: usize = 6;

/// The trunk colour — reserved exclusively for `main`/`master` (colour slot 0).
/// No other branch is ever painted blue, so blue always means "the mainline".
pub const TRUNK_COLOR: &str = "#2f81f7"; // blue

/// Colours for every *non*-trunk branch. [`stable_color_slot`] hashes a branch's
/// *name* onto slots `1..=BRANCH_PALETTE`, so the same branch keeps the same
/// colour across every operation; this array must stay exactly `BRANCH_PALETTE`
/// long (a test pins it). None of these is ever the trunk blue.
pub const BRANCH_COLORS: [&str; BRANCH_PALETTE] = [
    "#3fb950", // green
    "#d29922", // amber
    "#db61a2", // pink
    "#a371f7", // purple
    "#f78166", // coral
    "#39c5cf", // cyan
];

/// Background fill for hollow merge / stub nodes — the canvas colour, so a merge
/// or a stub reads as a ring rather than a filled dot. Also the text colour on
/// filled badges, where it gives dark-on-bright, GitHub-label-style contrast.
pub const BADGE_DARK: &str = "#0d1117";

/// Alias kept for the merge-node / stub-ring fill (same value as [`BADGE_DARK`]).
pub const MERGE_FILL: &str = BADGE_DARK;

/// Fill for the `HEAD` badge — a bright neutral so "you are here" stands apart
/// from any branch colour.
pub const HEAD_BADGE: &str = "#e6edf3";

/// Fill for tag badges — a consistent tag colour, regardless of branch.
pub const TAG_BADGE: &str = "#d29922";

/// The palette slot for a branch (or synthetic line) name: slot 0 is reserved
/// for the trunk, every other name hashes (FNV-1a) onto slots
/// `1..=BRANCH_PALETTE`. A pure function of the name, so the same branch gets
/// the same colour in every graph, whatever else changed — and a branch stub
/// gets exactly the colour its line will have once it owns commits. Two
/// branches can share a colour (few slots, many branches); that beats colours
/// that shuffle on every operation.
pub fn stable_color_slot(name: &str) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    1 + (h % BRANCH_PALETTE as u64) as usize
}

/// Colour for the given branch slot. Slot 0 is always the trunk blue (`main`);
/// every other slot cycles through the non-trunk palette, so blue is unique to
/// the mainline and no side branch is ever painted blue. The layout's per-branch
/// `color` slot ([`crate::model::GraphRow::color`]) feeds straight in, so the same
/// branch always lands on the same colour — dot, line, badge and stub alike.
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
        assert_eq!(
            branch_color(BRANCH_COLORS.len()),
            BRANCH_COLORS[BRANCH_COLORS.len() - 1]
        );
        assert_eq!(branch_color(1), branch_color(1 + BRANCH_COLORS.len()));
    }

    /// `stable_color_slot` is a pure function of the name and always lands in the
    /// non-trunk slot range `1..=BRANCH_PALETTE` — the layout adds the trunk's 0.
    #[test]
    fn stable_color_slot_is_deterministic_and_in_range() {
        for name in [
            "main",
            "feature",
            "origin/x",
            "a-really-long-branch-name",
            "",
        ] {
            let s = stable_color_slot(name);
            assert_eq!(s, stable_color_slot(name), "same name, same slot");
            assert!(
                (1..=BRANCH_PALETTE).contains(&s),
                "slot {s} in 1..=BRANCH_PALETTE"
            );
        }
    }

    /// The palette must be exactly as long as the hash modulus, and all-distinct —
    /// otherwise different stable slots would silently collapse onto one colour and
    /// the "same branch, same colour" guarantee would skew.
    #[test]
    fn the_palette_matches_the_slot_space() {
        assert_eq!(BRANCH_COLORS.len(), BRANCH_PALETTE);
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
    /// at the colour level, and the invariant that keeps a dot and its label the
    /// same colour: both go through [`branch_color`] on the same slot.
    #[test]
    fn a_stub_renders_the_same_colour_its_line_will_have() {
        use crate::layout::layout_with_refs;
        use crate::model::{CommitSummary, GitRef, Oid, RefKind};

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
        let stub = g
            .stubs
            .iter()
            .find(|s| s.name == "fork")
            .expect("fork is a stub");
        // The view colours a stub with plain `branch_color(stub.color)`; that
        // must equal the colour of a future line owned by the same branch name.
        assert_eq!(
            branch_color(stub.color),
            branch_color(stable_color_slot("fork"))
        );
        assert_ne!(branch_color(stub.color), TRUNK_COLOR);
    }
}
