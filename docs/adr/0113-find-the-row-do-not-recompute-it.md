# ADR 0113 — Find the row, do not recompute it

- **Status:** Accepted — implemented, mutation-proved two ways
- **Date:** 2026-09-03
- **Issue:** #634
- **Extends:** [ADR 0110](0110-a-page-is-what-the-frame-just-said-it-was.md)
- **Supersedes / superseded by:** —

## Context

ADR 0110 gave `gv-tui` page keys and a zoom toggle, and established that **a
page is the pane's current visible height**. Making the cursor move further per
keystroke had a consequence nobody wrote down: a cursor that can cross a whole
viewport in one key can leave it.

That hazard has three faces in the conflicts pane, and they were found and
fixed one at a time.

```mermaid
flowchart TD
    Z["<b>#625 gave the cursors page keys</b><br/>one key now crosses a whole viewport"]
    A["<b>The text caret</b><br/>insert mode, in the Result section"]
    B["<b>The file cursor</b><br/>the conflict list"]
    C["<b>The block cursor</b><br/>the editor, outside insert mode"]
    A1["followed already —<br/>caret_row"]
    B1["fixed in #632<br/>ADR 0110's late defect"]
    C1["<b>still broken — this ADR</b>"]

    Z --> A --> A1
    Z --> B --> B1
    Z --> C --> C1

    classDef start fill:#1f2d3d,color:#ffffff,stroke:#0d1620,stroke-width:2px
    classDef face fill:#e8eef5,color:#1f3a5c,stroke:#3d6591,stroke-width:1px
    classDef done fill:#e8f1ea,color:#14612f,stroke:#1f5c3a,stroke-width:1px
    classDef open fill:#7a2e2e,color:#ffffff,stroke:#521c1c,stroke-width:3px
    class Z start
    class A,B,C face
    class A1,B1 done
    class C1 open
```

### The defect

On `Screen::Editor` outside insert mode, three facts combined:

- `move_cursor` and `jump` move `editor.block` — and nothing else;
- `focus_row` delegated to `caret_row`, which returns `None` when
  `!editor.inserting`;
- `self.scroll` is `0` on that screen and is never moved there.

So `view_offset` had no focus to follow, returned the unmoved scroll, and in a
file with more conflicts than the overlay is tall the selected block heading
was simply drawn off screen.

```mermaid
flowchart TD
    K["<b>User presses End</b>"]
    M["<b>editor.block = 11</b><br/>the cursor<br/>really did move"]
    F["<b>focus_row → None</b><br/>caret_row refuses<br/>outside insert mode"]
    V["<b>scroll stays 0</b><br/>never touched<br/>on this screen"]
    D["<b>view_offset → 0</b> — the window still draws <b>Conflict 1 of 12</b>"]
    H["<b>End and PageDown look INERT</b>, and the choice keys now act on a conflict the user cannot see"]

    K --> M & F & V
    M & F & V --> D --> H

    classDef act fill:#1f2d3d,color:#ffffff,stroke:#0d1620,stroke-width:2px
    classDef step fill:#e8eef5,color:#1f3a5c,stroke:#3d6591,stroke-width:1px
    classDef bad fill:#8a5200,color:#ffffff,stroke:#5c3600,stroke-width:2px
    classDef worst fill:#7a2e2e,color:#ffffff,stroke:#521c1c,stroke-width:3px
    class K act
    class M,F,V step
    class D bad
    class H worst
```

The last step is the one that matters. This is not a cosmetic scrolling bug: the
choice keys write a resolution for `editor.block`, so an invisible block cursor
is a way to resolve the wrong conflict — the same class of hazard as an
invisible text caret in a buffer that accepts every keystroke.

### The constraint the issue placed on the fix

The issue explicitly declined to fix this inside #632, and said why. The
obvious fix needs the absolute row of block *n*, which is a **third**
implementation of the editor's row arithmetic:

