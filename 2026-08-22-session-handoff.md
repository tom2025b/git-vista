# Handoff — paste this into the new session

Session ran 2026-08-22, 05:40 to ~15:45 EDT. Nothing below needs the old
transcript. Every claim here was verified at 15:45, not recalled.

## Read this first: yesterday's handoff lied, and that is the lesson

The previous handoff stated things that were false — that M4 was at 0%, that a
branch was unpushed when it was pushed, that a cleanup was complete when it
was not. Three separate wrong claims, each written from memory rather than
measurement, each of which cost real time today.

**So: verify before you assert.** `gh api`, `git log`, `milestone-bars`. If
you find something in this file that does not match the repository, the
repository is right.

## State, measured at 15:45

| | |
|---|---|
| `tom2025b/Git-Vista` | **PUBLIC** (flipped today), 0 forks, 0 stars |
| Actions | enabled, and **free** now the repo is public |
| `main` | `3a562941`, clean tree, gate green and gatehouse-verified |
| M4 | **55.6%** — 5 shipped, 4 open, 2 cut |
| global `user.email` | `262510778+tom2025b@users.noreply.github.com` |

## What shipped today

| PR | What |
|---|---|
| 435 | `fix(#434)` the gate can fail again + ADR 0065 |
| 437 | `fix(#436)` the frontend compiles again |
| 439 | `M4.31a (#428)` inspect a conflict — four panes + ADR 0066 |
| 440 | `M4.31b (#429)` whole-file resolution + ADR 0067 |
| 441 | `chore` redact the personal email from tracked docs |

## Next work: the rest of M4.31

Three sub-issues remain under #84.

1. **#430 — M4.31d, binary/rename/delete UX.** The natural next one.
   `NotTextResolvable` already models all three cases; this is rendering them
   distinctly instead of lumping them together. Low risk, and #428/#429 built
   the surface it renders into.
2. **#431 — M4.31e, survives reconnect and crash.** Medium. Leans on the
   operation lifecycle rather than on new conflict logic.
3. **#432 — M4.31c, block and line resolution + manual editing.** **Needs an
   ADR before any code.** It carries file content through a `Plan`, which is
   hashed, reviewed and replayed. Fable's independent review already exists:
   `~/projects/_claude-outputs/2026-08-22_fable-conflict-content-transport.md`.
   Its sharpest finding: an editor seeded from the working-tree marker file is
   invisible to both the porcelain generation and the index checksum, so it
   must be digested into a `conflict-v1:` token the way `diff-v1:` digests
   patch bytes.

Then #84 closes and M4 finishes.

## Loose ends

- **`ci/allow-manual-dispatch` is committed locally but NOT pushed.** GitHub
  refused it: this token lacks the `workflow` scope, so it cannot push changes
  under `.github/workflows/`. Tom pushes it himself, or drop it — it only adds
  `workflow_dispatch`.
- **CI proven working post-quota, 2026-08-22.** Tom re-ran the stuck 08-20
  scheduled run by hand (attempt 2, `run_started_at` 20:00 UTC) — all 7 jobs
  green with real durations (10s–13m43s), not the 4-second zero-step shape.
  Run: `github.com/tom2025b/git-vista/actions/runs/32337083677`. One caveat:
  the workflow file on `main` still only triggers on `push`/`pull_request`/
  `schedule` — `workflow_dispatch` (the pending `ci/allow-manual-dispatch`
  commit, unpushed, needs the `workflow` token scope) is still not there, so
  today's re-run worked because an *existing* run could be manually re-run,
  not because on-demand triggering exists yet.
- **Secret scanning + push protection are OFF** and are **free on public
  repos**: `github.com/tom2025b/Git-Vista/settings/security_analysis`. Push
  protection is the guard that would have stopped the committed sign-in
  tokens. Worth suggesting once.
- **#438**, flaky: `recovery_center::a_stale_claimed_undo_is_refused…` fails
  roughly one full-suite run in three. Found by the repaired gate.
