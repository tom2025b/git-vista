# Git-Vista — working agreement for Claude sessions

Read this first. It exists so two Claude accounts (used one at a time, alternating)
never lose work to a token-out and can always resume a half-finished milestone.

## The one rule that matters: commit WIP often

Sessions run out of tokens mid-task. **Uncommitted work is the only work that gets
lost.** So, on anything non-trivial:

- Checkpoint frequently with **`./dev wip [msg]`** — it adds, commits (as
  `claude_2010`), and pushes. Every push is a durable checkpoint on GitHub that the
  next session, on either account, can resume from. Commit even if incomplete or
  mid-refactor; a WIP branch is cheap, redone work is not.
- Keep **`handoff.md`** current as you go (gitignored, repo root): the goal, a
  `[ ]`/`[x]` checklist, and an explicit **"next step."** This is the human-readable
  map; the git commits are the durable bytes.
- Resume with **`./dev resume`** — shows the branch, the WIP commits not yet on
  main, uncommitted changes, and `handoff.md`.

## The `./dev` commands (bash, no install needed)

| Command | What it does |
|---|---|
| `./dev roadmap` | Milestones by M-band + next open issues. |
| `./dev start <issue#>` | Branch off fresh `main` as `feature/mX.YY-slug`. |
| `./dev wip [msg]` | Checkpoint: add + commit (claude_2010) + push. |
| `./dev resume` | Branch state + WIP commits + `handoff.md`. |
| `./dev gate` | Full CI gate: fmt, clippy (native + wasm), test, `trunk build`. |
| `./dev signin` | Reprint the server sign-in link(s). |
| `./dev serve [--lan] [path]` | Start the server (delegates to `./gv`). |

## Milestones & issues

- Milestones are named `M1 …` through `M8 …`; the active one is **M1 — Foundation**.
  `./dev roadmap` is the source of truth for what's done and what's next.
- One issue → one `feature/mX.YY-slug` branch → one PR that says `Closes #<issue>`.
  Branch-per-issue keeps the two accounts from colliding.
- **Before starting an issue, assign it to yourself on GitHub** (or add an
  `in-progress` label) so the other account sees it's taken.

## Definition of done for a milestone

1. `./dev gate` is green (all five checks).
2. Verify the change actually runs (drive it, don't just trust tests) — for server/
   auth work that means a live server + a real request, per the security model.
3. Commit, push, open the PR (`Closes #<issue>`), merge to `main`, delete the branch.
4. If it implements something in `docs/SECURITY_MODEL.md`, add/keep an ADR under
   `docs/adr/` and annotate the model where implemented.

## Commit identity

Claude's commits use author name **`claude_2010`** with the
`262510778+tom2025b@users.noreply.github.com` email (distinguishes them from Tom's
`tomb` commits in `git log`; the noreply email avoids GitHub's GH007). `./dev wip`
and the manual commit path both set this per-commit.
