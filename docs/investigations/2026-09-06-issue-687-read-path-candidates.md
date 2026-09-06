# Issue #687 — read-path candidates

The reconciliation sweep now retains the symbolic HEAD already returned by
`read_refs_at`, avoiding its separate `read_head_branch_blocking` open. The
feed fills `Observed::head_branch` before calling the existing generation
fold; operation preconditions keep their original observation path. Both
paths use gix's ref-name shortening, including symbolic targets outside
`refs/heads`. Detached HEAD stays branchless; unborn symbolic HEAD keeps its
name. There is no new generation recipe, cache, wire shape, or ADR.

## Measurement correction

The earlier baseline/optimized timing table in this document is withdrawn.
Both fresh target directories lacked `gv-sandbox`. The ignored harness
discarded return values, so the four git components timed failed reads
(0.00–0.03 ms), not production work. The claimed whole-path improvement was
therefore invalid even with repeated samples. Ref-only timings do not repair
that comparison. The harness now rejects a blind production reading before
measurement and during every whole-path iteration, and asserts raw git
commands succeeded. Its component-sum difference is labelled timing noise,
not concurrency savings: all production reads are awaited sequentially.

Replacement measurements and final validation are pending.

## Ref-walk investigation

The pinned gix 0.84 implementation of `Reference::peel_to_id` calls
gix-ref 0.64's `ReferenceExt::peel_to_id`. That refreshes the packed-ref
buffer, follows symbolic targets, and, unless a peeled target is already
available, reads objects until the first non-tag object. Replacing this
with the raw target ID would skip tag peeling and missing-object checks.
The harness adds open-plus-enumeration without peeling to separate listing
from object work. Its raw `for-each-ref` comparison now requests peeled tag
IDs, but is still a scale comparison, not proof of identical error handling
or recursive tag semantics.

## Correctness checkpoint

The real-repository test creates both branches before the first reading,
requires readable/stable input, and verifies that switching branches at the
same tip leaves the ref map unchanged while changing the generation. A
separate pure-fold test holds the tip and every other input constant. The
feed/operation comparison covers attached, detached, unborn, tag-symbolic,
and linked-worktree HEAD states.

Atlas records 371 and 375 describe earlier revisions; record 372 exposed a
vacuous token-change assertion (a new ref and unreadable-input nonces could
change the token). Fresh proofs against the corrected implementation are
pending; earlier IDs are retained as history, not final verification.

Signed: codex
