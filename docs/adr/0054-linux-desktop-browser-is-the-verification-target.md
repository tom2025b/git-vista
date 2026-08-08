# ADR 0054 — The Linux desktop browser is the verification target; iPad is deferred

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

The owner's answer to that is not a workaround but a different delivery model
entirely — a native app with the server bundled in, so there is nothing to forward.
See the amendment below and #367.

## Decision

**The Linux desktop browser is the verification target for this application.**
iPad support is **deferred** — no intermediate access mechanism is adopted in its place.
See the amendment below for where this is expected to go instead (a native client, #367).

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

- **A dormant code path.** With no touch device reaching the app,
  `drag_threshold("touch") → 10px` (`geometry.rs:44`) does not fire in practice; the
  4px mouse path always will. The touch tuning is not merely unverified, it is
  **unexercised**. Its unit tests still pass and pin the intended values, so the
  behaviour is preserved for the native client in #367 — but nothing exercises it end
  to end, and nobody should read "touch-optimised" as "touch-tested".
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
verification gap exactly where it is. The owner's position is blunter — *"no self
hosting tunnel"* — which rules out the whole category rather than this instance of it.

**VNC to this box, using the iPad as a display.** Appeared in this ADR's first draft and
was **withdrawn the same day** by the owner ("forget the VNC"). It has the same defect as
tunnel automation: it makes reaching the app someone else's problem instead of removing
the problem. #367 removes it.

## Where this is implemented

| Concern | Location |
| --- | --- |
| The harness that is now the target | `ci/browser/` (Playwright + Chromium), wired into `./dev gate` via `./dev browser` |
| The dormant touch path | `crates/git-vista/src/geometry.rs:44` (`drag_threshold`), tests at `:324-328`; used at `gestures.rs:173` |
| Standing "unverified on device" caveats to re-read under this ADR | `docs/PERFORMANCE_BUDGETS.md`, #362, #364, #209 criterion 4 |
| The tunnel/token friction that motivated this | `./dev testbed`'s own output; the bootstrap token is single-use by design |

## Amendment, 2026-08-08 (same day, hours later) — VNC is out; the destination is a native Swift interface

**VNC is withdrawn from this decision entirely.** It appeared in the first draft as the
stopgap for reaching the app from an iPad; the owner dropped it within the hour
("forget the VNC"). It is not the plan, not a fallback, and should not be built toward.
The original title of this ADR named it and has been corrected — this note records that
edit rather than hiding it.

What replaces it is a **vision, not a commitment**, stated before stepping away from the
repository:

> "When I revisit this in a year or so I want to add Mac and Windows along with iPad —
> I will do it in **Swift**. A way to access this without the website at all."

So: a **native Swift interface as an *option*, alongside the browser** — not a
replacement for it, and not scheduled. The browser UI remains the shipping product. The
near-term decision is unchanged and simpler than the first draft made it: iPad support is
**deferred**, full stop, with no intermediate access mechanism.

Three consequences follow, and they matter more than the near-term decision:

**1. The HTTP API is the durable product surface, not the wasm frontend.** If Mac,
Windows and iPad arrive as native Swift clients, the wasm UI becomes *one client among
several* rather than *the* application. Every endpoint is then a public contract with
consumers this repository does not contain and cannot refactor in step with.

**2. That raises the value of typed, mode-explicit endpoints, and lowers the value of
UI-shaped ones.** `POST /api/diff/spec` (landed today) is the right shape for this
future precisely because it takes an explicit `DiffSpec` — four named modes, validated
newtypes, no implicit HEAD — rather than assuming "one commit versus its parent" the way
`GET /api/diff/{id}` does. A Swift client can express any of the four without guessing.
`git-vista-protocol` becomes the shared contract, and its existing discipline (internally
tagged enums, `validated_string!` newtypes, golden fixtures pinning wire shape) stops
being tidiness and starts being an interface guarantee.

**3. The touch code is no longer dormant-forever — it is dormant-until-Swift.** The
consequence recorded above (no touch device reaches the app, so
`drag_threshold("touch")` never fires) still holds *today*. But a native iPad client
would deliver real touch, so that path has a live destination rather than being an
orphan awaiting deletion. Keeping it is now clearly correct rather than merely cheap.

**What this does not change:** the Linux desktop browser is still the verification target
*now*, `ci/browser` is still the harness that reaches the view, and the wasm-seam
argument in the body above is untouched. This amendment records where the road goes, not
a different road.

**Read this before proposing UI-only work.** A year-from-now session that treats the wasm
frontend as the product will optimise the wrong layer. The API is what the next three
clients will consume.

## SECURITY_MODEL.md annotation

None. This changes no authorization, sandbox, or transport behaviour. ADR 0005's
loopback/LAN split is untouched — this decision narrows how the app is reached, it does
not widen it.

A note for whoever takes up #367: bundling the server into a native app **does** touch
this boundary, because the loopback/LAN distinction ADR 0005 rests on is drawn around a
listening socket. An in-process server changes what that socket is and who can reach it.
That is a security-model question to answer in #367's own ADR, not one this one settles.

---

**Signed:** 2025 · 2026-08-08T07:56:00-04:00
