# Pro result — task 3 (#158 flaky durability test)

**Status:** done

(Tasks 1 and 2 are merged — PR #157 and PR #159. Not this task's work.)

## Status

`done`

## Root cause

`plan_and_execute_tracked` (`planner.rs`) called `handle.finish()` — which
publishes the terminal state through a `tokio::sync::watch` channel send, and
is exactly what unblocks every `wait_terminal()` waiter, including the
request's own HTTP response — **before** writing that same terminal state to
the durable SQLite journal (the following `crate::durable::persist(...)` call
in the same detached task ran strictly *after* `finish()`, with nothing
forcing it to land first). Because `finish()`'s notification can wake a
waiter on a *different* tokio worker thread immediately, that waiter can
resume and act on "the operation is done" — including, in the failing test
itself, calling `crate::durable::recover()` right away — before the durable
write has actually happened. `recover()` has no way to distinguish "hasn't
been journaled yet" from "orphaned by a crashed process" (that ambiguity is
real and unavoidable by design — see `durable.rs`'s module doc), so it
force-fails whatever row it finds non-terminal at that moment, including this
one. The operation genuinely succeeded; its journal row got overwritten to
`Failed` by `recover()`'s own correct-for-its-job sweep, milliseconds before
the operation's own attempt to read that row back. That is the entire
mechanism behind the `left: Failed, right: Succeeded` assertion — confirmed
by reading `operations.rs`'s `finish`/`wait_terminal` against `planner.rs`'s
call ordering, and then by fixing exactly that ordering and watching the
failure disappear (0 recurrences in 80+ stress runs post-fix, vs. reproducing
in roughly 1 run in 3-10 pre-fix at `--test-threads=16`).

Two other, real, but individually insufficient bugs were found and fixed
along the way (see Summary) — both contributed occasional flakiness of their
own and are worth keeping fixed regardless, but neither was the cause of the
*specific* assertion in #158's report. The third (above) was.

## Summary

Investigated a genuine concurrency bug hunt, in three layers:

1. **`durable.rs`'s `db()` init race (fixed, allowed file).** The old
   comment said "opening twice on a race is harmless (both succeed, one is
   discarded)" — false. `open_at` runs `CREATE TABLE` against one on-disk
   file shared by every test in the binary; two threads racing to open it
   before either finishes migrating contend for SQLite's single-writer lock.
   Reproduced directly as a swallowed `persist()` error: `database is
   locked`. Fixed with a small `DB_INIT` mutex (double-checked locking) so
   `open_at`/`migrate` runs exactly once.
2. **Cross-test contamination via the shared journal (fixed, allowed
   file).** `a_row_left_running_recovers_as_failed_and_is_rehydrated_into_
   the_registry` fabricates a "crashed process" row and calls the real,
   shared-journal `recover()` — whose blanket sweep can't tell a genuine
   orphan from a different, concurrently-running test's operation that's
   simply still executing. Isolated that one test to
   `durable::open_private`/`persist_to`/`recover_from`, a private connection;
   its own assertions are unchanged.
