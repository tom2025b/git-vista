<!--
Verbatim report from codex cloud, 2026-08-25, reviewing the range
25589084..ab51a3ce (the 25 August six-session batch).

Restored from the downloaded copy: that download escaped markdown punctuation
and HTML-encoded parentheses. Only that encoding was undone. No wording,
finding, severity or signature has been altered.

Brief it answers: design-docs/handoffs/2026-08-25-codex-outside-review-of-todays-batch.md
-->

# Git-Vista adversarial review

**Repository:** `tom2025b/git-vista`
**Base:** `2558908487c36eef7d3275e6fc12e8df8a2223ef`
**Head:** `ab51a3ce3e4c98af59162460af47152f2a2e7b15`
**Reviewed range:** `25589084..ab51a3ce`
**Crate delta:** 60 files, `+9,193/-1,171`
**Method:** read-only static review

## Executive result

I found three high-severity defects, five medium-severity defects, and four low-severity defects. The most consequential are not failures of the new validation code. They are failures at seams the acceptance work did not model:

1. The stash wire contract changed incompatibly while both sides still advertise protocol version 6. The protocol mechanism expressly exists to stop the cached-PWA/restarted-server combination that this change now accepts.
2. The composed Pop is not an operation. It is three independently locked requests, so a mutation between the conflict scan and Drop can remove the applied work while the Drop still succeeds.
3. A lost HTTP response is converted into a definite semantic outcome. A completed Apply can be reported as “nothing was applied,” and a completed Drop can be reported as “the entry is still in the list.”

The persistence answer is deterministic from the code: a stored `GitOperation::PopStash` does fail enum deserialization, but that error is converted to `None`; the row is skipped with a generic stderr line. It does not panic. The payload is not versioned. Because the undecodable row retains a unique idempotency key in SQLite but is absent from the in-memory registry, reuse of that key can execute a fresh Git operation whose journal writes fail.

Every finding below is marked **REASONED**. In this report, that means derived from the checked-out source and history, not reproduced by executing the project. There are no **VERIFIED** findings. That distinction is deliberate.

## Findings

### High — H1. An incompatible stash API ships as protocol v6, so negotiation accepts peers that cannot communicate

**Status:** REASONED

The version module says its purpose is to let a long-lived cached PWA and a freshly restarted server detect a request/response disagreement before the client misreads it. It says to bump the version when an older peer would misread a changed contract (`crates/git-vista-protocol/src/version.rs:3-7,21-24`). Nevertheless, `PROTOCOL_VERSION`, `MIN_CLIENT_PROTOCOL`, and `MAX_CLIENT_PROTOCOL` all remain 6 (`version.rs:45-57`) after two incompatible changes:

- `GET /api/stashes` removed the required `index` response field. The old frontend `StashEntry` had required `entry`, `index`, `oid`, `message`, and `time` fields; the head DTO has only `entry`, `oid`, `message`, and `time` (`crates/git-vista-protocol/src/dto.rs:1272-1317`).
- `POST /api/stash/branch` changed from the flat body `{name, entry, expected_oid}` to `{name, target: {entry, expected_oid}}`, and the new request types reject unknown fields (`dto.rs:1344-1351,1383-1398`). ADR 0079 itself records that body change.

The response DTO’s omission of `deny_unknown_fields` helps an old client tolerate a field *added* by a new server. It does nothing when a required old field is *removed*. Serde rejects the entire `Vec<StashEntry>` with “missing field `index`”; it does not deserialize the row with `index` “absent.” The same error applies in the other direction to required request fields.

**Concrete failing sequence**

1. A browser retains the pre-#495 PWA bundle. That client advertises protocol 6.
2. Git-Vista restarts at `ab51a3ce`. The server advertises and accepts protocol 6.
3. Negotiation succeeds.
4. The server returns a listing such as:
  ```json
  [{"entry":"stash@{0}","oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","message":"WIP","time":0}]
  ```
5. The old client rejects the whole response because `index` is missing. The drawer enters its read-failure path; it does not receive a compatible list.
6. If the old client sends `{"name":"rescue","entry":"stash@{0}","expected_oid":"aaaa…"}` to the new Branch endpoint, deserialization rejects it because `target` is missing and the old flat keys are unknown. A new cached client against an old server fails symmetrically because the old body requires flat `entry` and `expected_oid`.

This is precisely the stale-client/fresh-server case the v6 negotiation comments promise to stop. The current code instead gives it a green compatibility decision.

ADR 0079’s alternative analysis is incomplete because it considers duplicate Rust declarations but not already-deployed declarations. Shared DTOs prevent future source drift inside one revision; they do not make an incompatible revision compatible. Its stated failure premise is also wrong: a missing required field does not become an absent optional value and an empty drawer.

