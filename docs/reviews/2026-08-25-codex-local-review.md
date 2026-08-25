<!--
Verbatim report from codex running LOCALLY on the operator's Landlock host,
2026-08-25, reviewing 25589084..ab51a3ce (the 25 August six-session batch).

Companion to docs/reviews/2026-08-25-codex-cloud-review.md, which reviewed the
same range from a cloud container and could therefore only REASON. This one
could RUN, and six of its seven findings are REPRODUCED against real git, the
real server, and real Chromium.

Brief it answers: design-docs/handoffs/2026-08-25-codex-outside-review-of-todays-batch.md
-->

# Outside review of the 25 August 2026 batch

**Reviewed target:** `main` at `ab51a3ce3e4c98af59162460af47152f2a2e7b15`  
**Reviewed range:** `25589084..ab51a3ce` — 60 files, +9,193/-1,171 under `crates/`  
**ADRs reviewed:** 0078, 0079, 0080, 0081, 0082  
**Mode:** report only; no source fixes, branches, commits, issues, or pull requests

## Verdict

The batch is not clean. I found seven concrete defects. Six were reproduced through the current binary or frontend with real repositories, persisted data, or exact HTTP responses. The seventh—the `AppliedWithConflicts` 200-response defect—was reproduced at the server/frontend contract boundary, but I could not make ordinary Git 2.43 produce that server verdict.

The highest-cost defect is not the journal-window concern suggested in the brief. That boundary is handled correctly. It is stash apply: a real `git stash apply --index` can modify a tracked file, fail while restoring an untracked file, leave no unmerged index entries, and make Git-Vista tell the user that the working tree was “left untouched.” That is an actively misleading data-state report.

| Rank | Severity | Status | Finding |
|---:|:---:|:---:|---|
| 1 | High | **REPRODUCED** | A partially applied stash can be reported as “left untouched.” |
| 2 | High | **REPRODUCED** | Removing `PopStash` silently strands persisted v2 operation rows. |
| 3 | Medium | **REPRODUCED** | Batch IDs can collide across restarts and bind a pointer to the wrong refs. |
| 4 | Medium | **REPRODUCED** | Removing `StashEntry.index` without a protocol bump breaks an already-valid v6 client. |
| 5 | Medium | **REPRODUCED** | An untracked-only stash cannot be inspected before the UI offers destructive Drop. |
| 6 | Medium | **REPRODUCED AT CONTRACT BOUNDARY** | A legitimate 200 `AppliedWithConflicts`/`Unverifiable` response is rendered as clean Apply success. |
| 7 | Low | **REPRODUCED** | A stale Push preview can produce a no-op while the UI claims changes were stashed. |

I also mutation-tested both retired `#[should_panic]` pins in `activity.rs`. One retired test is vacuous as a proof of the production writer, although a separate server test catches both mutations. The other catches removal but survives a plausible weakening; a synthetic companion catches that weakening. Those are test-integrity results, not additional product defects.

## Evidence standard and isolation

“REPRODUCED” below means I observed the claimed wrong state, response, or rendered UI—not merely that a unit test accepted a fabricated object. Temporary probes were added only inside an archive of the exact target, run, then removed. I subsequently created a fresh archive from `ab51a3ce` and compared it recursively against the exercised tree, excluding only generated build/browser directories. The comparison was byte-identical. The shared checkout was not used as the source under test because another session advanced it while this review was running.

The three required executions on the exact target all passed:

| Command | Result |
|---|---|
| `buildlock cargo test -q --workspace` | Pass; core 724 passed/2 ignored, protocol 153 passed, server binary 932 passed/4 ignored, all other targets green. |
| `buildlock ci/browser/run.sh` | Pass; 68/68 tests in real Chromium against the real server. |
| `buildlock cargo test -q -p git-vista-server --bin git-vista-server -- --test-threads=16` | Pass; 932 passed, 0 failed, 4 ignored. |

Every Cargo invocation, including mutation and focused-probe runs, used `buildlock`. Browser runs used the repository script, which holds the same build lock. PDF rendering uses the separate mandated render lock.

## Findings

### 1. High — a failed Apply can change files while the UI says “left untouched”

