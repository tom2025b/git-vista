# Work queue — for Codex and thomas2010, while thomas2025 is out ~2 days

This account is out until Tue night. **Written 2026-08-01, verified against the live
repo right before writing** — the 32 M2 sub-issues were filed tonight; 3 are already
closed. Do not trust an older copy of this file if one exists.

**Signed:** thomas2025 · 2026-08-01

---

## Read first

1. `handoff.md` (repo root) — tonight's session in full, what's verified vs. just claimed.
2. `design-docs/2026-08-01-issue-65-sheet-decision.md` — why #65's sheet was split off.
3. This file.

## Ground rules, each earned tonight, not theoretical

- **The 60s checkpointer is the sole git writer.** `pgrep -af autocheckpoint` — restart
  it if dead (it dies silently on branch switches; happened at least twice tonight).
  Continue the `auto-checkpoint N` series from `git log --all`, **not**
  `git log --oneline -200` — the bounded form undercounts past a merge.
- **Never delete a branch.**
- **A green test that proves nothing is worse than a red one.** Before trusting a pass,
  ask what would make it pass while the mechanism was broken.
- **A ruleset now protects `main`** (`20171903`, all 7 CI checks required, active). It
  was broken for the first hour it existed — required a check name GitHub truncates —
  found and fixed. If a merge is unexpectedly blocked, check the actual check names
  match what the ruleset requires before assuming it's your PR's fault.
- **CI re-runs on every rebase/merge onto an already-updated `main`.** The ruleset
  requires an up-to-date branch, so expect a second CI wait on merge — not a hang.

---

## Immediate — #65's sheet, sitting mid-handoff

`/home/tom/projects/Git-Vista-codex`, branch `codex/65-sheet-wiring`, commit `8043ca1`.
Codex reports full gate green, 271 tests, independent review clean. **Verified
independently tonight**: real commit, pushed, matches origin, output files exist. One
false alarm already ruled out — `symlinked_exclude_containment.rs` fails when run from
inside *that worktree* specifically (gitlink `.git` file, not directory) but passes
identically in the primary checkout; Codex never touched that test file. Not a real
failure, don't re-chase it.

**Not yet done:** the PR was never opened (Codex asked 3 options, thomas2025 recommended
"open the PR" but stopped before executing so a person could decide fresh). Do that:
push already done, just needs `gh pr create` + normal CI + merge.

**#65 does not close even after that merges** — it needs a human iPad pass regardless
(drag feel, gesture smoothness, nothing here is verifiable from a terminal). That step
is Tom's, not an agent's.

## M1 status

**39/40.** Only #65 remains, blocked on the above.

## M2 — real count, verified just now: 29 of 32 open

Three closed tonight: #221 (batch cat-file), #241 (offline guard), #243 (offline docs).

### The one scheduling fact that matters more than anything else

ADR 0016's write funnel — `git-vista-protocol/src/plan.rs`, `planner.rs`,
`sandbox/mod.rs::network_need_for_operation`, `planner/contract_suite.rs` — is touched
by nearly every "typed operation vocabulary" issue below. **Two agents editing any of
these four files at the same time will collide.** Serialize anything that touches them;
parallelize everything else.

### Genuinely parallel right now (verified no shared files)

| Issue | What | Model/effort |
|---|---|---|
| #226 | Commit draft persistence across tab suspension | sonnet / low-med |
| #245 | MCP crate scaffold + loopback auth handshake | sonnet / medium — **new crate, review before anything builds on it** |
| #242 | Wire offline state into mutation UI (banner, disable actions) | depends on #241, which is now closed — unblocked |

### Funnel-touching — serialize, one owner at a time

#219/#220 (discard), #222–225 (amend), #227–233 (remote ops), #235–240 (tags),
#247–249 (MCP write path). Each parent's `a` slice (vocabulary) must land before its
siblings; read each issue's own `Depends on` line, it was verified against the repo
when filed.

### Close-out issues — do last, not first

#234, #244, #250 verify their parent's acceptance criteria against what actually shipped.
Filing these before the real work is done produces nothing; they're listed for
completeness of the 32, not as something to start on.

## Known debt, not blocking, worth an issue if nobody's filed one

- `crates/git-vista/src/api.rs` and everything `menu.rs`-adjacent: zero executed test
  coverage, structurally — both `#[cfg(target_arch = "wasm32")]`-gated, no wasm test
  harness in this repo. Bit real work twice tonight (#217, #241). Not a regression to
  fix reflexively, but real risk for anything touching those files blind.
- Print Graph builds the whole loaded history into one unvirtualized SVG — plausible
  cause of an iPad memory lockup tonight, never confirmed. If reproduced again, the
  distinguishing test is whether memory spikes sharply *at* Print vs. climbs steadily
  during ordinary scroll.
- `linux-ops-suite#96` — generalize `rex-check`/`gv-report` into an opsmcp
  `repo_report` tool. Prompt already committed:
  `~/projects/Linux-Ops-Suite/IMPLEMENT_PROMPT_repo_report.md`. Different repo, zero
  collision risk with anything above — good filler if Git-Vista work is blocked on
  review.
