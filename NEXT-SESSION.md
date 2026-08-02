# Work queue — verified against the live repo 2026-08-02

**Written after an overnight agent session. Everything below was checked against `gh` and
`git log origin/main` at the time of writing, not copied from a previous version of this file.**

## Read this part even if you skip the rest

The previous version of this document listed **#245, #226 and #242 as open work ready to start.
All three were already closed and merged.** A 20-agent workflow launched against them and burned
roughly fifteen agents rebuilding work that already existed before it was caught and killed.

**A handoff document is stale the moment it is written — including this one.** Before starting
any issue: `gh issue view <n>` for its real state, and grep the tree for the artifacts it claims
to create. Every workflow in this repo now opens with a cheap per-lane triage agent that does
exactly that before an expensive build agent starts. Keep that gate; it caught a second blocked
lane the very next round.

**Signed:** thomas2025 · 2026-08-02

---

## Where things actually stand

| Milestone | State |
|---|---|
| **M1 — Foundation** | 39 done / 0 open. Closed. |
| **M2 — Daily Driver** | 18 done / 37 open. The active milestone. |
| M3–M9 | Untouched. |

### Landed overnight

| PR | Issue | What |
|---|---|---|
| `#283` | #218 | A failed session bootstrap self-heals instead of leaving the app permanently stuck behind a 401 with no cookie. |
| `#279` | #219 | Typed discard/restore working-tree operations (M2.18a). **Issue deliberately left open** — see below. |
| `#285` | #222 | Typed `AmendCommit` operation, DTO, plan wiring (M2.19a). Contract only, no execution. |
| `#287` | #228 | Network-tier exec harness: askpass hardening, credential redaction, **ADR 0036**. |
| `#286` | #211 | Diff performance budget. **Issue deliberately left open** — see below. |

**No PRs were open when this was written.** The whole stack merged.

## Start here

### 1. #219 needs a human, not an agent

The code is merged and green. **What has not happened is a drive on the iPad.** It is left open
on purpose: #65 was auto-closed by GitHub before its verification pass and had to be reopened,
and closing on a green CI run is the "committed ≠ done" mistake this repo keeps re-learning.

Three paths worth driving specifically, because no test can reach them:
- Discarding a **staged-only** change (bare `git checkout --` was a silent no-op here; the fix is
  `git checkout HEAD --`).
- Deleting an untracked file, checking the confirmation copy never implies it is recoverable.
- Naming a **directory** to either operation — both must refuse rather than recurse.

### 2. Three defects filed from review passes, all the same species

Each was found by reading merged, green, already-reviewed code. All three are cases where **a
test passes because it compares against its own copy of the value it is supposed to be checking.**
That pattern is worth a deliberate sweep beyond these three.

- **#284** — `git clean`'s stdout parse is locale-dependent. Under a translated locale a fully
  successful delete returns 409 telling the user their files survived, *after* they are
  irreversibly gone. Plus duplicate paths inflating the reported count.
- **#288** — the clone registry's defensive re-insert fabricates a placeholder URL that is
  **byte-identical to the string all 17 tests use**, so no test can distinguish the fabrication
  from correct behavior. Currently unreachable; the failure mode if reached is a false
  "you reused that key for a different URL" 409 telling the user to fix a problem that does not
  exist.
- **#289** — #278's clone polling greps for `"already in progress"` in the server's 409 message,
  with **no shared constant** — the client test uses its own hand-written copy of the server's
  string. Reword that sentence server-side and polling silently stops working, with both crates'
  tests and all seven CI checks green, on the exact flaky-tunnel path the feature exists for.

### 3. #211 is open on purpose

No virtualization is wired into the diff render path **at all** today — `CumulativeHeights` and
`visible_range` appear only in comments. The budget measures the real (flat) path and says so.
When virtualization is actually wired in (part of #69, M2.16), it needs its own budget test at
that layer. Do not close #211 before that exists.

## M2 scheduling — the one fact that matters most

ADR 0016's write funnel — `git-vista-protocol/src/plan.rs`, `planner.rs`,
`sandbox/mod.rs::network_need_for_operation`, `sandbox/dispatch.rs`, `planner/contract_suite.rs`,
`durable.rs::recovery_oid` — is touched by nearly every "typed operation vocabulary" issue.
**Run at most ONE funnel-touching issue at a time**; parallelize everything else freely.

Funnel-touching: **#227** (remote ops), **#235** (tags), **#247** (planner split), #223 (amend
execution). Each parent's `a` slice lands before its siblings — read each issue's own
`Depends on` line.