**Why high:** normal deployment plus an ordinary cached PWA is sufficient. All stash listing and Branch behavior can fail before any Git command, while the compatibility screen falsely says the peers agree.

---

### High — H2. Composed Pop has a scan-to-Drop TOCTOU window that can discard the stash after its applied changes were removed

**Status:** REASONED

`compose_pop` performs three network requests: Apply, GET conflict state, then Drop (`crates/git-vista/src/features/stash/signals.rs:42-64`). Each POST independently enters `plan_and_execute`, builds its own plan, acquires the repository coordinator, checks freshness, executes, and releases the coordinator (`crates/git-vista-server/src/planner.rs:337-399`). The GET is outside that lock. No lock or operation identity spans all three steps.

Drop protects the identity of the stash entry by re-resolving `(selector, expected_oid)` before mutation, which is good. But the Drop plan has no precondition connecting the working tree to the state observed after Apply (`planner.rs:1672-1691`). The selector/OID compare-and-swap proves only that `stash@{N}` still names the same stash commit. It does not prove the tree still contains the changes that justified deleting it.

**Concrete failing sequence**

1. `stash@{0}` contains a tracked edit to `a.txt`; the worktree is initially clean.
2. The Pop client’s Apply request succeeds. The server releases the repository lock.
3. `GET /api/conflicts` returns `Clear`.
4. Before the Drop request *builds its plan*, another session discards the changes to `a.txt`, or a terminal runs `git reset --hard HEAD`. The stash reflog is unchanged.
5. The Drop request now builds against this new, current state. Its generation is fresh, so `enforce_fresh` correctly accepts it. `stash@{0}` still resolves to the expected OID, so the executor compare-and-swap also accepts it.
6. Drop removes the stash. The client reports `Popped` even though `a.txt` contains none of the stash’s changes.

The timing in step 4 matters. If the interfering mutation happened after the Drop plan was built, the generation gate could reject it. If it happens before that plan is built—as the three-request design permits—the new state is treated as the valid starting state.

ADR 0078 correctly rejects the old one-row direct `git stash pop` executor because one terminal row could not represent partial outcomes. It then treats client composition as if separating the rows solved the truth problem. It solves record vocabulary but not atomicity or causality. The ADR’s alternatives are a false choice between the deleted direct executor and client orchestration. A server endpoint could orchestrate Apply and Drop as linked child records while retaining one coordinator guard, or the product could withhold Pop until that orchestration exists. ADR 0078 actually names linked durable child records as a prerequisite, then dismisses it as a later milestone while retaining the unsafe user action.

**Why high:** the sequence ends with the only durable copy of the selected stash removed and the expected applied content absent from the tree, while the UI claims completion. Recovery pins may make recovery possible, but they do not make “Popped” true.

---

### High — H3. Lost responses are treated as definitive refusals, making the UI assert false repository state

**Status:** REASONED

The HTTP timeout code expressly says dropping the fetch future does not abort the browser request and the server may still complete it (`crates/git-vista/src/api.rs:105-117`). `send_write_with_key` retries once under the same key, then returns only `Err(String)` if the second attempt also fails (`api.rs:244-327`). The key and possible operation identity are not returned on this error path.

The stash layer then collapses transport uncertainty into operation refusal:

- Apply maps any network/timeout error to `ApplyOutcome::Refused` (`features/stash/signals.rs:42-46`).
- `Refused + ConflictScan::Clear` becomes `PopVerdict::NotApplied`, whose structural tree state is `Untouched` and whose text says the working tree was left untouched (`features/stash/core.rs:620-632,650-657,693-706`).
- Drop maps any network/timeout error to `DropOutcome::Refused`, which becomes `AppliedNotDropped`; `entry_retained()` then says the stash is still present (`signals.rs:53-62`; `core.rs:679-682,710-714`).

All stash POSTs do, in fact, enter the tracked planner (`crates/git-vista-server/src/handlers/stash.rs:379-451`; `planner.rs:210-263`). The detached server operation survives client disconnect and is specifically designed to make its result discoverable. `operations::lookup_by_key` exists for reconciliation (`crates/git-vista-server/src/operations.rs:456-481`), but these callers discard `_key` and never query it.

**Concrete failing Apply sequence**

1. The first Apply POST reaches the server and applies the stash cleanly.
2. Its response is lost or the client timeout wins. The browser-side future is dropped; the detached server operation continues.
3. The retry uses the same key and replays/awaits the same operation, but that response is also lost or times out.
4. `apply_stash_request` returns `Err`, so `compose_pop` records `ApplyOutcome::Refused`.
5. Connectivity recovers for `GET /api/conflicts`; it returns `Clear` because the apply was clean.
6. `drop_gate` returns `NotApplied`. The UI says nothing was applied and the tree is untouched, while the applied changes are present.

