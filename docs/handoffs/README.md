# Handoffs a cloud session can actually read

A cloud Claude Code session **clones this repository**. It cannot see anything
Git ignores — which includes all of `design-docs/`.

So a handoff written for a cloud session lives **here**, tracked, or it is not
reachable at all. Handing a session a `design-docs/handoffs/…` path gives it a
path to a file that does not exist in its world.

That mistake has now been made twice: the untracked `CLOUD-1..6` batch on 2026-08-23 and
the `CLOUD-7..10` batch on 2026-08-25. Both times all the sessions asked the
same question at once, which is the tell.

## The rule

- **A handoff for a cloud session goes in `docs/handoffs/`**, is committed, and
  is pushed before the session is told about it.
- **A handoff for a local session may stay in `design-docs/handoffs/`** — a
  session on this box reads the disk directly, so the gitignore is irrelevant
  there.
- Give the session the **in-repo path**, and the one command that reaches it
  even before the branch is merged:

  ```
  git fetch origin && git show origin/main:docs/handoffs/<file>.md
  ```

## Why these are tracked rather than pasted

Pasting a 300-line prompt into four sessions is slow, error-prone, and painful
on a tablet. It also leaves no record of what each session was actually told —
and the record is the point: these documents state the acceptance criteria a PR
is judged against, and a later reader deserves to see the instructions beside
the result.

Historical note, kept deliberately: each of these carries corrections to the
plan that produced it, found by checking the repository rather than trusting the
issue. Two of the four say the job is not what its issue claims. That is worth
reading before writing the next batch.

---

## The 25 August batch — CLOUD-1 … CLOUD-6

**Numbered by MERGE ORDER, not by when they were written.** "Run 1 first" and
"merge 1 first" are deliberately the same number, so a session's name is also
its place in the queue.

| # | Handoff | Issue(s) | ADR | Merge order |
|---|---|---|---|---|
| **1** | `CLOUD-1-issue-495-stash-dtos.md` | #495 | 0079 | **FIRST of all six** — rewrites stash field names everywhere; everything else would pay for its rebase |
| **2** | `CLOUD-2-issues-493-494-stash-executor.md` | #493 + #494 | 0078 | after #1 |
| **3** | `CLOUD-3-issue-485-journal-quadratic.md` | #485 | 0080 | **before #6** — shares `activity.rs` and `planner/fetch.rs` with it |
| **4** | `CLOUD-4-issue-438-parallel-test-race.md` | #438 | 0083 | independent |
| **5** | `CLOUD-5-issues-496-365-fixtures-finished.md` | #496 + #365 | 0082 | independent |
| **6** | `CLOUD-6-issue-486-tips-unknown-fold.md` | #486 | 0081 | **after #3** — same two files, and the collision is textual |

**Only two of the six are truly independent (#4 and #5).** The batch's own
truth-check found that #3 and #6 both edit `crates/git-vista-core/src/activity.rs`
and `crates/git-vista-server/src/planner/fetch.rs` — and not incidentally: they
edit *the same doc-comment block*, `activity.rs` ~601-624, which lists two known
defects, one each. Each fix deletes its own numbered item and whichever lands
second must reword the block header. Both handoffs originally claimed to be
independent. Both were wrong.

**ADR numbers are assigned here, up front, on purpose.** When four sessions run
at once and each picks "the next free number", they all pick the same one and
the index conflicts. That happened on 25 August. 0074–0077 are taken; this batch
claims 0078–0083.

### One handoff was written and withdrawn

`CLOUD-4` was originally about **#326** (moving `shape()`'s match arms into
their per-operation modules). The truth-check refuted three of its claims —
its measurements were eighteen days stale, and issue #326 *explicitly forbids*
the single-sweep approach the handoff instructed, for a reason that was live
inside this very batch. It is parked at
`docs/handoffs/parked/CLOUD-X-issue-326-planner-shape.md`, kept rather than
deleted, with the full account in `docs/handoffs/parked/README.md`. #438 took
its slot.

### A name collision worth knowing about

An **earlier, different** `CLOUD-1 … CLOUD-6` batch was written on 23 August and
lives in `design-docs/handoffs/` — untracked, local to Tom's box, and about
entirely different issues (#449 capture-refs, cold-build measurement, and so
on). Some of those filenames also appear in git history from before
`design-docs/` was gitignored.

