# Decisions — #566 frame bound and fixture honesty

- 2026-08-27T14:22:41-04:00 — Build both independent defects against PR #566's current head before merging main, because Tom explicitly corrected the handoff ordering and neither fix depends on PR #570.
- 2026-08-27T14:22:41-04:00 — Work only in `/tmp/git-vista-codex-566.aR6wdZ/repo` on `claude/cloud-3-540-size-hint-gate`, because the live checkout's index is owned by another process.
- 2026-08-27T14:22:41-04:00 — Serialize both edits in one clone and one middleware file, because their source and mutation experiments overlap even though their invariants are independent.
- 2026-08-27T14:22:41-04:00 — Use test-first red/green cycles and mechanism mutations, because each fix must carry evidence that would have caught both the original defect and a distinct weakened implementation.
- 2026-08-27T14:25:41-04:00 — Treat `buildlock cargo test -p git-vista-server middleware` at 32 passed and 0 failed as the branch baseline, because it exercised the intended main-binary middleware harness rather than a filtered zero-test target.
- 2026-08-27T14:29:36-04:00 — Model defect 1 with a truthful exact-zero body whose empty frames are always ready, and bound the wait from another OS thread, because a runtime timer cannot preempt the non-yielding poll loop under test.
- 2026-08-27T14:29:36-04:00 — Make the defect 1 regression distinguish non-termination from the wrong `Ready(Overflow)` exhaustion policy, because the required two mechanism mutations must fail for different reasons.
- 2026-08-27T14:34:05-04:00 — Enable Tokio time in the defect 1 worker runtime, because `split_at_limit_when_ready` itself constructs a zero-duration timer and a timer-disabled runtime makes the test error before reaching the missing frame bound.
- 2026-08-27T14:39:31-04:00 — Accept the corrected defect 1 RED run as causal evidence: the exact single-test command failed after 10.00s with `recv_timeout: Timeout`, proving the current success reader never returns on the truthful endless-empty-frame body.
- 2026-08-27T14:39:31-04:00 — Mirror `split_at_limit`'s counter placement but return `NotReady(rejoin(...))` at the same budget, because the success reader must forward an unverified body unlabeled rather than classify budget exhaustion as overflow.