**Concrete failing Drop sequence**

1. Apply and the conflict scan succeed.
2. Drop reaches the server, removes the stash, and its response is lost twice.
3. The client maps the transport error to `AppliedNotDropped` and asserts the entry remains in the list. A subsequent reload proves the opposite.

Branch and Push have the same transport ambiguity: a Branch request can have created and checked out a branch and removed the stash before the client reports an error; Push can have created a stash before reporting failure.

Idempotency prevents double execution. It does not tell the caller which execution result occurred after both response paths are lost. The code has the reconciliation primitive but throws away the information needed to use it.

**Why high:** ordinary network loss causes the product to give safety-relevant, structurally encoded false facts. In Pop, those facts decide whether the destructive half is sent.

---

### Medium — M1. Removing `PopStash` makes persisted rows disappear and creates an idempotency hole on key reuse

**Status:** REASONED

This is the requested persistence trace, reasoned end to end rather than run.

`GitOperation` is an internally tagged, closed enum with no catch-all (`crates/git-vista-protocol/src/plan.rs:526-536`). Its own test proves unknown `op` names fail deserialization (`plan.rs:1844-1849`). SQLite stores the bare enum JSON in `operation_json` (`crates/git-vista-server/src/durable.rs:179-195,419-467`), using:

```rust
serde_json::to_string(&status.operation)
```

There is no payload envelope or enum-format version. `PRAGMA user_version = 2` versions the SQLite column schema, not `operation_json`’s vocabulary.

On load, `row_to_status` executes:

```rust
operation: serde_json::from_str::<GitOperation>(&operation_json).ok()?,
```

(`durable.rs:504-559`). Therefore an old `{"op":"pop_stash",...}` record produces a Serde error, `.ok()` discards the error, `?` returns `None`, and the caller skips the row. A generic line is written to stderr—“journal row for an operation id didn’t decode; skipped”—but the error and operation ID are not surfaced. `load_all_blocking` omits it from startup recovery (`durable.rs:484-501`); paged history counts the row for cursor purposes but returns no status (`durable.rs:653-709`); direct lookup treats undecodable and nonexistent identically and produces 404 behavior (`durable.rs:760-795`). It is a **deserialize-error converted to skip-with-log**, not a panic.

`crates/git-vista-protocol/tests/fixtures/plan_v1.json` is not a versioned wire envelope. “v1” exists only in the fixture filename; the serialized plans contain no format-version field. The Pop entry was deleted from that golden file in place. HTTP protocol version also stayed 6.

**Concrete failing stored input**

An operations row contains:

```json
{"op":"pop_stash","entry":"stash@{0}","expected_oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
```

with operation ID `O` and idempotency key `K`.

1. Start the head binary. The row fails `GitOperation` deserialization and is skipped. If it was left `Running`, startup recovery never terminalizes it. Recovery Centre history omits it, and lookup of `O` acts like it never existed.
2. The SQLite row remains, including its `idempotency_key TEXT NOT NULL UNIQUE` value (`durable.rs:179-185`). But `operations::rehydrate` never receives it, so the in-memory registry has no `K` (`operations.rs:484-509`).
3. Submit any currently valid operation under `K`. `operations::admit` sees no in-memory record, mints a fresh ID, and admits it (`operations.rs:391-448`).
4. The accepted-row persist attempts `INSERT ... ON CONFLICT(id) DO UPDATE` (`durable.rs:424-437`). The conflict is on the *other* unique constraint, `idempotency_key`, so the write fails. `persist` logs and swallows that failure (`durable.rs:387-403`).
5. The planner still executes Git (`planner.rs:284-326`). Its terminal persist fails the same way. After restart, only the old undecodable row remains, so `K` can admit and execute again.

ADR 0078 offers a material reachability caveat: no supported route ever issued a Pop plan, and plans are server-issued and hashed. I found no supported path that proves such rows exist in installations. That lowers this from High to Medium. It does not make the compatibility behavior sound: the enum is explicitly persisted vocabulary, its golden “v1” member was removed without a migration, and the generic loader’s skip policy breaks the durable idempotency guarantee for any unknown operation—not just Pop.

---

### Medium — M2. Branch-from-stash is destructive on the server and non-destructive in the UI

**Status:** REASONED