So `CLOUD-1` names two different jobs depending on which directory you are in.
The tracked namespace here — `docs/handoffs/` — is the one that is the record;
the numbers restart at 1 per batch, and the batch is identified by this section
rather than by an ever-climbing counter. If that ever stops being clear enough,
the fix is a dated subdirectory per batch, not a renumber of what has already
been handed to a session.

### What every handoff in this batch carries

Three things the previous batch proved necessary, in every one of the six:

- **An assigned ADR number**, for the reason above.
- **A stated merge order**, because two of them touch the same files.
- **An explicit instruction to say in the PR body that `ci/browser/run.sh` is
  unrun** — not to leave it implicit. The browser leg cannot run in a cloud
  container (`landlock_abi=-1`; the server refuses to start without its strict
  sandbox tier and INV-13 gives it no degraded mode), and every one of the three
  defects found in PR #490 was found by that leg and by nothing else.

---

## A fresh clone's server suite fails by the hundreds until `gv-sandbox` is built

This one bites **local clones on real hardware too**, not just cloud
containers — codex hit it on this box on 2026-08-25, working #438 in its own
clone on the SSD.

`gv-sandbox` is a sibling binary target of the server crate
(`crates/git-vista-server/src/bin/gv-sandbox/`). The sandboxed tests do not
link it — they **exec `target/debug/gv-sandbox` at runtime**. A target
selection like

```
cargo test -p git-vista-server --bin git-vista-server
```

builds only the named binary, so in a cold clone the helper does not exist and
hundreds of sandboxed tests fail at spawn. That storm looks like a
catastrophic regression; it is a missing build product.

**The tell:** the first baseline in codex's #438 campaign reported 12/12 runs
failed. It did not believe the number, dug, found the missing helper, built it
explicitly, and discarded the invalid baseline. The honest baseline was 5/12.

**The rule for any clone-based session:** before running server tests with a
`--bin` selection, build the helper once —

```
cargo build -p git-vista-server --bin gv-sandbox
```

— or run one unfiltered `cargo test -p git-vista-server` first, which builds
every target of the crate. Put this line in the handoff itself; a session that
has not read this file will otherwise spend its first hour on a phantom.

---

## The 26 August batch — `2026-08-26/CLOUD-1 … CLOUD-5`

**First batch to use the dated-subdirectory fix** this README promised when
`CLOUD-1` stopped being unambiguous — three batches now share that name, so
from here a handoff's identity is `2026-08-26/CLOUD-N`, and the numbers still
mean MERGE ORDER.

| # | Handoff | Issue | ADR | Merge order |
|---|---|---|---|---|
| **1** | `2026-08-26/CLOUD-1-issue-336-collapse-route-local.md` | #336 | 0084 | **FIRST** — touches the contended middleware/handlers/planner area |
| **2** | `2026-08-26/CLOUD-2-issue-521-journal-rollback.md` | #521 | 0085 | **before #3** — same activity/journal subsystem; its format decision lands first |
| **3** | `2026-08-26/CLOUD-3-issue-487-push-fold.md` | #487 | 0086 (reserved, may go unused) | after #2, rebased on it |
| **4** | `2026-08-26/CLOUD-4-issue-520-floor-pin.md` | #520 | 0087 | independent — CI + docs only |
| **5** | `2026-08-26/CLOUD-5-issue-335-signature-status.md` | #335 | 0088 | **LAST** — wire-format change lands on a quiet main |

ADR numbers assigned up front as always: 0083 is taken; this batch claims
0084–0088, and a reserved number that goes unused stays burned rather than
reassigned.

### Two issues were considered and rejected by the truth-check, on the record

- **#326** (planner `shape()` arms): its own text says the arms move *when a
  milestone touches their operation* and explicitly forbids the sweep — a
  dedicated extraction session contradicts the tracked decision. This is the
  SECOND time #326 has been pulled from a batch for this reason (see the
  parked `CLOUD-X` above); it should not be offered to a session again while
  that policy stands.
- **#450** (lesson tool): its own sequencing says "after #92 (Explain Mode)",
  and #92 is open — the single sentence-source it must share with Explain
  Mode does not exist yet. Premature, not wrong.

Every handoff in this batch carries the three standing environment rules
(baseline-diff instead of "workspace green", the `gv-sandbox` build line, the
browser-leg-unrun statement) plus per-handoff citations truth-checked against
`682f3061` on the morning of dispatch — two of the five issues had already
drifted (functions moved by the planner split; `rewrap_error` lives in
`middleware.rs` now), which is the recurring argument for checking.