**Status: REPRODUCED with real Git, the current server, and Chromium.**

Concrete sequence:

1. Commit `tracked.txt` containing `base`.
2. Change it to `from-stash`, add untracked `collision.txt`, and create a stash with `--include-untracked`.
3. Create a different untracked `collision.txt` in the working tree.
4. Apply the stash through Git-Vista.

Git first wrote the stashed content into `tracked.txt`, then failed to restore the untracked `collision.txt` because that path already existed. It exited 1. There were no unmerged index entries. The stash remained. The final repository state was:

```text
 M tracked.txt
?? collision.txt

tracked.txt   = from-stash
collision.txt = already-here
stash@{0}     = still present
git ls-files -u = empty
```

Git-Vista returned HTTP 400 and the composed Pop UI displayed:

```text
Nothing was applied
Your working tree was left untouched.
```

That assertion is false: `tracked.txt` was changed by the failed operation.

The state transition is:

```text
Git writes tracked.txt
        |
Git fails restoring untracked collision.txt, exit 1
        |
server scans only `git ls-files -u`, finds no unmerged entries
        |
HTTP 400 Refused + Continuation::Clear
        |
client maps this to NotApplied / Untouched
        |
UI claims “left untouched” while disk is changed
```

The implementation explains the result. The conflict scan is exclusively `git ls-files -u -z`; an empty result is `Continuation::Clear` (`crates/git-vista-server/src/conflicts.rs:232-254`, `332-339`). A failed Apply plus Clear becomes `ApplyVerdict::Failed` (`crates/git-vista-server/src/planner/stash.rs:224-235`, `401-455`). The frontend discards the structured response body on any non-OK response (`crates/git-vista/src/api/stash.rs:196-205`). The Pop composition maps `Refused + Clear` to `NotApplied`, maps that to `Untouched`, then renders “Nothing was applied” and “left untouched” (`crates/git-vista/src/features/stash/signals.rs:42-50`; `crates/git-vista/src/features/stash/core.rs:627-636`, `693-707`, `721-729`, `823-826`).

The existing contract test is weaker than this input: it stashes a colliding untracked path but does not include a second tracked payload that Git can apply before the untracked restoration fails (`crates/git-vista-server/src/contract_suite.rs:697-765`). It therefore proves refusal and retention, not untouchedness.

**Required property:** a nonzero Apply result plus an empty unmerged-index scan cannot establish that the worktree is untouched. The server must either compare a pre/post state, retain enough Git output/state to report partial application, or describe the result as unknown/possibly changed. The client must not manufacture `Untouched` from `Refused + Clear`.

This also contradicts the intended safety semantics in ADR 0077/0078: retention of the stash is valuable, but it does not undo changes already applied to the worktree.

### 2. High — deleting `PopStash` silently strands persisted operation rows

**Status: REPRODUCED against the current durable store; historical reachability also executed at `25589084`.**

Concrete persisted input:

```json
{
  "op": "pop_stash",
  "repo": "/tmp/repo",
  "selector": "stash@{0}",
  "expected_oid": "0123456789012345678901234567890123456789"
}
```

I inserted that operation into a schema-v2 durable `operation_records` row with the state `running`/`executing`, then opened and recovered the store with the current binary. Current `GitOperation` deserialization rejected the removed tag. The row did not panic the server and was not converted to an explicit incompatibility record. Instead:

- single-record lookup returned `None`, which the handler maps to 404;
- recovery returned no record to fail or reconcile;
- history omitted the row;
- the database row remained indefinitely in its pre-upgrade running state;
- the only diagnostic was a generic log that the journal row did not decode and was skipped.

The durable schema is version 2 and stores `operation_json TEXT NOT NULL` (`crates/git-vista-server/src/durable.rs:56-73`, `171-200`). Writes serialize the current bare operation enum (`419-465`). Reads call current `GitOperation` deserialization and turn failure into `None` (`504-559`); list/history code skips such rows (`484-501`, `657-673`). Startup recovery only force-fails records that successfully decode (`838-857`). The recovery-center lookup then cannot distinguish “unknown operation from an older supported binary” from “no such record” (`crates/git-vista-server/src/recovery_center.rs:760-795`, `823-839`).

