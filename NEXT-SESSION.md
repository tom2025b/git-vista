# Work queue — for Codex and thomas2010, while thomas2025 is out ~2 days

This account is out until Tue night. **Written 2026-08-01, verified against the live
repo right before writing** — this file was overwritten mid-session once already by a
branch checkout race; if you find a copy contradicting this one, trust `git log -1 --
NEXT-SESSION.md` for the real latest, and trust the live repo over any handoff doc.

**Signed:** thomas2025 · 2026-08-01

---

## Read first

1. `handoff.md` (repo root) — tonight's session in full, what's verified vs. just claimed.
2. `docs/adr/0035-inspector-bottom-sheet-wiring.md` — the sheet decision, just merged.
3. This file.

## Ground rules, each earned tonight, not theoretical

- **The 60s checkpointer is the sole git writer.** `pgrep -af autocheckpoint` — restart
  if dead. Continue the `auto-checkpoint N` series from `git log --all`, **not**
  `git log --oneline -200` (undercounts past a merge).
- **Never delete a branch.**
- **A green test that proves nothing is worse than a red one.**
- **A ruleset protects `main`** (`20171903`, all 7 checks required, active). Every PR
  now needs to be up-to-date against `main` before merge, so expect one extra CI wait
  on merge — not a hang.

---

## Top priority — a real bug found in live testing tonight, NOT root-caused

**#260 — clone appears to succeed but history/graph never updates.** Reported live by
Tom. `sandbox::clone_live`'s test proves the sandboxed git spawn works and produces a
real `.git` — it does NOT prove the HTTP response reaches the frontend or that the
frontend reacts (reload/select/epoch-bump). That's the leading suspect. A real clone
through the actual HTTP handler, end to end, has not been driven since tonight's
#216/#221 changes to `handlers/clone.rs`. **This may mean clone is currently broken for
real use** despite every existing test passing — start here before anything else.

## M1 status: 39/40

Only #65 remains open, and it's down to **verification, not code** now. All three code
gaps are fixed and merged tonight:
- `#258` — ARIA on 4 disabled menu items, 44px commit-dot hit target.
- `#259` / ADR 0035 — the inspector bottom sheet wired to `ShellMode`, drag follows the
  finger and resolves via `SheetState` on release. Built by Codex (recovered mid-session
  after it lost its own resume state), independently verified, merged.

**#65 does not close until a human drives it on the iPad.** Nothing about the sheet's
drag feel or on-screen correctness has been verified — that step is Tom's alone. If he's
available, that's the highest-value single action left in M1.

## M2 — 29 of 32 open (3 closed tonight: #221, #241, #243)

### The one scheduling fact that matters most

ADR 0016's write funnel — `git-vista-protocol/src/plan.rs`, `planner.rs`,
`sandbox/mod.rs::network_need_for_operation`, `planner/contract_suite.rs` — is touched
by nearly every "typed operation vocabulary" issue below. **Two agents editing any of
these four files at once will collide.** Serialize those; parallelize everything else.

### Genuinely parallel right now

| Issue | What |
|---|---|
| #226 | Commit draft persistence across tab suspension |
| #245 | MCP crate scaffold + loopback auth handshake — **new crate, review before anything builds on it** |
| #242 | Wire offline state into mutation UI — depends on #241, now closed, so unblocked |

### Funnel-touching — serialize, one owner at a time

#219/#220 (discard), #222–225 (amend), #227–233 (remote ops), #235–240 (tags),
#247–249 (MCP write path). Each parent's `a` slice (vocabulary) lands before its
siblings — read each issue's own `Depends on` line.

### Close-out issues — do last

#234, #244, #250 verify their parent's acceptance criteria against what actually
shipped. Starting these before the real work is done produces nothing.

## Known debt, not blocking

- `crates/git-vista/src/api.rs` and `menu.rs`-adjacent code: zero executed test
  coverage, structurally (`#[cfg(target_arch = "wasm32")]`-gated, no wasm harness in
  this repo). Bit real work twice tonight.
- Print Graph builds the whole loaded history into one unvirtualized SVG — plausible
  cause of an iPad memory lockup tonight, never confirmed.
- `linux-ops-suite#96` — generalize `rex-check`/`gv-report` into an opsmcp
  `repo_report` tool. Prompt already committed there. Different repo, zero collision
  risk with anything above.
