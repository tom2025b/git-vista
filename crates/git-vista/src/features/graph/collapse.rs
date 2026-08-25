//! Display-space projection that folds runs of consecutive WIP-checkpoint
//! commits into one summary node (#374).
//!
//! "Consecutive" means consecutive *in the commit chain* — each member the
//! sole parent of the one above it, all in one lane — not consecutive on
//! screen. The two coincide right up until a branch and its diverged
//! remote-tracking twin put two checkpoint chains in the graph at once: date
//! order interleaves them row by row, and a scan over display neighbours then
//! sees nothing but cross-chain pairs and folds nothing (#478). `find_runs`
//! walks parent pointers instead, and a folded run takes the slot of its
//! newest member while its other members leave display space wherever they
//! sit.
//!
//! Framework-free and host-tested, matching this crate's `core.rs`
//! convention: no Leptos, no `#[cfg(target_arch = "wasm32")]` gate, so
//! `cargo test` actually executes it. The wiring that consumes it
//! (`app/canvas.rs`) is wasm-only and is verified by a Playwright test
//! instead — see this feature's plan for why both are required.

use std::collections::{HashMap, HashSet};

use git_vista_core::model::{Edge, GraphRow, Oid};

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
    /// A folded run of `count` WIP checkpoints, drawn in the slot its
    /// *newest* member would have occupied — `anchor_row_index`, the
    /// topmost member in display order. `lane`/`color` are copied from that
    /// row: arbitrary but consistent, since every member shares a lane by
    /// construction of the grouping rule.
    ///
    /// `count` is a member count, **not** a row span: a run's members need
    /// not be adjacent in display order (#478). Two diverged branches whose
    /// checkpoint chains interleave produce two runs whose members alternate
    /// row by row, and each folds into its own slot with the other's rows
    /// still between them in `LoadedHistory.rows`. Use
    /// [`DisplayProjection::display_of_row`] to map a raw row to its slot;
    /// `anchor_row_index .. anchor_row_index + count` is not that range.
    WipGroup {
        anchor_row_index: usize,
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

impl DisplayEdge {
    /// The display rows this edge spans, topmost first — what a viewport
    /// filter has to compare against.
    ///
    /// Raw edges always run downward (a parent is below its child), and
    /// before #478 so did display edges: a folded run replaced a contiguous
    /// span, and contiguous spans collapse in order. Non-adjacent folding
    /// breaks that. When two interleaved chains fold, an edge from the
    /// *tail* of the lower chain to a fork point inside the upper one is
    /// redrawn between the two markers — and the upper chain's marker sits
    /// above, so `from_display > to_display`. `edge_path` draws that
    /// perfectly well (its maths is symmetric); a culler comparing the two
    /// ends positionally does not, which is why the span is taken here
    /// rather than assumed at each call site.
    pub fn span(&self) -> (usize, usize) {
        if self.from_display <= self.to_display {
            (self.from_display, self.to_display)
        } else {
            (self.to_display, self.from_display)
        }
    }
}

/// A run of WIP checkpoints the user has opened, kept so one section can be
/// folded again on its own (#374 follow-up).
///
/// An expanded run is emitted as ordinary `Single`s, which makes it
/// indistinguishable from unrelated commits by the time a view sees it — so
/// the fact that these particular rows *were* a foldable run has to be
/// carried explicitly, or the only way back is the topbar toggle that folds
/// the entire graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WipRun {
    /// Every member's raw row index, ascending — i.e. newest first, the same
    /// order `LoadedHistory.rows` is in.
    ///
    /// A list, not a `start + count` range (#478). A run is a parent→child
    /// chain, and two chains can interleave row for row in display order, so
    /// a range would name the *other* chain's commits as members of this one
    /// — which is exactly the mis-grouping the negative case forbids.
    pub rows: Vec<usize>,
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
    /// Raw row index -> the display slot showing it, `None` for a row folded
    /// away into a group whose slot is elsewhere. Built during the projection
    /// walk rather than searched for afterwards: with non-adjacent folding a
    /// group no longer covers a row *range*, so there is nothing an item can
    /// be interrogated about to answer `display_of_row` (#478). Private, and
    /// the reason this struct is only ever built by `project` or `default`.
    row_display: Vec<Option<usize>>,
}

