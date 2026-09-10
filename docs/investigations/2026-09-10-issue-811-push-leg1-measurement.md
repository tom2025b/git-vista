# #811 Leg 1 measurement: the push is queued behind `lock(None)`

Date: 2026-09-10

Base: `bbd47193cd60d5c373b80dc30b624362def8d5e7` (`origin/main`)

Lane: `r5-e2`

Investigator: `codex`

## Result

**OBSERVED.** In an unmodified run of the requested combination — the
`planner::` and `sandbox::escape_suite` filters, 20 libtest threads, and 40
`stress-ng` CPU workers — the named push test failed at Leg 1. The run reported
381 passed, 1 failed, 1 ignored, and 1006 filtered out in 236.25 seconds.

The same combination was then run from an instrumented scratch copy outside the
repository. For the failing test's repository, the monotonic trace was:

| Event | `CLOCK_MONOTONIC` (s) | Delta from previous |
|---|---:|---:|
| test driver task spawned | 21305.332234305 | — |
| entered the call to `coordinator::lock(None)` | 21306.107281634 | 0.775047329 s |
| Leg 1 assertion, condition false | 21325.347453329 | 19.240171695 s |
| returned from `coordinator::lock(None)` | **not observed** | — |
| spawned the `git push` child | **not observed** | — |
| `reference-transaction committed` hook entry | **not observed** | — |
| first tracking-ref observation by `within()` | **not observed** | — |

The instrumented run reported 380 passed, 2 failed, 1 ignored, and 1006
filtered out in 239.22 seconds. The second failure was the unrelated
`create_tag_signing_fails_fast_with_a_typed_reason_and_touches_nothing` timing
bound; it is recorded below and was not changed.

**REASONED FROM THOSE OBSERVATIONS.** The roughly 20-second Leg 1 budget was
spent as 0.775 seconds on the pre-lock path (task scheduling plus plan building)
and then at least 19.240 seconds inside the unbounded
`coordinator::lock(None)` await. The await never returned before
Leg 1 asserted, so there was no push process whose startup, hook, or tracking-ref
update could consume the time. The lock call accounts for 96.1% of the measured
driver-to-assert interval; the pre-lock path accounts for the other 3.9%.

This proves the previously correlated lead for this reproduced failure: the
test times out while queued on the shared `None` coordinator key. It does not
fail in a gap between Leg 1 and Leg 2, and it does not reach the sandbox reaper.

## Current-source verification

**OBSERVED.** I read issue #811 in full with:

```text
gh issue view 811 --json number,title,state,body,comments,url,createdAt,updatedAt
```

GitHub reported the issue `OPEN`, with no comments. I then opened each live
source site instead of carrying over the issue's line references:

- `HANG` is still 20 seconds at
  `crates/git-vista-server/src/planner/push_suite.rs:1086`; `PROMPT` is still 3
  seconds at line 1101.
- The test begins at `push_suite.rs:1437`. Leg 1 is now lines 1454-1460, and its
  failure assertion is line 1455. The issue's headline reference to line 1437
  now identifies the test declaration, not the assertion.
- `run_tracked` calls `pipeline`, and `pipeline` passes `None` to
  `plan_and_execute_in` (`push_suite.rs:148-177`).
- `plan_and_execute_within` builds the plan before awaiting
  `crate::coordinator::lock(repo_id)` (`planner.rs:593-610`).
- `coordinator::lock(None)` maps to the process-global `Key::Unregistered` and
  awaits `guard.lock_owned()` with no timeout (`coordinator.rs:40-81`).
- The `git push` child is downstream of that await: execution reaches
  `exec_push`, then `git_streamed_for`, whose child spawn is at
  `git_cmd.rs:652-660`.
- The local `reference-transaction` hook sleeps for `HANG` only after its
  `committed` entry (`push_suite.rs:1388-1406`). It cannot run before the push is
  spawned.

**REASONED.** Those live sites predict the ordering the trace showed:
plan-build, coordinator await, push spawn, hook entry, tracking-ref observation,
Leg 1. They also explain why all tests using `repo_id = None` share one FIFO
mutex even though their temporary repositories are unrelated.

