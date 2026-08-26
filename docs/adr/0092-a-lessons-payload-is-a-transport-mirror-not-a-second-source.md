# ADR 0092 — A lesson's payload is a transport-side mirror of `Explanation`, never a second source of what a plan means

**Status:** Accepted — implemented and tested
**Date:** 2026-08-26
**Issue:** [#450](https://github.com/tom2025b/git-vista/issues/450) — CLOUD-4, the MCP lesson tool
**Supersedes:** nothing · **Superseded by:** nothing

---

## Context

#450 asks for a **read** tool on `git-vista-mcp` that turns a plan into structured teaching data — the same facts #92's Explain Mode panel shows, reachable by an MCP client instead of only the wasm viewer. The issue's placement decision is explicit and not up for relitigating here: *structured lesson DATA, not HTML*. Rendering taste belongs to whatever renders next (the app's panel today; an artifact pipeline, `teacher-thing`, `decksmith`, tomorrow), not to a Rust MCP server whose every other tool on this surface already returns a typed DTO.

The hard constraint is #92's own: `git_vista_protocol::explain(&Plan) -> Explanation` is the **one** function that decides what a plan means, and `Explanation` deliberately does **not** derive `Serialize` — see `explain.rs`'s module doc, "Nothing new crosses the wire." That decision was made for the wasm viewer, which already holds the `Plan` locally and calls `explain` in-process. `git-vista-mcp` is a different situation: it is a separate binary that receives a `Plan` **as JSON** (from a prior `plan_*` tool call) and must hand a lesson back as JSON too. Something has to serialize.

Three shapes were available for that something:

1. **Derive `Serialize` on `Explanation` and its facts, in the protocol crate.** Rejected outright — it directly reverses the "nothing new crosses the wire" decision that `explain.rs` states as load-bearing, for the convenience of one caller.
2. **Give `git-vista-mcp` a second, hand-written classification of what a plan means**, independent of `explain`, so it can build its own JSON without touching `Explanation` at all. Rejected: that is a second source of truth for exactly the fact #92's acceptance criterion 3 exists to prevent ("the lesson a page shows and the explanation the app shows cannot drift"). Two independent classifiers of the same plan drift the moment one of them gains a case the other does not.
3. **A transport-side mirror, owned by `git-vista-mcp`, that maps `Explanation` structurally.** Chosen.

## Decision

**`crates/git-vista-mcp/src/lesson.rs` defines `Lesson`/`LessonSection`/`LessonTopic`/`LessonFact` — a `Serialize`-only echo of `Explanation`/`Section`/`Topic`/`ExplanationFact`, owned by the transport crate, not the domain crate.** `to_lesson(&Explanation) -> Lesson` is the whole of the mapping: two small exhaustive `match` statements (`lesson_topic`, `lesson_fact`), each arm carrying its source value through by clone or copy, never substituting, computing, or omitting one. Nothing here decides what a plan means; `explain` already decided, once, and `to_lesson` only decides how to spell that decision as JSON.

This keeps *transport is not domain* true in both directions at once: `git-vista-protocol` gains no wire commitment it did not already want, and `git-vista-mcp` gains no second opinion about what a plan means. The parity this buys is mechanical rather than aspirational — `lesson::tests::every_lesson_fact_matches_the_explanation_it_was_built_from` walks every fact `explain()` produces for a battery of plans and asserts the lesson's own serialized `kind`/`value` pair, computed **independently** of `to_lesson` (never by calling the function under test — the standing house rule), matches exactly.

**The `get_lesson` tool takes the exact `plan` object a `plan_*` tool returned, and makes no network call.** A `plan_*` tool's `POST /api/plan` already evaluated every precondition against the live repository at build time; the `Plan` a client holds already *is* live repository state, frozen into a typed value. Re-deriving its lesson is therefore a pure, local, offline function of bytes the caller already has — the same computation `execute_tool`'s `execute_plan` already performs on the identical `plan` argument before it ever builds an HTTP request. This is also why the tool composes with a `git-vista-fixtures` broken repository exactly as readily as with a real one (acceptance criterion 4): nothing in `get_lesson` inspects where the `Plan` came from, only what it contains.

**`NetworkNeed` gained `#[derive(Serialize, Deserialize)]` in `git-vista-protocol`.** Every other type an `ExplanationFact` can carry (`Precondition`, `RefChange`, `RecoveryStrategy`, `Advisory`, `RiskLevel`, `WorktreeEffect`, `IndexEffect`) was already `Serialize` — each is also a field (or built from one) of `Plan`, which crosses the wire today. `NetworkNeed` was the one exception: an internal classification of remote effect (`network_need_for_operation`) that never previously left the server process. Deriving `Serialize` on it is a capability addition with no behavior change to anything that already exists, and it means `LessonFact::Remote(NetworkNeed)` needs no bespoke mirror enum of its own — the one genuinely shared, minimal change this issue's `allowed_paths` for `git-vista-protocol/src/` anticipates.

**Wire shape.** `LessonFact` is adjacently tagged (`{"kind": "precondition", "value": {...}}`), matching the convention `RefState` already uses in this same crate for a tagged enum whose payload is a nested tagged struct — not a new convention invented for this issue. `LessonTopic` serializes to the same six `snake_case` names Explain Mode's own topics are, in the same fixed order `explain()` always emits them, empty sections included (see `explain.rs`'s own doc for why a section is never hidden).

