# Cloud handoff — #478, WIP folding across interleaved chains

**Written:** 2026-08-25 · **By:** max (CLI session on Tom's box) · **For:** a cloud Claude Code session on `tom2025b/git-vista`

> Paste everything below the line into a fresh cloud session started on this
> repository. It is self-contained: it names no file the cloud cannot see.

---

```yaml
task_id: gv-478-interleaved-wip-folding
issue: 478
repo: tom2025b/git-vista
base: main                      # at or after the merge of PR #480
branch: feature/issue-478-wip-folding-interleaved-chains
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max          # the literal tag in every ADR / doc signature line
allowed_paths:
  - crates/git-vista/src/features/graph/collapse.rs
  - crates/git-vista/src/render/nodes.rs
  - crates/git-vista/src/render/edges.rs
  - crates/git-vista/src/app/canvas.rs
  - crates/git-vista/src/state.rs
  - crates/git-vista/src/menu.rs
  - ci/browser/fixture.mjs
  - ci/browser/tests/wip-collapse.spec.mjs
  - ci/browser/tests/harness-selfcheck.spec.mjs
  - docs/adr/               # new ADR + README index row + pdf/ if you can render
forbidden_paths:
  - design-docs/            # untracked here; it does not exist in your clone
  - handoff.md              # same
  - crates/git-vista-server/**   # this defect is entirely display-space
acceptance:
  A1: two diverged chains that interleave in display order each fold into their own group
  A2: a genuinely contiguous run still folds exactly as today — existing tests unchanged
  A3: checkpoints from two DIFFERENT chains are never folded together, even when adjacent
  A4: the projection still emits exactly one uniform slot per display item
  A5: an open run can still be re-folded from its own menu item
```

---

## What is wrong

`collapse::project` walks `rows` in **display order** and extends a run only
while *adjacent* rows satisfy `same_run`:

```rust
fn same_run(newer: &GraphRow, older: &GraphRow) -> bool {
    is_wip_checkpoint(&newer.commit.summary)
        && is_wip_checkpoint(&older.commit.summary)
        && newer.lane == older.lane
        && newer.commit.parents.len() == 1
        && newer.commit.parents[0] == older.commit.id
        && older.commit.parents.len() <= 1
}
```

A branch and its **diverged remote-tracking twin** both carry checkpoint chains
with the same summaries. The graph orders rows by date, so the two chains
interleave perfectly, and every display-adjacent pair is (chain A, chain B):
different lane, and not parent-and-child either. Every run measures 1, `MIN_RUN`
is 2, nothing folds — on one of the longest checkpoint runs in the repository.

**The predicate is correct about every pair it is shown.** It is simply never
shown a pair from the same chain. `collapse.rs` already asserts in a comment that
"every member shares a lane" — true of a *run*, false of the *scan that finds
runs*, and this is the case that separates them.

Tom found this by scrolling the real app, and named the cause before anyone read
the code.

## The direction — and the trap

**Scan the parent links, not display positions.** Build the chains from ancestry,
then fold each chain into one slot at its newest member, dropping the chain's
other members from display space.

**Do not relax the lane check to make adjacent rows match.** It would fold two
different branches' checkpoints into one group and claim a chain that does not
exist. A visible annoyance traded for a quiet lie is a bad trade. A3 is the
acceptance criterion that exists to stop exactly this, and it is load-bearing.

## The analysis that has already been done here — verify it, do not trust it

This was investigated in the CLI session before being handed off. Treat every
claim below as a lead to check against the source, not as fact; plan citations
in this repository have been wrong six times, including a function that never
existed.

**The structural obstacle.** `DisplayItem::WipGroup { start_row_index, count, .. }`
replaces a **contiguous span**, and `display_of_row` decides membership with a
range check `row >= start && row < start + count`. A scattered run has no span,
so that range check silently mis-maps the moment members are not adjacent. Any
fix that leaves that field name in place will compile and be wrong.

**A shape that appears to work**, offered as a starting point rather than a
decision:

- Rename the field so the compiler forces every consumer to be revisited —
  `anchor_row_index` (the run's **newest** member), keeping `count`, `lane`,
  `color`. `DisplayItem` stays `Copy`, so `render/edges.rs` and `render/nodes.rs`
  keep their `.copied()` reads.
- `WipRun` becomes `{ anchor_row_index, rows: Vec<usize> }`, so it stops being
  `Copy`. Consumers to fix, all found by grep: `state.rs` (`MenuData.wip_run`),
  `menu.rs` (`Callback<WipRun>`), `app/canvas.rs` (`on_fold_wip` currently
  iterates `start..start+count` to clear the expanded set — it must iterate
  `run.rows`).
- Give `DisplayProjection` a `row_to_display: Vec<Option<usize>>` built once
  during projection, and back `display_of_row` with it. This is what makes
  membership correct for scattered runs, and it also removes the existing
  quadratic edge remap (a linear scan over items, per edge).
- Build the chains with an `Oid -> row_index` map. For each checkpoint row with
  exactly one parent, look the parent up; if `same_run(row, parent_row)` holds,
  link them. Then walk each chain from its head.

**Two hazards in that construction, both worth a test:**

1. **A parent claimed by two children.** Two branches can descend from the same
   checkpoint commit. If a row is the same-run parent of more than one row, the
   chain must **break** there rather than picking a winner — picking one would
   splice two branches' histories into a single group, which is A3 again in a
   different costume.
2. **`expanded` membership.** `project` currently treats a run as open when
   *any* member index is in the expanded set, and advances a whole run at a time
   so an opened run does not immediately regrow a smaller group from its tail
   (that regression is pinned by two existing tests — read their comments before
   touching the loop). Preserve both properties with scattered members.

**What is expected NOT to need changing**, but check rather than assume: the
geometry and the culler. Every display item still occupies exactly one
`ROW_HEIGHT` slot, and `viewport::visible_row_range` / `geometry::node_cy` only
assume a fixed stride over *some* item count (A4). Removing non-adjacent rows is
a different operation from collapsing a span, so this deserves a look, not a
shrug.

## Tests you must write

`collapse.rs` is framework-free and host-tested, so the core work is `cargo test`
territory. Add to its existing `mod tests`:

- **The interleaved case (A1).** Two chains of three, alternating in row order,
  different lanes, each row's sole parent being its own chain's next member.
  Assert two `WipGroup`s of three, and that no group's members come from both
  chains.
- **The negative (A3), and this is the one that carries the weight.** Two
  checkpoint rows that are display-**adjacent** but belong to different chains —
  different parents — must produce two `Single`s, never a group. A test that only
  checks "the interleaved case now folds" passes against a version that folds
  everything adjacent, which is precisely the wrong fix.
- **A parent with two same-run children** — the chain breaks, nothing is spliced.
- **Regression (A2):** the existing tests must pass **unmodified**. If you find
  yourself editing `a_run_of_three_wips_folds_into_one_group`,
  `a_lone_wip_commit_is_not_grouped` or
  `a_run_broken_by_a_real_commit_becomes_two_groups` to make your change pass,
  stop — the change is wrong, or the behaviour moved and needs Tom's decision.

**Prove the new tests can fail, two different ways each, and say so in the PR.**
The `failure-atlas` MCP that normally does this is a local server you will not
have. Do it by hand and be explicit that it was by hand: apply the mutation,
run the target, record the verdict, revert. Pick mutations that fail
*differently* — remove the mechanism, and weaken it. One mutation is not proof: a
test in this repo once survived a mutation that swapped a verb (the assertion was
inert) and caught the one that removed a marker. Either alone gives the wrong
answer.

Suggested pairs: (a) drop the "two children breaks the chain" guard; (b) drop the
parent-link requirement so any two same-lane checkpoints link.

## The browser half

`cargo test` never compiles `mod app`, so the Rust suite proves the projection is
**correct** and the browser suite proves it is **reached**. `ci/browser/` is the
harness; read its `README.md` first — it explains why it runs inside
`unshare --net` and what that costs.

Extend `ci/browser/fixture.mjs` with a repository holding a branch and a diverged
remote-tracking twin, both carrying checkpoint chains, so the interleave is real
rather than simulated. Follow the existing pattern: separate repo per shape, with
a comment saying which defect the shape exists for. Then extend
`wip-collapse.spec.mjs`.

`harness-selfcheck.spec.mjs` requires every assertion in the suite to be shown
going red against a deliberately-broken DOM. **Add the matching entry** — that
file's own header says to, and it earned its place by catching a vacuous mutation
of itself on its first run.

**If the browser leg cannot run in your environment, say that plainly in the PR
and do not describe the gate as green.** It needs Node ≥ 20, a Chromium build
under `~/.cache/ms-playwright`, and an unprivileged user namespace. Whether that
works in a cloud session is genuinely unknown here — finding out is useful
information in itself, so report what happened either way. A leg you could not
run is a *result*, and reporting it is worth more than a green summary that
quietly omits it.

## House rules that bind this task

The repository's `CLAUDE.md` is tracked and you will have it. The parts that
differ in a cloud session, or that are easy to get wrong:

- **`buildlock` is a local wrapper that does not exist in your environment.**
  Run `cargo` and `trunk` directly. Do not try to install it.
- **Commit identity per commit**, exactly:
  `git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com commit …`
  Never let a gmail address near a commit; this repository has local git config
  that will supply one if you let it.
- **Branch → PR → merge, and NEVER delete a branch**, local or remote. The
  walkable history is teaching material. No force-push, ever.
- **Write an ADR** under `docs/adr/NNNN-slug.md`, add its row to
  `docs/adr/README.md`, and sign it `max`. The next free number is whatever is
  actually free when you look — check, do not assume. The decision worth
  recording is *how a run is identified* and why relaxing the lane check was
  refused.
  - Diagrams: every `classDef` that sets a `fill` must also set a `color`, and
    node titles use `<b>title</b>` in a plain label — never `**bold**` inside a
    backtick label, which ignores the class colour entirely and renders
    unreadable on GitHub in dark mode.
  - If you cannot render the PDF twin into `docs/adr/pdf/`, say so in the PR;
    Tom's box will render it.
- **`design-docs/` is gitignored and is not in your clone.** Do not create it,
  and do not reference it in anything you write.
- **There is no live server to drive**, and nothing you do should assume one.
  Tom drives the result on his own machine.

## What "done" looks like

A pushed branch, a PR that says `Closes #478`, and a body that states: what the
chain-building rule is, which tests were added, the **two hand-run mutations per
new test with their verdicts**, which gate legs actually ran and which did not,
and anything you found that this handoff got wrong. That last item is expected
rather than optional — the analysis above was written without touching the
render path.

---

**Signed:** max · 2026-08-25T07:05:00-04:00
