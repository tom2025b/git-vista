# Cloud handoff — #77, the stash drawer (M3.24)

**Written:** 2026-08-25 · **By:** max (CLI session on Tom's box) · **For:** a cloud Claude Code session on `tom2025b/git-vista`

> **Read the correction first.** The 16-hour plan for today, and the session note
> it quotes, are both wrong about what is left on this issue — and wrong in the
> direction that matters.

---

## Correction, before you start

An older session note says *"#77's remaining work is its HTTP endpoints — the
shortest path to a bar that visibly moves."* Today's plan repeated it.

**The endpoints are done.** `crates/git-vista-server/src/main.rs` routes
`/api/stashes`, `/api/stash/show`, and — behind the write gate —
`/api/stash/push`, `/api/stash/apply`, `/api/stash/drop`, `/api/stash/branch`.
`handlers/stash.rs` is 324 lines with substantial reasoning in its doc comments.
The planner carries `PushStash`, `ApplyStash`, `PopStash`, `DropStash` and
`BranchFromStash` as real `GitOperation`s, each with a `RecoveryStrategy`
(`RecreateStashEntry`). `git-vista-protocol` has a `StashSelector` newtype that
refuses anything but `stash@{<digits>}`, with the argument for that grammar
written out.

**What does not exist is the frontend.** Searching `crates/git-vista/src` for
"stash" finds exactly one file — `icons.rs`, which has an icon. No API client
function, no drawer, no menu item, no state.

So this is **not** the shortest path to a moving bar. It is a whole UI slice
against a server that is already built and already reasoned about. That is a
good job — the hard thinking is done and it is written down — but size it
honestly before starting, and tell Tom if it will not fit.

Note also: `main.rs` explains why there is deliberately **no** `/api/stash/pop`
route even though `PopStash` exists as an operation ("pop is apply-then-drop and
one operation row…"). Read that comment in full before designing the drawer's
pop affordance. It is a decision, not an omission.

---

```yaml
task_id: gv-77-stash-drawer
issue: 77
milestone: M3 — Parallel Work & Recovery [V2]
repo: tom2025b/git-vista
base: main
branch: feature/m3.24-stash-drawer
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max
allowed_paths:
  - crates/git-vista/src/**          # the frontend slice
  - ci/browser/**                    # fixture + spec
  - docs/adr/
  - crates/git-vista-server/**       # ONLY if a real gap is found; see below
forbidden_paths:
  - design-docs/            # untracked here; not in your clone
  - handoff.md
acceptance:                 # the issue's own, restated
  A1: stash content is inspectable before apply or drop
  A2: staged and untracked options are explicit
  A3: conflicts enter the shared continuation workflow
  A4: pop is not reported complete while conflicts remain
  A5: activity and generation updates are correct
```

## Where to start reading

In this order, because each explains the next:

1. `crates/git-vista-server/src/handlers/stash.rs` — the endpoints, and the
   reasoning for each. `show_stash`'s comment states A1 as its own criterion and
   why the flag set (`--no-color`, `--no-textconv`) is what it is.
2. `crates/git-vista-server/src/main.rs` around the stash routes — which are
   write-gated, and the deliberate absence of a pop route.
3. `crates/git-vista-protocol/src/newtype.rs`, `StashSelector` — the exact
   grammar the client must produce. Do not build selectors by string formatting
   in the UI without going through it.
4. `crates/git-vista-server/src/planner.rs`, the five stash arms — what each
   operation's recovery strategy is, because the UI has to be honest about what
   is reversible.
5. An existing frontend slice to copy the shape from. The conflict panes
   (#428/#429/#432) are the most recent full slice: pure core + signals wrapper
   + view + browser spec.

## The shape the frontend must take, and why

This repository has a hard rule about where decisions live, and it exists
because of six shipped-green-and-useless tests:

**`mod app` and every view module are `#[cfg(target_arch = "wasm32")]`, so
`cargo test` never compiles them.** Anything decided inside a view can never be
host-tested, and "renders nothing" is exactly how such defects present. So:

- **Pure decisions go in their own host-compiled module**, not in a `match`
  inside markup. `crate::head_notice` and `crate::hook_policy_disclosure` are the
  worked examples — small modules, `#[cfg_attr(not(any(target_arch = "wasm32",
  test)), allow(dead_code))]` in `main.rs`, tests beside them.
  For this slice that means at minimum: what a stash row says, which actions an
  entry offers and which are refused with a reason, and how a conflicted pop is
  labelled (A4).
- **The view draws what the pure module returns**, and a browser spec proves the
  module is *reached*.

`ci/browser/README.md` states the split plainly: the Rust suite proves the core
is correct, the browser suite proves it is reached. Read it before writing
either.

## The criteria, and the traps in each

- **A1 — inspectable before apply or drop.** The endpoint exists; the UI has to
  make inspection the *default* motion rather than a hidden one. Dropping is
  irreversible from the user's point of view, so a drop offered without a way to
  look first is the defect this criterion names.
- **A2 — staged and untracked options are explicit.** "Explicit" means the user
  can see what will and will not be captured *before* pushing, not that a flag
  exists somewhere. An untracked file silently left behind is data the user
  believes they stashed.
- **A3 — conflicts enter the shared continuation workflow.** There is an existing
  conflict model (`git-vista-protocol/src/conflict.rs` names stash pop among the
  six operations it covers) and existing conflict panes. Route into them. Do not
  build a second, stash-shaped conflict UI — that is the drift argument from
  #448 in a different place.
- **A4 — pop is not reported complete while conflicts remain.** This is the
  load-bearing negative of the whole slice. A pop that conflicts has *applied
  something* and *dropped nothing*, and a UI that says "popped" there has lied
  about the user's data. Write the test for this one first, and write it as a
  negative: a conflicted pop must **not** report success.
- **A5 — activity and generation updates are correct.** The feed and the graph
  generation both move on writes. Check what the other write slices do rather
  than inventing a convention.

## Scope fence

- **The server is done. Treat it as read-only** unless you find a real gap, in
  which case say so explicitly in the PR and keep the server change minimal and
  separately committed. A frontend task that quietly rewrites the backend is
  impossible to review.
- **Do not add a `/api/stash/pop` route** because it seems missing. Read the
  comment that says why it is not there; if you disagree with it, that is an ADR
  and a conversation, not a commit.
- **If the slice will not fit your window, cut it by capability, not by
  quality.** List + inspect, shipped complete with tests, is a real deliverable.
  All six actions, half-tested, is not.

## Tests

- Host tests for every pure decision module you add.
- **Prove each new invariant test can fail, two different ways**, and say in the
  PR that you did it by hand — the `failure-atlas` MCP that normally does this is
  a local server you will not have. One mutation is not proof; pick two that fail
  differently, one removing the mechanism and one weakening it. A test in this
  repository once survived a mutation that swapped a verb (the assertion was
  inert) and caught the one that removed a marker.
- A browser spec proving the drawer is reached, with a fixture repository that
  has real stash entries. Follow `ci/browser/fixture.mjs`'s pattern: a separate
  repo per shape, a comment naming the defect or criterion it exists for.
- **Add the matching entry to `ci/browser/tests/harness-selfcheck.spec.mjs`** —
  that file requires every assertion in the suite to be shown going red against a
  deliberately-broken DOM, and its own header says to add one whenever you add an
  assertion.

## House rules that bind this task

The repository's `CLAUDE.md` is tracked and you will have it. What differs in a
cloud session:

- **`buildlock` is a local wrapper that does not exist for you.** Run `cargo` and
  `trunk` directly.
- **Commit identity per commit**, exactly:
  `git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com commit …`
  This repository has local git config that will supply a personal gmail address
  if you let it.
- **Branch → PR → merge, never delete a branch**, no force-push.
- **Write an ADR** under `docs/adr/NNNN-slug.md` (next free number — check, do not
  assume), add its row to `docs/adr/README.md`, sign it `max`. The decision worth
  recording is how a conflicted pop is represented, since that is where the UI can
  most easily claim something untrue.
  - Diagrams: every `classDef` that sets a `fill` must also set a `color`, and
    node titles use `<b>title</b>` in a plain label — never `**bold**` inside a
    backtick label, which ignores the class colour and renders unreadable on
    GitHub in dark mode.
  - If you cannot render the PDF twin into `docs/adr/pdf/`, say so; Tom's box
    will render it.
- **`design-docs/` is gitignored and not in your clone.** Do not create it.
- **There is no live server.** Tom drives the result on his own machine, and for
  a UI slice that human pass is the real acceptance — say in the PR exactly what
  he should try, in order.

## What "done" looks like

A pushed branch and a PR saying `Closes #77` (or naming precisely which criteria
it does and does not close), whose body carries: which capabilities shipped, the
two hand-run mutations per new invariant test with verdicts, which gate legs
actually ran and which could not, the drive-it script for Tom, and anything this
handoff got wrong. The corrections at the top of this file were found by grepping
for ten minutes; assume there are more.

---

**Signed:** max · 2026-08-25T07:20:00-04:00
