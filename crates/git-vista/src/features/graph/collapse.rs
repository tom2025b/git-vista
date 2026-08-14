//! Display-space projection that folds runs of consecutive WIP-checkpoint
//! commits into one summary node (#374).
//!
//! Framework-free and host-tested, matching this crate's `core.rs`
//! convention: no Leptos, no `#[cfg(target_arch = "wasm32")]` gate, so
//! `cargo test` actually executes it. The wiring that consumes it
//! (`app/canvas.rs`) is wasm-only and is verified by a Playwright test
//! instead — see this feature's plan for why both are required.

use std::collections::HashSet;

use git_vista_core::model::{Edge, GraphRow};

/// True for the exact message shape `~/.local/bin/autocheckpoint` produces:
/// `wip(#123): auto-checkpoint 456`. Deliberately strict — a commit that
/// merely mentions "wip" in prose, or a hand-written `wip(#12): fix thing`,
/// is real work and must never be folded away.
pub fn is_wip_checkpoint(summary: &str) -> bool {
    let Some(rest) = summary.strip_prefix("wip(#") else {
        return false;
    };
    let Some((digits, rest)) = rest.split_once(')') else {
        return false;
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(rest) = rest.strip_prefix(": auto-checkpoint") else {
        return false;
    };
    // Require a boundary after the literal so "auto-checkpointer" doesn't
    // match, but allow anything after it (a counter, a later suffix).
    rest.is_empty() || rest.starts_with(' ')
}

/// One rendered slot in display space. Every variant occupies exactly one
/// `ROW_HEIGHT` slot — that uniformity is what lets `viewport::
/// visible_row_range` and `geometry::node_cy` stay unchanged, since both
/// only ever assume a fixed stride over *some* row count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayItem {
    /// One real commit, at `row_index` in `LoadedHistory.rows`.
    Single { row_index: usize },
    /// A folded run of `count` consecutive WIP checkpoints starting at
    /// `start_row_index`. `lane`/`color` are copied from the first member —
    /// arbitrary but consistent, since every member shares a lane by
    /// construction of the grouping rule.
    WipGroup {
        start_row_index: usize,
        count: usize,
        lane: usize,
        color: usize,
    },
}

/// An edge with both endpoints already resolved to display-space indices.
/// Lanes copy through from the source `Edge` unchanged — collapsing moves
/// rows vertically, never between lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayEdge {
    pub from_display: usize,
    pub from_lane: usize,
    pub to_display: usize,
    pub to_lane: usize,
}

/// A run of WIP checkpoints the user has opened, kept so one section can be
/// folded again on its own (#374 follow-up).
///
/// An expanded run is emitted as ordinary `Single`s, which makes it
/// indistinguishable from unrelated commits by the time a view sees it — so
/// the fact that these particular rows *were* a foldable run has to be
/// carried explicitly, or the only way back is the topbar toggle that folds
/// the entire graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WipRun {
    pub start_row_index: usize,
    pub count: usize,
}

/// The whole projection for one render pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayProjection {
    pub items: Vec<DisplayItem>,
    pub edges: Vec<DisplayEdge>,
    /// Runs that would be folded but are currently open. Empty whenever
    /// collapsing is switched off globally: with the topbar toggle off the
    /// user has asked to see everything, and offering "fold this section"
    /// would contradict the switch they just threw.
    pub expanded_runs: Vec<WipRun>,
}

impl DisplayProjection {
    /// Display-space index of the slot showing raw row `row_index` — the
    /// group's own slot when that row is inside a folded run. `None` only
    /// when the row is outside the projected range entirely.
    /// The open run this raw row belongs to, if any — what a row's own view
    /// needs in order to offer "fold these N checkpoints".
    pub fn run_containing_row(&self, row_index: usize) -> Option<WipRun> {
        self.expanded_runs.iter().copied().find(|run| {
            row_index >= run.start_row_index && row_index < run.start_row_index + run.count
        })
    }

    pub fn display_of_row(&self, row_index: usize) -> Option<usize> {
        self.items.iter().position(|item| match *item {
            DisplayItem::Single { row_index: r } => r == row_index,
            DisplayItem::WipGroup {
                start_row_index,
                count,
                ..
            } => row_index >= start_row_index && row_index < start_row_index + count,
        })
    }
}

