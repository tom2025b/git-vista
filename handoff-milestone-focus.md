# Handoff — M1.13b Close-Out + M2 Sub-Issue Kickoff (focused session)

**Written:** 2026-07-31T08:12:12-04:00 · **Signed:** thomas2025 · 2026-07-31T08:12:12-04:00

> This file is for a **brand-new Claude Code session** opening in
> `/home/tom/projects/Git-Vista` with **no memory of any prior conversation**.
> It is scoped to one job: **finish milestone M1.13b (issue #66, PR #207) and
> start filing/picking up the M2 sub-issue breakdown.** It is deliberately
> self-contained — read this file and you have everything needed to start
> without re-reading `design-docs/2026-07-31-milestone-plan.md` first (though
> that plan is the full source of truth if you need more detail than the
> summary below).

---

## THIS IS NOT YOUR ONLY JOB IN THIS REPO — read this before touching anything

There is likely **another Claude session running in this same repo right
now**, working on the iOS-Shortcuts-bridge brainstorm, tracked in the repo's
regular `handoff.md` (gitignored, repo root). **That work is NOT yours.**
Do not pick it up, do not edit `handoff.md`'s content about it, do not
duplicate effort. Your scope is the punch list and sub-issues below, full
stop. If you finish those and have budget left, check in before wandering
into other tracked issues.

---

## CRITICAL — coordination rules, read before any git or server action

### 1. A background git-checkpointer may already be running — check before starting one

A background `autocheckpoint` process may **already be alive** for this repo
from a different session/window. Only **one** checkpointer may write git
history at a time — two running concurrently will race the same commit and
can corrupt it.

**Before doing anything else, run:**

```
pgrep -af autocheckpoint
```

- **If it shows a process** (e.g. `bash /home/tom/.local/bin/autocheckpoint /home/tom/projects/Git-Vista ...`)
  — **do not start a second one.** Just work; the existing one is already
  checkpointing your commits every ~60s.
- **If nothing shows** — start one, continuing the checkpoint series. Find
  the highest existing number first:

  ```
  git log --oneline -30 | grep -o 'auto-checkpoint [0-9]*' | head -5
  ```

  As of this writing the highest is **`auto-checkpoint 537`**, so a freshly
  started checkpointer should use `START_N=538`. Re-check at the time you
  actually start it — it may have advanced. Launch via the shared script,
  not hand-rolled:

  ```
  autocheckpoint /home/tom/projects/Git-Vista <scratch-dir> 60 66
  ```

  (60-second interval, issue number 66 for the commit message tag.)

### 2. NEVER restart the dev server without asking Tom first

Port 8080 is Tom's **live iPad session** right now. Do not run `./dev serve`,
`./gv`, `gv --stop`, or the raw binary, and do not stop/restart anything
already running. If a task genuinely requires driving the live app (e.g. a
testbed pass), that is explicitly **Tom's own action** (see P3h below) — not
something this session executes. Ask first, always.

### 3. Never delete branches. Commit as `claude_2010`.

Standing repo rule: every fix goes on its own branch, PR, merge to main,
**branch is never deleted** afterward (no `git branch -d`, no GitHub
auto-delete). Claude's commits use author `claude_2010` with email
`262510778+tom2025b@users.noreply.github.com` — `./dev wip` sets this
automatically; if committing manually, set it per-commit, not globally.

### 4. Subagents never touch git

If you fan out to subagents for research/drafting, **none of them may run
`git add/commit/push/checkout/switch/reset/rebase/stash`, delete a branch,
or run `./dev wip`.** The background checkpointer is the sole git writer.
Subagents read, draft changes (exact `old_string`/`new_string`), and hand
them back for a single agent to apply — see the two-phase pattern in the
plan's Section 4.

---

## TOP 5 PRIORITY ACTION ITEMS — start here

Full detail and verification trail is in
`design-docs/2026-07-31-milestone-plan.md`; this is the executive summary so
you can act immediately.

1. **P1a — Fix the CI preflight gap (hard blocker on PR #207).**
   `.github/workflows/ci.yml` only adds the host-capability setup (bwrap +
   userns-unclamp preflight) to the `sandbox` job. Since M1.13b routes *every*
   git spawn through the sandbox chokepoint, the `core` and `contract` jobs
   now hit the same Strict-tier construction with no preflight —
   `CapabilityAbsent` is a deliberate hard failure, so **111 tests fail in
   Core** and the **Contract job fails outright**. Fix: add the same
   host-capability setup step to `core` + `contract` jobs.
   **Model/effort: sonnet, medium.** ~30–45 min, single-agent, one YAML file.

2. **P1b — Fix the escape-battery fixture git-identity gap (hard blocker).**
   `escape_contract.rs::fixture()` deliberately builds a repo with **no
   local git identity** (by design — identity should flow through the
   sandboxed `$HOME` grant, not be hard-coded). This works on Tom's box
   (configured `~/.gitconfig`) but a fresh GitHub Actions runner has none, so
   the seed commit fails before any sandbox invariant is exercised — **17/17
   escape+hook tests fail in the Sandbox CI job.** This is exactly what issue
   **#203** already tracks. Fix at the CI-environment level (e.g. `git
   config --global` step in the workflow) — do **not** touch R7's pinned-env-
   profile intent in the fixture itself, that's the wrong layer to patch.
   **Model/effort: sonnet, high** (needs judgment to fix at the right layer).
   ~20–40 min, single-agent, same PR as P1a likely.

3. **P3 + P3h — Rebuild the stale testbed, then get Tom's human-in-the-loop
   pass.** `~/projects/Git-Vista-testbed-8081` is pinned at auto-checkpoint
   525 — **12 checkpoints behind** current tip (537) — and predates both the
   Codex provenance-repair fix and today's CI run. Its recorded resume
   command in `handoff.md` (`./target/debug/git-vista-server` from inside
   that dir) will also fail as written — no `target/` exists there; the
   binary actually built into the main tree's shared `CARGO_TARGET_DIR`.
   Rebuild the testbed fresh (haiku/sonnet, low effort, ~2 min) before asking
   Tom to drive the human testbed pass (his action, 15–30 min) — this is the
   repo's own Definition of Done requirement, not optional.

4. **P2 — Mark PR #207 ready for review, but only after P1a/P1b are green.**
   Currently `isDraft: true`. Marking ready with 3 failing checks would
   surprise Tom — fix the CI gaps first, then either mark ready or explicitly
   flag remaining red checks to him. (Note: this repo is on a free GitHub
   plan, branch protection returns 403 — these checks are almost certainly
   not merge-blocking at the platform level; `./dev gate`/`./dev verify` is
   the gate that actually matters here.)

5. **P6 — Explicitly close the C11 residual-risk thread before merge.**
   C11 (2026-07-29 adversarial review) found 0/5 sampled escape-battery
   tests "PROVES containment" because they ran through a since-deleted test
   harness (`shim_cli::launch`) instead of the real production path. Strong
   structural evidence this is fixed (a tripwire test,
   `r6_every_inside_leg_spawns_through_the_production_seam`, explicitly
   guards against the deleted path and passes today) — but nobody has
   written the explicit "re-read C11's five-test table against current line
   numbers, confirm each row now passes" closure the way Task 28 did for its
   finding. **Model/effort: session model (opus), high** — this is exactly
   "adversarial verify, security boundary, the one stage that must not be
   wrong." ~45–90 min. Optionally pair with an independent crosscheck agent,
   mirroring the Task 27 implementer+verifier pattern already used
   successfully on this branch.

Lower priority but in the plan: P5 (three small docs-currency drifts —
`SECURITY_MODEL.md` banner, ADR 0030 addendum, issue #66's stale `#208`
checkbox — sonnet, low/medium, ~15–20 min, good small fan-out candidate);
P4 (issue #65 real-device iPad accessibility verification — human action,
not agent work); P7 (#188 SSH carve-out — explicitly deferred by Tom, not
blocking #66).

---

## After #66 closes: M2 sub-issue breakdown (not yet filed on GitHub)

The plan proposes filing sub-issues under the existing M2 parent issues
(`#68`–`#75`, `#153`, `#170` — content is "Daily Work: Status to Push" per
`docs/GIT_CLIENT_ROADMAP.md`; note the milestone *object* titles in GitHub
are one step out of sync with the issue title-prefix numbering — see plan
Section 2.1, an editorial call for Tom, not something to silently fix).

**None of these sub-issues exist yet** — the plan proposes them; filing
and/or picking them up is Tom's call, and closing #66 unblocks two of the
parent issues directly (#72 and #73 both explicitly name M1.13 as a
dependency).

Quick reference (full model/effort/workflow/timeline tables are in the
plan's Sections 3–5):

| Parent | Proposed sub-issues | Notes |
|---|---|---|
| #68 (Status) | 68f (close-out, verification only) | sonnet, low |
| #69 (Diff) | 69e (accessible nav), 69f (perf budget) | sonnet, medium |
| #70 (Staging) | 70a→70e, sequence a→b→c then d/e can fan out | mixed sonnet medium/high |
| #71 (Discard) | 71a (typed ops, irreversible — adversarial-verify candidate), 71b (UI) | sonnet high/medium |
| #72 (Commit) | 72a→72b sequence, then 72c/72d parallel; 72b is adversarial-verify candidate (rewriting shared history) | sonnet medium/high |
| #73 (Remotes) | 73a→73b→73c→73d strict sequence; 73b (credentials) is opus-level adversarial-verify | sonnet + opus for 73b |
| #74 (Tags) | 74a, 74b — good parallel-fan-out candidates | sonnet low/medium |
| #75 (PWA) | 75a (cites ADR 0032 directly — no service worker), 75b | sonnet medium |
| #153 (MCP server) | 153a (read-only tools) then 153b (write-capable — opus adversarial-verify, new less-trusted caller of the write path) | sonnet then opus |
| #170 | Standalone, no split — well-scoped perf refactor | sonnet medium |

**Workflow guidance in one line:** single-file/human-only tasks get no
workflow; genuinely independent files (68f, 69f, 74a, 74b, 75a, 75b, 153a,
170) are good parallel fan-out; anything touching a shared file
(`planner.rs`, `git_cmd.rs`, `styles.css`, `ci.yml`) uses the two-phase
research-then-single-writer pattern, never parallel `Edit` calls on the same
file.

---

## Reference: source plan document

Everything above is summarized from `design-docs/2026-07-31-milestone-plan.md`
(written 2026-07-31, verified against the repo, GitHub, and a local CI run
— not inferred from the milestone brief). Go there for: the full root-cause
mermaid diagrams for P1a/P1b, the complete punch-list table (Section 1's
summary table), the full sub-issue breakdown prose (Section 2.3), the
complete model/effort table (Section 3), the complete workflow-verdict table
(Section 4), and the complete timeline table (Section 5). That document
itself is marked as a **strong draft** pending Tom's sign-off on the sub-issue
breakdown and timelines specifically — the CI/testbed/C11 punch-list
findings (Section 1) are independently verified facts, not proposals.

---

**Signed:** thomas2025 · 2026-07-31T08:12:12-04:00
