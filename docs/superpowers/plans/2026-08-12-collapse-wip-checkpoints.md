# Collapse WIP-Checkpoint Commits Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fold runs of 2+ consecutive `wip(#N): auto-checkpoint` commits into one expandable summary node in the graph view, so real work isn't buried under checkpoint noise.

**Architecture:** A pure, host-tested projection layer (`features/graph/collapse.rs`) turns `LoadedHistory`'s raw rows/edges into a shorter **display index space** (`Vec<DisplayItem>` + `Vec<DisplayEdge>`). `LoadedHistory` itself — and every invariant `apply_page` enforces — is never touched. The five `<For>` loops in `canvas.rs` iterate display indices instead of raw row indices; `visible_row_range` and `node_cy` need no change because a `WipGroup` occupies exactly one `ROW_HEIGHT` slot.

**Tech Stack:** Rust, Leptos 0.6 CSR (wasm32), Playwright for browser verification.

**Branch:** `feature/issue-374-collapse-consecutive-wip-checkpoint-comm` · **Closes** #374

## Global Constraints

- **Git history is never rewritten.** Display-only. No server, protocol, or `LoadedHistory` change.
- **`LoadedHistory.rows`/`.edges`/`oid_to_row` are read-only to this feature.** Never re-synthesized, never fed back into `apply_page`.
- Commit identity as `Claude_Max <262510778+tom2025b@users.noreply.github.com>`, set per-commit via `-c`, never repo/global config.
- New pure logic goes in a framework-free module (no Leptos, no `#[cfg(target_arch = "wasm32")]`) so `cargo test` reaches it — matching the project's `core.rs` convention.
- `./dev gate` must be green before the PR: fmt, clippy (native + wasm), tests, `trunk build`, browser.
- **Sole git writer is the background checkpointer.** Do not run `git add`/`commit` concurrently with it; the commit steps below are the plan's own, run inline.

---

### Task 1: WIP detection predicate

**Files:**
- Create: `crates/git-vista/src/features/graph/collapse.rs`
- Modify: `crates/git-vista/src/features/graph/mod.rs` (add `pub mod collapse;`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn is_wip_checkpoint(summary: &str) -> bool`

- [ ] **Step 1: Write the failing test**

Create `crates/git-vista/src/features/graph/collapse.rs` with only the test module and a stub:

```rust
//! Display-space projection that folds runs of consecutive WIP-checkpoint
//! commits into one summary node (#374).
//!
//! Framework-free and host-tested, matching this crate's `core.rs`
//! convention: no Leptos, no `#[cfg(target_arch = "wasm32")]` gate, so
//! `cargo test` actually executes it. The wiring that consumes it
//! (`app/canvas.rs`) is wasm-only and is verified by a Playwright test
//! instead — see this feature's plan for why both are required.

/// True for the exact message shape `~/.local/bin/autocheckpoint` produces:
/// `wip(#123): auto-checkpoint 456`. Deliberately strict — a commit that
/// merely mentions "wip" in prose, or a hand-written `wip(#12): fix thing`,
/// is real work and must never be folded away.
pub fn is_wip_checkpoint(summary: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
```

- [ ] **Step 2: Run test to verify it fails**

First wire the module so it compiles — add to `crates/git-vista/src/features/graph/mod.rs`:

```rust
pub mod collapse;
```

Run: `cargo test -p git-vista --bin git-vista-ui collapse::`
Expected: FAIL — `real_checkpoint_messages_match` panics (stub returns `false`).

- [ ] **Step 3: Write minimal implementation**

Replace the stub body. No regex crate — the frontend has no regex dependency and adding one to a wasm bundle for this is not worth it:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p git-vista --bin git-vista-ui collapse::`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/git-vista/src/features/graph/collapse.rs crates/git-vista/src/features/graph/mod.rs
git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(#374): is_wip_checkpoint predicate for autocheckpoint messages"
```

---

### Task 2: The display projection — types and row grouping

**Files:**
- Modify: `crates/git-vista/src/features/graph/collapse.rs`

**Interfaces:**
- Consumes: `is_wip_checkpoint` (Task 1); `git_vista_core::model::{GraphRow, Edge, Oid}`.
- Produces:
  - `pub enum DisplayItem { Single { row_index: usize }, WipGroup { start_row_index: usize, count: usize, lane: usize, color: usize } }`
  - `pub struct DisplayEdge { pub from_display: usize, pub from_lane: usize, pub to_display: usize, pub to_lane: usize }`
  - `pub struct DisplayProjection { pub items: Vec<DisplayItem>, pub edges: Vec<DisplayEdge> }`
  - `pub fn project(rows: &[GraphRow], edges: &[Edge], collapse_enabled: bool, expanded: &std::collections::HashSet<usize>) -> DisplayProjection`

- [ ] **Step 1: Write the failing test**

Append to `collapse.rs` (inside the existing `mod tests`, plus the new public types above it):

```rust
#[cfg(test)]
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

#[cfg(test)]
fn wip_row(i: usize, n: usize, parent: Option<&str>) -> GraphRow {
    row(i, &format!("wip(#66): auto-checkpoint {n}"), parent)
}
```

```rust
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
            matches!(p.items[1], DisplayItem::WipGroup { start_row_index: 1, count: 3, .. }),
            "{:?}", p.items[1]
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
        assert!(p.items.iter().all(|i| matches!(i, DisplayItem::Single { .. })));
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
        assert!(matches!(p.items[0], DisplayItem::WipGroup { start_row_index: 0, count: 2, .. }));
        assert!(matches!(p.items[1], DisplayItem::Single { row_index: 2 }));
        assert!(matches!(p.items[2], DisplayItem::WipGroup { start_row_index: 3, count: 2, .. }));
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
        assert!(p.items.iter().all(|i| matches!(i, DisplayItem::Single { .. })));
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
            "{:?}", p.items
        );
        assert!(matches!(p.items[1], DisplayItem::WipGroup { start_row_index: 1, count: 2, .. }));
    }

    #[test]
    fn collapse_disabled_yields_one_single_per_row() {
        let rows = vec![wip_row(0, 2, Some("c1")), wip_row(1, 1, None)];
        let p = project(&rows, &[], false, &HashSet::new());
        assert_eq!(p.items.len(), 2);
        assert!(p.items.iter().all(|i| matches!(i, DisplayItem::Single { .. })));
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
        assert!(p.items.iter().all(|i| matches!(i, DisplayItem::Single { .. })));
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
            matches!(p.items[0], DisplayItem::WipGroup { lane: 2, color: 5, .. }),
            "{:?}", p.items[0]
        );
    }