/// True when `newer` and `older` are adjacent members of one foldable run:
/// both WIP checkpoints, same lane, `older` is `newer`'s *sole* parent, and
/// neither is itself a merge commit. Both parent-count checks matter: the
/// first stops a merge from being absorbed when it plays `newer` (multiple
/// parents means `older` isn't its sole one); the second stops one when it
/// plays `older` (a 2-parent commit reachable as *someone's* sole parent is
/// still a merge in its own right, and folding it away would hide that
/// topology join even though the child->it edge is unambiguous). The
/// checkpointer never makes merges, but this function must not assume that
/// of every caller.
fn same_run(newer: &GraphRow, older: &GraphRow) -> bool {
    is_wip_checkpoint(&newer.commit.summary)
        && is_wip_checkpoint(&older.commit.summary)
        && newer.lane == older.lane
        && newer.commit.parents.len() == 1
        && newer.commit.parents[0] == older.commit.id
        && older.commit.parents.len() <= 1
}

/// Minimum run length worth folding. Turning a single dot into a "1 WIP
/// commit" marker is net-noisier than leaving it alone.
const MIN_RUN: usize = 2;

/// Project raw rows and edges into display space. Pure: no I/O, no signal
/// reads, no mutation of the inputs.
pub fn project(
    rows: &[GraphRow],
    edges: &[Edge],
    collapse_enabled: bool,
    expanded: &HashSet<usize>,
) -> DisplayProjection {
    let mut items = Vec::with_capacity(rows.len());
    let mut expanded_runs: Vec<WipRun> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        // Always advance a whole run at a time, never one row at a time. A
        // run is decided *before* the expanded set is consulted, so opening
        // one takes its every member out of folding together. Advancing
        // row-by-row instead re-examined the tail as a run in its own right,
        // which is still adjacent, still all checkpoints, and still >=
        // MIN_RUN — so a three-member run grew a fresh two-member group the
        // moment the user opened it and the marker never went away (#374,
        // caught by the browser spec, pinned by the two tests below).
        let mut end = i;
        while end + 1 < rows.len() && same_run(&rows[end], &rows[end + 1]) {
            end += 1;
        }
        let count = end - i + 1;
        // Membership anywhere in the run, not only at its first row: an
        // append can put a NEWER checkpoint above a run the user already
        // opened, moving the start index out from under the recorded one.
        let user_expanded = (i..=end).any(|r| expanded.contains(&r));
        if collapse_enabled && count >= MIN_RUN && !user_expanded {
            items.push(DisplayItem::WipGroup {
                start_row_index: i,
                count,
                lane: rows[i].lane,
                color: rows[i].color,
            });
        } else {
            if collapse_enabled && count >= MIN_RUN {
                // Foldable, but open: remember it so this one section can be
                // folded again without touching the rest of the graph.
                expanded_runs.push(WipRun {
                    start_row_index: i,
                    count,
                });
            }
            items.extend((i..=end).map(|row_index| DisplayItem::Single { row_index }));
        }
        i = end + 1;
    }

    let projection = DisplayProjection {
        items,
        edges: Vec::new(),
        expanded_runs,
    };
    let display_edges = edges
        .iter()
        .filter_map(|e| {
            let from_display = projection.display_of_row(e.from_row)?;
            let to_display = projection.display_of_row(e.to_row)?;
            // Both endpoints inside the same folded run: the edge was
            // internal to it and has nothing left to connect.
            if from_display == to_display {
                return None;
            }
            Some(DisplayEdge {
                from_display,
                from_lane: e.from_lane,
                to_display,
                to_lane: e.to_lane,
            })
        })
        .collect();

    DisplayProjection {
        edges: display_edges,
        ..projection
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_core::model::{CommitSummary, Oid};

    fn row(i: usize, summary: &str, parent: Option<&str>) -> GraphRow {
        GraphRow {
            commit: CommitSummary {
                id: Oid(format!("c{i}")),
                parents: parent.map(|p| vec![Oid(p.to_string())]).unwrap_or_default(),
                summary: summary.to_string(),
                author: "Claude_Max".to_string(),
                time: 1_700_000_000 + i as i64,
            },
            row: i,
            lane: 0,
            refs: Vec::new(),
            color: 0,
            on_remote: false,
        }
    }

    fn wip_row(i: usize, n: usize, parent: Option<&str>) -> GraphRow {
        row(i, &format!("wip(#66): auto-checkpoint {n}"), parent)
    }

    #[test]
    fn a_run_of_three_wips_folds_into_one_group() {
        // c0 (real) <- c1,c2,c3 (wip) <- c4 (real); newest first, each row's
        // parent is the row below it.
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            wip_row(3, 1, Some("c4")),
            row(4, "docs: earlier real work", None),
        ];
        let p = project(&rows, &[], true, &HashSet::new());
        assert_eq!(p.items.len(), 3, "{:?}", p.items);
        assert!(matches!(p.items[0], DisplayItem::Single { row_index: 0 }));
        assert!(
            matches!(
                p.items[1],
                DisplayItem::WipGroup {
                    start_row_index: 1,
                    count: 3,
                    ..
                }
            ),
            "{:?}",
            p.items[1]
        );
        assert!(matches!(p.items[2], DisplayItem::Single { row_index: 4 }));
    }

    #[test]
    fn a_lone_wip_commit_is_not_grouped() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 1, Some("c2")),
            row(2, "docs: more real work", None),
        ];
        let p = project(&rows, &[], true, &HashSet::new());
        assert_eq!(p.items.len(), 3);
        assert!(p
            .items
            .iter()
            .all(|i| matches!(i, DisplayItem::Single { .. })));
    }

    #[test]
    fn a_run_broken_by_a_real_commit_becomes_two_groups() {
        let rows = vec![
            wip_row(0, 4, Some("c1")),
            wip_row(1, 3, Some("c2")),
            row(2, "feat: interrupting real work", Some("c3")),
            wip_row(3, 2, Some("c4")),
            wip_row(4, 1, None),
        ];
        let p = project(&rows, &[], true, &HashSet::new());
        assert_eq!(p.items.len(), 3, "{:?}", p.items);
        assert!(matches!(
            p.items[0],
            DisplayItem::WipGroup {
                start_row_index: 0,
                count: 2,
                ..
            }
        ));
        assert!(matches!(p.items[1], DisplayItem::Single { row_index: 2 }));
        assert!(matches!(
            p.items[2],
            DisplayItem::WipGroup {
                start_row_index: 3,
                count: 2,
                ..
            }
        ));
    }

    #[test]
    fn a_wip_worded_merge_commit_is_never_absorbed() {
        // Two parents: a merge. Even with a matching message it must stay a
        // Single — folding a merge away would hide a real topology join.
        let mut merge = wip_row(1, 2, Some("c2"));
        merge.commit.parents.push(Oid("cX".to_string()));
        let rows = vec![wip_row(0, 3, Some("c1")), merge, wip_row(2, 1, None)];
        let p = project(&rows, &[], true, &HashSet::new());
        assert_eq!(p.items.len(), 3, "{:?}", p.items);
        assert!(p
            .items
            .iter()
            .all(|i| matches!(i, DisplayItem::Single { .. })));
    }

    #[test]
    fn a_lane_change_breaks_a_run() {
        let mut off_lane = wip_row(1, 2, Some("c2"));
        off_lane.lane = 1;
        let rows = vec![wip_row(0, 3, Some("c1")), off_lane, wip_row(2, 1, None)];
        let p = project(&rows, &[], true, &HashSet::new());
        assert_eq!(p.items.len(), 3, "{:?}", p.items);
    }

    #[test]
    fn a_broken_parent_chain_breaks_a_run() {
        // Row 0's parent is NOT row 1 — they are not a linear chain, so they
        // must not be folded together even though both match.
        let rows = vec![
            wip_row(0, 3, Some("somewhere-else")),
            wip_row(1, 2, Some("c2")),
            wip_row(2, 1, None),
        ];
        let p = project(&rows, &[], true, &HashSet::new());
        assert!(
            matches!(p.items[0], DisplayItem::Single { row_index: 0 }),
            "{:?}",
            p.items
        );
        assert!(matches!(
            p.items[1],
            DisplayItem::WipGroup {
                start_row_index: 1,
                count: 2,
                ..
            }
        ));
    }

    #[test]
    fn collapse_disabled_yields_one_single_per_row() {
        let rows = vec![wip_row(0, 2, Some("c1")), wip_row(1, 1, None)];
        let p = project(&rows, &[], false, &HashSet::new());
        assert_eq!(p.items.len(), 2);
        assert!(p
            .items
            .iter()
            .all(|i| matches!(i, DisplayItem::Single { .. })));
    }

    #[test]
    fn an_expanded_group_renders_its_members_individually() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 2, Some("c2")),
            wip_row(2, 1, None),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(1); // keyed by start_row_index
        let p = project(&rows, &[], true, &expanded);
        assert_eq!(p.items.len(), 3, "{:?}", p.items);
        assert!(p
            .items
            .iter()
            .all(|i| matches!(i, DisplayItem::Single { .. })));
    }

    #[test]
    fn expanding_a_long_run_does_not_refold_its_tail() {
        // The regression the 2-member case above cannot catch (#374): with
        // three or more members, un-folding only the run's FIRST row leaves
        // rows 2..n adjacent, still a valid run, and still >= MIN_RUN — so a
        // second group appears the instant the first is opened and the marker
        // never goes away. Expanding a run must expand the whole run.
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            wip_row(3, 1, Some("c4")),
            row(4, "docs: earlier real work", None),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(1); // the group's start_row_index, what the tap sends
        let p = project(&rows, &[], true, &expanded);
        assert_eq!(p.items.len(), 5, "{:?}", p.items);
        assert!(
            p.items
                .iter()
                .all(|i| matches!(i, DisplayItem::Single { .. })),
            "{:?}",
            p.items
        );
    }

    #[test]
    fn expanding_from_any_member_expands_the_whole_run() {
        // Membership is tested across the run, not only at its start, so a
        // run that later grows a new head (a page appending a NEWER
        // checkpoint above an already-opened run) stays open instead of
        // silently re-folding around the row the user opened.
        let rows = vec![
            wip_row(0, 3, Some("c1")),
            wip_row(1, 2, Some("c2")),
            wip_row(2, 1, None),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(1); // a middle member, not the run's first row
        let p = project(&rows, &[], true, &expanded);
        assert_eq!(p.items.len(), 3, "{:?}", p.items);
        assert!(
            p.items
                .iter()
                .all(|i| matches!(i, DisplayItem::Single { .. })),
            "{:?}",
            p.items
        );
    }

    #[test]
    fn an_expanded_run_is_recorded_so_one_section_can_be_refolded() {
        // #374 follow-up: the topbar toggle folds the WHOLE graph. To offer
        // "fold just this section" the projection has to remember which runs
        // are open, since an expanded run is emitted as ordinary Singles and
        // is otherwise indistinguishable from unrelated commits.
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            wip_row(3, 1, Some("c4")),
            row(4, "docs: earlier real work", None),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(1);

        let p = project(&rows, &[], true, &expanded);

        assert_eq!(
            p.expanded_runs,
            vec![WipRun {
                start_row_index: 1,
                count: 3
            }]
        );
        assert_eq!(
            p.run_containing_row(2),
            Some(WipRun {
                start_row_index: 1,
                count: 3
            })
        );
        assert_eq!(p.run_containing_row(0), None);
        assert_eq!(p.run_containing_row(4), None);
    }

    #[test]
    fn a_folded_run_is_not_offered_for_refolding() {
        let rows = vec![wip_row(0, 2, Some("c1")), wip_row(1, 1, None)];

        let p = project(&rows, &[], true, &HashSet::new());

        assert_eq!(p.expanded_runs, Vec::new());
    }

    #[test]
    fn no_run_is_offered_when_collapsing_is_switched_off_globally() {
        // With the topbar toggle off the user has asked to see everything;
        // offering "fold this section" would contradict the switch they just
        // threw.
        let rows = vec![wip_row(0, 2, Some("c1")), wip_row(1, 1, None)];
        let mut expanded = HashSet::new();
        expanded.insert(0);

        let p = project(&rows, &[], false, &expanded);

        assert_eq!(p.expanded_runs, Vec::new());
    }

    #[test]
    fn a_run_below_the_fold_threshold_is_never_offered_for_refolding() {
        let rows = vec![row(0, "feat: real work", Some("c1")), wip_row(1, 1, None)];
        let mut expanded = HashSet::new();
        expanded.insert(1);

        let p = project(&rows, &[], true, &expanded);

        assert_eq!(p.expanded_runs, Vec::new());
        assert_eq!(p.run_containing_row(1), None);
    }

    #[test]
    fn a_group_takes_its_first_members_lane_and_color() {
        let mut first = wip_row(0, 2, Some("c1"));
        first.lane = 2;
        first.color = 5;
        let mut second = wip_row(1, 1, None);
        second.lane = 2;
        second.color = 5;
        let p = project(&[first, second], &[], true, &HashSet::new());
        assert!(
            matches!(
                p.items[0],
                DisplayItem::WipGroup {
                    lane: 2,
                    color: 5,
                    ..
                }
            ),
            "{:?}",
            p.items[0]
        );
    }

    fn edge(from_row: usize, to_row: usize) -> Edge {
        Edge {
            from_row,
            from_lane: 0,
            to_row,
            to_lane: 0,
        }
    }

    #[test]
    fn edges_internal_to_a_folded_run_are_dropped() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            wip_row(3, 1, Some("c4")),
            row(4, "docs: earlier", None),
        ];
        // 0->1 (into the run), 1->2 and 2->3 (internal), 3->4 (out of it).
        let edges = vec![edge(0, 1), edge(1, 2), edge(2, 3), edge(3, 4)];
        let p = project(&rows, &edges, true, &HashSet::new());
        // Display space: [Single(0), WipGroup(1..4), Single(4)] = 0, 1, 2.
        assert_eq!(p.edges.len(), 2, "{:?}", p.edges);
        assert_eq!(
            p.edges[0],
            DisplayEdge {
                from_display: 0,
                from_lane: 0,
                to_display: 1,
                to_lane: 0
            }
        );
        assert_eq!(
            p.edges[1],
            DisplayEdge {
                from_display: 1,
                from_lane: 0,
                to_display: 2,
                to_lane: 0
            }
        );
    }

    #[test]
    fn edge_lanes_pass_through_unchanged() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 2, Some("c2")),
            wip_row(2, 1, None),
        ];
        let edges = vec![Edge {
            from_row: 0,
            from_lane: 3,
            to_row: 1,
            to_lane: 7,
        }];
        let p = project(&rows, &edges, true, &HashSet::new());
        assert_eq!(p.edges[0].from_lane, 3);
        assert_eq!(p.edges[0].to_lane, 7);
    }

    /// The paired positive: with collapse OFF the same input keeps every
    /// edge, so the dropped-edge assertion above is capable of failing
    /// rather than passing because the fixture had no internal edges.
    #[test]
    fn collapse_disabled_keeps_every_edge() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            wip_row(3, 1, Some("c4")),
            row(4, "docs: earlier", None),
        ];
        let edges = vec![edge(0, 1), edge(1, 2), edge(2, 3), edge(3, 4)];
        let p = project(&rows, &edges, false, &HashSet::new());
        assert_eq!(p.edges.len(), 4);
        assert_eq!(
            p.edges[3],
            DisplayEdge {
                from_display: 3,
                from_lane: 0,
                to_display: 4,
                to_lane: 0
            }
        );
    }

    #[test]
    fn display_of_row_maps_a_member_to_its_group_slot() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            wip_row(3, 1, Some("c4")),
            row(4, "docs: earlier", None),
        ];
        let p = project(&rows, &[], true, &HashSet::new());
        assert_eq!(p.display_of_row(0), Some(0));
        // Every member of the run resolves to the group's one slot.
        assert_eq!(p.display_of_row(1), Some(1));
        assert_eq!(p.display_of_row(2), Some(1));
        assert_eq!(p.display_of_row(3), Some(1));
        assert_eq!(p.display_of_row(4), Some(2));
        assert_eq!(p.display_of_row(99), None);
    }

    #[test]
    fn real_checkpoint_messages_match() {
        assert!(is_wip_checkpoint("wip(#66): auto-checkpoint 690"));
        assert!(is_wip_checkpoint("wip(#374): auto-checkpoint 1"));
        assert!(is_wip_checkpoint("wip(#1): auto-checkpoint 999999"));
    }

    #[test]
    fn near_misses_are_left_alone() {
        // Real work that merely mentions the word.
        assert!(!is_wip_checkpoint("fix: stop losing wip on crash"));
        // Hand-written wip commit, not the checkpointer's.
        assert!(!is_wip_checkpoint("wip(#12): fix the thing"));
        // Right prefix, wrong suffix.
        assert!(!is_wip_checkpoint("wip(#12): autocheckpoint 4"));
        // Missing the issue number the checkpointer always writes.
        assert!(!is_wip_checkpoint("wip: auto-checkpoint 4"));
        // Not at the start of the line.
        assert!(!is_wip_checkpoint("revert wip(#66): auto-checkpoint 690"));
        assert!(!is_wip_checkpoint(""));
    }

    #[test]
    fn trailing_content_after_the_number_still_matches() {
        // The checkpointer's own format may grow a suffix; the counter is
        // the last thing it writes today, but matching must not depend on
        // the line ending exactly there.
        assert!(is_wip_checkpoint("wip(#66): auto-checkpoint 690 (rebased)"));
    }
}
