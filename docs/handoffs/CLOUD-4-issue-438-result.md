# CLOUD-4 result — #438 could not be reproduced, because it cannot be *run*

**Written:** 2026-08-25 · **By:** a cloud Claude Code session on `tom2025b/git-vista`
· **Pairs with:** `CLOUD-4-issue-438-parallel-test-race.md`

> **Read this before the next session picks up #438.** The blocker is not the
> race and not the difficulty of catching an intermittent failure. The two
> tests #438 is about **cannot execute in a cloud container at all**, serially
> or in parallel, on unmodified `main`. No fix is in this branch, deliberately.

---

## The result in one line

`--test-threads=1` does **not** make the failure disappear here — and that does
**not** refute the process-global hypothesis, because at one thread the tests
never get far enough to race. They are refused by the sandbox tier before they
reach anything #438 is about.

That distinction is the whole point of this document. The handoff says,
correctly, that if `--test-threads=1` doesn't help then `CURRENT` is not the
culprit and that is a big finding. **That inference does not apply to this
run.** Do not read this result as "the hypothesis is dead."

---

## What actually happens in a cloud container

On `main` @ `d1d38cc`, no edits, the server unit-test binary:

```
$ target/debug/deps/git_vista_server-<hash> --test-threads=1
test result: FAILED. 592 passed; 320 failed; 3 ignored; 0 measured; finished in 175.81s
```

**320 of 915 fail.** Both of #438's tests are in that set. Attributing every
failing test's own stdout:

| Failing tests | Cause |
|---:|---|
| 268 | print the strict-sandbox refusal verbatim |
| 52 | downstream of it — `CheckFailed { GitSpawnFailed }`, "couldn't run git" |
| **320** | **all one cause** |

The refusal, printed 535 times across the run:

> `this operation runs in the strict sandbox tier and this host cannot provide
> it (missing: landlock_abi>=6, bwrap). Per ADR 0029 the operation is refused
> rather than run in a weaker tier`

Confirmed independently of the server's own probe, so this is not a bug in
`sandbox::probe`:

```
$ python3 -c "... libc.syscall(444, None, 0, 1) ..."   # landlock_create_ruleset
landlock abi: -1 errno: 38 Function not implemented
$ cat /sys/kernel/security/lsm
(no such file — no LSMs at all)
$ unshare -Ur true && echo userns-ok
userns-ok
```

Kernel `6.18.44-fc-v21`. Landlock is absent from the kernel, not merely
unconfigured. `seccomp` and `user_namespaces` are present; the strict tier
requires all four knobs (`sandbox/probe.rs`, `missing_capabilities`), so
**installing `bwrap` would not help** — the handoff was right about that and
right for the reason it gave.

## The trap this sets, which I walked into first

Running the six `CURRENT`-writing tests together at 8 threads fails **20/20**.
That looks exactly like a reproduction and it is not one. The same set fails at
`--test-threads=1` too. Anyone who reports that 20/20 as the repro has reported
an environment failure as a race.

Cheapest way to not be fooled: **before believing any red here, re-run it at
`--test-threads=1`.** If it is still red, it is the container.

## What the handoff got wrong

> "This is a Rust-test-harness task and should not need [the browser leg]."

Both named tests shell out to git through `git_cmd::sandboxed`, which needs the
same INV-13 / ADR 0029 strict tier the browser leg needs. It is the *same*
missing capability, reached by a different path. Acceptance criteria 1, 3, 5 and
8 are unreachable in a cloud container.

This is a fourth refuted claim in the handoff written to replace one that the
batch's truth-check refuted on three. The check that would have caught it: run
the named tests, once, before writing the handoff.

---

## The diagnosis, which *is* established — by source, not by argument

Reading is not blocked by the sandbox. The mechanism is nameable, and one
detail of it explains the exact wording CI saw.

**The three reads of one global.** In
`a_stale_claimed_undo_is_refused_and_the_branch_is_left_alone`:

1. `recovery_center.rs:1962` — `set_current(&f.repo, Active)`.
2. `recovery_center.rs:1976` — `planner::selection_tokens()`, **which re-reads
   `CURRENT`**, and its result is stamped onto `durable_row.worktree`.
3. `recovery_center.rs:1006` — `recover_operation` reads `CURRENT` a third time
   via `resolve_target()` to obtain `repo`.

**Why the observed message pins the interleaving.** If another test moves
`CURRENT` between (1) and (2), the row is stamped with the *other* repo's
worktree — so the step-1b guard `row.worktree != current_worktree`
(`recovery_center.rs:1043`) **passes**, and step 2
(`recovery_center.rs:1056`) classifies against the wrong tempdir. There,
`classify_reset_ref` finds neither the recovery pin nor `refs/heads/main`, so it
returns `Expired`, and step 3's `else` emits verbatim:

> "This operation can no longer be recovered — its recovery point is no longer
> available."

which is byte-identical to the CI failure. A move *after* (2) would instead trip
step 1b and produce the "belongs to a different worktree" message — **which is
not what CI saw.** The message therefore selects one interleaving out of two,
rather than merely being consistent with "something raced".

**The comment that made this invisible.** `state.rs:701-703` says:

> "keeping every global mutation in a single test means parallel test threads
> never fight over the process-wide selection *(no other test touches it)*"

The parenthetical is false. Other `set_current` callers in the same test binary:

| Site | Reached from |
|---|---|
| `recovery_center.rs:1962` | `a_stale_claimed_undo_is_refused_and_the_branch_is_left_alone` |
| `recovery_center.rs:2079` | `a_row_from_a_foreign_worktree_is_refused_not_executed_against_the_current_selection` |
| `handlers/tags.rs:758` | `build_tagged_fixture`, called from `handlers/tags.rs:783`, `:802`, and `main.rs:941` |

Six-plus test bodies write that global. The comment asserting otherwise is
plausibly why the race was never expected — and correcting it matters
independently of the fix, because it is the sentence a future author would
trust.

## What is left to do, and where

Everything, in an environment that has Landlock ABI ≥ 6 or `bwrap`. Suggested
order for the session that can run it:

1. Reproduce at high thread counts — the rate is low and load-dependent
   (five clean full-suite runs locally; one observed double failure in Actions
   run `32834326578`). Confirm `--test-threads=1` clears it *there*, where the
   tests can actually run.
2. Fix per the handoff's option 1 — per-test selection threaded through, not a
   shared global. Option 2 (a mutex over the six-plus writers) is the fallback.
3. Correct `state.rs:701-703` in the same commit as the fix.
4. Criterion 5 — prove it goes red two ways — is only meaningful against an
   observed rate, so it belongs to the same session as step 1.

**Unblocked and still owed either way:** criterion 7, updating #438 to name the
second test (`state::tests::selection_flow_carries_mode_and_gates_writes`). It
is currently a one-test issue about a two-test problem.

## For the rest of the batch

Every other handoff in this batch tells its session to "re-run before believing
these two tests are yours". That advice is now wrong in a more useful way: **no
cloud session can run them at all**, and every cloud session in this batch is
looking at ~320 failures on clean `main`. `cargo test --workspace` green is not
an achievable acceptance criterion in this container for any of them.