impl DisplayProjection {
    /// How many WIP runs this projection contains, folded or open (#382).
    ///
    /// Counts BOTH, deliberately. The number exists to answer "are there runs
    /// in this history at all", which an operator cannot otherwise tell from
    /// a viewport showing twenty rows out of hundreds — a graph whose runs sit
    /// thirty commits down looks exactly like a graph with none, and reads as
    /// a broken feature. Dropping a run from the count when it is expanded
    /// would report 0 over a graph plainly showing checkpoints.
    ///
    /// Zero when collapsing is switched off globally, because then nothing is
    /// being hidden and a count of hidden runs would describe nothing.
    pub fn wip_run_count(&self) -> usize {
        let folded = self
            .items
            .iter()
            .filter(|item| matches!(item, DisplayItem::WipGroup { .. }))
            .count();
        folded + self.expanded_runs.len()
    }

    /// The open run this raw row belongs to, if any — what a row's own view
    /// needs in order to offer "fold these N checkpoints".
    pub fn run_containing_row(&self, row_index: usize) -> Option<WipRun> {
        self.expanded_runs
            .iter()
            .find(|run| run.rows.contains(&row_index))
            .cloned()
    }

    /// Display-space index of the slot showing raw row `row_index` — the
    /// group's own slot when that row is inside a folded run. `None` only
    /// when the row is outside the projected range entirely.
    pub fn display_of_row(&self, row_index: usize) -> Option<usize> {
        self.row_display.get(row_index).copied().flatten()
    }
}

/// True when `newer` and `older` are consecutive members of one foldable run:
/// both WIP checkpoints, same lane, `older` is `newer`'s *sole* parent, and
/// neither is itself a merge commit. Both parent-count checks matter: the
/// first stops a merge from being absorbed when it plays `newer` (multiple
/// parents means `older` isn't its sole one); the second stops one when it
/// plays `older` (a 2-parent commit reachable as *someone's* sole parent is
/// still a merge in its own right, and folding it away would hide that
/// topology join even though the child->it edge is unambiguous). The
/// checkpointer never makes merges, but this function must not assume that
/// of every caller.
///
/// "Consecutive" is about the *chain*, never about display order: `find_runs`
/// hands this only genuine child/sole-parent pairs, which is what lets a run
/// whose members are scattered down the screen still be one run (#478).
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

/// Every foldable run in `rows`, each as its members' raw row indices,
/// ascending.
///
/// **Scanned per chain, not per display position** (#478). The old scan
/// extended a run only while *display-adjacent* rows satisfied [`same_run`],
/// which silently required a run to be contiguous on screen. It is not: a
/// branch and a diverged remote-tracking twin both carry `wip(#N):
/// auto-checkpoint M`, the graph orders rows by date, and the two chains
/// interleave perfectly — so every adjacent pair came from *different*
/// chains, every run measured 1, and the longest checkpoint stretches in a
/// repository were the ones that never folded.
///
/// Following the parent pointers instead makes adjacency irrelevant while
/// keeping every guarantee [`same_run`] gives, because it is still the sole
/// judge of membership: two checkpoints join one run only when one is the
/// other's *sole parent* and they share a lane. Interleaved neighbours fail
/// that as decisively here as they did before — this widens which pairs the
/// predicate is *shown*, never what it accepts.
fn find_runs(rows: &[GraphRow]) -> Vec<Vec<usize>> {
    // Where each commit landed, so a checkpoint can reach its parent's row
    // without depending on the parent being the next row down.
    let mut row_of: HashMap<&Oid, usize> = HashMap::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        row_of.insert(&r.commit.id, i);
    }

    // `next[i]`: the row holding row `i`'s parent, when the two are members
    // of one run. Every link is a `same_run` verdict on the exact pair the
    // predicate is about — child and its sole parent.
    let mut next: Vec<Option<usize>> = vec![None; rows.len()];
    let mut children = vec![0usize; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        let Some(parent) = r.commit.parents.first() else {
            continue;
        };
        // Parent not loaded (below the last page, or unreachable): nothing to
        // chain onto. The run simply ends here and grows on the next page.
        let Some(&p) = row_of.get(parent) else {
            continue;
        };
        if same_run(r, &rows[p]) {
            next[i] = Some(p);
            children[p] += 1;
        }
    }

    // A commit two checkpoints both claim is a fork point: the chain splits
    // there and neither branch owns it. Cut both links rather than picking a
    // side — a group is supposed to stand for one chain, and a fork point
    // folded into one of them would hide that the other started there.
    let forks: Vec<bool> = children.iter().map(|&n| n > 1).collect();
    for link in next.iter_mut() {
        if link.is_some_and(|p| forks[p]) {
            *link = None;
        }
    }

    // With forks cut, every row has at most one predecessor, so each chain
    // has exactly one head: a linked row nothing links *to*.
    let mut has_predecessor = vec![false; rows.len()];
    for &link in next.iter() {
        if let Some(p) = link {
            has_predecessor[p] = true;
        }
    }

    let mut runs = Vec::new();
    for head in 0..rows.len() {
        if has_predecessor[head] || next[head].is_none() {
            continue;
        }
        let mut members = vec![head];
        let mut cur = head;
        while let Some(p) = next[cur] {
            members.push(p);
            cur = p;
        }
        // Parents sort below their children in display order, so the walk
        // already yields ascending indices; sorting says so unconditionally
        // rather than leaving the anchor at the mercy of that invariant.
        members.sort_unstable();
        if members.len() >= MIN_RUN {
            runs.push(members);
        }
    }
    runs
}