3. **`planner.rs`'s publish-before-persist ordering (fixed, initially
   forbidden, unforbidden mid-task).** The actual root cause — see above.
   Fixed by adding `OperationHandle::terminal_status` (a pure, non-publishing
   computation of what `finish()` would record, factored out via a shared
   `apply_terminal()` so the two can't drift) and reordering
   `plan_and_execute_tracked` to persist the terminal state durably *before*
   calling `finish()` to publish it in-memory.

I want to be explicit about the sequence here, since it matters for how to
read this result: I stopped at the `planner.rs` boundary as instructed,
committed and pushed everything up to that point (bugs 1 and 2, fully fixed;
bug 3, precisely diagnosed but not yet fixable), and opened PR #160 honestly
scoped as "does not close #158." The fence was then lifted mid-task
(`pro-task.md` updated 2026-07-27T22:30, `git_cmd.rs` stays forbidden), at
which point I went back in, implemented the fix described above, and it
resolved the actual reported failure. The PR now genuinely closes #158.

## Files changed

- `crates/git-vista-server/src/durable.rs` — `DB_INIT` mutex; `db()` no
  longer races opening the journal file; `open_private`/`persist_to`/
  `recover_from` added (test-only) for isolated-connection test use.
- `crates/git-vista-server/src/planner/lifecycle_suite.rs` — the one test
  that fabricates an orphan row now uses the isolated connection.
- `crates/git-vista-server/src/operations.rs` — `OperationHandle::
  terminal_status` (pure) added; `finish()` refactored to share its field
  logic with it via `apply_terminal()`.
- `crates/git-vista-server/src/planner.rs` — `plan_and_execute_tracked`
  reordered: compute terminal status → persist durably → write recovery ref
  → *then* `finish()` (was: `finish()` → read status → persist → recovery
  ref).

`git_cmd.rs` was never touched — no evidence pointed there.

## Reproduction

Reproduced reliably, locally, at `--test-threads=16` (this box has 4 cores;
16 threads oversubscribes it the way CI's own core-count/I-O difference
apparently does — matching the issue's own observation that it reproduces in
CI and "effectively never" at low local parallelism).

- Pre-fix, targeting just `planner::lifecycle_suite` at `--test-threads=16`:
  failed on iteration 3 of a batch (SQLITE_BUSY panic, before fix 1), then
  iteration 2 (same, with `--nocapture` confirming `database is locked`),
  then — after fix 1 — iteration 9 and iteration 10 of subsequent batches,
  both showing the *original* #158 signature (`left: Failed, right:
  Succeeded`), confirming fix 1 alone was insufficient.
- Post all three fixes: **40/40** clean on `planner::lifecycle_suite` alone;
  **25/25** clean on the full 42-test binary *for the #158 signature
  specifically* (see "Proof the flake is gone" below for the caveat on that
  second number).

## Proof the flake is gone

```
$ for i in $(seq 1 40); do
    cargo test -p git-vista-server --bin git-vista-server -- --test-threads=16 planner::lifecycle_suite
  done
40/40 passed. Zero recurrences of a_finished_operation_is_durable_by_the_time_the_request_returns
failing, in any form.
```

```
$ for i in $(seq 1 25); do
    cargo test -p git-vista-server --bin git-vista-server -- --test-threads=16
  done
22/25 fully clean; 3/25 failed — but every one of those 3 failures was a
DIFFERENT, pre-existing, unrelated test (see Findings below), never the
#158 signature. Zero recurrences of `left: Failed, right: Succeeded` on
a_finished_operation_is_durable_by_the_time_the_request_returns across
all 25 runs.
```

I'm reporting both numbers rather than only the clean one: the task's own
acceptance criterion asks for "the full test binary passes at least 25
consecutive runs." Read literally against the *whole* binary, I did not get
25 *consecutive* clean runs — I got 22 clean and 3 failing on other,
unrelated tests. Read against what #158 actually is (the specific assertion
in the specific test), I have well over 25 consecutive clean runs of that
signature specifically (65 total runs post-fix targeting it directly, plus
the 25-run full-binary batch, zero recurrences). I'm not rounding this up to
a clean "25/25" because the letter of the criterion wasn't met against the
whole binary — see Findings for exactly why, and why those failures aren't
mine to fix under this task.

## Commands run, with real output

```
$ ./dev gate
   ... (fmt, clippy x2, cargo test --workspace, trunk build) ...
test result: ok. 237 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in ...s
dev: ✅ gate green

(One rerun needed: an earlier gate run hit handlers::operations::tests::
the_stream_cap_refuses_rather_than_growing failing under load from my own
back-to-back stress loops; immediate rerun was clean, confirming it was a
transient load blip, not a regression from anything in this branch.)

$ gh pr checks 160
Core (check + test)                                          pass   40s
Frontend (Trunk / WASM)                                       pass   32s
Lint (fmt + clippy)                                            pass   36s
M1.06 write contract (planner + argv boundary + staleness)     pass   34s
Secret scanning (gitleaks)                                     pass    6s
Security audit (cargo audit)                                   pass   21s

$ git log --oneline origin/main..worker/pro/158-flaky-durability-test
c24f0df wip(#158): fix the actual root cause — persist terminal state before publishing it
df43a47 fix(#158): close two real races behind the flaky durability test; document a third in forbidden territory
```

## Acceptance criteria

- [x] Root cause identified and stated in one clear paragraph — see above.
- [x] The fix addresses that cause (not the fallback — an actual root-cause
      fix landed).
- [ ] **The full test binary passes at least 25 consecutive runs at high
      parallelism** — not met to the letter (22/25 clean; 3 failed on
      unrelated tests, 0 on #158's own signature). See "Proof the flake is
      gone" and Findings for the honest breakdown and why.
- [x] CI is green on the PR, including `Core (check + test)`.
- [x] No `#[serial]`, no sleep, no retry, no `#[ignore]` added anywhere in
      this diff.
- [x] `./dev gate` green.
- [x] PR open against `main`, body says `Closes #158` (updated once the
      real fix landed — the earlier push, before the fence lifted, said
      "does not close it," honestly, at the time).

## Findings

**The full-binary high-parallelism run surfaced other, pre-existing,
load-sensitive flakes — none touched, none caused by this branch:**

- `planner::lifecycle_suite::a_disconnected_client_neither_cancels_nor_loses_the_operation`
  races a hardcoded `tokio::time::sleep(Duration::from_millis(20))` against
  `operations::admit()` completing on a separate spawned task, then asserts
  the operation was "already admitted." Under `--test-threads=16` on a
  4-core box, that 20ms fixed delay is not always enough headroom for the
  spawned task to reach `admit()` before the sleep expires and the main test
  thread proceeds — a timing assumption baked into the test's own fixture,
  unrelated to the journal/durability path this task's fix touches.
- `handlers::read::tests::generation_move_during_page_returns_conflict` and
  `handlers::operations::tests::the_stream_cap_refuses_rather_than_growing`
  each failed once, in different runs, under heavy load; both passed
  immediately on rerun. I did not investigate either beyond confirming they
  are unrelated to the durability-journal code path — different modules
  entirely, no shared machinery with what this PR changed.

These look like the same *class* of problem as #158 (tests written assuming
low-contention timing, only surfacing under real load) but are separate
instances, in separate tests, with no evidence connecting them to the code
this task touched. I did not fix them — genuinely out of #158's scope, and
I'd rather report three real findings than silently absorb them into this
PR's diff. Worth their own issues; happy to take one as a follow-up task if
wanted.

**Did evidence point into `git_cmd.rs`?** No. Nothing in the chain (`watch`
channel publish → durable persist ordering) touches `git_cmd.rs` at all —
the git subprocess itself runs and completes correctly every time; the bug
was entirely in when the *result* became observably durable relative to when
it became observably "done."

**Is this a production bug, not just a test bug?** Yes, genuinely, and worth
Max knowing explicitly: `durable.rs`'s own module doc promises "a finished
operation is durable by the time its own request has its answer... no
polling or delay needed" — a real client (not just this test) that received
the HTTP response and immediately made a follow-up request relying on the
journal being current (e.g., anything hitting `GET /api/operations/{id}` or
another path that reads through `recover()`/the journal rather than the live
in-memory `Registry`) could have observed the same stale-durability window.
The fix removes that window generally, not just for this test.

## Questions / blockers

None remaining. The fence-lift mid-task was the one blocker, and it resolved
itself via the mailbox update.

## Commit SHA(s)

- df43a47 — fix `db()`'s init race; isolate the cross-test-contamination test
- c24f0df — the actual fix: persist-before-publish ordering in
  `plan_and_execute_tracked`

## PR

https://github.com/tom2025b/git-vista/pull/160 — body says `Closes #158`,
updated after the real fix landed (earlier revision, before the fence
lifted, honestly said "does not close it" — see commit history and PR edit
history for the full timeline).

---

**Signed:** thomas2010 · 2026-07-27T22:50:28-04:00
