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
| **1** | `CLOUD-1-issue-495-stash-dtos.md` | #495 | 0079 | **FIRST** — rewrites stash field names everywhere; everything else would pay for its rebase |
| **2** | `CLOUD-2-issues-493-494-stash-executor.md` | #493 + #494 | 0078 | after #1 |
| **3** | `CLOUD-3-issue-485-journal-quadratic.md` | #485 | 0080 | independent |
| **4** | `CLOUD-4-issue-326-planner-shape.md` | #326 | none | collides with #2 — whichever is ready second rebases |
| **5** | `CLOUD-5-issues-496-365-fixtures-finished.md` | #496 + #365 | 0082 | independent |
| **6** | `CLOUD-6-issue-486-tips-unknown-fold.md` | #486 | 0081 | independent |

**ADR numbers are assigned here, up front, on purpose.** When four sessions run
at once and each picks "the next free number", they all pick the same one and
the index conflicts. That happened on 25 August. 0074–0077 are taken; this batch
claims 0078–0082.

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
