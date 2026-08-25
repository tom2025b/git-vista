# Cloud handoff — #329, the fetch flood: verify what is left of it

**Written:** 2026-08-25 · **By:** max (CLI session on Tom's box) · **For:** a cloud Claude Code session on `tom2025b/git-vista`

> **Read the first section before anything else.** This task is not what the
> issue says it is, and the 16-hour plan for today is wrong about it too.

---

## Correction, before you start

The plan called #329 "the one that actually pays" and described the fix as
"record one event carrying the count, not one per reference touched." That plan
was written without checking whether the fix had already landed.

**It has.** `git-vista-core/src/activity.rs` carries `fold_ref_update_bursts`,
introduced by `5d900fbe fix(#329): fold a fetch/pull ref-update burst into one
feed row` (2026-08-06, on `main`), called from the feed assembly at
`activity.rs:543`. It folds each burst of `Fetch`/`Pull` remote-tracking ref
updates within `FETCH_BURST_GAP` into a single counted row —
"fetch — 94 refs updated".

Its own doc comment explains why it lives at the **read** path rather than the
write path, and the reasoning is good: a fetch run from the terminal produces
reflog lines and no journal entry at all, so an operation id stamped by the app
could never group it. Folding both sources in the pure core covers the app's
fetches and everyone else's with one rule. It also states two properties worth
knowing before you touch it — a pull's own branch movement is deliberately never
folded, and the fold is safe for undo *by construction* because `undo_hint` has
no arm for `Fetch` or `Pull`.

There is more history: `0a7ba777 fix(#327,#328): … revert #329 (made the feed
worse)` shows an earlier attempt at this was reverted. Read that commit before
proposing anything, so a third attempt does not rediscover a second-time
mistake.

**So the issue is open, and the symptom it describes appears to be fixed.** That
is the task.

---

```yaml
task_id: gv-329-verify-fetch-volume
issue: 329
repo: tom2025b/git-vista
base: main
branch: chore/issue-329-verify-fetch-burst-fold   # only if code changes; see below
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max
shape: investigate first, change only what the investigation justifies
forbidden_paths:
  - design-docs/            # untracked here; not in your clone
  - handoff.md
```

## What you are actually being asked

Answer three questions with evidence, in this order. Stop and report as soon as
the answer is "nothing more to do" — a PR that closes an issue with a written
verdict is a complete result here.

**1. Does the fold actually cover the reported case?**

Build a repository whose `origin` legitimately updates many remote-tracking refs
in one fetch, drive the feed assembly over the resulting events, and count the
rows a reader would see. The issue's evidence is 94 journal `Fetch` lines
producing 94 feed rows; the fold should turn that into one. Look for the gaps its
own comment implies rather than only the happy path:

- refs whose updates straddle `FETCH_BURST_GAP` — one slow fetch could split
  into several counted rows, which may be correct or may be a defect
- a fetch immediately followed by a pull (they group separately, on purpose)
- the "tips unknown — git could not be read" entry, which the comment says is
  journaled *instead of* per-ref entries and must stay a run of one
- a fetch that updates exactly one ref, which is deliberately left untouched

**2. Is the remaining volume a defect in its own right?**

The fold is at the read path, so **the journal file still stores one line per
ref**. That is a different cost from feed noise: file size, and the work every
read does before folding. Yesterday's three performance fixes (24 August) made
reading that journal bounded, linear and single-copy — which is either enough,
or it is not, and the honest answer needs a number rather than an opinion.

Measure it: journal bytes per fetch, and the cost of `read_all` plus the fold at
a realistic line count. If the numbers say the read path handles it comfortably,
**say so and recommend closing #329** rather than inventing work. If they say
otherwise, a write-path change is a real proposal — but it must reckon with the
reason the read-path fold was chosen (terminal fetches have no journal entry at
all), and that reason has not gone away.

**3. Does anything else write per-item events for one user action?**

The issue says "check the same shape for `Pull` before assuming it is
fetch-only." Widen that: look for any handler that journals in a loop.
`journal::append` has few call sites — `handlers/mod.rs::journal_app_event` and
`activity.rs`'s synthesized branch-deletion — so this is a bounded read, not a
survey. If a second shape exists, **file it as its own issue** and name it in the
PR; do not fold an unrelated fix into this one.

## Ground truth you cannot reach from the cloud

Tom's own repository journal currently holds **4 lines** (2 `BranchDeleted`,
2 `Commit`) — the 94-line state came from a clone whose `origin` had been pointed
at a large public repository during a device pass. So there is no live evidence
here to re-examine, and a fixture is the only honest way to reproduce it. Say in
the PR that the reproduction was synthetic, because it was.

## House rules that bind this task

The repository's `CLAUDE.md` is tracked and you will have it. What differs in a
cloud session:

- **`buildlock` is a local wrapper that does not exist for you.** Run `cargo`
  directly.
- **Commit identity per commit**, exactly:
  `git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com commit …`
  This repository has local git config that will supply a personal gmail address
  if you let it. Never let it.
- **Branch → PR → merge, never delete a branch**, no force-push.
- **If you change behaviour, write an ADR** and add its row to
  `docs/adr/README.md`, signed `max`. If your verdict is "close the issue", an
  ADR is not needed — the PR body, or a comment on #329, is the record.
- **Any test you add to pin an invariant must be shown able to fail, two
  different ways.** The `failure-atlas` MCP that normally does this is a local
  server you will not have; do it by hand and say in the PR that it was by hand.
  One mutation is not proof — pick two that fail differently, one removing the
  mechanism and one weakening it.
- **`design-docs/` is gitignored and not in your clone.** Do not create it.
- **There is no live server.** Nothing you write should assume one.

## What "done" looks like

A PR whose body answers the three questions with numbers, states plainly whether
#329 should be closed, and — if it should not — says exactly what remains and
why the read-path fold does not cover it. **A well-evidenced "this is already
fixed, close it" is a success, not a wasted session.** The failure mode to avoid
is a change made because a task was assigned.

---

**Signed:** max · 2026-08-25T07:10:00-04:00