```

Add these imports at the top of `collapse.rs`:

```rust
use std::collections::HashSet;

use git_vista_core::model::{Edge, GraphRow};
```

and inside `mod tests`: `use git_vista_core::model::{CommitSummary, Oid};`

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p git-vista --bin git-vista-ui collapse::`
Expected: FAIL to compile — `project`, `DisplayItem`, `DisplayEdge`, `DisplayProjection` not defined.

- [ ] **Step 3: Write minimal implementation**

Add above the test module in `collapse.rs`:

```rust
/// One rendered slot in display space. Every variant occupies exactly one
/// `ROW_HEIGHT` slot — that uniformity is what lets `viewport::
/// visible_row_range` and `geometry::node_cy` stay unchanged, since both
/// only ever assume a fixed stride over *some* row count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayEdge {
    pub from_display: usize,
    pub from_lane: usize,
    pub to_display: usize,
    pub to_lane: usize,
}

/// The whole projection for one render pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayProjection {
    pub items: Vec<DisplayItem>,
    pub edges: Vec<DisplayEdge>,
}

impl DisplayProjection {
    /// Display-space index of the slot showing raw row `row_index` — the
    /// group's own slot when that row is inside a folded run. `None` only
    /// when the row is outside the projected range entirely.
    pub fn display_of_row(&self, row_index: usize) -> Option<usize> {
        self.items.iter().position(|item| match *item {
            DisplayItem::Single { row_index: r } => r == row_index,
            DisplayItem::WipGroup { start_row_index, count, .. } => {
                row_index >= start_row_index && row_index < start_row_index + count
            }
        })
    }
}

