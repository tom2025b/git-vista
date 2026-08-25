# Handoffs a cloud session can actually read

A cloud Claude Code session **clones this repository**. It cannot see anything
Git ignores — which includes all of `design-docs/`.

So a handoff written for a cloud session lives **here**, tracked, or it is not
reachable at all. Handing a session a `design-docs/handoffs/…` path gives it a
path to a file that does not exist in its world.

That mistake has now been made twice: the `CLOUD-1..6` batch on 2026-08-23 and
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
