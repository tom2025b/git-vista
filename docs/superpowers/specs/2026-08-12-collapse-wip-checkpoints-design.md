# Design: Collapse consecutive WIP-checkpoint commits in the graph view

**Date:** 2026-08-12
**Status:** Approved, proceeding to implementation plan
**Signed:** 2025 · 2026-08-12T02:41:24-04:00

## Problem

The autocheckpoint script (`~/.local/bin/autocheckpoint`) commits every 30–1800
seconds during a long session, as `wip(#N): auto-checkpoint <M>`. On a real
working branch this produces long runs of near-identical commits that bury the
real work in the graph view — dozens of checkpoint dots between two meaningful
commits. Tom wants the graph to read cleanly day-to-day without losing anything:
git history itself must stay exactly as committed, never rewritten.

## Non-goals

- **No git history rewriting.** Squashing/rebasing WIP commits away for real is
  explicitly out of scope and would violate the repo's standing never-rewrite-
  pushed-history rule. This is a *display* feature only.
- **No server or wire-protocol change.** Everything needed to detect and group
  WIP commits (the commit message, already-computed lane/row) is already
  present in `GraphRow` as shipped today.
- **No collapsing of non-consecutive WIP commits.** A WIP commit sitting
  between two real commits is left as-is. Collapsing only runs of 2+ in a row.

## Design

### Detection — `is_wip_checkpoint`

A pure function matching the checkpointer's exact message shape:
`^wip\(#\d+\): auto-checkpoint\b`. Lives alongside the new grouping logic
(new module, see below), host-tested against real examples pulled from this
repo's own `git log` plus adversarial near-misses (a commit that merely
mentions "wip" in prose, a message starting `wip(#N)` without the
`auto-checkpoint` suffix, a `wip(#N): auto-checkpoint` inside a much longer
first line).

### Grouping — `build_display_rows`

New module `crates/git-vista/src/features/graph/collapse.rs` (framework-free
`core.rs`-style, matching the project's convention — no Leptos, no
`#[cfg(target_arch = "wasm32")]`, host-testable with `cargo test`).

```rust
pub enum DisplayItem {
    Single { row_index: usize },
    WipGroup { start_row_index: usize, count: usize, oldest_oid: Oid, newest_oid: Oid },
}

pub fn build_display_rows(
    rows: &[GraphRow],
    collapse_enabled: bool,
    expanded_groups: &HashSet<usize>, // keyed by start_row_index
) -> Vec<DisplayItem>
```

A run collapses only when **every** condition holds for each consecutive pair
in the run:
- Both rows match `is_wip_checkpoint`.
- Same `lane`.
- The earlier row (older) is the **sole parent** of the later row (newer) —
  guards against swallowing a merge commit that happens to carry a matching
  message; the checkpointer never produces merges, but this function must not
  assume that of every caller.

Minimum run length to collapse: **2**. A lone WIP commit renders normally —
turning one dot into a "1 WIP commit" group is net-noisier, not cleaner.

`expanded_groups` lets a tapped group render its real rows on the next pass
without touching the persisted collapse preference — expand is per-group,
transient (not persisted across reload), and keyed by the group's
`start_row_index` so it survives re-renders within a session but resets on
reload (reload re-collapses everything, consistent with the pref being "on").

### Edge projection

`GraphRow.row`/`.lane` (the authoritative values `LoadedHistory` validates and
indexes by by — `oid_to_row`, focus, menus) are **never modified**. The
collapse layer computes its own `render_row` per `DisplayItem` purely for
vertical position (`node_cy`), by walking `DisplayItem`s in order and
assigning consecutive render-row slots (a `WipGroup` consumes one slot
regardless of `count`).

Edges are projected alongside:
- An edge whose `from_row` and `to_row` are **both** inside one collapsed
  run's row range is dropped from rendering — it was internal to the run.
- An edge crossing a run's boundary (entering from before the run, or leaving
  after it) is rewritten to reference the group's `render_row` at the crossed
  end, keeping the other end's `render_row` as computed above.

This projection runs fresh on every render pass from `c.loaded.rows` /
`c.loaded.edges` — it is never fed back into `LoadedHistory::apply_page`, so
none of that struct's contiguous-row / no-duplicate-OID / monotonic-lane
invariants are touched by this feature at all.

### Rendering

`render/nodes.rs`'s `build_node` and `render/edges.rs`'s `build_edge` take a
`DisplayItem` (or the small enum above) instead of iterating `c.loaded.rows`
directly. A `DisplayItem::WipGroup` renders as a distinct small node — a
different shape/label (`"⋯ 12 WIP commits ⋯"` per the earlier mockup) with a
click handler that inserts its `start_row_index` into `expanded_groups`
instead of opening the usual per-commit context menu.

### Persistence

`crates/git-vista/src/prefs.rs`, same shape as the two existing icon prefs:

```rust
pub fn load_collapse_wip_pref() -> bool   // default true — see rationale below
pub fn store_collapse_wip_pref(v: bool)
```

localStorage key `git-vista.collapse-wip`. **Default ON**, per Tom's own
call — the point of the feature is a clean graph by default; toggling off is
the escape hatch when every checkpoint matters (e.g. bisecting recent WIP
history).

### Topbar

A third `<button class="refresh">` in `app/mod.rs`, beside the existing
"Icons: glyphs/text" and "Dot icons: on/off" buttons, following their exact
signal + toggle-closure + `title=` pattern (`app/mod.rs:514-528`,
`:709-724`).

## Testing

- `is_wip_checkpoint`: real examples from this repo's own `git log`
  (`wip(#66): auto-checkpoint 690`, etc.) plus the adversarial near-misses
  listed above under Detection.
- `build_display_rows`: consecutive runs of 2/3/many collapse; a lone WIP
  commit does not; a run interrupted by a real commit splits into two groups
  (or a group + a single, depending on run length either side); a WIP-worded
  merge commit is never absorbed into a group; edges crossing a run's
  boundary are rewritten to the correct `render_row`; a group in
  `expanded_groups` renders as individual `Single` items instead.
- No server-side tests needed — nothing server-side changes.
- Existing browser tests (`reachability.spec.mjs`'s topbar-chip fixture
  check) should be unaffected; a new Playwright test may be worth adding for
  the collapsed-group tap-to-expand interaction, decided during
  implementation.

## ADR

Tom asked for one on this branch (standing "always do ADR on any notable
architecture decision" rule, and explicit ask this session). Draft during
implementation once the exact module boundary is settled — it will record:
why display-only over history rewrite, why consecutive-only over any-position,
why default-ON, and the row/edge invariant-preservation reasoning above so a
future reader doesn't have to re-derive it from the diff.
