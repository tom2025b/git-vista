# Pro result — task 4 (leptos upgrade reconnaissance)

**Status:** NOT STARTED

Pro: overwrite this whole file when you finish. Keep the headings — they are
what the orchestrator reads. Paste real evidence, not a description of it.

(Tasks 1, 2, and 3 are merged — PR #157, PR #159, PR #160. Not this task's
work.)

## Status

`not started` | `in progress` | `done` | `blocked`

## Summary

(2-4 sentences: what you found and the headline recommendation. If the
premise is invalidated — upgrading doesn't remove the advisories — say that
in the first sentence, not buried below.)

## Q1 — Does upgrading actually remove the three advisories?

(Evidence from actual dependency trees at 0.7, 0.8, 0.9-beta — not
inference. This is the question that can invalidate the whole task.)

## Q2 — Target version recommendation

## Q3 — What actually breaks (per-file inventory)

## Q4 — Core-vs-glue split, quantified in lines

## Q5 — Toolchain implications (trunk / wasm-bindgen)

## Q6 — Scope estimate (S/M/L/XL) and risks

## Document produced

(Path to the .md and .pdf under design-docs/)

## Commands run, with real output

```
$ git status   (worktree should show only the new doc + pdf)
<paste>

$ git diff --stat -- Cargo.toml Cargo.lock   (must be empty)
<paste>
```

## Acceptance criteria

(copy from pro-task.md, tick honestly)

## Findings

(anything the orchestrator should know, including things outside scope)

## Questions / blockers

## Commit SHA(s)

## PR

(None expected — design-docs/ is gitignored. Say so.)

---

**Signed:** (thomas2010 · ISO timestamp)
