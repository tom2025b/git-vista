# 0078 — There is one pop, it is composed, and apply asks before it claims

**Status:** Accepted — implemented and tested; the two new pipeline tests are unrun in this environment (see Consequences)
**Date:** 2026-08-25
**Issues:** [#493](https://github.com/tom2025b/Git-Vista/issues/493), [#494](https://github.com/tom2025b/Git-Vista/issues/494)

---

## Context

These are one decision wearing two issue numbers. #493 asks whether `GitOperation::PopStash` should exist at all; #494 reports that the sibling of its executor does not do the thing `exec_pop_stash` documents as essential. Answering the first settles the second.

**`plan.rs` said two incompatible things forty-six lines apart.** `PopStash { entry, expected_oid }` was a live variant at line 1175, with a doc comment citing `/api/stash/pop` as its route and promising that "the executor re-reads the conflict state after the pop". Line 1221 said `// PopStash is deliberately ABSENT (M3.24, decided 2026-08-18)`.

The variant was not a stub. Planner dispatch, executor, risk level, precondition, recovery strategy, sandbox argv census, network need, an MCP exposure decision, a golden wire fixture, and a contract test named `pop_stash_refuses_to_report_complete_while_conflicted` — every layer handled it. And no client could reach any of it, because `main.rs` registers `/api/stash/push`, `/apply`, `/drop` and `/branch`, and no pop. `exec_pop_stash` labelled its own log lines and refusals `/api/stash/pop`, an endpoint that does not exist.

`plan_golden.rs:710` carried the same contradiction in miniature: a comment reading "`PopStash` is absent from the enum on purpose, so it is absent here too", 524 lines below a fixture that built one.

Two further facts decided this rather than merely describing it.

**The implementation contradicted the spec that authorised it.** `docs/superpowers/specs/m3.24-stash.md` §5 opens *"Pop is apply-then-drop, executed as two steps. **Do not shell out to `git stash pop`.**"* `planner/stash.rs:254` was `run_git(repo, need, &["stash", "pop", entry.as_str()])`.

**The prerequisites §5 names had not landed.** Before `PopStash` may ship, §5 requires either composite outcomes persisted on the operation row (`apply_failed` / `applied_drop_failed` / `completed`) with a Recovery Centre rendering for each, or `PopStash` as durable orchestration over linked child records. Neither exists. The reasoning is that one row carries one `state`, so "nothing ran" and "changes applied, entry still present" both record as `Failed` — and a reconnecting client cannot tell which working tree it is looking at.

Meanwhile the frontend had already shipped the composed form (PR #490): apply → read the conflict state → drop, gated by `features::stash::core::drop_gate`, argued in ADR 0077.

## Decision

**`PopStash` is removed. A pop is composed from `ApplyStash` and `DropStash`, and the `ABSENT` comment becomes true.**

**And `exec_apply_stash` re-reads the conflict state on both outcomes** — the guarantee `exec_pop_stash` documented alone. Deleting pop would otherwise have deleted the only place that check existed.

### The apply executor is three pieces, and that is the interesting part

Splitting the executor into a pure decision, a pure rendering, and a thin shell is what turns #494's untestable corner into ordinary unit tests.

| piece | what it is | tested by |
|---|---|---|
| `apply_verdict(succeeded, scan)` | pure; six `(exit status, scan)` combinations → one of five verdicts | unit tests, all six rows pinned |
| `render_apply(verdict, git_said, entry)` | pure; verdict → status + body | unit tests, all five verdicts |
| `exec_apply_stash` | spawn git, scan, journal | pipeline tests in `contract_suite` |

The verdicts:

| verdict | when | says |
|---|---|---|
| `Applied` | git succeeded, scan clear | 200 — **the only verdict claiming completion** |
| `AppliedWithConflicts` | git succeeded, scan blocked | **200**, says NOT complete, names the paths |
| `FailedWithConflicts` | git failed, scan blocked | 400, git's stderr **plus** the paths |
| `Failed` | git failed, scan clear | 400, git's stderr alone |
| `Unverifiable` | git succeeded, scan failed | **200**, says NOT complete, names the gap |

### The status mirrors git's exit status; the verdict rides in the body

**2xx exactly when `git stash apply` succeeded**, whatever the scan then found. `AppliedWithConflicts` and `Unverifiable` are 200, not 4xx, and that is load-bearing in two places neither of which can read a response body.

**`ApplyOutcome`, and ADR 0077 D6.** `api::stash::apply_stash_request` derives its entire outcome from `resp.ok()` — any non-2xx becomes `ApplyOutcome::Refused`. `drop_gate` sets `PopVerdict::Conflicted { apply_refusal }` from that, and D6 turns it into one of two sentences: `None` → *"The changes were applied but left conflicts"*, `Some(_)` → *"Applying the stash hit conflicts"*. A 409 on `AppliedWithConflicts` makes D6's `None` branch — which exists for precisely this case and no other — unreachable, and tells a user their apply was refused while their changes sit in the working tree. The same argument demotes `Unverifiable`: a 4xx there produces `RefusedUnverified`, whose sentence is *"whether anything reached the tree is genuinely unknown"*, and it is not unknown — git said it applied.

**The durable operation row.** `operations::apply_terminal` maps `status.is_success()` to `Succeeded` or `Failed` with nothing between them. A 4xx on a succeeded apply records `Failed` for an operation git performed — *"the record says only `Failed`, indistinguishable from nothing happened"*, which is verbatim the single-row limit that keeps `PopStash` out of the enum. `Succeeded` is honest here in a way it never was for a pop: apply's contract is *restore the changes and keep the entry*, and a conflicted apply did both. Nothing is lost either way, which is exactly what is not true of a pop.

So no verdict is demoted for what the *scan* found. The "not complete" claim lives in the body — and because the status has been spent on git's exit code, the body is the only channel left that can carry it. A test asserts that obligation directly: a 2xx that is not a finished apply must disclaim in its body.

**The two `Err` rows differ on purpose.** On the failure path git has already said the operation did not succeed and its stderr is the better message, so a broken scan costs only the conflict detail. On the success path the scan is the only thing between a green response and an unread working tree, so a broken scan withdraws the claim entirely. Collapsing those two arms is the obvious simplification and it is wrong in one direction; a test asserts the asymmetry with a message saying so.

### What was measured

Two claims in #494 needed checking rather than repeating.

**Can `git stash apply` exit 0 while leaving conflicted paths behind?** On git 2.43.0, eight shapes were driven directly — delete/modify, modify/delete, add/add on an untracked stash, `--index` against a diverged HEAD, a content conflict applied into a dirty tree, mode-vs-content, rename-vs-modify, symlink-vs-file. **None produced exit 0 with unmerged index entries.** Every conflicting shape exits 1; the two that exit 0 (mode, rename) leave nothing unmerged because git merged them cleanly.

So `AppliedWithConflicts` is **not a bug being fixed**. It is a guarantee held against a future git or a conflict shape not yet found, and it is stated as a property because a proxy that happens to agree is a weaker promise than a check that asks. Saying otherwise in a commit message would claim a user-visible defect nobody can demonstrate.

**What the measurement did turn up is a reachable defect, in the executor being deleted.** `exec_pop_stash` matched `(_, Ok(c))` for everything that was not a clean success, so a plain refusal with a *clear* index produced *"Popping stash@{0} left conflicts, so it is NOT complete"* above an empty detail block — a conflict report about a tree with no conflicts. It is reachable: an untracked file in the stash colliding with a committed file of that name exits 1 with `git ls-files -u` empty and stderr *"could not restore untracked files from stash"*. The new `Failed` arm exists to not inherit that, and a test drives exactly that shape.

## Alternatives considered, and why they lost

**Wire `/api/stash/pop` and keep the variant.** Overturning the 2026-08-18 decision is allowed; doing it without addressing its argument is not. The argument is that one durable row cannot distinguish "nothing ran" from "applied, entry retained", and it is untouched — the Recovery Centre still has no "applied, entry retained" class, and neither §5 prerequisite has landed. Taking this option honestly would mean building composite outcomes or durable orchestration first, then rewriting the executor as apply-then-drop because §5 forbids shelling out to `git stash pop`. That is a milestone, not a contradiction fix, and it would ship a wire promise no executor can currently keep.

**Fix only the doc comment.** #493's minimum option. It removes the contradiction between two comments and leaves the one that matters: a fully wired unreachable variant, a contract test asserting a guarantee for an operation no client can invoke, and a golden fixture pinning a wire shape nothing can send. The tree would still carry an enum arm the spec forbids, one route registration away from shipping.

**Add the conflict re-read to apply without touching pop.** Closes #494 and leaves #493 exactly as it was — including `exec_pop_stash`, whose `(_, Ok(c))` defect would then exist in a second executor rather than being deleted.

**Keep the executor as one function and record the untestable arms as a gap**, which is what `exec_pop_stash` did in a fifteen-line comment. Defensible, and the comment was honest. But four of six combinations were left unproven when the only obstacle was that the decision was welded to a git spawn. Splitting the decision out costs one enum and proves them.

## Consequences

`GitOperation` drops from 38 variants to 37. Two hardcoded count tripwires in `sandbox/dispatch.rs` move with it — they are tripwires beside the real census guard, which compares against serde's generated variant list and cannot be left stale.

**`pop_stash` leaves the v1 wire vocabulary.** No route ever built such a plan and plans are server-issued and operation-hashed, so nothing in the wild carries one. `tests/fixtures/plan_v1.json` loses its `pop_stash` plan and `plan_golden.rs` its fixture, which makes the comment already sitting there true.

Two contract tests are deleted (`pop_stash_removes_the_entry_on_a_clean_pop`, `pop_stash_refuses_to_report_complete_while_conflicted`) and two added for apply. Eight unit tests are new.

The frontend is untouched and keeps working: a refused apply is still 400 and a succeeded one still 2xx, which is what `ApplyOutcome` and `drop_gate` read. ADR 0077 D3's client-side scan on the apply-only path is now redundant rather than wrong — the server names the paths itself — and can be dropped whenever that crate is next opened. It was left alone here deliberately; the frontend was out of scope for this change.

**The two new pipeline tests were not executed on the branch that wrote them.** This container reports `landlock_abi=-1`, so the strict sandbox tier refuses every git spawn (INV-13 gives no degraded mode, per ADR 0029) and all 321 pipeline tests in `git-vista-server` fail identically before reaching their assertions — `ci_preflight_host_meets_the_declared_minimum` names the reason. Installing `bwrap` removed one of the two missing prerequisites and not the kernel one. Measured against `main` in the same container, the branch's failure set is unchanged: the two new tests fail where the two deleted pop tests used to, 320 either way. **The unit tests and the git measurements above did run here.**

## Findings recorded while implementing

**F1 — A weak test passed a mutation and was widened because of it.** `exactly_one_verdict_reports_the_apply_as_complete` originally filtered on `body.starts_with("Applied stash@{0}.")`. The mutation "soften `AppliedWithConflicts`'s wording to *Applied stash@{0}, with conflicts*, keep the 409" **survived** — the comma defeated the prefix. The filter now tests `starts_with("Applied ")`: the body may not *open* by asserting the apply happened, whatever the status says. Found by running the mutation, not by reading the test.

**F2 — A mutation harness reported false negatives.** The first battery flagged four mutations as "did not compile" because it grepped for `^error`, which matches cargo's own `error: test failed, to rerun pass…` line printed on every red run. Four real kills read as invalid mutations. A mutation harness that cannot distinguish a compile failure from a caught mutation gives the wrong answer in the safe-looking direction, and the two survivors it did report were the only reason it was checked.

**F3 — Every test here is killed at least two ways, and it was run.** Twenty-three mutations across `apply_verdict`, `render_apply` and `conflict_detail`, each applied alone and reverted: five kill the decision table, two the asymmetry test, two the completion test, two the path-naming test, four the status/stderr test, two the `Clear`-renders-nothing test, three the unreadable-paths test, and six the status-mirrors-git test. The per-test tallies are in the doc comments beside them.


**F4 — The guarantee is bounded by the index, and that bound is worth naming.** A ninth shape was tried: a custom merge driver (`.gitattributes` + `merge.<name>.driver`) that writes conflict markers into the file and exits 0. `git stash apply` then exits **0**, `git ls-files -u` is **empty**, and the working tree contains `<<<<<<<` markers. Neither the exit code nor `crate::conflicts::continuation` catches it, because both are index-shaped and the index is clean. This is not a regression — the whole conflict model is built on unmerged index stages (ADR 0063) — and a user who configures a merge driver that lies has broken a contract git itself relies on. It is recorded because "apply now asks whether conflicts remain" should not be read as "apply now detects conflict markers"; it asks the index, and the index is what it reports.

**F5 — A 4xx on a succeeded apply was shipped, and cross-session review caught it.** The first version of this branch returned 409 from `AppliedWithConflicts` and 400 from `Unverifiable`, copying `exec_pop_stash`'s status codes without checking what reads them. Both are wrong for the reasons in § "The status mirrors git's exit status", and both were invisible to every test on this branch because the frontend and `operations.rs` are in crates the stash executor's tests never touch. The unreachability of `AppliedWithConflicts` on git 2.43.0 made it worse, not better: the arm exists *only* to hold a guarantee against a future git, and in that future it would have rendered the wrong sentence. `the_status_mirrors_gits_exit_status_not_the_scan` now pins the coupling in the crate that can break it, with the reason in the assertion message rather than left to a reviewer.

**F6 — The status is spent, so the body carries the obligation.** Once 2xx means "git succeeded", a 2xx no longer distinguishes finished from unfinished, and a mutation that dropped `"NOT complete"` from `Unverifiable`'s body **survived** the completion test — that test reads bodies opening with `"Applied "`, and `Unverifiable`'s opens with `"Applying "`. The fix was not to widen the prefix but to assert the obligation where it belongs: every 2xx that is not a finished apply must disclaim in its body. That assertion also kills the equivalent mutation on `AppliedWithConflicts`.

## Correction (2026-08-25, same day): it was not unreachable

**This ADR, the issue that prompted it (#493), and the merge review that
accepted it all said `PopStash` was unreachable because no route built one.
That is false, and it was found the same afternoon by an outside model (codex)
reviewing this batch.**

The generic plan seam reached it end to end:

| step | code |
|---|---|
| `POST /api/plan` | `plan_operation(Json(op): Json<GitOperation>)` — a **bare, client-supplied** operation |
| plan construction | `shape()` and the executor dispatch both carried `PopStash` arms (`planner.rs:1672`, `:2610`) |
| `POST /api/execute-plan` | `execute_plan(Json(plan): Json<Plan>)` → `submit_plan_tracked(plan)` — takes the client's plan and **does not rebuild it**; the plan's own hash is the approval |

Both routes are `Authz::SessionAndCsrf` and live only in the write-mode router,
so this needed an authenticated session — the operator, or the MCP server,
which drives these exact two endpoints on purpose.

### Why the correction matters more than the wording

The spec §5 gate on `PopStash` is not stylistic. It exists because a pop that
applies and then fails to drop has no durable representation: the operation row
says `Failed`, which is indistinguishable from "nothing happened" while the
user's changes are in the tree. **So the exposure was data-shaped, not
access-shaped** — an operation could reach a state the Recovery Centre had no
way to render, which is precisely the situation §5 was written to prevent.

Between 2026-08-18 and 2026-08-25 that was reachable by any session holder.

### What this does NOT change

**The decision stands, and it was worth more than this ADR claimed.** Deleting
the variant is what actually closed the path: `"op":"pop_stash"` now fails
deserialization at the wire boundary, before any planner code runs. Option A
was right. The argument given for it — "it is dead code contradicting a
comment" — understated it. It was live code contradicting a spec.

### The process lesson

Four passes asserted "unreachable" without checking the generic seam: the issue
author, the implementing session, the pre-dispatch truth-check fan-out, and the
merging session. All four were Claude. The seam is documented in
`handlers/plan.rs`'s own header — *"`GitOperation` is already the closed,
internally-tagged wire … and answers a `Plan`; the execute endpoint takes that
same"* — so it was not hidden. It was simply not the question any of them
thought to ask.

**A "no route points at it" claim is a claim about EVERY route, including the
generic ones.** Grep for the type, not for the variant name.

