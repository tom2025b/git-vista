# ADR 0011 — Gesture slop is pointer-type-aware (touch 12px, mouse/pen 4px)

- **Status:** Accepted (implemented)
- **Date:** 2026-07-19
- **Milestone / issue:** #115, PR #116 (merged `2025b8b`)
- **Supersedes / superseded by:** —

## Context

The graph's gesture layer classifies a pointer sequence as a *tap* (opens
the node menu) or a *drag* (pans) using a movement threshold. That threshold
was a single constant, 4px — correct for a mouse, but a fingertip on iPad
glass wobbles more than 4px during a deliberate tap. Result: on the iPad,
node taps were silently reclassified as tiny pans and the menu never opened,
while every other control (topbar, links) worked — a device-specific dead
feature that shipped through a desktop-verified gate.

## Decision

The drag threshold is a pure function of the DOM `pointerType`:
`drag_threshold(pointer_type)` in `geometry.rs` returns **12.0 for
`"touch"`, 4.0 for mouse/pen/unknown**. The gesture layer calls it per
event. Being a pure host-side function, it is unit-tested in the normal
native test suite (`a_touch_tap_tolerates_finger_wobble_a_mouse_click_stays
_precise`), not behind a browser.

## Alternatives considered

- **Raise the single constant for everyone** (e.g. 12px). Rejected: a mouse
  is precise; tripling its slop makes small deliberate drags feel dead on
  desktop to fix a touch-only bug.
- **Time-based tap detection** (tap = short press regardless of movement).
  Rejected: adds a latency/feel trade-off and still needs a movement bound;
  the per-type threshold is the minimal change that matches the physical
  difference.
- **Browser-level click delegation** (trust the `click` event only).
  Rejected: the gesture layer must still decide tap-vs-pan for pointer
  capture, so the threshold decision cannot be avoided — only tuned.

## Consequences

- Input-feel constants now live in `geometry.rs` as pure, host-tested
  functions; future tuning is a one-line change with a test.
- Establishes the policy that **touch and mouse thresholds are independent**;
  any future gesture (long-press, pinch deadband) should follow it.
- Verified on real hardware: iPad node taps work; desktop 6px mouse
  movement still pans, not taps.