- **~929 MB of build artifacts** in history on two feature branches. Not in
  any current tree, so a normal clone does not fetch it.
- **`backupsage` is public with one gmail-authored commit** (bd50e227, April
  8). Tom's call whether to care.

## The privacy incident — settled, do not re-litigate

Half of today went to this. It is closed. Do not reopen it, and do not repeat
the wrong versions:

- **No credential was ever leaked.** Three independent scanners agree: max's
  pattern scan, fable's object-database scan, and `gitleaks` over 3,436
  commits. Sign-in tokens *were* committed in console logs — Tom's memory was
  right — but they are single-use, already redeemed, loopback-only, and not
  reachable from any ref GitHub serves. **Nothing to rotate.**
- **The root cause was the global git config**, which still said
  `thomaslane2025@gmail.com` and was inherited by 39 of 50 repos. Fixed today.
  That is why every prior cleanup "failed": they scrubbed output and left the
  tap running.
- **CLAUDE.md's never-touch-the-global-config rule is obsolete** — it existed
  to protect Tom's name on his own commits, and `user.name` is still `tomb`.
- **The guard hook was blocking 6 of 13 commit paths.** `rebase`, `merge`,
  `cherry-pick`, `revert`, `stash` and `am` all walked past it, and `rebase`
  re-stamps the committer on every replayed commit — which is why ~149 commits
  became 298 identity occurrences. Now 12 of 13. Probe:
  `/tmp/claude-1000/hookprobe.py`.
- **CLAUDE.md says "91 commits on a public main."** The real number was 298
  identity occurrences. Both max and fable measured it. The file is stale.
- **Tom retired the old address from GitHub** and hardened his Google account
  (60 → 17 OAuth grants, new password, new recovery email, landline recovery
  phone, regenerated backup codes on paper, passkeys). The addresses left in
  history are inert strings.

## Two corrections worth carrying forward

Both came from Tom saying **"check it a different way."** It changed the
answer both times:

- The `backup/main-with-gmail-2026-08-04` branch was described as a redundant
  gmail archive. It is not: 379 commits, 364 already on a local branch, and
  the **15 unique ones are all clean noreply**. Deleting it would have cost
  commits and bought zero privacy. **Kept.**
- Deleting the 1,110 `refs/replace/*` removed the *index*, not the *objects*.
  Verified afterwards: GitHub still serves commit `004c1136` by SHA, authored
  with the gmail. A real reduction, not a fix. The 230 `refs/pull/*` are
  GitHub-managed and undeletable.

## How to work here

- **`./dev gate` can fail again** (#434/ADR 0065). Trust it now. Prove it with
  `ci/gate_errexit_test.sh`.
- **`./dev browser` does NOT run `trunk build`.** Run trunk first or you test a
  stale bundle.
- **Merged is not deployed.** `trunk build` rewrites `crates/git-vista/dist/`;
  a hard reload reaches Tom. Never restart the server on 8080 — it rotates his
  token.
- **`buildlock` every cargo/trunk invocation.**
- **A WIP checkpointer owns the git index.** Do not fight it; it commits on a
  timer.
- **Mutations: two minimum, failing differently.** Today one survived and
  exposed a genuinely inert test (`with_content`'s guard, only the `Ok` path
  tested). Another survived and drove a design change: a handler-side
  `WorktreePath::new` call was skippable, so the DTO field became a
  `WorktreePath` and the check is now structural.
- **Say when a stretch is docs-only** so Tom can drop to a cheaper model. This
  was not done today and it cost him money. ADRs, briefs, PR bodies, issue
  bodies — all prose.

## Tom, right now

He was exposed by repeated Claude failures and was rightly furious. He is
calm now and the incident is resolved, but **do not be breezy about security
here** — he was seriously compromised in 2023 across several devices. Name the
surface, say who it opens it to, and say what still holds.

What he actually wants: **his work public, with credit.** Git-Vista is public
as of today. `linux-ops-suite`, `backupsage` and the MCP repos are candidates
he has named.

**Signed:** max · 2026-08-22T15:45:00-04:00