This is a compatibility break in persisted state, not merely dead source cleanup. The wire protocol is still exactly v6 (`crates/git-vista-protocol/src/version.rs:39-57`), and a `Plan` does not embed an independent serialization version (`crates/git-vista-protocol/src/plan.rs:1565-1604`). The filename `plan_v1.json` is not a runtime version gate.

ADR 0078's reachability premise is also too strong. At `25589084`, the server exposed generic authenticated `/api/plan` and `/api/execute-plan` routes. `/api/plan` accepted a bare `GitOperation`, and the enum/fixture included `PopStash` (`25589084:crates/git-vista-server/src/main.rs:618-630`; `25589084:crates/git-vista-server/src/handlers/plan.rs:44-109`; `25589084:crates/git-vista-protocol/tests/fixtures/plan_v1.json:202-210`). I ran that historical handler with the Pop JSON; it successfully produced a Pop plan. The first-party browser and MCP did not expose a dedicated Pop command, but an authenticated v6 HTTP client could create and execute it. “No client could reach it” and “nothing in the wild” are therefore not justified conclusions.

**Required property:** persisted enums need compatibility tombstones or an explicit durable migration. A protocol bump alone would not repair already-written rows. At minimum, unknown historical operations must remain visible, be marked terminal/incompatible during recovery, and never masquerade as a missing record.

### 3. Medium — restart-time batch-ID collision can resolve refs from the wrong event

**Status: REPRODUCED across two fresh server processes. The proposed 1,000-line boundary failure was tested and did not occur.**

`mint_batch_id` combines realtime nanoseconds with a process-local counter. If the system time is before the Unix epoch, `duration_since(UNIX_EPOCH)` falls back to zero; each new process starts its counter at zero (`crates/git-vista-server/src/journal.rs:136-150`). There is no boot nonce, process identity, or journal scan in the identifier.

I used an `LD_PRELOAD` clock shim that made `CLOCK_REALTIME` pre-epoch, then caused two fresh processes to append batches to the same real repository journal. Both minted `0-0`. The journal contained:

```text
process 1: pointer batch 0-0
process 1: anchor  batch 0-0, refs main = 4b3efb5c...
process 2: pointer batch 0-0
process 2: anchor  batch 0-0, refs main = 871ee66d...
```

The repository's current `main` was `871ee66d...`, but resolving the second pointer returned `4b3efb5c...`. Resolution searches for the first full capture with the matching ID (`crates/git-vista-core/src/activity.rs:271-285`), so a collision is a silent wrong-ref result rather than missing data or an error.

This condition is uncommon, but the cost is exactly the defect family #485/#486 set out to remove: a historical API/MCP event confidently carries another event's refs. It can occur after a restart when wall-clock behavior repeats or falls back—not only under the preload used to make it deterministic. Current frontend rendering does not display these maps, and undoability does not depend on them, which limits immediate UI impact; the API/MCP history remains wrong.

**Required property:** a batch identifier used as a persistent foreign key must be unique for the lifetime of the journal, not merely likely-unique within a process. A random/boot component, a persistent monotonic source, or a journal-local collision check would satisfy that stronger property.

#### Clean result: the 1,000-line tail boundary is handled correctly

I constructed 1,003 journal lines: a five-event captured batch followed by 998 filler events. The oldest pointer and its carrier both survived the 1,000-line tail. I then requested an output limit of one so the carrier was absent from the returned feed while the surviving pointer still needed resolution. It resolved correctly.

The reason is structural. The carrier is the last event in its batch, so any suffix containing an earlier pointer also contains the later carrier. `assemble_feed` folds and truncates the output but resolves against the original untruncated journal (`crates/git-vista-core/src/activity.rs:602-645`). The writer determines the final capture-needing event and emits the batch in one append (`crates/git-vista-server/src/journal.rs:201-278`). A targeted suffix-order test covers this property (`1692-1726`).