```mermaid
flowchart TD
    L["<b>One layout:<br/>what the editor draws</b>"]
    I1["<b>1 · visit_editor</b><br/>the walk that emits the rows"]
    I2["<b>2 · editor_result_first_row</b><br/>arithmetic, for the caret"]
    I3["<b>3 · the proposed<br/>absolute row of block n</b><br/>arithmetic, for the block cursor"]
    P["<b>row_count_agrees_with_the_rows_<br/>actually_emitted</b><br/>pins 1 against 2"]
    R["<b>3 is pinned by nothing</b><br/>free to drift from both"]

    L --> I1 & I2 & I3
    I1 --- P
    I2 --- P
    I3 --> R

    classDef one fill:#1f2d3d,color:#ffffff,stroke:#0d1620,stroke-width:2px
    classDef impl fill:#e8eef5,color:#1f3a5c,stroke:#3d6591,stroke-width:1px
    classDef pin fill:#e8f1ea,color:#14612f,stroke:#1f5c3a,stroke-width:1px
    classDef risk fill:#7a2e2e,color:#ffffff,stroke:#521c1c,stroke-width:3px
    class L one
    class I1,I2 impl
    class I3 risk
    class P pin
    class R risk
```

The comment above `row_count` already asks nobody to add one quietly. The
issue's own acceptance criteria therefore demanded the third copy be pinned by
extending the existing agreement test.

## Decision

**There is no third implementation. `focus_row` finds the row in the output
`visit_rows` already produces.**

`Row` has carried a `selected: bool` since the pane was written — it is how the
renderer draws the highlight. So the row the viewport must not lose is already
marked, in the drawn output, by the one implementation that defines the layout:

```rust
Screen::Editor => self.caret_row().or_else(|| self.selected_row_index()),
```

```mermaid
flowchart TD
    Q["<b>Where is the block cursor drawn?</b>"]
    A["<b>Recompute it</b><br/>4 + the widths of<br/>every block above"]
    A1["a third copy of the layout,<br/>needing a test to watch it drift"]
    A2["<b>and that test can only ever<br/>report drift AFTER it happens</b>"]
    B["<b>Find it</b><br/>walk visit_rows, take the<br/>first selected row"]
    B1["an index INTO the drawn output,<br/>so it cannot disagree with it"]
    B2["<b>the drift is not detected —<br/>it is impossible</b>"]

    Q --> A --> A1 --> A2
    Q --> B --> B1 --> B2

    classDef q fill:#1f2d3d,color:#ffffff,stroke:#0d1620,stroke-width:2px
    classDef bad fill:#7a2e2e,color:#ffffff,stroke:#521c1c,stroke-width:2px
    classDef good fill:#e8f1ea,color:#14612f,stroke:#1f5c3a,stroke-width:1px
    class Q q
    class A,A1,A2,A3 bad
    class B,B1,B2,B3 good
```

This is the general principle the ADR is named for, and it applies past this
pane: **when a value can be *read* from the artefact a system already produces,
reading it beats deriving it a second way and testing that the two agree.** A
pinned second implementation is a good answer when there is no artefact to
read. Here there was one.

### The caret still wins where both exist

In insert mode `visit_editor` marks **two** rows selected: the block heading
the cursor is on, and the caret's line down in the Result section. Both are
real. The one the viewport must not lose is the caret — that is the buffer
taking keystrokes — so `caret_row` is asked first and the walk is only a
fallback.

```mermaid
flowchart TD
    S["<b>Screen::Editor</b>"]
    I{"<b>inserting?</b>"}
    C["<b>caret_row</b><br/>the line taking keystrokes"]
    W["<b>selected_row_index</b><br/>the block heading"]
    N["insert mode marks TWO rows selected;<br/>a naive first-selected walk would<br/>find the HEADING, not the caret"]

    S --> I
    I -->|yes| C
    I -->|no| W
    C -.-> N

    classDef s fill:#1f2d3d,color:#ffffff,stroke:#0d1620,stroke-width:2px
    classDef q fill:#e8eef5,color:#1f3a5c,stroke:#3d6591,stroke-width:1px
    classDef pick fill:#e8f1ea,color:#14612f,stroke:#1f5c3a,stroke-width:1px
    classDef note fill:#8a5200,color:#ffffff,stroke:#5c3600,stroke-width:2px
    class S s
    class I q
    class C,W pick
    class N note
```

That ordering is why `focus_row` is not simply "the first selected row on any
screen". It is a real decision, and getting it backwards is a live-caret
regression rather than a cosmetic one.

### The cost, and the asymmetry that is left

The walk is O(rows) and runs once per draw, on a screen that already visits
once to build the window. Conflict counts are bounded by what a merge produced;
this is not a hot loop.

`Screen::List` keeps its one-line arithmetic (`1 + cursor`, row 0 being the
heading) rather than walking. That asymmetry is deliberate: the list's
selection is a function of `cursor`, which the list owns, and the expression is
short enough to read against `visit_list` at a glance. The editor's was not —
it was a sum over heterogeneous block widths, which is exactly the kind of
expression that drifts.