/// True when `newer` and `older` are adjacent members of one foldable run:
/// both WIP checkpoints, same lane, and `older` is `newer`'s *sole* parent.
/// The sole-parent check is what stops a merge commit carrying a matching
/// message from being folded away — the checkpointer never makes merges,
/// but this function must not assume that of every caller.
fn same_run(newer: &GraphRow, older: &GraphRow) -> bool {
    is_wip_checkpoint(&newer.commit.summary)
        && is_wip_checkpoint(&older.commit.summary)
        && newer.lane == older.lane
        && newer.commit.parents.len() == 1
        && newer.commit.parents[0] == older.commit.id
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
    let mut i = 0;
    while i < rows.len() {
        if collapse_enabled && !expanded.contains(&i) {
            let mut end = i;
            while end + 1 < rows.len() && same_run(&rows[end], &rows[end + 1]) {
                end += 1;
            }
            let count = end - i + 1;
            if count >= MIN_RUN {
                items.push(DisplayItem::WipGroup {
                    start_row_index: i,
                    count,
                    lane: rows[i].lane,
                    color: rows[i].color,
                });
                i = end + 1;
                continue;
            }
        }
        items.push(DisplayItem::Single { row_index: i });
        i += 1;
    }

    let projection = DisplayProjection {
        items,
        edges: Vec::new(),
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p git-vista --bin git-vista-ui collapse::`
Expected: PASS, 12 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/git-vista/src/features/graph/collapse.rs
git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(#374): display-space projection folding consecutive WIP runs"
```

---

### Task 3: Edge projection tests

**Files:**
- Modify: `crates/git-vista/src/features/graph/collapse.rs` (tests only — `project` already handles edges from Task 2)

**Interfaces:**
- Consumes: `project`, `DisplayEdge` (Task 2).
- Produces: nothing new — this task proves the edge half of Task 2's implementation, which currently has no test naming it.

- [ ] **Step 1: Write the failing test**

The edge code shipped in Task 2 untested on purpose — this task is where it earns its keep. Append to `mod tests`:

```rust
    #[cfg(test)]
    fn edge(from_row: usize, to_row: usize) -> Edge {
        Edge { from_row, from_lane: 0, to_row, to_lane: 0 }
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
        assert_eq!(p.edges[0], DisplayEdge { from_display: 0, from_lane: 0, to_display: 1, to_lane: 0 });
        assert_eq!(p.edges[1], DisplayEdge { from_display: 1, from_lane: 0, to_display: 2, to_lane: 0 });
    }

    #[test]
    fn edge_lanes_pass_through_unchanged() {
        let rows = vec![
            row(0, "feat: real work", Some("c1")),
            wip_row(1, 2, Some("c2")),
            wip_row(2, 1, None),
        ];
        let edges = vec![Edge { from_row: 0, from_lane: 3, to_row: 1, to_lane: 7 }];
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
        assert_eq!(p.edges[3], DisplayEdge { from_display: 3, from_lane: 0, to_display: 4, to_lane: 0 });
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p git-vista --bin git-vista-ui collapse::`
Expected: FAIL to compile — the `edge` helper is new; if Task 2's edge code is correct the assertions pass once it compiles. **If any assertion fails, Task 2's edge logic is wrong — fix it there, not by weakening the assertion.**

- [ ] **Step 3: Make them pass**

If Step 2 showed only the compile error and the tests pass once it compiles, no implementation change is needed — this task is a coverage task. If an assertion genuinely failed, fix `project`'s edge arm in Task 2's code.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p git-vista --bin git-vista-ui collapse::`
Expected: PASS, 16 tests.

- [ ] **Step 5: Mutation-prove the drop rule**

The whole point of the edge arm is dropping internal edges. Prove that assertion can actually fail (project standing rule — use the tool, never a hand-rolled patch script; commit first so the clone sees the code):

```bash
git add -A && git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "test(#374): edge-projection coverage for the collapse projection"
```

Then call the `failure-atlas` MCP's `mutation_check` with:
- `repo`: `/home/tom/projects/Git-Vista`
- `file_path`: `crates/git-vista/src/features/graph/collapse.rs`
- `old_string`: `            if from_display == to_display {\n                return None;\n            }`
- `new_string`: `            if false {\n                return None;\n            }`
- `test_commands`: `[["cargo","test","-p","git-vista","--bin","git-vista-ui","collapse::"]]`
- `run_key`: `gitvista-374-collapse`

Expected: `caught`. Anything else means the drop rule is untested — fix the test before continuing.

---

### Task 4: The collapse preference

**Files:**
- Modify: `crates/git-vista/src/prefs.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn load_collapse_wip_pref() -> bool`, `pub fn store_collapse_wip_pref(on: bool)`

- [ ] **Step 1: Write the implementation**

No test step: this file is `web_sys`-only, has no existing tests, and cannot run on the host — matching the two prefs already there. Append to `crates/git-vista/src/prefs.rs`, after `store_node_icons_pref`:

```rust
/// localStorage key for the WIP-collapse preference: "on" (default) or "off".
const COLLAPSE_WIP_KEY: &str = "git-vista.collapse-wip";

/// Load the "fold runs of auto-checkpoint commits into one node"
/// preference (#374). **Defaults on**: the graph is unreadable on a working
/// branch otherwise — the checkpointer commits every 30s during a session,
/// so real commits end up buried under dozens of near-identical dots.
/// Turning it off shows every checkpoint, for when they matter (bisecting
/// recent WIP history).
pub fn load_collapse_wip_pref() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(COLLAPSE_WIP_KEY).ok().flatten())
        .is_none_or(|v| v != "off")
}