The frontend says `StashAction::Branch` is non-destructive, and its comment states the view scales confirmation ceremony on that classification (`crates/git-vista/src/features/stash/core.rs:347-355`). The server classifies the same `GitOperation::BranchFromStash` as `RiskLevel::Destructive` because the stash entry is removed (`crates/git-vista-server/src/planner.rs:1640-1670`). The server’s successful response explicitly says “The stash entry has been removed” (`planner/stash.rs:502-520`).

The view prompts only for a branch name, presents no destructive confirmation, and on success says only “Created the branch and applied the stash there” (`crates/git-vista/src/features/stash/view.rs:463-486`). The client API discards the server response body (`crates/git-vista/src/api/stash.rs:238-259`), so the one response that discloses removal is never shown.

**Concrete failing interaction**

1. A user clicks “Branch from stash” on their only stash.
2. They enter `rescue`; no confirmation says the entry will be consumed.
3. `git stash branch rescue stash@{0}` succeeds and drops the stash.
4. The UI reports only branch creation/application. On reload, the stash has disappeared.

The frontend’s rationale—Branch consumes only after a successful apply “where by construction it fits”—does not change whether the operation removes the user’s recovery object. It also does not match the codebase’s own definition of `RiskLevel`: recoverability and expected success do not make a deletion non-destructive.

---

### Medium — M3. ADR 0082’s required floor job compiles and executes an unverified mutable upstream ref

**Status:** REASONED

The CI job clones `https://github.com/git/git` at tag `v${floor}.0`, builds it, installs it, and executes it (`.github/workflows/ci.yml:186-226`). It verifies the printed version, but it does not pin a commit, verify a tag signature, or check a source archive/tree hash. The cache key includes only runner OS and the parsed floor number (`ci.yml:186-192`).

**Concrete failing sequence**

1. The cache is absent or expired.
2. The upstream `v2.32.0` ref resolves to different content because the tag was moved or the upstream/retrieval path was compromised.
3. CI compiles and executes that content inside the required merge job.
4. A malicious binary prints `git version 2.32.0`, passes the identity assertion, and is cached under the trusted `gv-git-floor-<os>-2.32` key.

ADR 0082 says source-building is the only option that follows the documentation heading automatically, and rejects a pinned image because it would put the version in a second place. That does not fairly represent the alternatives. A small reviewed mapping from supported version to Git commit/tree hash, or a checked source checksum alongside the parsed version, is intentional provenance—not accidental drift. The heading can remain the source of the *version* while CI separately proves it got the reviewed source for that version.

The cache key also omits architecture and runner/toolchain image. That is more likely to cause a diagnostic infrastructure failure than a false green, but it reinforces that the cache is treated as trusted without carrying enough identity.

The decision to exercise the floor is otherwise sound. The named expectations and shell-side anti-vacuity check are materially better than a skip-capable test. The defect is provisioning provenance, not the existence of the required leg.

---

### Medium — M4. `gv-fixture` accepts an arbitrary deletion root, and one shape deletes an undeclared sibling

**Status:** REASONED

The new CLI accepts any caller path and dispatches it directly to a fixture builder (`crates/git-vista-fixtures/src/bin/gv-fixture.rs:16-30`). Every builder starts through `fresh`, which recursively removes the supplied path when it exists (`crates/git-vista-fixtures/src/browser.rs:55-65`). There is no guard that the target is a temporary directory, empty staging directory, or recognizable prior fixture.

The `interleaved-wip` shape goes farther: it computes `root.parent()/twin-origin.git` and recursively deletes that sibling if present (`browser.rs:484-495`). The CLI documentation says the named directory is emptied; it does not disclose mutation outside that directory.

**Concrete failing invocations**

- From a parent directory, run `gv-fixture main /work/git-vista`. The tool recursively deletes the existing checkout at `/work/git-vista` before initializing a fixture there.
- Run `gv-fixture interleaved-wip /tmp/work/repo` while `/tmp/work/twin-origin.git` is an unrelated repository. The tool deletes the unrelated sibling even though the user named only `/tmp/work/repo`.

This is a developer/test utility rather than a network-exposed server path, which limits severity. But its interface makes destructive behavior easy to invoke accidentally, and the sibling deletion violates the command’s stated target boundary.

---

### Medium — M5. The batched journal is not backward-readable: rollback drops N−1 events from every batch

**Status:** REASONED

ADR 0080 introduced `RefsAtEvent::InBatch` into an append-only JSONL journal (`crates/git-vista-core/src/activity.rs:173-250`). The new binary remains backward-compatible with old `Captured` rows by making new capture fields optional. The reverse direction was not considered.

