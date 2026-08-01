---
task: Wire the bottom-sheet inspector into the UI (#65 / M1.12)
repo: /home/tom/projects/Git-Vista
issue: 65
base_branch: main
suggested_branch: codex/65-sheet-wiring
allowed_paths:
  - crates/git-vista/src/features/shell/signals.rs
  - crates/git-vista/src/features/shell/mod.rs
  - crates/git-vista/src/app/mod.rs
  - crates/git-vista/styles.css
forbidden_paths:
  - crates/git-vista/src/features/shell/sheet.rs   # model is DONE — read it, do not change it
  - crates/git-vista/src/features/shell/core.rs    # ShellMode/ModeSettler are live and correct
  - crates/git-vista-server/**                     # server is not involved
  - crates/git-vista/src/menu.rs                   # another agent is fixing ARIA there
  - crates/git-vista/src/features/a11y/**          # another agent is fixing hit targets there
acceptance:
  - id: sheet_renders
  - id: placement_is_mode_driven
  - id: detents_wired_to_gesture
  - id: state_survives_mode_change
  - id: gate_green
---

# Wire the inspector bottom sheet — the last code gap in M1

This is the final piece of work standing between Git-Vista and closing **M1** (currently
38 closed / 1 open). The logic you need is already written, already tested, and waiting
for a caller.

## Do not start by writing code. Start by reading `sheet.rs`.

`crates/git-vista/src/features/shell/sheet.rs` is **916 lines with 33 passing host tests
and zero callers**. That is deliberate, not abandoned. Its own module doc explains:

> *"Everything about it that is **rendering** is absent, on purpose and by constraint:
> `styles.css` is not this lane's file, no sheet element is emitted anywhere in the crate
> today… the model is settled and tested; the sheet does not exist on screen. Wiring it is
> the next slice, and it needs the CSS half to land first."*

**You are that next slice.** Your job is the rendering half. The decisions — which detent a
released drag lands on, what happens past either end, which placement each `ShellMode`
resolves to — are already made and proven. Do not re-derive them, do not re-implement them,
and do not "improve" them. Call them.

The types you will consume:

- `SheetDetent` — summary / half / full
- `SheetGeometry` — the measurements
- `SheetState` — the live state machine
- `InspectorPlacement` — where the inspector sits in a given mode
- `default_placement_for(mode: ShellMode) -> InspectorPlacement`

## Why it was built this way — read this before you decide to restructure anything

From the same module doc:

> *"the part of M1.12 that kept shipping unverified was the part braided into a `web_sys`
> closure. A pure decision type is checkable without a browser, and on this project nobody
> has a browser."*

That is the single most important sentence in this brief. **This project cannot run browser
tests.** `./dev gate` runs `cargo test --workspace` on the *native* target;
`crates/git-vista/src` is largely `#[cfg(target_arch = "wasm32")]` and is therefore never
compiled by it. The wasm32 step in the gate only *lints*; `trunk build` only *builds*.
Neither executes a single test.

This has bitten the project repeatedly and recently. On 2026-07-31 two separate defects
shipped through a fully green gate for exactly this reason — a missing ARIA attribute in
`menu.rs` and an untested guard in `api.rs`. Both were found by a human reading code, not
by CI.

**So: any logic you write must go somewhere host-testable.** If you find yourself putting a
decision inside a `web_sys` closure, stop and move it into a pure function next to
`sheet.rs`'s existing model, where a host test can reach it. Rendering wiring in the closure
is fine. *Decisions* in the closure are the mistake this codebase keeps making.

## What exists today

| Piece | State |
|---|---|
| `ShellMode::for_width` — Compact <600, Portrait 600–1023, Wide 1024–1439, UltraWide ≥1440 | live, emits one CSS class at `app/mod.rs:404` |
| `ModeSettler` — settles resize drags, no mid-drag thrash | live, host-tested |
| `sheet.rs` model | built, 33 tests, **no callers** |
| Mode-scoped CSS | **4 rules total** — one topbar padding tweak, three `.detail-panel` width overrides |
| `.detail-panel` | `position: fixed` in *every* mode (`styles.css:427`) |

So "wide vs portrait vs compact" currently means the same fixed overlay at a different
width. The compact case is the weakest: a fixed overlay on a phone-width viewport is
exactly what a bottom sheet exists to fix.

`docs/IPAD_DESIGN.md:63` specifies the target in one sentence: *"The bottom sheet has
detents for summary, half-height, and full-height content."* The design question is
settled — this is implementation.

## The work

1. **Consume `default_placement_for(mode)`** so the inspector's placement follows
   `ShellMode` instead of being a fixed overlay in all four bands.
2. **Emit the sheet element** for whichever placements resolve to a bottom sheet, and write
   the CSS half in `styles.css`. Note `.shell-compact` / `.shell-portrait` / `.shell-wide` /
   `.shell-ultrawide` are already on `<main>` — key off them.
3. **Wire drag gestures to `SheetState`.** Feed the gesture into the existing model and
   render what it returns. Do not compute detents yourself.
4. **Preserve state across a mode change**, which the model already handles — verify you are
   actually calling that path rather than resetting.

## Constraints

- **Smallest correct change.** Do not refactor `ShellMode`, `ModeSettler`, or the existing
  `.detail-panel` beyond what placement requires.
- **`sheet.rs` is read-only to you.** If you believe the model is wrong, say so and stop —
  do not change it. It is the one part of this feature with real test coverage.
- **Touch nothing under `forbidden_paths`.** Two other agents are working in `menu.rs` and
  `features/a11y/` on ARIA and hit-target fixes for this same issue. Colliding with them
  costs more than it saves.
- Run `cargo fmt --all` then `./dev gate` and report the **real** final line. Do not report
  success you did not observe.

## Definition of done

- The inspector's placement is driven by `ShellMode` via `default_placement_for`.
- A bottom sheet renders in the placements that call for one, with detents wired to
  `SheetState` rather than to logic you wrote.
- Any new decision logic is host-testable and has a test that runs under `./dev gate` —
  named in your report.
- `./dev gate` green.
- **Say plainly which parts have no executed test coverage** because they are wasm-only.
  That list is expected to be non-empty; hiding it is the failure mode, not having it.

## What you cannot verify, and must not claim

You have no browser. You cannot confirm the sheet looks right, drags smoothly, or behaves
under Stage Manager. **Do not claim it works** — claim it compiles, that the gate is green,
and that the model calls are correct by inspection. A human iPad pass is what closes #65,
and it happens after you.