## Alternatives considered

**The third arithmetic implementation, pinned by extending the agreement
test** — what the issue asked for. Rejected because the walk makes the pin
unnecessary: a test that watches two implementations agree is strictly worse
than having one. It is the right answer only when the drawn artefact cannot be
consulted.

**Make `Screen::List` walk too, for symmetry.** Rejected for now. It would
replace a one-line expression with a linear scan and buy nothing — the list's
arithmetic has no room to drift. Noted here so the asymmetry reads as a choice
rather than an oversight.

**Move `scroll` when `editor.block` moves**, instead of teaching `view_offset`
to follow. Rejected: it puts viewport policy in the key handlers, where every
future cursor movement has to remember it. ADR 0110 deliberately put the
follow in one place, and `view_offset` is that place.

**Mark only one row selected in insert mode**, so a first-selected walk is
unambiguous. Rejected: both highlights are correct and the user wants to see
both. The ambiguity belongs in `focus_row`'s ordering, not in the renderer.

## Consequences

- `End`, `PageDown`, `Home` and `PageUp` move the visible window on the editor
  screen, and the choice keys can no longer act on a conflict that is off
  screen.
- The editor's row layout has **two** implementations again, not three, and the
  new consumer is derived from the walk rather than added beside it.
- A general rule now has a worked example in this repository: **find it in the
  output, do not recompute it.** Anywhere a system already emits the thing you
  are about to derive, deriving it is a second source of truth.
- All three faces of the #625 paging hazard are now closed.

## Verification

`./dev gate` green: 179 `gv-tui` tests, the workspace suites, clippy,
`cargo fmt --check`, `trunk build`, and 83 browser tests. 7/7 required checks
green in CI on PR #639.

The test builds 12 conflicts separated by context lines into a 6-row viewport,
pages `End` / `PageUp` / `Home` / `PageDown`, and asserts the selected row is
among the rows `window(view_offset(h), h)` returns — **not** that
`editor.block` reached the last index, which would have passed throughout the
defect's life. It also asserts up front that the fixture is taller than the
viewport, so it cannot quietly stop testing anything.

### Mutation matrix

`failure-atlas mutation_check` against committed HEAD `eb72946d`, working tree
reported clean. Both baselines green; both mutated legs reached and failed an
assertion.

| Mutation | Result | How it failed |
|---|---|---|
| **remove**: drop the `.or_else`, leaving exactly the code as it stood | **caught** (195) | offset `0`, window draws "Conflict **1** of 12" |
| **weaken**: replace the walk with the arithmetic the issue proposed, counting four rows per conflict and forgetting the context blocks between them | **caught** (196) | offset `43`, window draws "Conflict **9** of 12" |

```mermaid
flowchart TD
    T["<b>Cursor on Conflict 12 of 12</b><br/>a 6-row viewport"]
    M1["<b>remove</b><br/>offset 0"]
    M1R["draws Conflict 1 of 12 —<br/>the viewport NEVER MOVED"]
    M2["<b>weaken</b><br/>offset 43"]
    M2R["draws Conflict 9 of 12 —<br/>the viewport moved to the<br/>WRONG CONFLICT"]
    G["<b>correct</b><br/>the selected row is drawn"]

    T --> M1 --> M1R
    T --> M2 --> M2R
    T --> G

    classDef t fill:#1f2d3d,color:#ffffff,stroke:#0d1620,stroke-width:2px
    classDef mut fill:#e8eef5,color:#1f3a5c,stroke:#3d6591,stroke-width:1px
    classDef bad fill:#7a2e2e,color:#ffffff,stroke:#521c1c,stroke-width:2px
    classDef good fill:#e8f1ea,color:#14612f,stroke:#1f5c3a,stroke-width:1px
    class T t
    class M1,M2 mut
    class M1R,M2R bad
    class G good
```

They fail **differently**, and the second is the one worth reading. It is
precisely the drift the issue predicted, and it fails while looking entirely
plausible on screen — a highlighted conflict heading, drawn inside the window,
that is simply not the one the cursor is on. A test asserting only that
`editor.block` reached the last index passes against **both** mutations. That
is why the assertion had to be about the rows actually drawn.

---

**Signed:** max · 2026-09-03T23:05:00-04:00