At the base revision, `RefsAtEvent` has only `Captured` and `CaptureFailed`; it has no catch-all variant. Its `read_all` parses each line as `ActivityEvent` and skips any line that fails Serde deserialization. A head-written anchor is still readable by the old binary because Serde ignores the new `batch` field on `Captured`. Every head-written `{"status":"in_batch",...}` referrer is an unknown enum variant and is skipped.

**Concrete failing sequence**

1. Head runs a fetch that moves 100 refs.
2. `append_all` writes 99 `InBatch` events and one final `Captured { batch: ... }` anchor (`crates/git-vista-server/src/journal.rs:193-278`).
3. The operator rolls back to the base binary while keeping the repository and its `.git/git-vista/journal.jsonl`.
4. The old reader logs 99 unreadable lines and retains only the anchor event. The activity feed represents a 100-ref action as one event and loses the other 99 until the binary is upgraded again.

The on-disk lines are not destroyed, so this is rollback-time incorrect rendering rather than permanent data loss. It is still a persisted-format break. ADR 0080 fairly rejects a side file, but it omits an important same-file alternative: a versioned batch-envelope record that old readers can skip as one unsupported record, or an explicitly versioned journal with a downgrade policy. A pointer variant embedded in each event maximizes the number of records an older reader rejects.

---

### Low — L1. Branch failure with a clear index is always mislabeled as conflicts

**Status:** REASONED

`exec_branch_from_stash` matches results in this order (`crates/git-vista-server/src/planner/stash.rs:502-554`):

1. clean success;
2. any conflict-scan error;
3. every remaining `Ok(Continuation)`.

The third arm does not distinguish `Continuation::Blocked` from `Continuation::Clear`. It logs and returns “left conflicts” even for the concrete tuple `(git success = false, conflict scan = Ok(Clear))`. Its `detail` is then empty.

ADR 0078 documents and fixes the identical wildcard bug in the deleted Pop executor. It gives a real failure shape: inability to restore an untracked stash file exits nonzero while `git ls-files -u` is empty. Branch retained the old match.

**Concrete failing sequence**

1. A stash includes untracked `u.txt`.
2. The current worktree also contains an untracked `u.txt`; Branch has only a `RefAbsent` precondition and does not require a clean worktree (`planner.rs:1640-1661`).
3. `git stash branch rescue stash@{0}` cannot restore the stashed untracked file and exits nonzero. The unmerged index is clear.
4. The executor takes `(_, Ok(Clear))` and reports HTTP conflict: “left conflicts,” with no conflicted paths.

I did not execute this Git shape here; its Branch-specific reachability is reasoned from the executor, its lack of a clean-tree precondition, and ADR 0078’s measured Apply/Pop failure shape. The pure result tuple is unambiguously misrendered even if a particular Git version makes the sequence harder to reach.

---

### Low — L2. “Per-selector” busy state is one overwriteable slot, so overlapping actions clear and relabel each other

**Status:** REASONED

`DrawerBusy` has only `Idle` or one `Working { selector, what }` value, yet its comment says the state is “held per-selector” (`crates/git-vista/src/features/stash/signals.rs:76-103`). `StashDrawer` owns a single `RwSignal<DrawerBusy>`; `begin` overwrites it and every `finish` unconditionally writes `Idle` (`signals.rs:105-115,223-232`). The view disables only the selector currently occupying that slot (`crates/git-vista/src/features/stash/view.rs:421-426`).

**Concrete failing sequence**

1. Start Apply on `stash@{0}`. Busy holds selector 0; other rows remain enabled.
2. Start Apply or Drop on `stash@{1}`. Busy is overwritten with selector 1; row 0 immediately re-enables while its request remains in flight.
3. Request 0 finishes first and calls `finish()`. Busy becomes `Idle` while request 1 is still in flight.
4. Every control is enabled. A second action on selector 1 gets a fresh idempotency key and enters the server. The repository generation gates will usually refuse one request, but the UI’s busy label and exclusion promise are false, and notices are overwritten in completion order rather than action order.

This is contained by the server’s coordinator, freshness checks, and stash identity CAS, so I do not classify it as a data-loss race. It is still a concrete concurrency defect in the changed UI state machine.

---

### Low — L3. ADR 0080 removed per-entry timestamp drift but did not bound post-fetch drift beyond the five-second attribution window

**Status:** REASONED

Fetch writes its reflogs during `git fetch`, then waits for Git to return, rereads all remote-tracking refs, computes the diff, and only then calls the journal path (`crates/git-vista-server/src/planner/fetch.rs:277-335`). `journal_app_events` takes the shared timestamp at that point (`crates/git-vista-server/src/handlers/mod.rs:139-158`). The fold treats a journal event and reflog event as the same only within five seconds (`crates/git-vista-core/src/activity.rs:468-472`).

