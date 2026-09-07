# #141 — interactive status chip

The existing /api/status read will supply a tappable status explanation panel.
No new endpoint or wire contract is needed. Explanations and available guided
actions belong in a host-tested pure core; the wasm view only renders and
routes to existing staging, commit review, pull/push confirmations, and Activity conflict review.

Unknown/loading/failed status must say it cannot tell, never imply clean.
Explain staged, unstaged, untracked and conflicted files in actual English;
explain upstream-relative ahead/behind counts using the branch/upstream names,
including clean, diverged, no-upstream and singular cases. Do not guess causes
that the status response does not establish.

Visualize and LAN sessions receive explanations without action buttons.
Active actions remain gated on available status and the existing write flow;
no tap on the chip executes Git. The chip must offer a 44x44 touch target,
keyboard access, and state announcements. Stale repository responses cannot
be presented as the current repository's status.

Required verification: exact-English core tests, a source census binding the
wasm rendering to the core, two disjoint Failure Atlas mutations, and browser
tests for tapping the chip, fixture sentences and absent read-only actions.
Use the repository gate before the implementation PR. No ADR unless a contract
change proves necessary. Finish report must quote the actual panel sentences.

Implementation checkpoint: pure sentences and action policy, repository-pinned
shared readings, touch-sized modal, existing action wiring, and source census
are implemented. Ten core tests and the initial five browser cases passed.
Failure Atlas caught direction mutation 388 and read-only mutation 389.
Final stage-to-commit and keyboard focus checks passed. After merging main
17fda1cf, the all-target workspace build passed and the full gate was green:
3379 host tests passed (21 ignored), 120 browser tests passed. The finish
report records the PR and quotes each state’s actual English.
Refs #141.

Review checkpoint (three independent readers: Codex gpt-5.6-sol, Claude Fable
5.1, grok 4.6). The implementation held; the proofs did not. Three fixes:
zero ahead/behind now discloses that Git reports the same zeros when it could
not compare, instead of asserting a distinction the wire format cannot carry
(no wire change); the unsupported-push test now asserts the guidance sentence,
not only an empty action list; and the retained-reading decision moved from the
wasm-only call site into host-compiled `current_reading`, whose reply carries
its own requested scope, so a duplicated or swapped comparison argument now
goes red on the host. The source census is a STRING census and its comment now
says only that. Failure Atlas mutations 390-395: six mutations, two disjoint
per tightened test, all caught on clean baselines.
Refs #141.
