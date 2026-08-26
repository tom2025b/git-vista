# CLOUD-2 · #551 — The decision spec for M12: how the app learns the repository changed under it

**Stage 1 of 4, and the gate for a whole milestone.** Five issues (#552–#556)
are explicitly blocked on this document. Nothing in M12 is built until it lands.

```yaml
task_id: gv-cloud-2-551
issue: 551
milestone: M12 — Reconciling External Changes
branch: claude/cloud-2-551-external-changes-spec
base: main
kind: DESIGN SPEC — a document, not an implementation
deliverable: docs/superpowers/specs/m3.26-external-changes.md   # tracked, plus its PDF
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max
allowed_paths:
  - docs/superpowers/specs/
  - docs/superpowers/pdf/
  - docs/adr/
forbidden_paths:
  - crates/                    # write NO code. This issue is the decision, not the build.
github_writes: open the PR. Do not merge, do not edit other issues.
```

## Why this is a document and not code

#79's five acceptance criteria each hide a design question with more than one
defensible answer. Building any one of them before the others are settled
produces a mechanism that has to be undone — this repository has paid for that
twice this month.

Model it on `docs/superpowers/specs/m3.23-worktrees.md`, which is tracked, in
this repo, and is the reason M11 could be split cleanly an hour ago. Same
shape: what exists today *checked rather than assumed*, the governing
constraint, numbered decisions, what this does NOT do, open questions,
alternatives considered, consequences.

## The five questions the spec must settle

1. **Watcher, sweep, or both** — and if both, **which is authoritative when they
   disagree.** A watcher that is *believed* with a sweep that *corrects* it is a
   different system from two equal sources, and the difference shows up exactly
   when something is already wrong.
2. **What is watched.** `.git/HEAD`, `.git/refs`, `.git/packed-refs`,
   `.git/index`, the worktree. Each has a different cost and a different miss
   mode. **`packed-refs` is the one that looks like it works**: a ref can change
   with no file under `refs/` moving at all, so a watcher pointed only at
   `refs/` misses every `git pack-refs`, every fetch that packs, and every gc.
3. **How self-generated writes are recognised.** The honest hard part. A flag
   the app sets before its own write is a flag that **stays set if the write
   panics** — and from then on real external changes are ignored and nothing can
   tell. Prefer a mechanism that *cannot get stuck* over one that is merely
   usually right. If a stuck-capable design is chosen anyway, its stuck state
   must be observable and self-clearing, and the spec must say so.
4. **What "stale" means to a plan already on screen.** See the finding below —
   this is narrower than it looks, and it is a security decision.
5. **The bound for a large repository**, and what is given up when it is hit. A
   watcher that silently stops watching is worse than one that never started.

## A finding to build on, verified in source — do not re-derive it wrongly

`generation_token(repo, observed)` in `crates/git-vista-server/src/planner.rs`
**already folds HEAD, every ref, `refs/stash` and the worktree status** into one
digest, and execution is admitted only while the live generation still equals
the plan's.

**So an external change already invalidates a plan — at execution time.** The
gap M12 closes is *promptness and honesty*, not safety. Today the user finds out
by being refused, and a plan sitting on screen after the repository moved looks
exactly like one that is still good.

Check this yourself against the source before relying on it. Then write the spec
knowing the safety property is already held, because a spec that proposes to
build it again is a spec that wastes a milestone.

## The rule that governs every one of the five

> **"I could not tell" must never render as "nothing changed."**

Every mechanism here has a failure mode where it stops seeing events, and every
one of those must be a **stated condition** rather than an absence. That is
already this repository's practice — `Obs`, `Advisory::DefaultBranchUnknown`,
`HeadBranch::Unknown`, `Blame::UnknownOperation` — and it is precisely the thing
a watcher design gets wrong by default, because a quiet watcher and a quiet
repository produce identical output.

## And the one that decides question 4

> **A plan that quietly re-derives itself is a plan the user did not approve.**

The options are therefore not equal. Telling the user their plan is stale and
offering to rebuild it preserves the approval boundary; silently rebuilding it
destroys the boundary while looking helpful.

There is a real edge worth deciding rather than ducking: a plan may be stale in
a way that does not matter (an unrelated ref moved) or in a way that changes
everything (the branch about to be force-pushed moved). That distinction is
*drawable* — `RefChange` names the refs the plan expects to move. Whether to
draw it is your recommendation to make.

## Diagrams — be generous, and follow the house rules exactly

At minimum: the watch/sweep interaction, and the self-write deduplication path.
More is better; prefer several small focused diagrams over one busy one.

**Every `classDef` that sets a `fill` MUST also set a `color`.** Without it the
diagram is perfect in the PDF and unreadable on GitHub in dark mode — measured
at 1.43:1 against a 4.5 floor across 230 style lines in this account's repos.
Node titles use `<b>Title</b>` in a plain label, **never** `**bold**` inside a
backtick label: bold in a backtick label ignores `color` entirely.

Render with `render-md-pdf`, put the PDF in `docs/superpowers/pdf/`, and verify:

```
pdftotext <pdf> - | grep -icE 'syntax error|mermaid version|unsupported markdown'
```

Zero, or it did not render. Then open it and look at a diagram page — the grep
cannot catch a diagram that rendered fine and is laid out badly.

## Acceptance

1. `docs/superpowers/specs/m3.26-external-changes.md`, tracked, PDF in
   `docs/superpowers/pdf/`.
2. All five questions answered, each with alternatives considered and the reason
   for the choice — not just the choice.
3. Diagrams as above, verified.
4. Open questions listed, and the ones that are **Tom's decision** named as his
   rather than left as findings.
5. An ADR number reserved for the decisions expensive to reverse. Next free
   number is **0092** — check `docs/adr/` before assuming.
6. **Every claim about existing code verified against the source, with a
   file:line.** Seven spec citations in this repository have been wrong. Cite
   nothing you have not opened.
7. A section saying plainly what the spec does NOT decide.

## What you must NOT do

- **Write no code.** Not a prototype, not a sketch, not "just the enum". The
  whole value of this issue is that it is decided before it is built.
- Do not decide anything that is Tom's to decide — name it and recommend.
- Do not merge your own PR.