Batching guarantees all journal entries share one timestamp. It does not guarantee that timestamp is close to Git’s reflog timestamps.

**Concrete failing input**

1. A repository has a very large remote-tracking ref namespace on slow or contended storage.
2. Fetch moves N refs and writes their reflog entries.
3. The post-fetch `remote_tracking_refs` observation takes more than five seconds before `journal_app_events` samples `now_secs()`.
4. None of the N journal rows attributes to its reflog counterpart. Both copies survive and the fold can report `2N` updates.

The changed comment at `activity.rs:762-769` says nothing can currently produce drift because Fetch and Pull are batched. That confuses within-batch skew with absolute skew. I did not measure a repository that crosses the five-second post-fetch boundary, so this remains Low and REASONED.

---

### Low — L4. The claimed cross-process-unique batch ID has no cross-process entropy

**Status:** REASONED

`mint_batch_id` is documented as unique across processes on one box (`crates/git-vista-server/src/journal.rs:136-142`). It is wall-clock nanoseconds plus a process-local counter initialized to zero (`journal.rs:143-150`). There is no PID, random component, host/process nonce, file lock, or check against existing IDs.

`refs_at` resolves a pointer with `journal.iter().find_map(...)`, selecting the first matching anchor (`crates/git-vista-core/src/activity.rs:271-287`).

**Concrete failing sequence**

1. Two Git-Vista processes journal their first multi-event batch for the same repository in the same clock tick. On a clock source whose effective resolution is coarser than a nanosecond, both observe the same `as_nanos()`; both counters are 0.
2. Both mint the same `<nanos>-0` ID but capture different ref maps.
3. When both batches are in the read window, referrers from the later batch resolve to the first anchor with that ID and display the wrong snapshot.

If `SystemTime` is before the Unix epoch, the code makes the clock half exactly zero, further weakening the claimed property. The collision window is narrow, hence Low; the absolute documentation claim is nevertheless not implemented.

## ADR review as decisions

### ADR 0078 — “There is one pop, it is composed”

**Decision judgment:** only half correct.

Deleting the unreachable direct `git stash pop` variant was locally correct. The implementation contradicted the specification, no route reached it, and its wildcard conflict rendering was wrong. Retaining that dead executor would have been misleading.

The ADR does not justify retaining Pop as client composition. Two rows make partial outcomes representable, but the three requests are neither atomic nor causally linked (H2), and transport ambiguity makes those rows lie to the client (H3). The alternative section understates the server-orchestration option that the ADR’s own context names. The correct product decision from its premises was either “build linked, guarded orchestration” or “do not offer Pop yet,” not “client composition is the one Pop.”

Its persistence conclusion—“nothing in the wild carries one”—may be true for supported producers, but it is not a migration. The code’s behavior for any existing row is skip-with-log, invisibility in recovery, and a durable-idempotency hole (M1). Deleting a golden-v1 member in place also contradicts the fixture’s role as a pinned vocabulary.

The new Apply decision table itself is careful. For an answered request, it distinguishes Git exit status from index conflict state, and it does not treat an unreadable scan as clear. I found no static error in that pure table.

### ADR 0079 — shared stash DTOs

**Decision judgment:** good intra-revision design, incorrect deployment decision.

Shared DTOs, `StashSelector`/OID newtypes, and strict write bodies are sound. The selector/OID pair is correctly re-resolved immediately before destructive execution and protects against reflog renumbering.

The ADR fails at versioning. It changes deployed response and request bodies without moving protocol v6 (H1). Its failure-mode argument is technically false: missing required Serde fields fail deserialization; they do not quietly become absent and render an empty list. The ADR treats “both ends compile together” as if both ends deploy together, despite this product’s explicit cached-PWA compatibility mechanism.

### ADR 0080 — one capture per journal batch

**Decision judgment:** performance goal correct; persisted representation under-argued.

Eliminating N full ref captures for N moved refs is the right performance decision. Within the current single-writer/current-reader case, placing the anchor last is correct for a tail-capped window, and an unresolved pointer degrades to no information rather than an empty map.

The chosen pointer format lacks:

- a journal format version or downgrade story (M5);
- an actually cross-process-unique ID (L4);
- referential integrity beyond a linear “first matching ID” scan;
- a timestamp guarantee relative to the reflog, as opposed to only equality within the batch (L3).

The alternatives discuss a side file and duplicated snapshots, but omit a versioned batch envelope in the same JSONL stream. That option preserves one capture per operation without inserting a new enum variant into N−1 ordinary event records.

### ADR 0081 — exclude the unreadable-ref admission from folding

**Decision judgment:** correct for the current closed producer set.

