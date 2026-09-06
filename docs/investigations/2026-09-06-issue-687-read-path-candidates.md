# Issue #687 — read-path candidates

The handoff's #661 measurement is structurally useful but its wall-clock
values are load-contaminated on this shared host. I will repeat the supplied
measurement harness before and after, with multiple runs, and report the
spread and host load instead of treating one sample as a speed claim.

Candidate 1 (`refs_reading`) is a ref-walk/API-shape investigation. It is not a
safe cache opportunity: the sweep is the authority and must observe current
refs. I will not replace a complete walk with a guessed incremental cache in
this issue. If the supplied measurements confirm the walk dominates, the
report will preserve that as an open investigation.

Candidate 2 is a bounded correctness-preserving optimization. `read_refs_at`
already computes `HeadAtEvent` while opening and walking refs. The planner
currently throws that result away, then opens the repository again only to
derive HEAD's short branch name. I will retain the symbolic branch in the
existing `GenerationParts` value and make the generation fold and live-feed
payload consume it. The separate `read_head_branch_blocking` call used only by
the generation/live-reading path can then be removed. Operation-specific
observation still keeps its `Observed::head_branch`, because executors use
that value for operation preconditions and execution messages.

The generation token remains the same logical recipe: HEAD's symbolic branch,
HEAD tip, every display ref, stash, merge.ff, and status. A failed ref read
remains unreadable and fails closed; detached and unborn HEADs remain branchless.
No contract or wire shape changes, so no ADR is planned.

Signed: codex

## Measurement snapshot

The supplied ignored harness was run for 20 warm iterations per component on
each revision. The baseline was a disposable worktree at `origin/main`
(`0d1d8634`); the optimized sample was this branch. The shared host reported
load averages of `6.46, 6.57, 5.13` during the optimized run, so these are
directional wall-clock observations rather than isolated benchmarks.

| component | baseline mean (range) | optimized mean (range) |
| --- | ---: | ---: |
| `refs_reading` | 81.73 ms (65.21–99.83) | 52.11 ms (39.33–72.27) |
| whole `live_reading` | 105.43 ms (68.17–130.95) | 56.75 ms (40.75–81.24) |

The baseline's separate `read_head_branch_blocking` read averaged 4.38 ms
(3.29–5.16). In the optimized path, `refs_reading` returns the symbolic HEAD
state it already obtained during the ref walk, so that second repository open
is gone. The ref walk remains the dominant cost: `gix::open_opts` alone was
2.81 ms median in the baseline sample versus 82.20 ms median for the full
`refs_reading` call.
