# ADR 0054 — The Linux desktop browser is the verification target; iPad is deferred to a VNC display

Date: 2026-08-08
Status: Accepted

Does not supersede any ADR. It **reinterprets the standing constraint** that 16 existing
ADRs and roughly half the Rust files were written under — see "What this does not
change" before assuming anything here loosens a design rule.

## Context

Git-Vista was designed iPad-first. That premise is visible everywhere: 44pt hit
targets, pointer-type-aware gesture slop (ADR 0011), an app shell that never scrolls
(ADR 0012), `viewport-fit=cover`, VoiceOver-shaped `aria-label`s, and a repeated
caveat in test comments and `docs/PERFORMANCE_BUDGETS.md` that the real rendering
path is *"expected, not verified from this box — it is on the iPad testbed list."*

**117 of 216 Rust files** mention iPad, Pencil, Safari or touch. **16 of 54 ADRs**
carry an iPad-shaped decision.

Two things changed.

**1. The owner's environment changed.** A U-shaped desk with a 50-inch monitor about
two feet away is now where this app is actually used. Stated plainly, twice, on
2026-08-08: *"I will probably just use it in Linux anyway, the screen is bigger"* and
*"the iPad was never a big deal after I sat at my box."*

**2. The iPad path costs more than it returns.** Reaching the app from the iPad needs
an SSH local port forward plus a **one-time** bootstrap token. Both are fragile in
combination: the tunnel drops silently, and a token consumed by a failed load cannot
be reused, so a dropped tunnel and an expired token present identically. That pairing
cost roughly thirty minutes in a single session on 2026-08-08 while trying to verify
one menu item.

And there is a cheaper way to get Linux onto the iPad when that is wanted at all:
**VNC to this box**. The iPad becomes a display for a Linux browser rather than a
client running Safari.

## Decision

**The Linux desktop browser is the verification target for this application.**
iPad access, when wanted, is a VNC session onto that same Linux desktop — the iPad is
a display, not a second client platform.

Concretely:

- A "device pass" is now **a real browser on this box**, driven by hand in Firefox or
  scripted in `ci/browser` (Playwright/Chromium). It is no longer a manual iPad pass.
- `ci/browser` stops being a *proxy* for the real target and becomes **the target
  itself**. Any claim about the rendered view is now testable rather than deferred.
- Safari-specific behaviour, VoiceOver behaviour, and real touch/Pencil input are
  **deferred, not abandoned** — unverified rather than unsupported.

## Why this is a large change to what "done" means

The most repeated caveat in this codebase is that `detail.rs`, `viewer.rs`, `menu.rs`
and the rest of the view layer are `#[cfg(target_arch = "wasm32")]`-gated, so
`cargo test` never compiles them and every claim about the rendered view is
unprovable from the host. That caveat is honest and it recurs in
`docs/PERFORMANCE_BUDGETS.md`, in #362, in #364, and in #209's criterion 4 — where a
mutation deliberately *survived* on host tests to prove the gap empirically.

That gap existed because the harness that could reach the view (Chromium on Linux)
was not the platform the claims were about (Safari on iPad). **Under this decision
those are the same platform**, and the gap largely closes: a Playwright spec is now
sufficient evidence, where before it was only indicative.

This is the main reason to take the decision, beyond convenience.

## What this does not change

- **No code is removed or rewritten.** 44pt targets, gesture slop and the
  unscrollable shell all work correctly with a mouse. They are design rationale, not
  iPad-only code paths.
- **The 16 iPad-shaped ADRs stay valid.** They argue why the design is touch-friendly;
  none of them break when touch is not the primary input. This ADR is the parent they
  point at for *which* of their constraints currently bind.
- **Accessibility is not deferred.** `aria-label`s, roles and the roving tab stop
  matter on any platform, and Chromium's accessibility tree can assert them — which
  VoiceOver never could from here. If anything this makes a11y *more* verifiable.

## Consequences

- **A dormant code path.** VNC delivers touch as mouse events, so
  `drag_threshold("touch") → 10px` (`geometry.rs:44`) will not fire in practice; the
  4px mouse path always will. The touch tuning is not merely unverified, it is
  **unexercised**. Its unit tests still pass and pin the intended values, so the
  behaviour is preserved for a future iPad pass — but nothing exercises it end to end,
  and nobody should read "touch-optimised" as "touch-tested".
- **Pencil affordances are speculative** until an iPad pass happens. #364's
  pointer-type half should be read in that light: still worth writing, but it now
  proves a path no current user takes.
- **A testing debt, not a rewrite debt.** If iPad ever becomes a first-class client
  again, the code is intact; what is missing is verification. That is a much cheaper
  thing to owe.
- **Documentation risk this ADR exists to manage.** Design docs describe a
  Pencil-enhanced, finger-first product. Without this record, a later session reads
  those as live requirements and builds or blocks accordingly. Anything claiming a
  touch or Safari guarantee should be read against this ADR first.

## Alternatives considered

**Keep iPad as a first-class target.** Rejected on evidence: the owner does not use it
that way, and the access path costs real time per session. Retaining it would keep the
whole view layer permanently unverifiable, since no harness here can drive Safari.

**Drop iPad support entirely and delete the touch code.** Rejected. It costs nothing
to keep — the code is correct and unit-tested — and deleting it would make a future
return expensive. Deferral is reversible; deletion is not.

**Tunnel automation** (a supervised port-forward, a longer-lived token). Rejected as
solving the wrong problem: it would make an unused path more reliable while leaving the
verification gap exactly where it is.

## Where this is implemented

| Concern | Location |
| --- | --- |
| The harness that is now the target | `ci/browser/` (Playwright + Chromium), wired into `./dev gate` via `./dev browser` |
| The dormant touch path | `crates/git-vista/src/geometry.rs:44` (`drag_threshold`), tests at `:324-328`; used at `gestures.rs:173` |
| Standing "unverified on device" caveats to re-read under this ADR | `docs/PERFORMANCE_BUDGETS.md`, #362, #364, #209 criterion 4 |
| The tunnel/token friction that motivated this | `./dev testbed`'s own output; the bootstrap token is single-use by design |

## SECURITY_MODEL.md annotation

None. This changes no authorization, sandbox, or transport behaviour. ADR 0005's
loopback/LAN split is untouched — a VNC session reaches the app over this box's own
loopback, which is the same posture a local Firefox has, not a new exposure.

---

**Signed:** 2025 · 2026-08-08T07:56:00-04:00