/// Persist the WIP-collapse preference. Best-effort, like the icon prefs.
pub fn store_collapse_wip_pref(on: bool) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(COLLAPSE_WIP_KEY, if on { "on" } else { "off" });
    }
}
```

- [ ] **Step 2: Verify it compiles for wasm**

Run: `cargo clippy -p git-vista --bin git-vista-ui --target wasm32-unknown-unknown -- -D warnings`
Expected: clean (unused-function warnings are fine here only if clippy does not error; if it errors on dead code, proceed to Task 6 which wires it, then re-run).

- [ ] **Step 3: Commit**

```bash
git add crates/git-vista/src/prefs.rs
git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(#374): persisted collapse-wip preference, default on"
```

---

### Task 5: Render builders take display items

**Files:**
- Modify: `crates/git-vista/src/render/nodes.rs` (`build_node`, `build_node_icon`)
- Modify: `crates/git-vista/src/render/edges.rs` (`build_edge`, `visible_edges`)
- Modify: `crates/git-vista/src/render.rs` or wherever `build_msg`/`build_meta` live (find with `grep -rn "pub fn build_msg" crates/git-vista/src`)
- Modify: `crates/git-vista/styles.css` (the `.wip-group` node style)

**Interfaces:**
- Consumes: `DisplayProjection`, `DisplayItem`, `DisplayEdge` (Task 2).
- Produces: builders whose signatures each gain `display: StoredValue<DisplayProjection>` as the parameter right after `ctx`, and whose trailing `i: usize` is now a **display-space** index:
  - `build_node(ctx, display, shell, moved, focus, camera, vp_h, i, on_expand)` where `on_expand: Callback<usize>` receives a group's `start_row_index`
  - `build_node_icon(ctx, display, nerd_icons, i)`
  - `build_msg(ctx, display, nerd_icons, moved, i)`
  - `build_meta(ctx, display, nerd_icons, i)`
  - `build_edge(ctx, display, ei)` — `ei` indexes `display.edges`
  - `visible_edges(display, range) -> Vec<usize>` — now reads `display.edges`, not `c.loaded.edges`

- [ ] **Step 1: Change `visible_edges` and `build_edge`**

In `crates/git-vista/src/render/edges.rs`, replace both functions. Note `edge_path` takes an `&Edge`, so build a temporary `Edge` from the `DisplayEdge` rather than duplicating the path math:

```rust
/// Indices of display edges whose row span intersects the visible display-row
/// window `[start, end)`. Same rule as before collapsing (#374): an edge is
/// kept whenever any part of it could cross the viewport, so a long line
/// passing through never blinks out at the window's edge. Rows still always
/// run downward, so the span is `[from_display, to_display]`.
pub fn visible_edges(display: StoredValue<DisplayProjection>, range: (usize, usize)) -> Vec<usize> {
    let (start, end) = range;
    display.with_value(|d| {
        d.edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.from_display < end && e.to_display >= start)
            .map(|(i, _)| i)
            .collect()
    })
}
```

```rust
pub fn build_edge(
    ctx: StoredValue<RenderCtx>,
    display: StoredValue<DisplayProjection>,
    ei: usize,
) -> View {
    let Some(de) = display.with_value(|d| d.edges.get(ei).copied()) else {
        return ().into_view();
    };
    // The two display slots this edge joins, so the colour rule below can
    // still ask which real commits they represent.
    let (Some(from_item), Some(to_item)) = display.with_value(|d| {
        (d.items.get(de.from_display).copied(), d.items.get(de.to_display).copied())
    }) else {
        return ().into_view();
    };
    ctx.with_value(|c| {
        let rows = &c.loaded.rows;
        // A group takes its first member's identity for colouring purposes.
        let row_of = |item: DisplayItem| match item {
            DisplayItem::Single { row_index } => rows.get(row_index),
            DisplayItem::WipGroup { start_row_index, .. } => rows.get(start_row_index),
        };
        let (Some(from), Some(to)) = (row_of(from_item), row_of(to_item)) else {
            return ().into_view();
        };
        let d = edge_path(&Edge {
            from_row: de.from_display,
            from_lane: de.from_lane,
            to_row: de.to_display,
            to_lane: de.to_lane,
        });
        let is_first_parent = from.commit.parents.first() == Some(&to.commit.id);
        let color = branch_color(if is_first_parent { from.color } else { to.color });
        view! {
            <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
        }
        .into_view()
    })
}
```

Add to `edges.rs`'s imports:

```rust
use git_vista_core::model::Edge;

