# ADR 0012 — The app shell never scrolls; all scrolling is internal

- **Status:** Accepted (implemented)
- **Date:** 2026-07-19
- **Milestone / issue:** #117, PR #118 (merged `0ac740d`)
- **Supersedes / superseded by:** —

## Context

The app shell (`.app`) was `height: 100vh` inside an unclamped `html/body`.
On iOS Safari `100vh` is the **large** viewport (URL bar collapsed), so with
the URL bar visible the shell overflowed the visual viewport and the page
became scrollable by the delta. Safari restores scroll offset across reloads
(frequent with tunnel reconnects), so the iPad kept coming up with the
topbar parked above the visible area until a manual scroll-up. Desktop never
reproduced it — there is no vh/visible delta there.

## Decision

Git-Vista is an app shell, not a document, and the page itself must never
scroll:

- `html, body { overflow: hidden; overscroll-behavior: none; }`
- `.app { height: 100vh; height: 100dvh; }` — the dvh line tracks the real
  visible box under Safari's bars (iOS 16.4+), with the vh line as the
  fallback for older engines.
- All real scrolling lives in inner overflow containers (`.detail-body`,
  `.ctx-menu`, `.viewer-body`), which are unchanged. Print mode keeps
  working because `html[data-print]` already restores `overflow: visible`
  for the flowing print surface.

## Alternatives considered

- **JS scroll reset on load** (`scrollTo(0,0)` at startup). Rejected: treats
  the symptom; the page can still be scrolled *after* load (rubber-banding,
  URL-bar collapse) and strand the topbar again.
- **`position: fixed` topbar.** Rejected: keeps the topbar visible but the
  rest of the shell still scrolls under it, misaligning the fixed-position
  context menus and dialogs that assume a static page.
- **`100dvh` alone, without clamping overflow.** Rejected: shrinks the
  overflow window but any residual scrollability (restored offsets,
  overscroll) recreates the bug; `overflow: hidden` removes the class of
  failure rather than one instance.

## Consequences

- The invariant "page scroll offset is always 0" now holds everywhere;
  fixed-position UI (menus, dialogs, overlays) can rely on viewport
  coordinates equaling page coordinates.
- Any future view that needs to scroll must bring its own overflow
  container — which is already the house pattern.
- Verified red→green in Chromium with a simulated 60px viewport delta
  (pre-fix the shell scrolled and the topbar left the viewport; post-fix
  scroll is impossible), plus gate + CI; real-iPad fresh-load confirmation
  is the remaining human check.
