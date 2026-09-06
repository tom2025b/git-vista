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

## Validated replacement measurements

Five baseline/optimized pairs, alternating order, used the existing ignored
harness with one cold iteration discarded and 20 warm iterations per
component. Both binaries had their own built `gv-sandbox`. Baseline production
code was `0d1d8634`; optimized code was `5685728f`. The baseline harness alone
received the same readability checks and enumeration probe. Both binaries
read one fixed local mirror snapshot (826 refs: 825 commit targets and one
tag object), checked out under `/tmp/gv-687-measure-repo`. This avoids other
lanes changing the measured ref set. Measurement ran 15:47–15:49 EDT with
one-minute load averages 4.70–6.07. It was not an idle-host benchmark.

| Component | Baseline median (range), ms | Optimized median (range), ms |
| --- | ---: | ---: |
| Whole `live_reading` | 283.74 (88.41–297.26) | 279.22 (85.08–294.14) |
| Separate HEAD branch open/read | 0.55 (0.32–1.76) | removed |
| `refs_reading` | 55.03 (38.70–75.66) | 45.75 (37.42–51.37) |
| gix open only | 0.78 (0.28–1.15) | 0.47 (0.30–1.72) |
| gix open and enumerate, no peeling | 3.53 (2.53–5.68) | 5.51 (2.02–11.28) |
| Raw `for-each-ref`, with peeled fields | 46.65 (12.43–51.96) | 41.61 (11.95–49.68) |

Whole-path run means, in chronological pair order, were baseline
`294.82, 297.26, 283.74, 110.88, 88.41` ms and optimized
`294.14, 279.22, 226.21, 85.08, 285.44` ms. Their means were 215.02 and
234.02 ms: choosing a mean versus a median changes the apparent direction.
There is **no demonstrated whole-sweep speedup**. The supported benefit is
one fewer blocking task and repository open per sweep, plus preserving the
branch name from the refs pass the sweep already performs. The unchanged
ref walk's timing difference is noise, not a ref-walk optimization.

At the existing 10x duty-floor rule, these samples imply baseline
0.884–2.973 seconds and optimized 0.851–2.941 seconds. Both straddle the
two-second base interval, so they do not justify revising #556's cadence
claim or closing #661. Logs are `/tmp/gv-687-verified-{baseline,optimized}-N.log`
for N=1..5; order and load are in `/tmp/gv-687-verified-runs.log`.

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

The enumeration probe is much cheaper than the full reader in both sets of
samples. Together with the source inspection, that points to resolving and
reading objects as the larger follow-up area. This change keeps the full
walk and peeling semantics. No incremental ref cache, raw-ID substitution,
or packed-buffer optimization is claimed safe or shipped here. The raw-git
ratio also varied too much to repeat #661's earlier 4–8x claim from this run.

## Correctness checkpoint

The real-repository test creates both branches before the first reading,
requires readable/stable input, and verifies that switching branches at the
same tip leaves the ref map unchanged while changing the generation. A
separate pure-fold test holds the tip and every other input constant. The
feed/operation comparison covers attached, detached, unborn, tag-symbolic,
and linked-worktree HEAD states.

Fresh failure-atlas proofs on implementation commit `5685728f` both had a
green unmutated baseline and a red mutated test:

- **378, caught:** replace the feed's `observed.head_branch =
  parts.head_branch.clone()` with `None`; the feed/operation token equality
  fails in `live_reading_matches_operation_generation_across_head_states`.
- **379, caught:** replace the generation fold's symbolic branch field with
  an empty string; `fold_generation_keeps_symbolic_head_distinct_from_tip`
  finds equal tokens for two different branches at the same tip.

The connected failure-atlas registration was checked first; calls used its
FastMCP in-process client. Both commands used `cargo test -p git-vista-server`
with the exact test name, so integration targets built `gv-sandbox` too.
Earlier records 371/375 were superseded. Record 372 exposed a vacuous
token-change assertion; records 376/377 rejected a production build that
used a dev-only gix dependency. Both defects were fixed before 378/379.

Full all-targets and gate results will be recorded in the finish report.

Signed: codex