The ADR inventories every current producer and uses a three-field discriminator only inside `Fetch | Pull`. The synthetic tests cover the two obvious weakenings. I found no current producer that shares the all-`None` Fetch/Pull shape, and no static misclassification in the partition.

The discriminator is semantic overloading rather than an explicit record kind, so it is future-fragile, but that is not a present defect with a concrete failing input. I do not report it as one.

### ADR 0082 — exercise the Git floor in mandatory CI

**Decision judgment:** testing policy correct; provisioning decision unsafe and alternatives unfairly narrowed.

Actually running the supported floor, comparing both versions with named expected outputs, and requiring shell proof that the floor leg ran are good decisions. I found no static vacuity in the report checks described by the ADR.

The source-build decision treats automatic version selection as more important than source provenance. A version string and a reviewed commit/checksum are separate facts; recording both is not harmful duplication. The implemented job trusts and executes whatever a mutable upstream ref supplies on a cache miss (M3).

The battery creates fixtures with the ambient current Git and reads them with both binaries. For the stated porcelain-parser question, that is a reasonable compatibility test; I did not find a concrete parser contract it misses in this range.

## Comment claims without enforcement

I searched changed Rust comments for absolute claims (`always`, `never`, `exactly`, `only`, `unique`, `must`, `by construction`, and `guarantee`) and followed the material ones into their code and tests. The table groups duplicate statements. “False now” means the checked-in code directly contradicts the comment. “Unpinned” means the fixture/code currently appears to create the property but the named test does not assert it.

|Location                                          |Asserted property                                                                       |What actually enforces or contradicts it                                                                                                                      |Concrete consequence                                                                                              |State                      |
|--------------------------------------------------|----------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------|---------------------------|
|`protocol/src/dto.rs:1262-1270`; ADR 0079:17-25   |A renamed/missing listing field deserializes as absent and the drawer renders empty     |Required fields have no defaults; Serde fails the whole `StashEntry`/`Vec`                                                                                    |Removing `index` breaks an old client instead of yielding a compatible empty row/list                             |False now; H1              |
|`frontend/api/stash.rs:187-193`                   |Apply is not operation-tracked                                                          |The handler calls the same tracked planner as every stash write                                                                                               |Maintainers are told reconciliation is unavailable even though `lookup_by_key` exists; the client discards the key|False now; H3              |
|`stash/signals.rs:27-35`                          |`drop_gate` ignores the conflict scan after a refused Apply                             |`Refused + Blocked` becomes `Conflicted`; `Refused + Clear` becomes `NotApplied`; scan failure becomes `RefusedUnverified`                                    |The stated reason for always issuing the GET is based on behavior the gate does not have                          |False now                  |
|`stash/core.rs:347-355`; DTO/API Branch docs      |Branch is non-destructive and the stash fits “by construction”                          |Server risk is `Destructive`; success drops the entry; dirty/untracked state can still make restoration fail                                                  |No deletion confirmation; failure can be mislabeled as conflict                                                   |False now; M2/L1           |
|`stash/core.rs:620-632,693-714`                   |`NotApplied` proves an untouched tree and stash retention is knowable in every case     |A lost Apply/Drop response is encoded as refusal                                                                                                              |UI can assert “untouched” after Apply completed, or “entry retained” after Drop completed                         |False now; H3              |
|`stash/signals.rs:76-103`; `stash/view.rs:421-426`|Busy state is held per selector                                                         |One slot is overwritten by the latest selector and any completion clears it                                                                                   |Concurrent rows re-enable and relabel one another                                                                 |False now; L2              |
|`stash/signals.rs:118-176`                        |`StashNotice.entry_retained` structurally states whether the entry remains              |`from_result(Ok)` always writes `false`, including Apply (entry retained) and Push (a new entry exists); the view destructures the field as `_`               |Any renderer that starts honoring the promised structural field immediately reports wrong state                   |False now, currently latent|
|`stash/core.rs:432-433`                           |`git stash push` on a clean tree exits nonzero with “No local changes to save”          |Server code correctly documents and handles Git exiting 0 with that stdout (`server/planner/stash.rs:141-145`)                                                |The client rationale records the opposite Git contract; a future simplification based on status would be wrong    |False now                  |
|`core/activity.rs:762-769`                        |Batching means nothing can currently drift beyond attribution slack                     |Timestamp is sampled only after post-fetch ref enumeration                                                                                                    |A >5-second observation delay duplicates journal and reflog events                                                |False now; L3              |
|`server/journal.rs:136-150`                       |Batch IDs are unique across processes                                                   |Wall time plus a process-local zero-based counter has no process identity                                                                                     |Same-tick first batches can resolve against the wrong anchor                                                      |False now; L4              |
|`fixtures/browser.rs:107-128`                     |`main` has five commits; `compare-mode.txt` contains `one/two/three/four` at four layers|The builder creates 7 commits (seed + 3 WIP + commits 2–4). `compare-mode.txt` contains only `one/two`; `three` is in `staged.txt`, `four` in `multi-hunk.txt`|A compare-mode bug can read the wrong file/layer while the fixture test still passes                              |False now                  |
|`fixtures/browser.rs:758-776`                     |The test proves `compare-mode.txt` differs at all four layers                           |Its last two assertions read different files, not `compare-mode.txt`                                                                                          |The test name and comment claim a sentinel contract it never checks                                               |False now                  |
|`fixtures/browser.rs:475-478,847-876`             |Two checkpoint chains alternate in display order, and the test pins every adjacent pair |The test only counts that each duplicated subject appears twice; it never checks adjacency or chain identity                                                  |A timestamp/order change can produce two separated blocks while the “alternate” test remains green                |Unpinned                   |
|`fixtures/browser.rs:746-755`                     |The WIP checkpoints “must be a run”                                                     |The test counts matching subjects but never asserts contiguity                                                                                                |Inserting a non-WIP commit inside the checkpoint range leaves the test green                                      |Unpinned                   |