I also enumerated production consumers. `/api/activity` and `/api/undoables` both read the journal and pass it through `assemble_feed` (`crates/git-vista-server/src/activity.rs:137-148`, `260-268`). MCP `get_activity` calls `/api/activity` rather than reading the journal (`crates/git-vista-mcp/src/tools.rs:345-351`). I found no production route that serializes `RefsAtEvent::InBatch` directly. A missing or corrupt anchor resolves to no refs, not an unrelated map, unless an identifier collides (`crates/git-vista-core/src/activity.rs:245-248`, `271-288`).

ADR 0080 is therefore sound about suffix/truncation ordering, but its process-unique identifier claim is false for persistent pointers.

### 4. Medium — dropping `StashEntry.index` broke protocol v6 without moving the gate

**Status: REPRODUCED with the pre-#495 v6 WASM client against the exact current server.**

The current response omits `index` (`crates/git-vista-protocol/src/dto.rs:1272-1317`; `crates/git-vista-server/src/handlers/stash.rs:254-279`). The pre-#495 frontend's v6 `StashEntry` required `index: usize` and deserialized the response directly (`a21c64a6:crates/git-vista/src/features/stash/core.rs:63-77`; `a21c64a6:crates/git-vista/src/api/stash.rs:61-67`). Both sides advertise and accept protocol v6. The current middleware accepts any client inside that unchanged version window (`crates/git-vista-protocol/src/version.rs:39-57`; `crates/git-vista-server/src/middleware.rs:82-132`).

I served the old built WASM bundle against the current exact server and opened the stash drawer in Chromium. The request was not rejected as an incompatible client. Instead, client deserialization failed with `missing field 'index'`, and the drawer showed an error. A focused cross-version Serde probe produced the same failure.

This directly disagrees with ADR 0079's compatibility conclusion. “No client reads the field” confuses explicit application access with Serde's required wire shape. Removing a required response field is a breaking change even if new source derives the same value elsewhere. The protocol floor/ceiling needed to move, or the old field needed to remain during a compatibility interval. It did neither.

#### Clean result: absurd selectors do not panic or wrap

The requested input `stash@{999999999999999999999}` is accepted by the selector grammar but `.index()` returns `None`; its integer conversion does not wrap or panic. The grammar caps the full selector at 32 characters, and `.index()` uses checked parsing (`crates/git-vista-protocol/src/newtype.rs:252-298`; `crates/git-vista-protocol/src/plan.rs:328-346`). I found no production caller that treats `.index()` as total after #495.

There is a minor documentation error: the comment at `crates/git-vista-protocol/src/plan.rs:316-320` says a 20-digit `usize::MAX` is twelve characters short of the 32-character cap, but the complete `stash@{...}` selector is 28 characters, four short. This is not a product finding.

### 5. Medium — untracked-only stash contents cannot be inspected before Drop

**Status: REPRODUCED with a real untracked-only stash, current server, and Chromium.**

Concrete sequence:

1. Create untracked `only-untracked.txt` containing known text.
2. Stash with `git stash push --include-untracked`.
3. Open Git-Vista's stash drawer and select that entry.
4. Choose “Show changes,” then inspect the available Drop action.

`git show stash@{0}^3:only-untracked.txt` proved the file and its content were present in the stash. Git-Vista showed only the generic message “No tracked-file changes...” and did not show the path or content. Drop remained available, including its destructive confirmation path.

The server decides a stash is inspectable from the ordinary `git stash show --patch` output and never requests untracked content (`crates/git-vista-server/src/handlers/stash.rs:60-100`). The frontend correctly knows an empty patch may mean an untracked-only stash, but can offer only a generic warning (`crates/git-vista/src/features/stash/view.rs:362-380`). Drop remains enabled and routable (`crates/git-vista/src/features/stash/core.rs:381-407`; `crates/git-vista/src/features/stash/view.rs:488-521`).

Recovery remains possible until Git garbage-collects the dropped objects, but that is not a substitute for the promised pre-destructive inspection. The UI offers irreversible intent while withholding the only changed paths and contents. Either Show must include the stash's untracked parent, or Drop must make the inspection limitation explicit enough that the user can make an informed decision.

### 6. Medium — 200 NOT-complete Apply responses become clean-success UI

**Status: REPRODUCED at the exact HTTP/WASM contract boundary; an ordinary Git trigger was not reproduced.**

