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

### Correction (2026-08-12, same session) — the real blast radius

The paragraphs above describe the *shape* of the transform correctly but
understated where it has to plug in. `LoadedHistory.rows`/`.edges` are not
read directly by one or two render functions — **five** separate `<For>`
loops in `crates/git-vista/src/app/canvas.rs` (edges, nodes, message tier,
meta tier, node-icon tier) all iterate a raw row-index range `(s..e)`
produced by `viewport::visible_row_range(camera, vp_h, row_count, overscan)`,
which itself inverts `node_cy(r) = PAD_Y + r * ROW_HEIGHT` — i.e. it
hard-assumes row index space is `LoadedHistory.rows`'s own 0-based index,
laid out at a uniform stride. `GraphFocus` (`features/a11y/focus.rs`) makes
the identical assumption for keyboard roving-tabindex: its own doc comment
says outright that `RenderCtx::loaded.rows` "already indexes them 0..
row_count exactly the way this model expects."

**Resolved design: a second, shorter index space ("display space"), not a
patch to `LoadedHistory`.** A `DisplayProjection` is computed as a memo
alongside `RenderCtx` — `Vec<DisplayItem>` (`Single{row_index}` or
`WipGroup{start_row_index, count, ..}`) plus `Vec<DisplayEdge>` with
endpoints already expressed as display-space indices. Because a `WipGroup`
still occupies exactly one `ROW_HEIGHT` slot (same visual weight as one
commit — this is what makes the whole approach work), `visible_row_range`
and `node_cy` need **no changes at all**: they're already generic over "some
row count," so the only change is what's *fed* to them —
`display.items.len()` instead of `c.loaded.rows.len()`, in exactly one place
(the `row_count` `RwSignal` currently set from `c.loaded.rows.len()` at
`canvas.rs:383-384`).

`LoadedHistory.rows`/`.edges`/`oid_to_row` themselves are **never modified,
never re-synthesized, and never fed back into `apply_page`** — this was the
one part of the original design that was right, and it's why `apply_page`'s
contiguous-row / no-duplicate-OID / monotonic-lane invariants stay untouched
by this feature. `oid_to_row` and its real consumers (checked this session:
only `render/labels.rs`'s stub-anchor lookup, comparing a stub's anchor
commit against a real `GraphRow.row` — no camera-jump-to-commit feature
exists anywhere in the app today) operate purely in raw-row space and are
never routed through display space at all, since branch-stub markers are a
separate render tier from the five collapsed-row `<For>` loops. Checked and
ruled out as a concern.

Also checked and ruled out: the SVG has no `viewBox` (`camera.rs`'s own doc
comment: "the SVG has no `viewBox`, so one user unit equals one CSS pixel"),
and no pan-clamp bound exists tied to row count — panning is free
translation, culling just stops rendering past the window. So a shorter
collapsed graph does not leave dead scroll space; there's no fixed-height
element sized off row count to leave dead space in.

### Edge projection

Edges are projected alongside the row projection, in the same memo:
- An edge whose `from_row` and `to_row` are **both** inside one collapsed
  run's row range is dropped from rendering — it was internal to the run.
- An edge crossing a run's boundary (entering from before the run, or leaving
  after it) is rewritten to a `DisplayEdge` referencing the group's
  display-space index at the crossed end, and the other end's own
  display-space index (computed by walking `DisplayItem`s in order — a
  `WipGroup` consumes one display-space slot regardless of `count`).
- Lane values (`from_lane`/`to_lane`) copy straight through from the source
  `Edge` unchanged — lanes never change, only row/display-index does.

### Rendering

The five `<For>` loops in `canvas.rs` change their `each=` source from
`(s..e)` over raw `LoadedHistory` row indices to `(s..e)` over
`DisplayProjection` indices (`visible_row_range` itself needs no change —
see above). `build_node`, `build_msg`, `build_meta`, and `build_node_icon`
(all in `render/nodes.rs`/`render.rs`) each currently take a raw `i: usize`
and do `c.loaded.rows.get(i)`; each instead takes the `DisplayProjection`
(via a `StoredValue`, same pattern as `ctx`) plus a display-space `i`,
resolves `display.items.get(i)`, and for `Single{row_index}` looks up
`c.loaded.rows[row_index]` exactly as today but positions at
`node_cy(i)` (display index) rather than `node_cy(gr.row)` (raw row) —
color/oid/menu data still comes from the real `GraphRow` via `row_index`.
`render/edges.rs`'s `build_edge` takes a `DisplayEdge` the same way.

A `DisplayItem::WipGroup` renders as a distinct small node — a different
shape/label (`"⋯ 12 WIP commits ⋯"` per the earlier mockup) with a click
handler that inserts its `start_row_index` into `expanded_groups` instead of
opening the usual per-commit context menu. Its color/lane come from its
first member row (arbitrary but consistent choice — all members share a lane
by construction of the grouping rule).

`GraphFocus`'s `row_count`/`active` are re-anchored to
`display.items.len()` — the same `row_count` `RwSignal` already feeds it
(`canvas.rs:184-185`, `set_row_count`), so this is the same one-line change
as the culler's, not a second wiring path. Tabbing onto a `WipGroup` and
pressing Enter/Space expands it via the identical `open_menu_at`-shaped
closure path `gestures::on_node_keydown` already calls — no new keyboard
plumbing, just a different action bound to the same hook.

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
  check) checked this session — neither `.graph-row` nor `data-row-index`
  appears in any of the 13 Playwright specs today, so none of them assume
  raw-row semantics for those selectors. Not a source of regression risk,
  but also not coverage for the new behavior.
- **A Playwright test for collapse-render + tap-to-expand is a required task
  in the implementation plan, not optional.** This project's own standing
  lesson (ADR 0054, and repeated real incidents) is that `cargo test` never
  executes `crates/git-vista/src` — it's wasm-gated — so a pure host test on
  `collapse.rs`'s logic proves the *algorithm* right and proves nothing about
  whether `canvas.rs`'s wiring, `GraphFocus`'s re-anchoring, or the tap
  handler actually work. With this feature defaulting ON, an unverified
  wiring bug ships to every session on first load, not behind an opt-in flag.

## ADR

Tom asked for one on this branch (standing "always do ADR on any notable
architecture decision" rule, and explicit ask this session). Draft during
implementation once the exact module boundary is settled — it will record:
why display-only over history rewrite, why consecutive-only over any-position,
why default-ON, and the row/edge invariant-preservation reasoning above so a
future reader doesn't have to re-derive it from the diff.