use crate::features::graph::collapse::{DisplayItem, DisplayProjection};
```

- [ ] **Step 2: Change `build_node`**

In `crates/git-vista/src/render/nodes.rs`, `build_node` gains `display` and `on_expand`, and its first act becomes resolving the display item. Replace the signature and the opening lines (everything from `pub fn build_node(` through `let cy = node_cy(gr.row);`) with:

```rust
pub fn build_node(
    ctx: StoredValue<RenderCtx>,
    display: StoredValue<DisplayProjection>,
    shell: Shell,
    moved: StoredValue<bool>,
    focus: RwSignal<GraphFocus>,
    camera: RwSignal<Camera>,
    vp_h: RwSignal<f64>,
    on_expand: Callback<usize>,
    i: usize,
) -> View {
    let Some(item) = display.with_value(|d| d.items.get(i).copied()) else {
        return ().into_view();
    };
    // A folded run renders as one marker that expands on tap, not as a
    // commit: it has no single identity, so none of the per-commit menu
    // data below applies to it (#374).
    if let DisplayItem::WipGroup { start_row_index, count, lane, color } = item {
        return build_wip_group(moved, on_expand, i, start_row_index, count, lane, color);
    }
    let DisplayItem::Single { row_index } = item else {
        return ().into_view();
    };
    ctx.with_value(|c| {
        // Checked, like every row lookup since paging (M1.10, #63): a `<For>`
        // key can outlive the shape it was built from by one frame.
        let Some(gr) = c.loaded.rows.get(row_index) else {
            return ().into_view();
        };
        let cx = node_cx(gr.lane);
        // Vertical position comes from the DISPLAY index, not `gr.row`:
        // collapsing shortens the space above this commit (#374). Everything
        // else below still reads the real `GraphRow`.
        let cy = node_cy(i);
```

The rest of `build_node`'s body is unchanged **except** `data-row-index=i` — leave it as `i`, which is now the display index, matching what `gestures::on_node_keydown`'s next-frame `.focus()` lookup and `GraphFocus` both now use.

Add to `nodes.rs`'s imports:

```rust
use crate::features::graph::collapse::{DisplayItem, DisplayProjection};
```

- [ ] **Step 3: Add the group node builder**

Append to `crates/git-vista/src/render/nodes.rs`:

```rust
/// A folded run of WIP checkpoints (#374): one hollow, dashed marker
/// carrying the count, which expands the run on tap or Enter/Space. Hollow
/// and dashed so it reads as "something omitted here" rather than as a
/// commit — a filled dot is a real commit everywhere else in this graph,
/// and a branch stub's hollow ring is already the established "not a commit"
/// vocabulary.
#[allow(clippy::too_many_arguments)]
fn build_wip_group(
    moved: StoredValue<bool>,
    on_expand: Callback<usize>,
    i: usize,
    start_row_index: usize,
    count: usize,
    lane: usize,
    color: usize,
) -> View {
    let cx = node_cx(lane);
    let cy = node_cy(i);
    let stroke = branch_color(color);
    let label = format!("⋯ {count} WIP commits ⋯");
    let expand = move |_: web_sys::PointerEvent| {
        if moved.get_value() {
            return;
        }
        on_expand.call(start_row_index);
    };
    let expand_kb = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" || ev.key() == " " {
            ev.prevent_default();
            on_expand.call(start_row_index);
        }
    };
    view! {
        <g class="graph-row wip-group">
            <circle
                cx=cx
                cy=cy
                r=NODE_RADIUS
                fill="none"
                stroke=stroke
                stroke-width="2"
                stroke-dasharray="3 2"
            >
                <title>{label.clone()}</title>
            </circle>
            <text x=cx + NODE_RADIUS + 8 y=cy + 4 class="wip-group-label" fill=stroke>
                {label.clone()}
            </text>
            <circle
                cx=cx
                cy=cy
                r=NODE_RADIUS + 15
                fill="transparent"
                class="node-hit"
                data-row-index=i
                role="button"
                aria-label=label
                aria-expanded="false"
                tabindex="-1"
                on:pointerup=expand
                on:keydown=expand_kb
            />
        </g>
    }
    .into_view()
}
```

- [ ] **Step 4: Change the three text-tier builders**

`build_node_icon` (in `nodes.rs`) and `build_msg`/`build_meta` (find their file with `grep -rn "pub fn build_msg" crates/git-vista/src`) each take `display` after `ctx` and resolve the item first. In each, replace the `let Some(gr) = c.loaded.rows.get(i) else { return ().into_view(); };` line with:

```rust
        // A folded group draws its own label in `build_wip_group`; the text
        // tiers skip it entirely rather than labelling an absent commit.
        let Some(DisplayItem::Single { row_index }) = display.with_value(|d| d.items.get(i).copied())
        else {
            return ().into_view();
        };
        let Some(gr) = c.loaded.rows.get(row_index) else {
            return ().into_view();
        };
```

and in each, change every `node_cy(gr.row)` to `node_cy(i)`.

- [ ] **Step 5: Add the group style**

Append to `crates/git-vista/styles.css`, after the `.node-icon` rules (find with `grep -n "node-icon" crates/git-vista/styles.css`):

```css
/* A folded run of auto-checkpoint commits (#374). Dimmer than a real
   commit's label on purpose: the marker's job is to say "nothing you care
   about is hidden here" while staying out of the way of the real history
   around it. */
.wip-group-label {
  font-size: 0.75rem;
  font-style: italic;
  opacity: 0.7;
}
```

- [ ] **Step 6: Verify it compiles (it will not link yet — canvas.rs is Task 6)**

Run: `cargo clippy -p git-vista --bin git-vista-ui --target wasm32-unknown-unknown -- -D warnings 2>&1 | head -40`
Expected: errors ONLY in `app/canvas.rs` about wrong argument counts to the builders. Any error inside `render/` means this task is not finished.

- [ ] **Step 7: Commit (compiles after Task 6; commit now so the checkpointer's snapshot is coherent)**

```bash
git add crates/git-vista/src/render crates/git-vista/styles.css
git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(#374): render builders address display space, add WIP-group marker"
```

---

### Task 6: Wire the projection into the canvas

**Files:**
- Modify: `crates/git-vista/src/app/canvas.rs` (the five `<For>` loops, `row_count`, and the new `display` memo + `expanded` signal)
- Modify: `crates/git-vista/src/app/mod.rs` (the topbar toggle button, ~`:709-724` beside the existing two)
- Modify: `crates/git-vista/src/state.rs` (add `collapse_wip` to `Settings`, matching `show_node_icons`)

**Interfaces:**
- Consumes: `project`, `DisplayProjection` (Task 2); `load_collapse_wip_pref`/`store_collapse_wip_pref` (Task 4); the Task 5 builder signatures.
- Produces: a working feature — nothing downstream.

- [ ] **Step 1: Add the setting**

In `crates/git-vista/src/state.rs`, find the `Settings` struct (`grep -n "show_node_icons" crates/git-vista/src/state.rs`) and add a field beside it:

```rust
    /// Fold runs of auto-checkpoint commits into one node (#374). A view
    /// preference like `show_node_icons`, not a zoom level.
    pub collapse_wip: RwSignal<bool>,
```

Then, in `crates/git-vista/src/app/mod.rs`, seed it exactly like `show_node_icons` at `:524-528`:

```rust
    let collapse_wip = create_rw_signal(load_collapse_wip_pref());
    let toggle_collapse_wip = move |_| {
        collapse_wip.update(|v| *v = !*v);
        store_collapse_wip_pref(collapse_wip.get_untracked());
    };
```

and add `collapse_wip` to wherever `Settings { .. }` is constructed, and `load_collapse_wip_pref, store_collapse_wip_pref` to the `use crate::prefs::{...}` line at `:58`.

- [ ] **Step 2: Add the topbar button**

In `crates/git-vista/src/app/mod.rs`, immediately after the "Dot icons" button (`:718-724`):

```rust
                <button
                    class="refresh"
                    on:click=toggle_collapse_wip
                    title="Fold runs of auto-checkpoint commits into one node, so real \
                           commits aren't buried under checkpoint noise. Turn off to see \
                           every checkpoint."
                >
                    {move || if collapse_wip.get() { "WIP: folded" } else { "WIP: shown" }}
                </button>
```

- [ ] **Step 3: Add the projection memo and expanded set in `canvas.rs`**

In `crates/git-vista/src/app/canvas.rs`, after `let row_count = create_rw_signal(initial_rows);` (`:171`):

```rust
    // Which folded runs the user has expanded this session, keyed by the
    // group's `start_row_index` (#374). Deliberately not persisted: a reload
    // re-folds everything, which is the point of the preference defaulting on.
    let expanded_groups = create_rw_signal(std::collections::HashSet::<usize>::new());
    // The display-space projection. A `StoredValue` (not a signal) for the
    // same reason `ctx` is one: the builders read it by value inside a
    // `<For>` child, and the `<For>` keys already carry what invalidates it.
    let display = StoredValue::new(DisplayProjection::default());
    // Recompute the projection whenever the rows, the preference, or the
    // expanded set change, and republish the display-space row count that
    // both the culler and `GraphFocus` read.
    create_effect(move |_| {
        let enabled = settings.collapse_wip.get();
        let expanded = expanded_groups.get();
        let epoch = layout_epoch.get();
        let _ = epoch;
        let projected = ctx.with_value(|c| {
            crate::features::graph::collapse::project(
                &c.loaded.rows,
                &c.loaded.edges,
                enabled,
                &expanded,
            )
        });
        row_count.set(projected.items.len());
        display.set_value(projected);
    });
    let on_expand = Callback::new(move |start_row_index: usize| {
        expanded_groups.update(|s| {
            s.insert(start_row_index);
        });
    });
```

Add to `canvas.rs`'s imports:

```rust
use crate::features::graph::collapse::DisplayProjection;
```

- [ ] **Step 4: Stop setting `row_count` from the raw row count**

At `canvas.rs:383-384` the append loop currently does:

```rust
                        ctx.with_value(|c| (c.loaded.rows.len(), c.loaded.is_complete()));
                    row_count.set(rows);
```

`row_count` is now display-space and owned by the Step 3 effect. Change that site to set the layout epoch (which the effect reads) rather than `row_count` directly — replace `row_count.set(rows);` with:

```rust
                    // `row_count` is display-space now (#374) and is owned by
                    // the projection effect; bumping the layout epoch is what
                    // makes that effect re-run against the new rows.
                    layout_epoch.update(|e| *e += 1);
                    let _ = rows;
```

**Verify while doing this** that `layout_epoch` is in scope at that point and is the signal the five `<For>` keys already read (`grep -n "layout_epoch" crates/git-vista/src/app/canvas.rs`). If the append loop already bumps `layout_epoch` on its own, drop the added line and keep only `let _ = rows;`.

- [ ] **Step 5: Pass `display` through the five `<For>` loops**

Change only the `children=` closures (the `each=`/`key=` shapes stay exactly as they are — they already iterate `(s..e)`, which is now display space because `row_count` is):

- Edges (`:505-512`): `each` becomes `{ row_count.get(); render::visible_edges(display, visible.get()) }`, `children=move |ei| render::build_edge(ctx, display, ei)`
- Nodes (`:516-524`): `children=move |(i, _)| render::build_node(ctx, display, shell, moved, focus, camera, vp_h, on_expand, i)`
- Message tier (`:536-545`): `children=move |(i, _, _)| render::build_msg(ctx, display, nerd_icons, moved, i)`
- Meta tier (`:548-557`): `children=move |(i, _, _)| render::build_meta(ctx, display, nerd_icons, i)`
- Node icons (`:565-574`): `children=move |(i, _, _)| render::build_node_icon(ctx, display, nerd_icons, i)`

- [ ] **Step 6: Build and gate**

Run: `cargo clippy -p git-vista --bin git-vista-ui --target wasm32-unknown-unknown -- -D warnings`
Expected: clean.

Run: `./dev gate`
Expected: green, all five checks. **`GraphFocus` needs no change** — it already reads `row_count` (`canvas.rs:184-185`), which is now display-space, so its `active`/`row_count` re-anchor for free. Confirm this by reading those lines rather than assuming.

- [ ] **Step 7: Commit**

```bash
git add crates/git-vista/src/app crates/git-vista/src/state.rs
git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(#374): wire the display projection and the WIP-fold toggle"
```

---

### Task 7: Browser verification (REQUIRED — not optional)

**Files:**
- Create: `ci/browser/tests/wip-collapse.spec.mjs`
- Reference: `ci/browser/tests/reachability.spec.mjs` (copy its fixture/bootstrap shape exactly)

**Interfaces:**
- Consumes: the whole feature.
- Produces: the only evidence that any of it works — `cargo test` never executes `crates/git-vista/src` (wasm-gated), so every host test in Tasks 1–3 proves the algorithm and nothing about the wiring. With the feature defaulting ON, an unverified wiring bug ships to every session on first load.

- [ ] **Step 1: Read the existing spec's setup**

Run: `cat ci/browser/tests/reachability.spec.mjs`
Note how it boots a server, seeds a fixture repo, and signs in. Copy that harness verbatim rather than inventing a second one; find where its fixture repo's commits are created (`grep -rn "auto-checkpoint\|git commit" ci/browser/`).

- [ ] **Step 2: Write the failing test**

The fixture needs a run of 3+ `wip(#N): auto-checkpoint` commits between two real ones. Add them to whatever fixture-seeding helper Step 1 found, then:

```javascript
import { test, expect } from '@playwright/test';
// ...copy the same imports/bootstrap reachability.spec.mjs uses...

test.describe('#374 WIP-checkpoint collapsing', () => {
  test('a run of checkpoints renders as one folded marker by default', async ({ page }) => {
    await openApp(page); // the harness helper from reachability.spec.mjs
    // Default is ON, so the run is folded before any interaction.
    const marker = page.locator('.wip-group');
    await expect(marker).toHaveCount(1);
    await expect(marker.locator('.wip-group-label')).toContainText('WIP commits');
    // The folded members are genuinely absent, not merely hidden.
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS);
  });

  test('tapping the marker expands the run into individual commits', async ({ page }) => {
    await openApp(page);
    const before = await page.locator('.graph-row').count();
    await page.locator('.wip-group .node-hit').click();
    await expect(page.locator('.wip-group')).toHaveCount(0);
    expect(await page.locator('.graph-row').count()).toBeGreaterThan(before);
  });

  test('the topbar toggle shows every checkpoint', async ({ page }) => {
    await openApp(page);
    await page.getByRole('button', { name: /WIP: folded/ }).click();
    await expect(page.locator('.wip-group')).toHaveCount(0);
    await expect(page.getByRole('button', { name: /WIP: shown/ })).toBeVisible();
  });
});
```

Set `EXPECTED_DISPLAY_ROWS` to the fixture's real count once Step 1 tells you how many commits it seeds — do not guess it; run the test once and read the actual number, then assert it deliberately.

- [ ] **Step 3: Run it against the pre-feature build to confirm it can fail**

```bash
git stash push -- crates/git-vista/src crates/git-vista/styles.css
./dev gate 2>&1 | tail -30   # browser leg should FAIL the three new tests
git stash pop
```

Expected: the three new tests fail (no `.wip-group` exists). **If they pass without the feature, they assert nothing — rewrite them.**

- [ ] **Step 4: Run the full gate with the feature**

Run: `./dev gate`
Expected: green, including the three new browser tests.

- [ ] **Step 5: Commit**

```bash
git add ci/browser
git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "test(#374): browser verification for WIP collapsing and tap-to-expand"
```

---

### Task 8: ADR, human testbed, PR

**Files:**
- Create: `docs/adr/0056-<slug>.md` and its PDF twin in `docs/superpowers/pdf/`
- Modify: `design-docs/WORKLOG.md` (+ re-render `WORKLOG.pdf`)

- [ ] **Step 1: Write ADR 0056**

Read `docs/adr/0055-status-readings-carry-a-server-stamped-age.md` for the house shape (metadata bullets → Context → Decision → Alternatives considered → Consequences → Where this is implemented → SECURITY_MODEL.md annotation → `**Signed:**`). Record specifically:
- Why display-only, never history rewriting (the standing never-rewrite rule).
- Why a **second index space** rather than patching `LoadedHistory` — and the load-bearing fact that a `WipGroup` occupies exactly one `ROW_HEIGHT` slot, which is what lets `visible_row_range`/`node_cy` stay untouched.
- Why consecutive-runs-only (merge protection, no lane-routing changes).
- Why default ON.
- What was checked and ruled out: `oid_to_row` has no camera-jump consumer; no `viewBox`/row-count-derived scroll extent exists; no Playwright test asserted on `.graph-row`/`data-row-index` before this feature.

Render the PDF into `docs/superpowers/pdf/` (ADR PDFs beside the `.md` are gitignored):

```bash
cp docs/adr/0056-<slug>.md docs/superpowers/pdf/0056-<slug>.md
~/.local/bin/render-md-pdf docs/superpowers/pdf/0056-<slug>.md
rm docs/superpowers/pdf/0056-<slug>.md
```

- [ ] **Step 2: Append to the worklog and re-render**

Add a dated entry at the TOP of `design-docs/WORKLOG.md` (issue/PR links, 2–4 lines on what and why), then `~/.local/bin/render-md-pdf design-docs/WORKLOG.md`.

- [ ] **Step 3: Commit and push**

```bash
git add docs/adr docs/superpowers/pdf design-docs
git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "docs(#374): ADR 0056 — WIP collapsing is a display projection, not a history rewrite"
git push
```

- [ ] **Step 4: Human testbed — Tom drives it before the PR merges**

Green tests are not a working app in this project (standing rule, and ADR 0054). Stand up a testbed on a free port and hand Tom the link:

```bash
./dev testbed feature/issue-374-collapse-consecutive-wip-checkpoint-comm 8082
```

Then start the built server with that worktree's own `XDG_STATE_HOME` and print a sign-in link with `./gv --token` from inside it (see how this was done for #373 earlier in this session). Ask Tom to confirm: the fold appears by default, the count is right, tapping expands, the toggle works, and the graph doesn't leave dead space where the folded rows were.

- [ ] **Step 5: Open the PR**

```bash
gh pr create --base main --head feature/issue-374-collapse-consecutive-wip-checkpoint-comm \
  --title "feat(#374): fold runs of auto-checkpoint commits into one expandable node" \
  --body "Closes #374 ..."
```

Body must state: display-only (no history rewrite, no server/protocol change), the display-index-space design, the test plan (host tests + the three browser tests + the mutation-proof result from Task 3), and a `**Signed:** 2025 · <ISO timestamp>` line.

- [ ] **Step 6: Land it**

Only after Tom confirms the testbed drive. Use the `land` skill (`/land 375` or whatever number the PR gets) — it waits for all seven required checks to exist and pass before merging, and refreshes the app mirror afterwards.

---

## Self-Review

**Spec coverage:** Detection → Task 1. Grouping rules (min run 2, lane, sole-parent, merge guard) → Task 2. Edge projection → Tasks 2+3. `expanded_groups` → Tasks 2 (logic) + 6 (signal). Persistence/default-ON → Task 4. Topbar toggle → Task 6. Rendering + group marker + CSS → Task 5. Display-index-space correction (five `<For>` loops, `row_count`, `GraphFocus`) → Task 6. Required browser test → Task 7. ADR → Task 8. No spec section is unimplemented.

**Placeholder scan:** Two deliberate lookups remain, both with the exact command to resolve them: `build_msg`/`build_meta`'s file (Task 5, `grep` given) and the browser fixture's commit-seeding helper plus `EXPECTED_DISPLAY_ROWS` (Task 7, `grep` given, with an explicit instruction not to guess the number). Everything else is literal code.

**Type consistency:** `DisplayItem`/`DisplayEdge`/`DisplayProjection`/`project`/`display_of_row`/`is_wip_checkpoint` are spelled identically in Tasks 1–7. Builders consistently take `display: StoredValue<DisplayProjection>` immediately after `ctx`, and their trailing `i` is display-space in every call site. `expanded_groups` is keyed by `start_row_index` everywhere (Task 2 tests, Task 5 `on_expand`, Task 6 signal).

**Risk called out:** Task 6 Step 4 changes who owns `row_count`, the single highest-risk edit in the plan — it carries an explicit verify-don't-assume instruction about `layout_epoch`, and Task 6 Step 6 does the same for `GraphFocus`.
