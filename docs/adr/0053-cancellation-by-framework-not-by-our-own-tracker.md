# ADR 0053 — "Cancellable" is satisfied by the framework and an id echo, not by a tracker of our own

Date: 2026-08-08
Status: Accepted — implemented

Supersedes nothing. Retires the primitive introduced for M2.16 (#69d),
`git_vista_core::request_generation`, without replacing it.

## Context

#69 (M2.16) carries the acceptance criterion *"Rendering is virtualized and
cancellable."* The virtualized half shipped in M2.16g (#350). For the cancellable
half, an earlier slice (#69d) landed `git-vista-core/src/request_generation.rs`:
a `RequestGeneration` newtype and a `RequestGenerationTracker` implementing
cancellation-by-discard — the caller stamps each request with the current
generation, and discards any response whose generation is no longer current by
the time it lands.

That module is pure, fully host-tested, and has **never had a consumer**. The
repository's own dead-code census flags it (`reachability_census.rs:829`),
framing the choice in its own words: *"a real, reportable finding for a human to
either wire up or remove."*

This ADR is that decision. It is worth recording rather than doing silently,
because the code that results is an **absence** — a future reader finding no
cancellation machinery anywhere in the diff path deserves to know that was
concluded, not overlooked.

The module's own doc comment argued its case on a specific scenario: *"A scroll
that outruns its own in-flight fetches must not let an earlier request's response
land after a later one and paint over newer content."* Before wiring it, we
checked whether that scenario exists in this application, and whether anything
already defends against it.

Three things turned out to be true.

**1. The scenario the module describes does not occur in this architecture.**
The diff view fetches the whole (capped) patch **once per commit** and windows it
client-side. `detail.rs`'s diff resource is keyed on `shell.detail_id()` alone;
scroll position lives in a separate signal used only by `render_window`, and is
never a resource source. Scrolling therefore issues no requests at all, so no
scroll can outrun anything.

**2. The framework already does exactly this, internally.** Leptos 0.6.15 — the
pinned version — implements the identical mechanism inside
`create_local_resource`. Its `load()` increments a shared version counter *before*
spawning each fetch, captures that generation by value in the spawned future, and
on resolution commits the result only if the captured generation still matches the
current one (`leptos_reactive-0.6.15/src/resource.rs:1375-1423`). A late-arriving
response from a superseded load is dropped before it ever reaches the resource's
value signal. This was verified by reading the vendored source at the exact
version `Cargo.lock` resolves, not from recollection of Leptos's behaviour.

**3. The application adds its own second guard on top of that.** Every diff and
detail response echoes the id (or path, or direction) it was fetched for, and both
surfaces re-check that echo against the *live* selection at render time before
painting: `detail.rs:580-584` (`Some(Ok(d)) if d.id != changes_id => Loading…`)
and `viewer.rs:134-152` for all three `ViewerDoc` variants. `viewer.rs:87-89` states
the rule outright: *"A stale response is ignored via the id/path echo, same rule as
the detail panel's fetches."*

So the realistic staleness path — tap commit A, tap commit B before A resolves, A
lands last — is already closed twice over, at two independent layers.

## Decision

**Delete `git_vista_core::request_generation` rather than wire it up.** Treat
#69's "cancellable" criterion as satisfied by the combination of Leptos's own
generation tracking and the application's id-echo guard, and record that here so
the absence is legible.

Concretely:

- Remove `crates/git-vista-core/src/request_generation.rs` and its `pub mod`
  declaration and doc-list entry in `crates/git-vista-core/src/lib.rs`.
- Remove its entry from `reachability_census.rs`'s `EXEMPT` table — the census's
  question is now answered, so the exemption has nothing left to exempt.

The user-visible guarantee the criterion is actually about — *nothing stale is
ever painted* — holds, and holds today, without this module.

## Alternatives considered

**Wire the tracker into the diff fetch anyway.** Rejected. It would add a third
layer of defence against a race already closed twice, in a codebase whose standing
rule is that complexity must earn its place. Worse, it would be *untestable in the
place it matters*: `detail.rs` and `viewer.rs` are `#[cfg(target_arch = "wasm32")]`-
gated and never compiled by `cargo test`, so the wiring would be exactly the
green-pure-core-test-standing-in-for-an-unproven-seam shape this repository has
been bitten by repeatedly. Three defences, one of them unprovable, is worse than
two provable ones.

**Keep the module unused, exempted, as a ready primitive.** Rejected. This is what
was already happening, and it is the shape the reachability census exists to
surface: a tested, unreachable function reads as working infrastructure to anyone
scanning the crate. Two other primitives in this same milestone (`CumulativeHeights`
in #69c, `scroll_to_reveal` in #350) sat unconsumed the same way and were each
eventually found by that census as real regressions. Leaving a third in place,
deliberately, would blunt the tool.

**Implement per-request abort (`AbortController`) instead.** Rejected for now, but
genuinely deferred rather than dismissed. Abort is a *complementary* optimisation —
it stops wasted network and CPU work that a doomed request would otherwise still
perform — but it is not required for the correctness property the criterion states.
Nothing in this decision precludes adding it if a real cost is ever measured.

## Consequences

- `git-vista-core` loses a module and its tests; nothing else changes behaviour,
  because nothing called it.
- #69's "cancellable" criterion is met by argument-plus-citation rather than by a
  line of our own code. That is why this ADR exists: the citation lives here.
- **A new fetch surface does not automatically inherit this.** The reasoning above
  depends on two specific properties: the fetch goes through a Leptos resource, and
  the response echoes back what it was fetched for. A future endpoint that streams,
  polls, or fetches per-scroll-range — for instance a `DiffSpec`-driven mode that
  refetches on range change rather than fetching one capped patch — would break the
  first property and must re-argue cancellation on its own terms. It should not
  cite this ADR as cover.
- If Leptos is upgraded past 0.6.15, property (2) is a **version-pinned claim** and
  must be re-verified against the new source. Property (3), the id echo, is ours and
  survives any upgrade — which is a good reason to keep it rather than lean on the
  framework alone.

## Where this is implemented

| Concern | Location |
| --- | --- |
| Framework-level generation tracking | `leptos_reactive-0.6.15/src/resource.rs:1375-1423` (vendored dependency, not our code) |
| Application-level id echo, detail panel | `crates/git-vista/src/detail.rs:580-584` (and `:432-440` for the commit body) |
| Application-level id echo, full-screen viewer | `crates/git-vista/src/viewer.rs:87-89`, `:134-152` |
| Single-fetch-per-commit architecture | `crates/git-vista/src/detail.rs:393-401` (resource keyed on `detail_id()` only), `:715` (scroll signal feeds windowing, not fetching) |
| The removal itself | `crates/git-vista-core/src/lib.rs`, `crates/git-vista/src/reachability_census.rs` |

## SECURITY_MODEL.md annotation

None. This decision has no security boundary: it removes an unused client-side
view-state primitive and changes no request, response, authorization, or sandbox
behaviour.

---

**Signed:** 2025 · 2026-08-08T05:08:00-04:00
