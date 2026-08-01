# Work queue — for the OTHER Claude account (new week Sat 6pm)

This account (thomas2025) is out of budget until **Tue 11:59pm**. Everything below is
ready to start. M1.13b is **merged and closed** — do not reopen it.

**Signed:** thomas2025 · 2026-07-31

---

## Read these first, in this order

1. `handoff-milestone-focus.md` — full state of today's session.
2. `design-docs/2026-07-31-premerge-test-checklist.pdf` — section 3 was never driven.
3. This file.

## Ground rules that bit us today

- **The 60s checkpointer is the SOLE git writer.** Kill it before any git write of your
  own (`pgrep -af autocheckpoint`), restart it after, continuing the number series.
- **Never delete a branch.** Testbed branches (`testbed/*`) are the only carve-out.
- **A green test that proves nothing is worse than a red one.** This milestone found
  *seven* green-but-wrong results. Before trusting a pass, ask what would make it pass
  while the mechanism was broken. Write the paired negative.
- **Verify every citation.** Six incidents of docs naming code that did not exist.

---

## Start here — first wave, verified conflict-free in parallel

These five do **not** touch the write funnel, which is why they can run at once. ADR 0016
routes every `GitOperation` variant through `plan.rs`, `planner.rs`,
`sandbox/mod.rs::network_need_for_operation` and `planner/contract_suite.rs` — so the
vocabulary slices (#219, #227, #232, #239, #247) collide with each other and must be
serialised. That is the single most important scheduling fact in M2.

| Issue | Model / effort | What |
|---|---|---|
| **#243** | sonnet / low | Align `docs/IPAD_DESIGN.md` with ADR 0032. Docs only — **do this first** as a cheap check that tooling and budget are healthy before spending on #221. |
| **#221** | sonnet / xhigh | Batch cat-file: collapse file-at-commit to one spawn. The only real thinking task in the wave. |
| **#226** | sonnet / low-med | Commit draft persistence across tab suspension. |
| **#241** | sonnet / medium | Connectivity signal + `refuse_if_offline`, mirroring the existing `refuse_if_lan_view` / `refuse_if_visualize` pattern. |
| **#245** | sonnet / medium | MCP crate scaffold. Scaffolds a whole new crate — review it before anything builds on it. |

Full set is **#219–#250** (32 sub-issues, 7 parents). Each body carries its own
verification note recorded against the code *as merged*.

---

## Unfinished from today — higher value than new features

### 1. #216 — clone can still double-spawn (real correctness gap, shipped to main)

`/api/clone` is **not operation-tracked**. It never reaches `operations::admit`, so the
idempotency key it carries buys it nothing, and two overlapping attempts really can run
two `git clone`s. `admit` already solves this for tracked writes — its own doc says "two
concurrent requests carrying the same key cannot both be admitted: the loser sees the
winner's record and awaits it."

**Fix:** make clone operation-tracked, or give it an equivalent in-progress guard.
Mitigated but not solved by `CLONE_TIMEOUT_MS = 570s`, which only makes the retry
unlikely to fire while the first attempt is still running.

### 2. Section 3 of the test checklist — ten write paths, zero automated coverage

`send_write_with_key` gained a timeout parameter and four call sites changed. **No test
anywhere makes a timeout actually fire.** Every mutation in the app goes through it:
branch, commit, stage, unstage, merge, checkout, delete, rebase, push, undo. This is on
`main` now.

### 3. Paired positive missing for the clone Drop guard

Cleanup moved into a `DestGuard` because a cancelled handler skips every match arm
(measured: `started=true, timeout_arm=false, dropped=true` on client disconnect, with the
paired positive showing `timeout_arm=true` when the client waits). What is **not** proven
is that the timeout path still removes the directory. Needs a test with an injectable
budget rather than a 600-second wait.

### 4. `menu.rs` stage/unstage fix has no coverage

#217's fix stopped stage/unstage calling `force_bump()` — they were discarding the whole
loaded graph to refresh a status chip. `menu.rs` is wasm32-only and the gate runs native
tests only, so it was verified by reading. **Drive it on the testbed.**

---

## The lesson worth carrying forward

**Cleanup written as a branch of a completion path only runs if that path is reached, and
cancellation is the absence of all branches.** Any handler creating external state — a
directory, a child process, a lock, a temp file — must attach cleanup to a value's
lifetime, not to control flow. Measured today against axum, not assumed. Worth grepping
for other instances.

---

## Server

A systemd user service (`git-vista.service`, port 8080) should be running from `main`.
`systemctl --user status git-vista`. Linger is enabled, so it survives logout and
restarts on failure.

**The SSH tunnel is client-side and will still drop** — that is iOS suspending Blink, not
a server fault. Restarting the tunnel and refreshing is the correct fix, not a bug.