### Since ADR 0036, network classification is load-bearing, not a label

Declaring `NetworkNeed::Remote` is now what routes a spawn through askpass hardening and
credential redaction. **A fetch/pull/push classified `Local` is a real credential-leak path, not
a cosmetic mislabel.** Read ADR 0036 before touching `network_need_for_operation`.

### Genuinely parallel right now

Verify each against `gh` before starting — that is the whole point of the top section.

| Issue | What | Notes |
|---|---|---|
| #220 | Discard/restore UI, tiered confirmation | Unblocked now that #219 merged. Frontend only. |
| #209 / #234 / #244 / #250 | Close-out audits of #68 / #73 / #75 / #153 | Read-only; produce findings, collide with nothing. |
| #284 / #288 / #289 | The three review-pass defects above | Small, independent diffs. |

## Two failure patterns that repeated overnight

**Tested but unreachable.** #228's exec harness shipped fully built, fully tested, and with
**zero production callers** — the hardening was unreachable from `exec_push`, the only path that
needed it, and every test passed. Caught by same-branch adversarial review one commit later, fixed
by wiring into the chokepoint that already existed (`git_cmd.rs`'s `sandboxed()`) rather than
adding a second entry point. Recorded in ADR 0036 as a rejected alternative. Build agents are now
required to report the concrete production caller with a file:line, and a reviewer traces it.

**A count-based guard that had silently stopped guarding.** `sandbox/dispatch.rs`'s
`every_operation()` asserted `GitOperation` had exactly 15 variants while the enum had grown to
18. It compiled, it passed, it protected nothing. Replaced in #285 with `variant_name()` +
`every_operation_declares_every_variant`, a real compile-time exhaustiveness check. A sweep found
the only other pinned counts (`route_authz.rs`) are cross-checked by a source scanner, so they are
sound — but the shape is worth recognising.

## Ground rules, each earned here

- **The 60s checkpointer is the sole git writer** in the primary checkout. `pgrep -af autocheckpoint`
  — it dies silently on branch switches. Continue the `auto-checkpoint N` series from
  `git log --all`, **not** `git log --oneline -200` (undercounts past a merge). Agents working in
  their own worktrees have their own index and are safe to commit there.
- **Never delete a branch.**
- **A green test that proves nothing is worse than a red one.** Six occurrences, and three more
  filed tonight.
- **The ruleset on `main`** (`20171903`, 7 required checks) requires every PR to be up to date
  with `main` before merge — expect one extra CI wait and a re-run on the merge commit.
  **Repo-wide auto-merge is disabled**, so `gh pr merge --auto` fails; merge by hand after the
  sandbox gate (~11 min, the long pole).
- **An ADR is required** for security-boundary changes and anything implementing
  `docs/SECURITY_MODEL.md`. That exact gap blocked #287's merge until ADR 0036 was written.

## Known debt, not blocking

- `crates/git-vista/src/api.rs`, `menu.rs`, `detail.rs`, `picker.rs` and siblings are
  `#[cfg(target_arch = "wasm32")]`-gated and **never compiled by `cargo test --workspace`**. No
  wasm harness exists in this repo, so "gate green" means nothing for those files. Standing
  workaround: extract decision logic into a host-tested `features/*/core.rs`.
- `tmp-mcp/gv_test_mcp.py` is tracked on `main` and **not** gitignored there — disposable testbed
  scaffolding whose own docstring says to delete the folder when M2 is done. A `/tmp-mcp/`
  gitignore entry sits uncommitted in the working tree. Decide deliberately: track it, or
  `git rm --cached` it.
- `docs/SECURITY_MODEL.pdf` is tracked beside its `.md` source — the anti-pattern the global
  PDF-hygiene rule forbids (rendered PDFs belong in one collected folder).
- Print Graph builds the whole loaded history into one unvirtualized SVG — plausible cause of an
  iPad memory lockup, never confirmed.
- `linux-ops-suite#96` — generalize `rex-check`/`gv-report` into an opsmcp `repo_report` tool.
  Different repo, zero collision risk.