`render_apply` deliberately returns HTTP 200 for `AppliedWithConflicts`, with a response body saying the operation is not complete and providing conflict/continuation detail. It likewise has a 200 `Unverifiable` outcome (`crates/git-vista-server/src/planner/stash.rs:311-363`). That distinction is lost at the first-party client boundary: the API wrapper turns every `resp.ok()` into `Ok(())` and discards the body (`crates/git-vista/src/api/stash.rs:196-205`). The signal state then marks the operation complete, and standalone Apply renders the fixed success text “Applied the stash. It is still in your list.” (`crates/git-vista/src/features/stash/signals.rs:154-176`; `crates/git-vista/src/features/stash/view.rs:90-106`, `434-446`).

I injected byte-for-byte semantic equivalents of the server's documented 200 NOT-complete responses through the real route boundary and drove the current WASM UI in Chromium. Both were accepted as clean Apply success. No conflict detail, affected path, or unverifiable warning survived.

I also drove an ordinary real conflicting stash apply. Git 2.43 exited nonzero, the server returned 400, the frontend showed refusal details, and the stash remained. The focused server integration test for this path passed. I could not make ordinary Git 2.43 return success while leaving unmerged entries, which the implementation itself notes is not known to occur (`crates/git-vista-server/src/planner/stash.rs:168-190`). `Unverifiable` remains reachable if the post-operation scan fails, including unusual repository or process failures.

MCP does not currently expose Apply or Drop (`crates/git-vista-mcp/src/plan_tools.rs:103-112`), and I found no second first-party client. This is still a live contract defect: the server intentionally defines a non-complete 2xx response while its only client defines every 2xx response as complete. If those verdicts are retained, Apply must deserialize and render the response body.

### 7. Low — a stale Push preview can report a stash that was never created

**Status: REPRODUCED with the current server and Chromium.**

Concrete sequence:

1. Put one untracked file in a repository.
2. Open the stash drawer, enable “include untracked,” and observe Push enabled.
3. Remove that file outside the browser before clicking Push.
4. Click Push.

The server correctly returned 200 with “Nothing to stash,” and no stash was created (`crates/git-vista-server/src/planner/stash.rs:141-150`). The frontend discarded the body on success (`crates/git-vista/src/api/stash.rs:158-184`), marked the action complete (`crates/git-vista/src/features/stash/signals.rs:154-168`), and displayed “Stashed your working tree changes” (`crates/git-vista/src/features/stash/view.rs:224-241`).

This does not lose data, hence Low, but it is a false confirmation on a state-changing action. The success payload needs to distinguish “created stash” from “no-op,” or the UI must refresh and phrase the result truthfully.

## Retired-test mutation audit

The two retired `#[should_panic]` pins in `crates/git-vista-core/src/activity.rs` were each tested against two mutations, as requested. Every mutation was applied in the exact-target archive, run under `buildlock`, and reverted before the clean-tree comparison.

### Pin 1: one capture per operation batch

The retired core test constructs a pre-batched synthetic journal and checks resolution/folding (`crates/git-vista-core/src/activity.rs:1681-1764`). It cannot exercise the server writer that decides which event carries the one full map. The production writer lives in `crates/git-vista-server/src/handlers/mod.rs:112-158`; fetch constructs the per-ref events in `crates/git-vista-server/src/planner/fetch.rs:496-531`, with a server-level assertion at `717-788`.

| Mutation | Retired core test | Production server test | Verdict |
|---|---:|---:|---|
| Remove batching; write a full capture on every eligible event | **GREEN** | **RED**: 12 captures, expected 1 | Retired test is vacuous as writer proof. |
| Weaken batching to one full capture per pair | **GREEN** | **RED**: 6 captures, expected 1 | Same verdict. |

**Verdict:** the retired test is not independently honest evidence for the production mechanism. The combined suite is adequate because the server test fails for both removal and weakening. The core test now documents consumer behavior only; its own comment acknowledges that it cannot prove writer behavior.

### Pin 2: fold must exclude ref-less fetch lifecycle rows

The production predicate excludes fetch lifecycle entries with no ref name before aggregation (`crates/git-vista-core/src/activity.rs:673-700`, `775-783`). The retired regression test is at `1422-1477`; its synthetic companion is at `1549-1556` and nearby.

