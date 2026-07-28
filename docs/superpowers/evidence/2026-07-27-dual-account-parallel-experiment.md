# Dual-account parallel work — experiment results

- **Date:** 2026-07-27
- **Setup:** Max account (thomas2025) as orchestrator, Pro account (thomas2010)
  as worker, running simultaneously on one box
- **Question asked:** does this save time, or only create coordination overhead?

## Verdict

**It works, and it pays — but not for the reason expected, and not at the task
size first assumed.**

| Task | Work | Outcome | Time |
|---|---|---|---|
| 1 | Recover ADR 0023, fix a dead index link | Merged (PR #157) | **Net negative** — ~39 min vs ~20 inline |
| 2 | #67 CI security gates, dependency register, version docs | Merged (PR #159) | **Genuine win** — ran fully concurrent with three design rounds on #66 |
| 3 | Fix #158 flaky test | In progress | Boundary held under real pressure |

## The mechanism

Two Claude Code sessions, independently authenticated, coordinating through
files and git — no live IPC exists between separate `claude` processes.

- **Isolation:** `CLAUDE_CONFIG_DIR=/home/tom/.claude-pro` relocates the entire
  config directory including credentials, so both accounts stay logged in
  simultaneously.
- **Git isolation:** a separate `git worktree` per session. This is not optional.
  Worktrees have independent index and HEAD, so each session's 60-second WIP
  checkpointer can run without racing the other. Two sessions in one directory
  would corrupt each other.
- **Mailbox:** `.claude/parallel/` — `orchestrator.md`, `pro-task.md`,
  `pro-result.md`, `state.json`. Gitignored, and both sessions use the same
  **absolute** path so it works across worktrees with no commit/push round-trip.
- **Human relay:** the operator carried time-sensitive messages verbally. Files
  are the durable record; the human is the low-latency channel. Neither replaces
  the other.

## Two setup traps that would silently break this

1. **`CLAUDE_CONFIG_DIR` orphans the global rules.** It relocates all of
   `~/.claude`, so `~/.claude/CLAUDE.md` does not load. The worker would run as
   a competent agent with none of the project's standing orders — commit
   identity, PDF conventions, never-delete-a-branch. Fixed by symlinking the
   global file into the alternate config dir.
2. **A repo's own `CLAUDE.md` may be untracked**, as it is here, so it does not
   travel into a new worktree either. Copy it explicitly.

Both fail silently. The worker looks fine and quietly ignores every rule.

## Protocol defect found the hard way

Assigning task 2 updated `pro-task.md` but left `pro-result.md` still reading
"task 1 — **done**". The worker checked the mailbox, saw a completed result, and
correctly concluded there was nothing new. Orchestrator's bug, not the worker's.

**Rule adopted:** an assignment is *three* files, never one — new task, reset
result, updated state. A mailbox whose files disagree is worse than no mailbox,
because the worker cannot tell which is stale.

## What actually made it worth doing

Not raw speed. Two things:

**1. A second context caught a failure a green check was hiding.** In task 2 the
worker's first secret-scanning job passed in six seconds while scanning **one
commit** of a 262-commit history — green, and inspecting almost nothing. It
caught this by reading the job log rather than trusting the checkmark, and
replaced the action with a pinned binary that genuinely scans full history. An
independent agent with its own context and an explicit "prove the job actually
did something" criterion found what a passing CI check concealed.

**2. Written boundaries held without supervision.** In task 3 the worker traced
a bug's root cause into `planner.rs`, recognized it as forbidden territory, and
**stopped** rather than widening scope — then quantified the residual failure
rate instead. It also made two unprompted good calls in task 1: preserving the
original author's signature instead of overwriting it, and verifying a README
row matched before editing it.

## The real limiting factor: task sizing

The overhead is roughly fixed per task — write `pro-task.md`, read
`pro-result.md`, review the diff. Call it 5–8 minutes once setup is amortized.
Setup itself was 21 minutes and is paid once.

So the economics are entirely about task duration:

- A 90-line spec for 20 minutes of work is **upside-down**. Task 1 proved the
  protocol and lost time doing it.
- The same 90-line spec for several hours of work is the **right shape**. Task 2
  paid for the whole experiment.

**Rule of thumb:** delegate when the task is genuinely independent, at least an
hour of work, and verifiable against written criteria without the orchestrator
re-deriving the reasoning. Do not delegate to keep the second account busy.

## What does not delegate well

Most of the originally-proposed worker list — ADR drafting, evidence documents,
PR descriptions — turned out to be **downstream of implementation that had not
happened yet**. Documentation about unsettled design cannot be written in
parallel with the design; it is dependent work wearing parallel clothing.

The genuinely parallel seams were structural: different directories
(`.github/` vs `crates/`), different concerns, no shared fixtures.

## Cost note

Right-size the worker per task rather than inheriting the session model.
Tasks 1 and 2 ran sonnet/medium; task 3 (a concurrency bug hunt) warranted
sonnet/high. Running the worker at the orchestrator's tier would have wasted
most of the savings.

---

**Signed:** thomas2025 · 2026-07-27T22:35:00-04:00