**Catalog placement.** `get_lesson` is listed as the eighth entry in `tool_catalog()`, appended after the six live-read tools and before `plan_*`. It is grouped with the reads for the census test's purposes even though it makes no HTTP call at all: it is not a write, and `plan_*`/`execute_plan`'s own census (pinning `execute_plan` as the catalog's last, only-mutating entry) is unaffected by where a second read-shaped tool lands ahead of it.

## Alternatives considered

**Route `get_lesson` through a new `/api/lesson` HTTP endpoint on the server**, mirroring the read-tool pattern the other six use. Rejected: the issue's own forbidden paths exclude `crates/git-vista-server/src/handlers/` for exactly this reason ("this is an MCP tool, not a new HTTP route"), and there is nothing a server round-trip would learn that the client-held `Plan` does not already carry — the server already spent the live-repository read building the plan in the first place.

**Accept the same free-form operation arguments `plan_*` tools take, and build-then-explain in one call.** Rejected: it would duplicate every `plan_*` tool's argument surface a second time inside `lesson.rs`, doubles the exposure surface `plan_tools.rs`'s closed-vocabulary discipline (ADR 0046) already governs once, and gains nothing over simply asking for the `plan` object the caller already has from calling that tool first.

## Consequences

**Good.** One function — `explain(&Plan)` — remains the entire truth about what a plan means, provably: `every_lesson_fact_matches_the_explanation_it_was_built_from` is a mechanical parity check, not a shared assumption, and it is anchored on independently hand-written expectations rather than on `to_lesson` itself. The tool needs no HTTP client wiring, no auth, no retry-on-401 — none of `authed_fetch`'s complexity — because it never leaves the process. It works identically whether the `Plan` it receives was built against a real repository or a `git-vista-fixtures` broken one.

**Costs, stated plainly.**

- `Lesson`'s shape is now a second enum family (`lesson.rs`) that must be kept in exhaustive lockstep with `Topic`/`ExplanationFact` by hand — the two `match` statements in `to_lesson` have no wildcard arm specifically so a new `Topic` or `ExplanationFact` variant fails `git-vista-mcp`'s build until it is mirrored, but that is a second place a future contributor must remember to touch, not zero places.
- `get_lesson` cannot explain repository state that never becomes a `Plan` — a conflict sitting on disk with no operation chosen yet, or a sequence mid-flight with nothing queued to continue — because `explain` takes only a `Plan`. A caller wanting a lesson about "what is wrong right now" still has to pick an operation, plan it, and ask for that plan's lesson; there is no bare "explain my repository" call. That follows directly from `explain`'s own signature (a pure function of `&Plan`, deliberately, per its module doc) and is not something this ADR's shape could have changed.
- The known `get_activity` 500-event pagination cap (`tools.rs:214`) was found again while auditing this surface for guardrail compliance. It is unrelated to plans or lessons and requires a server-side change outside this issue's allowed paths, so it is filed as [#562](https://github.com/tom2025b/git-vista/issues/562) rather than fixed here.

**Verification.** Two mutations against `to_lesson`'s mapping, failing differently — swapping `Topic::MustBeTrueFirst`/`Topic::WhatMoves` in `lesson_topic` (caught by `topics_serialize_to_the_six_fixed_snake_case_names_in_order`, a topic-order assertion) and collapsing `ExplanationFact::Advisory` into a fabricated `LessonFact::Recovery(NotNeeded)` in `lesson_fact` (caught by `every_lesson_fact_matches_the_explanation_it_was_built_from` and `get_lesson_returns_a_lesson_for_a_valid_plan`, a fact-identity assertion) — both restored byte-identical (`diff` clean) afterward. 1249 tests pass across the workspace excluding `git-vista-server` (blocked in this sandbox by missing kernel sandboxing privileges — landlock/seccomp/user namespaces, an environment limitation unrelated to this change and pre-existing on `main`) and `gv-scrollcast`/`ci/browser` (#503, cannot run in a cloud container).

---

**Signed:** max · 2026-08-26T00:00:00-04:00