| Mutation | Retired test | Synthetic companion | Verdict |
|---|---:|---:|---|
| Remove the exclusion | **RED**: feed became `fetch — 5 refs updated` | Not needed | Honest against removal. |
| Weaken exclusion to only `ref_name.is_none()` | **GREEN** | **RED** | Retired test does not guard the full predicate. |

**Verdict:** the retirement is only partially independently justified. The retired test catches total removal but not a plausible weakening. The companion test catches the weakening, so the current combined suite remains protective. No current production writer constructs the synthetic mixed shape, which is why the second mutation does not establish a present product defect; it establishes that the retired test alone is narrower than the claimed invariant.

## Mandated clean checks

### #365 / ADR 0082 — Git 2.32 floor provisioning

**Status: CHECKED; no defect found.**

The workflow does not independently hardcode the supported floor. It parses the version from the explicit heading in `docs/SUPPORTED_VERSIONS.md`, reports the resolved value, and uses that value in the cache key (`docs/SUPPORTED_VERSIONS.md:7`; `.github/workflows/ci.yml:147-171`, `186-191`). The Rust fixture test independently parses the same document, verifies the invoked binary identity, and drives the fixture matrix (`crates/git-vista-fixtures/tests/status_floor.rs:141-164`, `299-380`).

On a cold cache, source acquisition/build is attempted three times with backoff. Exhaustion fails the required job explicitly; it does not silently downgrade to ambient Git (`.github/workflows/ci.yml:193-226`, `251-302`). On a cache hit, no source fetch is required. Therefore, if all configured source locations permanently disappear and the cache is cold, merges do become blocked. That is the deliberate fail-closed behavior ADR 0082 describes, not a hidden fallback bug.

I did not destroy or intercept the remote source to reproduce permanent disappearance, and I did not locally rebuild Git 2.32 during this review. The exact workspace run exercised the ambient fixture leg. Existing mandatory CI evidence covers the provisioned-floor leg. The remaining risk is an explicit availability tradeoff: strict compatibility evidence is coupled to source/cache availability. I do not disagree with that decision, provided maintainers accept that a real floor change must update the document and that source loss is a merge-blocking incident.

### Journal call paths and append concurrency

**Status: CHECKED; no additional defect found.**

The activity and undoables handlers both resolve pointers through `assemble_feed`; MCP goes through the activity HTTP endpoint. Batch construction accumulates lines and performs one append/write for the batch (`crates/git-vista-server/src/journal.rs:258-278`), so I found no interleaving point at which another writer can insert lines inside one encoded batch. Missing anchors degrade to absent refs rather than an exception. Only the reproduced cross-process ID collision made a pointer bind to an unrelated full capture.

### Remaining 60-file sweep

**Status: CHECKED; no further concrete defect established.**

I accounted for every changed file in `25589084..ab51a3ce`, including fixture consolidation/status-floor plumbing, shared DTO call sites, journal/activity folding, Pop removal in protocol/server/MCP/sandbox layers, stash planner/handler/client state, argv boundaries, recovery history, and the five ADRs. I specifically looked for lock/order races, silent `Result`/Serde loss, comments asserting stronger invariants than code enforces, and clients that bypass the new abstractions.

I found these non-finding discrepancies, recorded here so they are not mistaken for unreviewed areas:

- `crates/git-vista/src/api/stash.rs:187-193` says Apply is not operation-tracked and ends with a dangling “which is”; the current planner pipeline does track it durably. This is stale documentation, not the cause of a separate failure.
- `crates/git-vista/src/features/stash/signals.rs:27-35` says the Pop drop gate ignores the conflict scan after refused Apply, but the code uses that scan. The inaccurate comment obscures finding 1.
- `StashNotice::from_result` records a successful standalone Apply as `entry_retained: false` even though Apply retains the stash. The current standalone view ignores that field, so I found no user-visible consequence.
- One drawer busy-state comment describes per-selector state while the signal stores one selector. Server-side repository guards and expected-OID checks preserved data safety in the sequences I could drive; I did not establish a wrong destructive action.
- ADR 0080 and nearby handler comments still refer to a pin as expected after its retirement. The executable coverage is in the server test discussed above.
- A golden-plan test comment says 25 plans while the strict fixture currently contains 37. The test derives its actual count and coverage from the fixture, so this is stale prose only.

