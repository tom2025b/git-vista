title: test(browser): harden repository mode-dialog openers
pr: 832
allow_closing: 830
---BODY---
## Summary

- replace instant mode-dialog visibility samples at all seven #830 sites with a 20-second Playwright wait, followed by the mode click and independent picker/dialog dismissal assertions
- make `openStashRepo` and `openWorktreeRepo` wait for their own repository label and an attached graph node, matching `openMergePreviewRepo`'s accepted-Frame epoch guard
- apply the same repository-specific readiness sequence to the five spec-local openers in `broken-head`, `conflict-editor`, `conflict-panes`, `nontext-conflicts`, and `wip-collapse`
- expand the source-structure check from two shared helpers to all seven #830 sites and run it as an early step in the browser CI job

I chose the issue-completing repair, so `Closes #830` and `allow_closing: 830` remain accurate. Graph-region visibility is not repository readiness: the previous repository's region can remain attached while `/api/select` settles. Each opener now identifies the repository rendered from the accepted Frame before accepting a node from that graph.

## Regression evidence

The Python check is deliberately narrow and textual. It enforces the exact wait/click/dismiss/readiness sequence and now runs in CI, but it cannot prove Playwright locator semantics or rendered behavior. The eight affected Playwright files are the semantic coverage; the targeted browser run exercised all 40 tests against the rebuilt wasm bundle.

- `python3 -m unittest -v ci.browser.tests.test_mode_dialog_openers`: 7 executed, 7 passed, 0 failed
- targeted Playwright inventory: 40 tests in 8 files
- targeted Playwright run: 40 executed, 40 passed, 0 failed, 0 skipped (3.1 minutes)
- locked Trunk candidate-bundle build: exit 0
- `git diff --check`: 0 errors

## Mutation evidence

Failure Atlas repository: `/home/tom/projects/gv-830`.

- mode-selection sequencing, worktree path:
  - record 557: `caught`; real tag-selector browser contract, Full-mode click removed
  - record 561: `caught`; instant visibility sample restored
- mode-selection sequencing, stash path:
  - record 565: `caught`; Full-mode click removed
  - record 567: `caught`; 20-second wait weakened to the instant visibility sample
- repository-specific epoch readiness:
  - record 568: `caught`; stash repository-label/node guard removed
  - record 569: `caught`; stash repository identity weakened to generic `/repo/i`

Records 565/567/568/569 use the CI-wired structural guard, so they prove that exact mechanism cannot be removed or weakened silently; they do not substitute for the 40-test browser run above. An initial underspecified weakening edit was rejected as ambiguous (record 566) and is not counted as evidence.

## Review findings

No blocking finding was overturned. The readiness objection was correct, and the five disclosed local openers meant the prior closer overstated the completed scope. This revision fixes both rather than removing the closer.

`openBranchMenu` remains unchanged: its visibility sample probes an optional merge item while walking graph nodes, not a guaranteed repository mode dialog, so it is outside #830's defect class.

Signed: codex

Closes #830