/// Project raw rows and edges into display space. Pure: no I/O, no signal
/// reads, no mutation of the inputs.
pub fn project(
    rows: &[GraphRow],
    edges: &[Edge],
    collapse_enabled: bool,
    expanded: &HashSet<usize>,
) -> DisplayProjection {
    // Runs are found once, over the whole history, before display space is
    // laid out — a run is a property of the commit chain, not of where its
    // members happen to sit on screen.
    let runs = find_runs(rows);
    let mut run_of_row: Vec<Option<usize>> = vec![None; rows.len()];
    for (id, members) in runs.iter().enumerate() {
        for &m in members {
            run_of_row[m] = Some(id);
        }
    }

    let mut items = Vec::with_capacity(rows.len());
    let mut expanded_runs: Vec<WipRun> = Vec::new();
    let mut row_display: Vec<Option<usize>> = vec![None; rows.len()];
    // Each run is decided once, at whichever of its members display order
    // reaches first — its anchor. Deciding again at every member would
    // re-examine the tail as a run in its own right, which is how a
    // three-member run grew a fresh two-member group the moment the user
    // opened it and the marker never went away (#374, caught by the browser
    // spec, pinned by the two tests below).
    let mut decided = vec![false; runs.len()];
    for i in 0..rows.len() {
        let Some(id) = run_of_row[i] else {
            row_display[i] = Some(items.len());
            items.push(DisplayItem::Single { row_index: i });
            continue;
        };
        let members = &runs[id];
        // Membership anywhere in the run, not only at its anchor: an append
        // can put a NEWER checkpoint above a run the user already opened,
        // moving the anchor out from under the row they tapped.
        let user_expanded = members.iter().any(|m| expanded.contains(m));
        if collapse_enabled && !user_expanded {
            if !decided[id] {
                decided[id] = true;
                let slot = items.len();
                items.push(DisplayItem::WipGroup {
                    anchor_row_index: i,
                    count: members.len(),
                    lane: rows[i].lane,
                    color: rows[i].color,
                });
                // Every member resolves to that one slot, including the ones
                // display order has not reached yet and the ones it already
                // passed — a run's members need not be adjacent (#478).
                for &m in members {
                    row_display[m] = Some(slot);
                }
            }
            // A folded member other than the anchor takes no slot of its own:
            // this is the removal, and it is what makes a non-contiguous run
            // foldable at all.
            continue;
        }
        if collapse_enabled && !decided[id] {
            // Foldable, but open: remember it so this one section can be
            // folded again without touching the rest of the graph.
            decided[id] = true;
            expanded_runs.push(WipRun {
                rows: members.clone(),
            });
        }
        row_display[i] = Some(items.len());
        items.push(DisplayItem::Single { row_index: i });
    }

    let display_of_row = |row: usize| row_display.get(row).copied().flatten();
    let display_edges = edges
        .iter()
        .filter_map(|e| {
            let from_display = display_of_row(e.from_row)?;
            let to_display = display_of_row(e.to_row)?;
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
        items,
        edges: display_edges,
        expanded_runs,
        row_display,
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
                    anchor_row_index: 1,
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
                anchor_row_index: 0,
                count: 2,
                ..
            }
        ));
        assert!(matches!(p.items[1], DisplayItem::Single { row_index: 2 }));
        assert!(matches!(
            p.items[2],
            DisplayItem::WipGroup {
                anchor_row_index: 3,
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
                anchor_row_index: 1,
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
                rows: vec![1, 2, 3]
            }]
        );
        assert_eq!(
            p.run_containing_row(2),
            Some(WipRun {
                rows: vec![1, 2, 3]
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

    // ── #382: the count that tells the operator runs exist off-screen ──

    #[test]
    fn a_projection_with_no_wip_runs_counts_zero() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            row(1, "docs: more real work", None),
        ];
        assert_eq!(
            project(&rows, &[], true, &HashSet::new()).wip_run_count(),
            0
        );
    }

    #[test]
    fn one_folded_run_counts_one() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            row(3, "docs: earlier", None),
        ];
        assert_eq!(
            project(&rows, &[], true, &HashSet::new()).wip_run_count(),
            1
        );
    }

    #[test]
    fn two_separated_runs_count_two() {
        let rows = vec![
            wip_row(0, 4, Some("c1")),
            wip_row(1, 3, Some("c2")),
            row(2, "feat: between the runs", Some("c3")),
            wip_row(3, 2, Some("c4")),
            wip_row(4, 1, None),
        ];
        assert_eq!(
            project(&rows, &[], true, &HashSet::new()).wip_run_count(),
            2
        );
    }

    #[test]
    fn an_expanded_run_still_counts() {
        // The whole point of the number is "runs exist here". Dropping a run
        // from the count the moment it is opened would make the badge blink
        // out exactly when the operator proved it was real — and would report
        // 0 on a graph plainly showing checkpoints.
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            row(3, "docs: earlier", None),
        ];
        let open: HashSet<usize> = [1].into_iter().collect();
        let p = project(&rows, &[], true, &open);
        assert!(
            p.items.len() > 3,
            "precondition: the run really is expanded {:?}",
            p.items
        );
        assert_eq!(p.wip_run_count(), 1);
    }

    #[test]
    fn with_collapsing_switched_off_entirely_the_count_is_zero() {
        // Toggle off means "show me everything"; there are no runs being
        // hidden, so a count of hidden runs would be a claim about nothing.
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 3, Some("c2")),
            wip_row(2, 2, Some("c3")),
            row(3, "docs: earlier", None),
        ];
        assert_eq!(
            project(&rows, &[], false, &HashSet::new()).wip_run_count(),
            0
        );
    }

    #[test]
    fn a_lone_checkpoint_is_not_a_run() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 1, Some("c2")),
            row(2, "docs: earlier", None),
        ];
        assert_eq!(
            project(&rows, &[], true, &HashSet::new()).wip_run_count(),
            0
        );
    }

    // ── #478: two diverged chains interleaved in display order ──
    //
    // A branch and its diverged remote-tracking twin carry the *same*
    // checkpoint messages on different commits, and the graph orders rows by
    // date — so the two chains alternate row for row. Every display-adjacent
    // pair is then a cross-chain pair, which is why the old adjacency scan
    // measured every run at 1 and folded nothing.

    /// A checkpoint row with an explicit id, parent, and lane, so two chains
    /// can be built that share message text and nothing else.
    fn chain_row(i: usize, id: &str, parent: Option<&str>, lane: usize, n: usize) -> GraphRow {
        let mut r = wip_row(i, n, parent);
        r.commit.id = Oid(id.to_string());
        r.lane = lane;
        r.color = lane;
        r
    }

    /// Local chain L1<-L2<-L3 (lane 0) and its diverged twin R1<-R2<-R3
    /// (lane 1), perfectly interleaved: L1, R1, L2, R2, L3, R3.
    fn interleaved_twins() -> Vec<GraphRow> {
        vec![
            chain_row(0, "L1", Some("L2"), 0, 3),
            chain_row(1, "R1", Some("R2"), 1, 3),
            chain_row(2, "L2", Some("L3"), 0, 2),
            chain_row(3, "R2", Some("R3"), 1, 2),
            chain_row(4, "L3", None, 0, 1),
            chain_row(5, "R3", None, 1, 1),
        ]
    }

    #[test]
    fn two_interleaved_chains_each_fold_into_their_own_group() {
        let p = project(&interleaved_twins(), &[], true, &HashSet::new());

        assert_eq!(p.items.len(), 2, "{:?}", p.items);
        assert!(
            matches!(
                p.items[0],
                DisplayItem::WipGroup {
                    anchor_row_index: 0,
                    count: 3,
                    lane: 0,
                    ..
                }
            ),
            "{:?}",
            p.items[0]
        );
        assert!(
            matches!(
                p.items[1],
                DisplayItem::WipGroup {
                    anchor_row_index: 1,
                    count: 3,
                    lane: 1,
                    ..
                }
            ),
            "{:?}",
            p.items[1]
        );
    }

    #[test]
    fn each_interleaved_members_row_resolves_to_its_own_chains_slot() {
        // The assertion above cannot tell a correct grouping from one that
        // folded three rows of the *wrong* chain: both give two groups of
        // three. Membership is what separates them, and `display_of_row` is
        // where every consumer reads it.
        let p = project(&interleaved_twins(), &[], true, &HashSet::new());

        // L1, L2, L3 — rows 0, 2, 4 — all show in the lane-0 group's slot.
        assert_eq!(p.display_of_row(0), Some(0));
        assert_eq!(p.display_of_row(2), Some(0));
        assert_eq!(p.display_of_row(4), Some(0));
        // R1, R2, R3 — rows 1, 3, 5 — all show in the lane-1 group's slot.
        assert_eq!(p.display_of_row(1), Some(1));
        assert_eq!(p.display_of_row(3), Some(1));
        assert_eq!(p.display_of_row(5), Some(1));
    }

    #[test]
    fn two_adjacent_checkpoints_from_different_chains_are_never_one_group() {
        // THE load-bearing negative (#478). Both rows are checkpoints, both
        // sit in the same lane, both have exactly one parent, and they are
        // display-adjacent — everything a "just relax the lane check" fix
        // would need. Neither is the other's parent, so they are two
        // different branches' work and folding them together would claim a
        // chain that does not exist.
        let rows = vec![
            chain_row(0, "A1", Some("A-parent"), 0, 7),
            chain_row(1, "B1", Some("B-parent"), 0, 7),
        ];

        let p = project(&rows, &[], true, &HashSet::new());

        assert_eq!(p.items.len(), 2, "{:?}", p.items);
        assert!(
            p.items
                .iter()
                .all(|i| matches!(i, DisplayItem::Single { .. })),
            "{:?}",
            p.items
        );
        assert_eq!(p.wip_run_count(), 0);
    }

    #[test]
    fn interleaved_chains_sharing_a_lane_still_fold_separately() {
        // Same shape as the twins above but with both chains drawn in ONE
        // lane, so the lane check cannot be what keeps them apart — only the
        // parent identity can. A1<-A2 and B1<-B2, interleaved.
        let rows = vec![
            chain_row(0, "A1", Some("A2"), 0, 2),
            chain_row(1, "B1", Some("B2"), 0, 2),
            chain_row(2, "A2", None, 0, 1),
            chain_row(3, "B2", None, 0, 1),
        ];

        let p = project(&rows, &[], true, &HashSet::new());

        assert_eq!(p.items.len(), 2, "{:?}", p.items);
        // Chain A's group holds rows 0 and 2 — never row 1, its neighbour.
        assert_eq!(p.display_of_row(0), Some(0));
        assert_eq!(p.display_of_row(2), Some(0));
        assert_eq!(p.display_of_row(1), Some(1));
        assert_eq!(p.display_of_row(3), Some(1));
    }

    #[test]
    fn a_checkpoint_two_chains_fork_from_joins_neither() {
        // P is the sole parent of both A2 and B2 and is itself a checkpoint
        // in their lane. Folding it into one of the two would hide that the
        // other branch started there, so the chains stop above it and P keeps
        // a row of its own.
        let rows = vec![
            chain_row(0, "A1", Some("A2"), 0, 3),
            chain_row(1, "B1", Some("B2"), 0, 3),
            chain_row(2, "A2", Some("P"), 0, 2),
            chain_row(3, "B2", Some("P"), 0, 2),
            chain_row(4, "P", None, 0, 1),
        ];

        let p = project(&rows, &[], true, &HashSet::new());

        assert_eq!(p.items.len(), 3, "{:?}", p.items);
        assert!(
            matches!(
                p.items[0],
                DisplayItem::WipGroup {
                    anchor_row_index: 0,
                    count: 2,
                    ..
                }
            ),
            "{:?}",
            p.items[0]
        );
        assert!(
            matches!(
                p.items[1],
                DisplayItem::WipGroup {
                    anchor_row_index: 1,
                    count: 2,
                    ..
                }
            ),
            "{:?}",
            p.items[1]
        );
        assert!(
            matches!(p.items[2], DisplayItem::Single { row_index: 4 }),
            "{:?}",
            p.items[2]
        );
    }

    #[test]
    fn opening_one_interleaved_run_leaves_the_other_folded() {
        let rows = interleaved_twins();
        // A MIDDLE member of the lane-0 chain, which in display order sits
        // between two rows belonging to the other chain.
        let open: HashSet<usize> = [2].into_iter().collect();

        let p = project(&rows, &[], true, &open);

        // L1, the R group, L2, L3 — the opened chain's three rows back in
        // place, the twin still one marker.
        assert_eq!(p.items.len(), 4, "{:?}", p.items);
        assert!(matches!(p.items[0], DisplayItem::Single { row_index: 0 }));
        assert!(
            matches!(
                p.items[1],
                DisplayItem::WipGroup {
                    anchor_row_index: 1,
                    count: 3,
                    ..
                }
            ),
            "{:?}",
            p.items[1]
        );
        assert!(matches!(p.items[2], DisplayItem::Single { row_index: 2 }));
        assert!(matches!(p.items[3], DisplayItem::Single { row_index: 4 }));
        // The open run is offered for re-folding by its real membership, not
        // by a row range that would sweep in the twin's commits.
        assert_eq!(
            p.expanded_runs,
            vec![WipRun {
                rows: vec![0, 2, 4]
            }]
        );
        assert_eq!(
            p.run_containing_row(4),
            Some(WipRun {
                rows: vec![0, 2, 4]
            })
        );
        assert_eq!(p.run_containing_row(1), None);
        assert_eq!(p.run_containing_row(3), None);
        // One folded, one open: both are runs this history holds.
        assert_eq!(p.wip_run_count(), 2);
    }

    #[test]
    fn a_fork_point_inside_a_folded_run_keeps_the_edge_that_reaches_it() {
        // The diverged-twin shape as it actually occurs: the two chains share
        // an ancestor. P is in lane 0, so it chains onto L2 and folds into
        // the lane-0 group; R2's link to it crosses lanes and does not.
        let rows = vec![
            chain_row(0, "L1", Some("L2"), 0, 3),
            chain_row(1, "R1", Some("R2"), 1, 3),
            chain_row(2, "L2", Some("P"), 0, 2),
            chain_row(3, "R2", Some("P"), 1, 2),
            chain_row(4, "P", None, 0, 1),
        ];
        let edges = vec![
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 2,
                to_lane: 0,
            },
            Edge {
                from_row: 1,
                from_lane: 1,
                to_row: 3,
                to_lane: 1,
            },
            Edge {
                from_row: 2,
                from_lane: 0,
                to_row: 4,
                to_lane: 0,
            },
            Edge {
                from_row: 3,
                from_lane: 1,
                to_row: 4,
                to_lane: 0,
            },
        ];

        let p = project(&rows, &edges, true, &HashSet::new());

        assert_eq!(p.items.len(), 2, "{:?}", p.items);
        // Three of the four edges are internal to one group or the other.
        // The survivor is R2 -> P, and it points UPWARD in display space: the
        // twin's marker is at slot 1, the fork point it descends from folded
        // into the marker at slot 0.
        assert_eq!(
            p.edges,
            vec![DisplayEdge {
                from_display: 1,
                from_lane: 1,
                to_display: 0,
                to_lane: 0,
            }],
            "{:?}",
            p.edges
        );
        // Which is why a culler must take the span rather than assume the
        // endpoints arrive in order.
        assert_eq!(p.edges[0].span(), (0, 1));
    }

    #[test]
    fn a_downward_edges_span_is_its_endpoints_unchanged() {
        // The paired positive for `span`: the ordinary case must not be
        // silently reordered.
        let e = DisplayEdge {
            from_display: 2,
            from_lane: 0,
            to_display: 7,
            to_lane: 1,
        };
        assert_eq!(e.span(), (2, 7));
    }

    #[test]
    fn a_chain_whose_parent_is_not_loaded_yet_still_folds_what_is_there() {
        // Paged history: the run's oldest loaded member points at a commit
        // below the last page. That is a missing row, not a broken chain, so
        // the members that ARE loaded still fold.
        let rows = vec![
            chain_row(0, "L1", Some("L2"), 0, 3),
            chain_row(1, "L2", Some("not-loaded"), 0, 2),
        ];

        let p = project(&rows, &[], true, &HashSet::new());

        assert_eq!(p.items.len(), 1, "{:?}", p.items);
        assert!(matches!(
            p.items[0],
            DisplayItem::WipGroup {
                anchor_row_index: 0,
                count: 2,
                ..
            }
        ));
    }
}