## Test provenance: which tests, on which host

I inspected the live PR descriptions and check rollups for #490, #491, #492, #498, #499, #500, #501, #502, and #503. All nine currently show seven successful CI checks. The author-session evidence was not uniform:

| PR | Author-session execution record |
|---:|---|
| #490 | Host tests were reported for core; cloud workspace could not run 320 Landlock-dependent server tests; browser listed but not run in cloud. |
| #491 | Cloud before/after workspace runs had the same 320 Landlock failures; browser not run there. |
| #492 | Browser not run in cloud; helper/stub and Node/import checks were used locally. |
| #498 | Cloud local run excluded the same 320 server tests; CI supplied the complete workspace result. |
| #499 | Cloud local run excluded the same 320; browser explicitly unrun there. |
| #500 | Cloud workspace was not green: 320 unavailable-tier failures plus four sandbox-sensitive failures on main; CI supplied the green gate. |
| #501 | Unit/protocol/MCP checks ran in cloud; two new server pipeline tests first ran in CI. Browser was not run there. |
| #502 | Server/browser were unrun locally and the local workspace had 320 failures; CI ran workspace and floor jobs. |
| #503 | Documentation-only session could not run the 320 Landlock-dependent tests; it cited separate host evidence for #438. |

This establishes a real provenance distinction: “CI passed” is not “the author ran the full suite on the cloud host.” Contrary to the review brief's blanket claim that the workarounds were silent, the current PR descriptions explicitly disclose most of these limitations. The durable exception is #502's immutable commit message (`7e1a71bb`), which claims a green 18-target, zero-failure workspace run based on grepping intermediate output even though the run ultimately had the known failures. The PR description was later corrected, but the commit-level statement remains false.

For this review, the host gap is closed at the exact aggregate target: workspace and dedicated server suites ran successfully on this Landlock-capable machine, and the full 68-test Chromium leg ran against its real server. Those green runs establish baseline health; the reproduced defects above demonstrate why baseline health does not establish correctness.

## Reproduction inventory

All temporary tests were removed after execution.

| Probe | Evidence |
|---|---|
| Partial stash apply with tracked payload plus untracked collision | Real Git state, current HTTP response, current Chromium UI; wrong untouched claim observed. |
| Legacy `PopStash` durable row | Real schema-v2 SQLite row; current lookup/recovery/history silently omitted it and left it running. |
| Historical Pop reachability | Exact `25589084` handler accepted the operation and built a plan. |
| 1,003-line journal boundary | Pointer resolved correctly after feed truncation; no defect. |
| Restart batch collision | Two processes minted `0-0`; second pointer resolved to first process's refs. |
| Old-v6/current-v6 stash response | Old WASM plus current server failed on missing `index` without version rejection. |
| Absurd selector | Parsed safely; `.index()` returned `None`; no defect. |
| Untracked-only inspection | Real stash plus Chromium omitted stored path/content while offering Drop. |
| Apply 200 NOT-complete | Current WASM rendered injected server-defined response as clean success. |
| Ordinary conflicted Apply | Real Git/server returned 400 with visible detail and retained stash; no defect on that ordinary path. |
| Stale Push preview | Real current server no-op plus Chromium false “Stashed” confirmation. |
| Four mutation runs | Results recorded in the mutation tables above. |

## Limits

- I did not reproduce a natural Git 2.43 `AppliedWithConflicts` success exit. Finding 6 is proven at the exact contract boundary, not with that ordinary Git trigger.
- I did not simulate permanent loss of the Git 2.32 source and a cold CI cache. I verified the fail-hard control flow and report that outage behavior as the intended design tradeoff.
- I tested the exact aggregate merge target, not every intermediate PR merge head. The brief says those intermediate compile/browser acceptance runs were already performed; repeating them would not add correctness evidence.
- I did not write fixes, modify production/tests, branch, commit, file issues, or open pull requests.

**Signed:** codex · 2026-08-25T13:23:35-04:00