## Measurement method

**OBSERVED.** The working tree stayed untouched while measuring. I copied the
39 MiB source tree, excluding `.git` and `target`, to
`/tmp/gv-811-instrumented` and added timestamp-only instrumentation there.
`git status --short --branch` in the real worktree remained:

```text
## docs/811-push-leg1-measurement...origin/main
```

The scratch instrumentation did four things:

1. Appended `CLOCK_MONOTONIC` timestamps immediately before and after the
   `coordinator::lock(repo_id).await` call, labeling `None` and `Some` keys
   separately.
2. Appended timestamps before the push spawn and immediately after `spawn()`
   returned, including the direct child PID.
3. Made the installed `reference-transaction` hook's first `committed` action
   append `Time::HiRes::clock_gettime(CLOCK_MONOTONIC)` to a repository-local
   trace. The test copied that line to the run trace before the unchanged Leg 1
   assertion fired.
4. Recorded the first successful tracking-ref poll and the Leg 1 condition. A
   gated scratch-only stamp at the first instruction of `gv-sandbox-reaper`
   recorded reaper startup by PID.

The test's 20-second value, condition, and assertion message were unchanged.
The local hook still slept for 20 seconds. No scratch instrumentation was copied
back into `crates/**`.

**REASONED.** The unmodified run failing first rules out the scratch writes as
the source of the Leg 1 failure. All events use the same Linux monotonic clock;
the timestamps can therefore be subtracted within a run without wall-clock
adjustment. The trace writes add small overhead, but cannot manufacture a
19.240-second interval inside an await that never returned.

## Commands and real counts

### Unmodified reproduction

**OBSERVED.** This is the substantive command (the surrounding shell retained
the exact stressor PID and stopped it after Cargo returned):

```bash
stress-ng --cpu 40 --cpu-method loop --timeout 120s --metrics-brief &
GV_BUILD_SLOTS=2 \
CARGO_TARGET_DIR=/home/tom/.cargo-targets/gv-811 \
buildlock cargo test -p git-vista-server -- \
  planner:: sandbox::escape_suite --test-threads=20
```

```text
test planner::push_suite::a_cancel_that_lands_after_the_ref_moved_reports_what_the_remote_accepted ... FAILED
test result: FAILED. 381 passed; 1 failed; 1 ignored; 0 measured; 1006 filtered out; finished in 236.25s
```

The 120-second stressor lifetime covered the named failure, which appeared
before the suite's later tail completed.

### Instrumented reproduction

**OBSERVED.** From `/tmp/gv-811-instrumented`, with the same Cargo target and
filters:

```bash
stress-ng --cpu 40 --cpu-method loop --timeout 300s --metrics-brief &
GV_811_TRACE=/tmp/gv811-contended.trace \
GV_811_REAPER_TRACE=/tmp/gv811-contended-reaper.trace \
GV_BUILD_SLOTS=2 \
CARGO_TARGET_DIR=/home/tom/.cargo-targets/gv-811 \
buildlock cargo test -p git-vista-server -- \
  planner:: sandbox::escape_suite --test-threads=20
```

```text
test result: FAILED. 380 passed; 2 failed; 1 ignored; 0 measured; 1006 filtered out; finished in 239.22s
```

`stress-ng` reported 40 CPU workers dispatched and 40 passed, running for
240.30 seconds before the wrapper stopped them with the completed test run.

### Loaded isolated control

**OBSERVED.** The same instrumented binary and 40 CPU workers, but only the
named test, reported 1 passed, 0 failed, 0 ignored, and 1388 filtered out in
2.17 seconds for the server test binary. Cargo also invoked its other test
targets; each correctly reported 0 selected tests under the exact filter.

```text
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1388 filtered out; finished in 2.17s
```

Its complete event sequence was:

