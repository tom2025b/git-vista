# ADR 0100 — A capability needs a door, and the door is a route

**Status:** Accepted — implemented, mutation-proved six ways, browser-verified,
and driven end to end against real git
**Date:** 2026-09-01
**Issues:** [#596](https://github.com/tom2025b/git-vista/issues/596) — M10.09,
cherry-pick has no UI
**Supersedes:** nothing · **Superseded by:** nothing

---

## Context

Since #576 this server has been able to cherry-pick. `GitOperation::CherryPick`
is in the protocol vocabulary, `planner::sequence_exec::exec_cherry_pick`
executes it, `planner.rs` dispatches to it, ADR 0099's preview engine draws it,
and `contract_suite`'s `a_cherry_pick_lands_a_commit_from_another_branch` proves
it works against real git.

None of that was reachable. Tom right-clicked a commit expecting to cherry-pick
it and read a twenty-item menu that did not offer the option. `grep -rn
"CherryPick" crates/git-vista/src/` returned exactly one file — a display label
for a past event in the Activity feed.

This is a shape worth naming, because it is the second time: #228 shipped an
executor with no caller, and this project has been paying for it since. A
capability with no door is indistinguishable, from where the user stands, from a
capability that does not exist.

```mermaid
---
config:
  flowchart:
    wrappingWidth: 400
---
flowchart TD
    S[<b>The server, since #576</b>]
    S --> O[<b>GitOperation::CherryPick</b><br/>in the closed vocabulary]
    S --> E[<b>exec_cherry_pick</b><br/>runs it against real git]
    S --> P[<b>Preview engine</b><br/>ADR 0099 draws the result]
    S --> T[<b>Contract test</b><br/>proves the pick lands]

    O --> G{<b>Can the app ask?</b>}
    E --> G
    P --> G
    T --> G
    G --> N[<b>No — there was no route<br/>and no menu item</b>]
    N --> U[<b>Indistinguishable from<br/>a feature that does not exist</b>]

    KEY[<b>LEGEND</b><br/>green - capability that already worked<br/>red - the gap this ADR closes]

    classDef good fill:#e8f5e9,stroke:#2e7d32,stroke-width:3px,color:#1b5e20
    classDef bad fill:#fdecea,stroke:#8b1a10,stroke-width:3px,color:#5c110a
    classDef neutral fill:#eef2f7,stroke:#33475b,stroke-width:3px,color:#16202b
    classDef legendbox fill:#f4f4f4,stroke:#666666,stroke-width:2px,color:#333333

    class O,E,P,T good
    class N,U bad
    class S,G neutral
    class KEY legendbox
```

A test suite tests what exists. Both #594 and #596 were invisible to 1,100
passing tests because both were *absences*, and nothing in a suite fails for a
thing that was never written.

## Decision

### 1. The door is a dedicated route, not the generic plan endpoints

`POST /api/plan` accepts an arbitrary `GitOperation` and `POST /api/execute-plan`
runs the `Plan` it returns. Both already existed. Cherry-pick could therefore
have been reached from the frontend with **no new server code at all**.

That is rejected. The frontend uses `/api/plan` only as a *read* — the
force-push entry point reviews a lease with it — and has never used
`/api/execute-plan` for anything. Every mutation it performs goes through its
own route, and four separate properties ride on that:

```mermaid
---
config:
  flowchart:
    wrappingWidth: 400
---
flowchart TD
    C[<b>A cherry-pick the user confirmed</b>]
    C --> A[<b>Dedicated route</b><br/>POST /api/cherry-pick]
    C --> B[<b>Generic pair</b><br/>/api/plan then /api/execute-plan]

    A --> A1[<b>Idempotency key</b><br/>a retry cannot pick twice]
    A --> A2[<b>Offline / visualize guards</b><br/>refused before the transport]
    A --> A3[<b>Operations registry</b><br/>tracked, reportable, resumable]
    A --> A4[<b>Route authz census</b><br/>classified SessionAndCsrf]

    B --> B1[<b>None of the four</b><br/>a write wearing a read's clothes]

    KEY[<b>LEGEND</b><br/>green - what a route buys<br/>red - what the shortcut costs]

    classDef good fill:#e8f5e9,stroke:#2e7d32,stroke-width:3px,color:#1b5e20
    classDef bad fill:#fdecea,stroke:#8b1a10,stroke-width:3px,color:#5c110a
    classDef neutral fill:#eef2f7,stroke:#33475b,stroke-width:3px,color:#16202b
    classDef legendbox fill:#f4f4f4,stroke:#666666,stroke-width:2px,color:#333333

    class A1,A2,A3,A4 good
    class B1 bad
    class C,A,B neutral
    class KEY legendbox
```

The generic pair keeps its place as a *planning* surface — build a plan, read
its risk, show the user what it means. It is not the execution path for the UI,
and this ADR is the record of that being a decision rather than an oversight.

### 2. The destination is read live, and "could not tell" stays its own answer

`OperationKind::CherryPick { commit, onto }` carries `onto: HeadBranch`, read
when the menu item is clicked — not from the graph, which can be stale. A pick
lands on whatever branch is checked out, never on the row that was tapped, so
the confirmation must name the real destination.

`HeadBranch` keeps three states and the dialog keeps them apart:

```mermaid
---
config:
  flowchart:
    wrappingWidth: 380
---
stateDiagram-v2
    [*] --> Reading: item clicked
    Reading --> Known: HEAD is a branch
    Reading --> Detached: read said detached
    Reading --> Unknown: read failed

    Known --> Offered: confirm enabled,<br/>names the branch
    Detached --> RefusedD: refused —<br/>the copy would be unreferenced
    Unknown --> RefusedU: refused —<br/>and says the read failed

    note right of RefusedU
        A failed read is NOT
        evidence of a detached HEAD.
        Collapsing them is the defect
        HeadBranch exists to prevent.
    end note
```

### 3. A blocked item says so, on screen — it does not vanish

`cherry_pick_offer(is_head, is_stub)` returns `Offered` or `Blocked(reason)`, and
a blocked item renders with its reason as **visible text**, not only a `title=`.
This is #65's rule, and #596 is the argument for it: an absent item taught Tom
that the app could not cherry-pick at all.

Two conditions block, and only two:

| Condition | Why | Knowable without the server? |
|---|---|---|
| Branch stub | A stub names a branch; a pick takes one commit | yes |
| The commit is HEAD | The empty pick — a *failure*, not a no-op | yes |
| Already an ancestor of HEAD | Same empty-pick failure | **no** |
| A merge commit | Needs `-m <mainline>`; that is `CherryPickMerge` | **no** |

The last two are deliberately *not* pre-empted. Answering them needs a
repository read, and one already exists: the confirm dialog's `/api/preview`
panel, which returns the empty-pick refusal verbatim and `Unsupported` for a
target with no sole parent. Guessing locally would mean a second implementation
of a question the server already answers exactly — the modelling failure ADR
0099 rejected, one layer up.

### 4. The copy states what failure leaves behind

Merge and revert either happen or do not. A cherry-pick has a third outcome:

```mermaid
---
config:
  flowchart:
    wrappingWidth: 400
---
flowchart TD
    R[<b>git cherry-pick COMMIT</b><br/>the executor passes NO --allow-empty]
    R --> OK[<b>Clean</b><br/>a new commit on the current branch]
    R --> CF[<b>Conflict</b><br/>exit 1]
    R --> EM[<b>Change already present</b><br/>exit 1 — the previous cherry-pick<br/>is now empty]

    CF --> MID[<b>Mid-sequence</b><br/>CHERRY_PICK_HEAD written<br/>HEAD where it was]
    EM --> MID
    MID --> FIX[<b>Needs --skip or --abort</b><br/>at a terminal —<br/>this app cannot do it yet]

    KEY[<b>LEGEND</b><br/>green - the success path<br/>amber - measured on this host, 2026-08-30<br/>red - state the app cannot clear]

    classDef good fill:#e8f5e9,stroke:#2e7d32,stroke-width:3px,color:#1b5e20
    classDef warn fill:#fff4e5,stroke:#a15c00,stroke-width:3px,color:#5c3400
    classDef bad fill:#fdecea,stroke:#8b1a10,stroke-width:3px,color:#5c110a
    classDef neutral fill:#eef2f7,stroke:#33475b,stroke-width:3px,color:#16202b
    classDef legendbox fill:#f4f4f4,stroke:#666666,stroke-width:2px,color:#333333

    class OK good
    class CF,EM warn
    class MID,FIX bad
    class R neutral
    class KEY legendbox
```

The dialog says all of it, including the last box. Promising a recovery the UI
does not have is the same defect `revert_message` exists to prevent one layer
down: telling the user something the tool will not actually do.

## Alternatives considered

**Drive `/api/plan` + `/api/execute-plan` from the frontend.** Zero new server
code. Rejected in §1 — it silently drops idempotency, both api.rs guards, the
operations registry and the authz classification. A write that looks like a read
to every census that watches writes.

**Put the item on the branch menu instead.** A pick's subject is a commit, not a
ref. The branch menu already offers merge, which takes a branch; putting a
commit-shaped operation beside it would make the menu's own grammar inconsistent.

**Block ancestors of HEAD by fetching the merge base on click.** A second
repository read per menu open, to answer a question `/api/preview` already
answers exactly and for free, inside the dialog the user is about to see.

**Also give revert a first-class commit-menu entry.** #596 raises it and it is
real — revert reaches the UI only as an Activity-panel undo hint. Deliberately
left out: it is a separate operation with its own copy and its own failure mode,
and folding it in would have made one reviewable change into two.

## Consequences

- **Protocol.** One new DTO, `CherryPickRequest { commit }`, with
  `deny_unknown_fields`. The id is the one the user reviewed and is never
  re-resolved through `rev-parse` — the same posture `AmendCommitRequest`'s
  `expected_tip` takes.
- **Three censuses gained a row**, and each one caught the omission before a
  human did: `offline_guard_audit`'s `OFFLINE_GUARDED`, `contract_suite`'s POST
  route and planner funnel tables, and `route_authz`'s `ROUTE_AUTHZ`
  (`SessionAndCsrf`, like every other local git write). This is what those
  censuses are for and they worked.
- **Cherry-pick inherits the #594 preview panel for free.** Routing through
  `shell.open_confirm` is the whole mechanism; no preview code was written here.
- **`CherryPickMerge` still has no door.** It needs a mainline, which needs a UI
  for choosing one. The plain route rejects nothing about it — a merge commit
  sent to `/api/cherry-pick` gets git's own "is a merge but no -m option was
  given", forwarded verbatim, rather than a guess.
- **The mid-sequence state remains unrecoverable from the app.** `SequenceSkip`
  and `SequenceAbort` exist in the vocabulary with no route and no UI. That is
  now a *stated* gap in user-facing copy rather than a silent one, and it is the
  obvious next slice.
- **The confirm modal has no `role="dialog"` and no class**, which the browser
  spec had to work around by locating it via its own buttons. Not changed here —
  it affects every dialog in the app — but recorded as a real accessibility
  finding for whoever picks it up.

## Verification

- Host tests for the gate and the copy in `features/dialogs/core.rs`, **mutation-
  proved six ways** — two differently-shaped mutations against each of three
  invariants (the HEAD gate, the mid-sequence warning, and the
  detached-vs-unreadable distinction), every one caught, each with a green
  baseline in the same invocation.
- A browser spec, because `cargo test` never executes `crates/git-vista/src/menu/`
  — it is wasm-gated, so a fully green suite is compatible with the item simply
  not being rendered.
- A real drive against a purpose-built repository — server, wasm app, real git,
  nothing mocked: the pick landed on `main` as a new commit, the file appeared,
  no `CHERRY_PICK_HEAD` was left behind, and the original commit stayed on its
  own branch.

---

**Signed:** fable · 2026-09-01T10:45:00-04:00
