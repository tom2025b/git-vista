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