| Event | `CLOCK_MONOTONIC` (s) | Relevant delta |
|---|---:|---:|
| test driver task spawned | 21440.525140388 | — |
| enter `coordinator::lock(None)` | 21441.368376345 | 0.843235957 s after driver spawn |
| exit `coordinator::lock(None)` | 21441.368434694 | **0.058349 ms in lock** |
| push spawn begins | 21442.009228766 | — |
| push spawn returns, PID 1994147 | 21442.010257656 | 1.028890 ms |
| reaper PID 1994147 enters `main` | 21442.010855521 | 1.626755 ms after pre-spawn stamp |
| hook `committed` entry | 21442.164922796 | 154.665140 ms after spawn returned |
| first tracking-ref observation | 21442.165742689 | 0.819893 ms after hook entry |
| Leg 1 assertion, condition true | 21442.165774105 | 0.031416 ms after observation |

### Same-combination scheduling control

**OBSERVED.** A later valid repeat of all 383 selected tests with 20 test
threads and 40 CPU workers passed: 382 passed, 0 failed, 1 ignored, and 1006
filtered out in 252.27 seconds. In that interleaving the target entered
`lock(None)` at 22047.841322574 and returned at 22047.841378724: 0.056150 ms.
It then reached the push, hook, ref observation, and Leg 1 normally.

**REASONED.** The same recipe can pass or fail according to where the target
lands in the shared `None` queue. Forty CPU workers alone do not create the
failure when the queue is empty; the failing measurement and this pass differ
at the coordinator await, before the push exists.

Two attempted confirmation invocations were rejected as evidence and
interrupted immediately because their stressor setup commands were malformed;
neither reached the target test, and no output from them is used above.

## Reaper check

**OBSERVED.** Git history shows the reaper implementation landed on 2026-09-08
(`254affff`, issue #728) and Network-tier wrapping followed the same evening
(`233e3ac0`, issue #757). GitHub reports #728 and #757 as resolved. The current
wrapper applies the reaper while constructing sandboxed commands
(`sandbox/spawn.rs:519-520, 592-608`).

For 19 push spawns in the failing instrumented suite, matching the push child
PID to the reaper's first instruction gave this pre-spawn-to-reaper-entry
distribution under the 40-worker load:

```text
matched_push_spawns 19
enter_to_reaper_ms min=1.203 median=2.714 max=5.300
```

The loaded isolated target measured 1.627 ms. The failing target had no push
spawn PID and no reaper event because it never returned from the coordinator
await.

**REASONED.** The temporal proximity of the reaper changes and the first
reported failure is coincidence for the measured Leg 1 mechanism. Reaper entry
cost is milliseconds, and this failure occurs upstream of the reaper by
construction and by trace.

## Which machine ran the 2026-09-09 failures?

**OBSERVED.** PR #805's body records a local command using
`/home/tom/.cargo-targets/gv-799` and the Leg 1 failure after 1373 other server
tests passed. PR #812's body records two local full-gate attempts with the same
named failure and 1373 other server tests passing. Neither PR body, issue #811,
nor their comments records a hostname, runner name, machine ID, or concurrency
inventory.

The GitHub Actions rollups do identify hosted run and job IDs, but those are
green: PR #805's Core job succeeded in run 34354061785, and PR #812's Core job
succeeded in run 34370032933. They are not the failing local gate processes.

**REASONED.** The common `/home/tom` and Cargo-target naming convention is
compatible with one workstation, but it is not evidence that the two local
gates—or three lanes—ran concurrently on the same physical machine. The
historical box cannot be determined from the surviving GitHub record. That
hypothesis should remain unestablished.

## Deliberately left alone

**OBSERVED.** The instrumented contended run also timed out in
`create_tag_signing_fails_fast_with_a_typed_reason_and_touches_nothing`, whose
20-second outer budget says its internal bound is 10 seconds. The unmodified
reproduction did not fail there, and this lane did not investigate or alter it.

No production code, test, timeout, assertion, or hook behavior changed in the
real worktree. No pinning test was added, so there are no mutation results. The
only repository artifact from this lane is this investigation document. Issue
#811 remains OPEN.