### The batch was re-checked a second time, and four of the five were wrong

**Kept here deliberately, because the second pass is the whole lesson.** The
first truth-check confirmed that cited *symbols existed*. It did not read the
code around them. A second pass an hour later — reading the actual source
regions rather than grepping for names — found a defect in four of the five
handoffs, every one of them the kind a session would have acted on:

| Handoff | What the first pass got wrong |
|---|---|
| CLOUD-1 (#336) | Repeated the issue's "no wire-level test covers `/api/fetch` or `/api/pull`". **`/api/pull` is covered** — `the_strategy_mandate_is_a_400_through_a_real_router` (`handlers/pull.rs:360`) layers the real `api_contract` middleware. Only `/api/fetch` lacks one. |
| CLOUD-3 (#487) | **Invented a correction that was itself wrong** — claimed the issue's `push.rs:684/:693` had drifted. They are exact: `:684` is `journal_updates`, `:693` its per-ref loop. Grepping found the *call sites* and mistook them for the definition. |
| CLOUD-4 (#520) | Said "required merge job" without establishing which job. It is the `core` job / "Core (check + test)" (`:127-128`) — true, but unverified when written. Also missed that the provisioning step only *prints* `git --version` without asserting it. |
| CLOUD-5 (#335) | Repeated "two real outcomes have nowhere to go". **`EXPKEYSIG`/`EXPSIG` are deliberately folded into the `GOODSIG` arm** at `tags.rs:613` with a documented rationale at `:608-612`. Only `REVKEYSIG` is a true fallthrough — and that rationale comment must be rewritten by the fix, which the first draft never mentioned. |

CLOUD-2 (#521) survived both passes unchanged; its ADR-0080 claim was
independently confirmed (0080 contains no discussion of rollback, downgrade,
or a versioned envelope).

**The rule this earns:** a citation check that only proves a symbol exists is
not a truth-check. Open the region and read what the code *does* — and treat
"the issue is wrong" as a claim needing the same evidence as any other,
because a confident wrong correction is worse than the stale line it
replaced.

---

## Never tell a cloud session "`cargo test --workspace` must be green"

**It cannot be.** Every handoff in the 25 August batch said it, and every one
was wrong.

A cloud container fails **320 of 915** `git-vista-server` tests on unmodified
`main`, before reaching anything a handoff is about. One cause, printed 535
times in a single run:

> `this operation runs in the strict sandbox tier and this host cannot provide
> it (missing: landlock_abi>=6, bwrap). Per ADR 0029 the operation is refused
> rather than run in a weaker tier`

268 of those tests print that refusal verbatim; the other 52 are downstream of
it (`CheckFailed { GitSpawnFailed }`, "couldn't run git"). Landlock is absent
from that kernel — verified by raw `landlock_create_ruleset` syscall,
independently of the server's own probe — so **installing `bwrap` changes
nothing**. `seccomp` and user namespaces are present; the strict tier needs all
four.

**Five sessions worked around this silently and none of them said so.** That is
the part that matters: an instruction a session cannot follow does not produce
a complaint, it produces a quiet workaround and a PR whose green claim means
something different from what the reader assumes.

### What to write instead

> Roughly 320 `git-vista-server` tests fail in your container for environmental
> reasons — the strict sandbox tier is unavailable there. **Run the suite on
> unmodified `main` first, keep that failing-test set, and compare yours
> against it. Only the difference is yours.** State the two counts in your PR
> body. Do not report a sandbox-refusal failure as a defect, and do not claim
> the suite is green.

### The trap that makes this worse than a nuisance

Running the six `CURRENT`-writing tests at 8 threads in a container fails
**20/20**. That looks exactly like a reproduction of #438's race, and it is an
environment failure — the same set fails at `--test-threads=1`. A session
reporting that 20/20 as a repro has reported the wrong bug convincingly.

The session that found this refused to report it, and refused to conclude from
`--test-threads=1` failing that the process-global hypothesis was dead, on the
grounds that the tests never got far enough to race. Both refusals were right.
Its write-up is `CLOUD-4-issue-438-result.md`, and it is the most useful
document the batch produced.

### The general rule

**Before writing an acceptance criterion, ask whether the session you are
sending it to can physically satisfy it.** A criterion that cannot be met is
not a high bar. It is an instruction to improvise, issued to someone with no
way to tell you they had to.