I did not include comments whose property is made structurally true by a type or directly asserted by a test merely because they use absolute language. Examples that survived this pass include selector syntax, required DTO flags, old optional journal fields, capture-failure versus empty maps, and the “anchor last” tail-window property.

## Full-sweep checks that found no defect

- **Stash identity CAS:** current selector/OID handling is internally consistent. The executor re-resolves immediately before Apply/Drop/Branch, so a stale selector that has renumbered is refused rather than redirected to another stash.
- **Current shared DTO alignment:** apart from deployment versioning, the head client and head server use the same types and validators. Strict request bodies and required push flags agree.
- **Answered-request Apply table:** the server distinguishes Git failure, blocked/clear conflict scan, and scan failure without treating unreadability as absence. I found no wrong arm in the pure Apply decision/rendering table.
- **ADR 0081 producer census:** current Fetch/Pull writers do not produce another all-`None` event that the admission exclusion would wrongly preserve outside the fold.
- **Current journal anchor placement:** for one writer and a current reader, placing the shared capture on the final batch line preserves the anchor in every newest-tail window that contains a referrer. Missing anchors resolve to no information, not an empty map.
- **Status-floor test semantics:** named expectations plus cross-version equality and the shell report check avoid the obvious “second binary never ran” green. I found no static parser mismatch in the changed status code.
- **Conflict fixtures:** the present-base and absent-base conflict shapes are asserted against Git’s index stages and agree with their descriptions. The non-text and editor fixture invariants I inspected are likewise checked against Git, not just builder intent.
- **Offline guard audit:** the changed completeness walk closes the specific `include_str!` census omission and appears sound within its source-text scanning model.
- **Range hygiene:** the checkout was the exact requested head, the base-to-head crate diff was exactly 60 files and `+9,193/-1,171`, and the working tree remained unchanged.

## What I could not check

- I did not run the project’s server or browser suites. The supplied cloud handoff establishes that this kernel lacks the required Landlock ABI and that the resulting sandbox refusals are environmental. Running those suites here would duplicate a known-invalid leg and risk laundering environment failures into findings.
- I did not empirically drive the network-loss sequences, scan-to-Drop interleaving, old/new cached PWA pairing, Branch untracked collision, slow-ref timestamp drift, or cross-process batch collision. Those findings are static **REASONED** conclusions, not reproductions.
- I did not mutate GitHub tags, execute the floor-provisioning job, or inspect external cache contents. The CI provenance finding is derived from the workflow’s missing pin/signature/hash checks.
- I could not establish that a supported released Git-Vista route ever persisted `PopStash`. ADR 0078’s assertion that no route constructed it is consistent with the routes and planner entry points I inspected. M1 therefore describes the deterministic handling of such a row and a general persisted-enum upgrade defect, with natural reachability unproven.
- I did not duplicate the separate Landlock-host Codex session’s empirical assignment.

## Bottom line

The range is strongest where it was reviewed most heavily: local DTO agreement, pure verdict tables, fixture construction, and mutation-resistant unit assertions. Its failures are at temporal and version boundaries: two binaries from adjacent deployments, two requests separated by an unlocked read, a server result separated from its lost response, and persisted enum data separated from the revision that wrote it.

The central design error is treating individually truthful records as a truthful composite operation. They are not, unless the code preserves causality across them. The Pop workflow currently does not.

**Signed:** codex · 2026-08-25T16:23:49Z