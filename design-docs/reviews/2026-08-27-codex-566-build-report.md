# PR #566 build report — ready-frame bound and fixture remaining size

**Branch:** `claude/cloud-3-540-size-hint-gate`

**Starting head:** `85577ead2a4e5b73cd24b460726ee5d0750aaab6`

**Scope:** the two blocking defects in the controlling handoff, built before the corrected last-step merge-down

## What changed

- `split_at_limit_when_ready` now counts yielded frames with the existing `MAX_SPLIT_FRAMES` budget. Exhaustion returns `ReadyOutcome::NotReady(rejoin(...))`, so an unverified success body is forwarded unlabeled rather than classified as overflow.
- `ReadyOnceThenNeverReady::size_hint` now reports two bytes before its first frame is served and one byte afterward, matching the `http_body::Body` remaining-byte contract.
- Added one direct regression for each defect: an externally bounded endless exact-zero body for the success reader, and a partial-consumption assertion around the fixture's first frame.

I did not modify `MAX_SPLIT_FRAMES`, `split_at_limit`, or their doc comments. The reserved frame-budget classification decision and its comment remain untouched.

## Test-first evidence

### Defect 1 — `split_at_limit_when_ready` frame bound

Exact command:

```text
buildlock cargo test -p git-vista-server middleware::tests::an_endless_run_of_empty_frames_does_not_spin_split_at_limit_when_ready -- --exact
```

- RED before production change: failed after 10.00 seconds at the external `std::sync::mpsc::recv_timeout` with `Timeout`.
- GREEN after the counter and conservative exhaustion return: 1 passed, 0 failed.

Two mechanism mutations, with the assertion unchanged:

1. Counter progress changed from `frames_read += 1` to `frames_read += 0`: failed after 10.00 seconds with the external timeout.
2. Exhaustion changed from `NotReady(rejoin(...))` to `Ready(Overflow)`: returned immediately and failed at `frame-budget exhaustion cannot classify this body as ready`.

After each mutation, the source and its saved green copy both had SHA-256 `19b3e20c8001683dddd1dd6046eadb366d51caa8e633b7233b3e098ae69a7062`.

### Defect 2 — `ReadyOnceThenNeverReady` remaining size

Exact command:

```text
buildlock cargo test -p git-vista-server middleware::tests::ready_once_then_never_ready_reports_one_remaining_byte_after_its_first_frame -- --exact
```

- RED before fixture change: after consuming the literal one-byte `{` frame, actual `Some(2)` differed from expected `Some(1)`.
- GREEN after deriving the hint from `served_first`: 1 passed, 0 failed.

Two mechanism mutations, with the assertion unchanged:

1. Restored the stale constant `SizeHint::with_exact(2)`: failed after consumption with actual `Some(2)`, expected `Some(1)`.
2. Over-decremented the served state to zero: failed after consumption with actual `Some(0)`, expected `Some(1)`.

After each mutation, the source and its saved green copy both had SHA-256 `afe332284443f855b3fa99358de07206f30c176d6d16b0f3bf3cd645f927bd46`. Rustfmt subsequently changed layout only; the final formatted source has SHA-256 `78318fbfa69f6e1c3e9c4b20f84f30b7a6ab2c22482a617af424ba122c231963`, and no mutant was retained.

## Pre-merge acceptance

- `buildlock cargo fmt --all -- --check` — passed after applying rustfmt to the new blocks.
- `buildlock cargo clippy -p git-vista-server --all-targets -- -D warnings` — passed.
- `buildlock cargo test -p git-vista-server middleware` — 34 passed, 0 failed, 956 filtered in the main-binary harness.

No workspace-wide or browser suite was run; both are outside this handoff's acceptance scope.

## Merge-down status and landability

**Merge-down: BLOCKED.** At the prescribed last-step check, PR #570 remained `OPEN`, with `mergedAt: null` and no merge commit. I did not fetch or merge stale `main`.

Both defects assigned to this build are resolved and evidenced on the current #566 branch. I do **not** consider #566 landable yet: after #570 lands, `origin/main` still must be merged into this branch and all three acceptance commands above rerun against the merged tree. The fresh independent skeptic review remains part of the landing workflow.

**Signed:** codex · 2026-08-27T15:00:03-04:00
