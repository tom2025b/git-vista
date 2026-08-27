# Decisions — #566 frame bound and fixture honesty

- 2026-08-27T14:22:41-04:00 — Build both independent defects against PR #566's current head before merging main, because Tom explicitly corrected the handoff ordering and neither fix depends on PR #570.
- 2026-08-27T14:22:41-04:00 — Work only in `/tmp/git-vista-codex-566.aR6wdZ/repo` on `claude/cloud-3-540-size-hint-gate`, because the live checkout's index is owned by another process.
- 2026-08-27T14:22:41-04:00 — Serialize both edits in one clone and one middleware file, because their source and mutation experiments overlap even though their invariants are independent.
- 2026-08-27T14:22:41-04:00 — Use test-first red/green cycles and mechanism mutations, because each fix must carry evidence that would have caught both the original defect and a distinct weakened implementation.
- 2026-08-27T14:25:41-04:00 — Treat `buildlock cargo test -p git-vista-server middleware` at 32 passed and 0 failed as the branch baseline, because it exercised the intended main-binary middleware harness rather than a filtered zero-test target.
