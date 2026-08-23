# CLOUD-3 — README audit, verified against source

**Date:** 2026-08-23 · **Branch:** `claude/readme-audit-verify-g8sa9h` · **Base:** `22a7b17`

## About this document

The task pointed at `design-docs/handoffs/CLOUD-3-verify-readme-audit.md`, which does
not exist here — and not because it was lost. `design-docs/` is gitignored
(`.gitignore:71`, "Local design documents — session artifacts, not product docs"), and
`README.md:103-105` says the same of handoffs: "Agent prompts, session handoffs, and
running project memory are local working material and are intentionally excluded from
the repository's public-facing docs." So the handoff was never committed, and a cloud
session — which gets only what the repo carries — cannot see it. It lives on the
author's machine. Confirmed absent from the working tree, all of git history, all ~250
remote branches, and the issue tracker.

(One file *is* tracked under `design-docs/` — `2026-08-18-wf-78-map-results.md`,
force-added past the rule in `ed9a18d`, "bank the map phase's drafts before the network
drops". The ignore rule still matches any new path there, which is why this report is
filed elsewhere. Force-adding is the escape hatch if a handoff ever needs to travel.)

The branch name (`claude/readme-audit-verify-...`) was therefore the only surviving
statement of intent, and this audit reads it the one way that has an object: **audit
README.md's claims against the source and report what holds.** Since no prior audit
exists anywhere to check, "verify the audit" can only mean "produce the verification".

Filed under `docs/superpowers/evidence/` rather than `design-docs/`, following the
dated-audit-evidence convention already in that directory, so the result is tracked
rather than sharing the handoff's local-only fate.

Every finding below cites a file and line that was opened and read. Nothing here is
asserted from a function's own output, and no citation is pasted unverified — all 18
were re-checked against the tree after the report was drafted.

**Scope caveat:** this container holds a *shallow* clone (`.git/shallow` present, 209
commits reachable). One README claim depends on full history and is marked
unverifiable rather than guessed at.

---

## Summary

| | Claims |
|---|---|
| Verified correct | 14 |
| Wrong or stale | 11 |
| Unverifiable here | 1 |

The README's *architecture* claims are in good shape — the security invariants,
the planner story, and the MCP isolation proof all hold up under reading. What has
rotted is **arithmetic and status**: every hard count in the document except two is
now wrong, and the Status section describes a milestone layout that no longer matches
the tracker in five places.

---

## A. Wrong or stale

### A1. "The workspace has six crates" — there are seven

`README.md:109` and the tree at `README.md:111-130` omit **`gv-scrollcast`**, which is
a full workspace member (`Cargo.toml:26`). Actual members: `git-vista-core`,
`git-vista-protocol`, `git-vista-git`, `git-vista-server`, `git-vista`,
`git-vista-mcp`, `gv-scrollcast`.

Related: `Cargo.toml`'s own header comment opens "Four crates, each with one job:" and
then lists six. Both the README and the manifest comment are behind the members list.

### A2. "64 ADRs, numbered 0001–0064" — there are 69

Stated twice, at `README.md:87` and `README.md:411`. `docs/adr/` holds **69** ADR files
numbered **0001–0069** with no gaps in the sequence.

This one matters more than a stale number normally would, because `README.md:412-414`
tells the reader to treat the ADR index as the living record precisely *because* the
prose drifts. The pointer to the tiebreaker is itself miscounted.

### A3. The MCP agent *can* write — execution has already shipped

The README says the agent surface is read + build-only, twice:

- `README.md:172-173` — "read tools + build-only plan tools (execution is a separate, later stage)"
- `README.md:218-220` — "with nothing touching the repository. Submitting an approved plan for execution is a separate, later stage."

Both are false as of the current tree. `execute_plan` is a live, advertised tool:

- `crates/git-vista-mcp/src/execute_tool.rs:1` — "`execute_plan` — the one MCP tool that can mutate (M2.23e, #249)"
- `crates/git-vista-mcp/src/tools.rs:68-70` — the write tool is appended to the catalog returned by `tool_catalog()`, which is what `tools/list` serves (`main.rs:127`)
- It POSTs the plan to `/api/execute-plan`, reaching `planner::submit_plan_tracked`

Issue #249 (M2.23e) is **closed**. This is the single most consequential inaccuracy in
the file: a reader auditing the agent's blast radius is told the write path does not
exist yet, when it does.

*What is still true, and worth keeping:* the plan tools themselves remain build-only,
and execution re-validates the plan against the live repository (operation hash, expiry,
generation, preconditions) before running. The README's claim is stale, not the design.

### A4. "`./dev gate` runs all five" — it runs six, and CI runs seven jobs

`README.md:342-343`: "These are the exact commands CI runs, in the order it runs them
(`./dev gate` runs all five)."

Two separate errors:

**The gate runs six steps.** `dev:65-79` (`gate_body`) runs the five listed commands
plus `cmd_browser` — a real Playwright suite (`dev:177-189`, requiring node and a
Playwright chromium, then `./ci/browser/run.sh`). `dev`'s own header comment says so:
"run the full CI gate (fmt, clippy x2, test, trunk build, browser)".

**They are not "the exact commands CI runs".** `.github/workflows/ci.yml` defines
**seven** jobs, all triggered on every PR to `main`:

| Job | Line | Covered by the five commands? |
|---|---|---|
| `lint` (fmt + clippy ×2) | 83 | yes |
| `core` (check + test) | 127 | yes |
| `contract` (M1.06 write contract + #67 route authz) | 209 | **no** |
| `sandbox` (#66 escape battery) | 271 | **no** |
| `frontend` (Trunk/WASM) | 389 | yes |
| `audit` (`cargo audit` + dependency registers) | 434 | **no** |
| `secrets` (gitleaks) | 544 | **no** |

`lint` additionally runs a "Shell launchers parse" step (`ci.yml:106`) and `core` runs a
git-version-floor check (`ci.yml:153`), neither of which appears in the README.

The gate does not even cover the sandbox job it shares a name with — `dev:191-195`
introduces `dev verify` as "the sandbox-specific checks `dev gate` does NOT run".

Net effect: a contributor who follows this section, gets green, and pushes can still go
red in CI on four of seven checks.

### A5. The LAN flag named in the security blockquote does not exist

`README.md:23`: "An **opt-in** LAN listener exists behind `--lan` / `GIT_VISTA_LAN_IP`".

`gv:456-458` **rejects** `--lan` outright:

```
--lan)
  echo "gv: --lan is disabled; Git-Vista only listens on 127.0.0.1:${PORT}." >&2
  echo "gv: use an SSH local-port forward for iPad access, or 'gv --lan-view' for a read-only LAN backup." >&2
```

The real flag is **`--lan-view`**, paired with `--lan-ip <addr>` (auto-detected, or
required explicitly when detection is ambiguous — `gv:453-455`, `gv:501-518`). That is
what exports `GIT_VISTA_LAN_IP` (`gv:574`). See ADR 0005 (`docs/adr/0005-lan-view-profile.md`).

The blockquote also omits the property that makes the feature defensible — that the LAN
listener is **read-only by construction**:

- `crates/git-vista-server/src/main.rs:384-390` — `full_routes` is `true` for loopback and
  `false` for the LAN listener, and "those routes are never *built* on the LAN router, not
  merely gated, so a mode-check regression can't reopen them"
- `main.rs:346-350` — LAN sign-in is additionally rate-limited per source IP; loopback is not

Naming the wrong flag while dropping the actual invariant makes the security summary
weaker and less accurate than the code it describes.

### A6. The Running section contradicts the blockquote

`README.md:276-278`: "Direct LAN access is deliberately disabled. `./gv --lan` is
rejected, and the server also refuses a non-loopback `GIT_VISTA_BIND_ADDR` override.
This keeps the plain-HTTP Git control surface off Wi-Fi, VPN, container, and public
interfaces."

Sentence by sentence this is defensible — `--lan` *is* rejected, the bind override *is*
refused (`state.rs:53`), and the *control* (write) surface genuinely does stay off Wi-Fi
because the LAN router registers no write routes. But "Direct LAN access is deliberately
disabled" reads plainly as "there is no LAN listener", which `--lan-view` falsifies. A
reader cannot hold this passage and the blockquote at A5 in mind at once.

### A7. Status — M2 is not closed (4 open issues)

`README.md:380-383`: "**M2 — Daily Driver: Status to Push [V1]** is **complete** (0 open)
… the installable PWA all landed. With M1 and M2 both closed, the V1 line is done."

Four M2 issues are open:

| Issue | Title |
|---|---|
| #75 | M2.22 Ship an Installable PWA with Safe Offline Read-Only Mode |
| #244 | M2.22d — Close out #75 (verification against acceptance criteria) |
| #357 | M2.17f — the per-line selection UI #215 deferred was never filed, and #70 still claims it |
| #396 | M2.19a — Verify commit-draft persistence survives iPad Safari suspension |

The README singles out the PWA as landed while **#75, the PWA's own umbrella issue, and
#244, its close-out verification, are both still open**. Since "the V1 line is done"
is derived from M2 being closed, that conclusion inherits the error.

### A8. Status — M3 says 5 open; there are 3

`README.md:385`: "(1 shipped, 5 open)". Actual: 1 shipped (#78, M3.25 Recovery Center),
**3 open** — #76 (M3.23 worktrees), #77 (M3.24 stash), #79 (M3.26 reconcile external changes).

Separately, `README.md:386-388` states "The stash drawer is complete: list, inspect, push,
apply, pop, drop and branch-from-stash" while **#77 *M3.24 Implement Complete Stash
Workflows* is open**. One of the two is wrong — either the prose overstates, or the issue
should have been closed when the work landed.

### A9. Status — M4 is finished, not "in progress (2 shipped, 4 open)"

`README.md:391`. Actual: **0 open M4 issues.** All six umbrella issues are closed:

| Issue | Outcome |
|---|---|
| #80 M4.27 Compare Any Two Repository States | shipped |
| #81 M4.28 Add Cherry-Pick and Revert Plans | shipped |
| #82 M4.29 Touch-First Interactive Rebase Planner | **cut** (ADR 0049) |
| #83 M4.30 Execute and Recover Interactive Rebases | **cut** (ADR 0049) |
| #84 M4.31 Build a Shared Conflict Resolution Workflow | shipped |
| #85 M4.32 Add a Guarded Force-with-Lease Workflow | shipped |

Plus sub-issues #428, #429, #430, #431, #432 — all closed. An honest line is
"4 shipped, 2 cut, 0 open".

`README.md:396-399` is stale in the same place: "Cherry-pick and merge-revert are on a
branch at the time of writing." #81 is closed and the M4.31 conflict work is on `main`
(`0e10bbb`, `c5f955d`, `22a7b17`).

### A10. Status — M5 has shipped nothing; all three closures were scope cuts

`README.md:401`: "**M5 — Investigation & Forges** is part-shipped (3 shipped, 3 open)."

The counts happen to match, but "shipped" is the wrong word for every one of them.
ADR 0049 closed #88, #90 and #91 as **won't-do**:

- `docs/adr/0049-v1-scope-freeze.md:109` — #88 M5.35 Provider-Neutral Forge Capabilities
- `:110` — #90 M5.37 Forgejo integration ("No adapter code exists")
- `:111` — #91 M5.38 GitLab integration ("Never started")

Correct: **0 shipped, 3 cut, 3 open** (#86, #87, #89). "Part-shipped" claims delivery
where the record says the opposite — these were removed from scope, not built.

### A11. Status — M9 and M10 are absent entirely

`README.md:401-403` runs M5 → M6 → M7 → M8 and stops. Six open issues are unaccounted for:

- **M9** — #130 (M9.01 multi-instance fleet), #132 (M9.03 stable graph identity), #136 (M9.07 time reconstruction)
- **M10** — #456, #457, #458 (the `gv-tui` terminal-client line, filed 2026-08-23)

M10 was filed the same day as this audit, so its absence is fresh rather than rotted.
M9's is not: ADR 0049 explicitly kept #130–#132 as deferred-not-cut, and the Status
section never picked them up.

---

## B. Verified correct

These were checked against source and hold. Listing them so a future edit does not
"fix" something that is already right.

1. **"23 build-only `plan_<operation>` tools"** (`README.md:217`) — exactly 23 `tool(...)`
   entries in `plan_tool_catalog` (`crates/git-vista-mcp/src/plan_tools.rs:238-554`).
   A 24th name, `plan_stage_selection`, appears only in a test asserting it is *rejected*
   (`plan_tools.rs:2064`), so it is correctly absent from the count.
2. **The read-tool list** — "graph, commit detail, diff, activity, status, repository
   selection" (`README.md:215-216`) maps exactly onto the seven registered read tools
   (`tools.rs:77-253`): `get_graph`, `get_commit_detail`, `get_commit_diff`,
   `get_activity`, `get_status`, and `list_repositories` + `select_repository` together
   as "repository selection".
3. **The dependency-graph proof is real, not vacuous** (`README.md:147-148`).
   `crates/git-vista-mcp/tests/no_write_dependency.rs` walks `cargo metadata`'s resolved
   `resolve.nodes` graph — not the manifest — so a *transitive* path to `git-vista-server`
   would fail it. `crates/git-vista-mcp/Cargo.toml` depends only on `git-vista-core`,
   `git-vista-protocol`, `serde`, `serde_json`. The test's own doc explains why a router
   test would be weaker. The README's "structurally unreachable, not merely unrouted" is
   an accurate description of the mechanism.
4. **M1 "39 issues shipped, 0 open"** (`README.md:374`) — exactly 39, once the three
   duplicate issues (#100, #101, #102, re-filings of M1.00/M1.01/M1.02) are excluded:
   15 umbrella (#53–#67) + 5 M1.06 sub-issues (#142–#146) + 18 M1.13b (#189–#206) + #208.
   Zero open. The one status figure in the file that is exactly right.
5. **"eighteen never-started issues closed as won't-do, each with an explicit return
   condition"** (`README.md:406-407`) — exactly 18: #82, #83, #88, #90, #91, #93, #94,
   #95, #96–#99, #133–#138. Each row in ADR 0049 carries a return-condition column.
6. **"M6 remains a single issue"** — #92 open; #93/#94/#95 cut. Correct.
7. **"M7 was retired"** — #96–#99 all closed as won't-do (ADR 0049:91-94). Correct.
8. **"M8 deleted as never-started"** — no M8 issue exists in the tracker. Correct.
9. **Server binds `127.0.0.1:8080`** (`README.md:21`) — `state.rs:31-32`.
10. **"an arbitrary bind override is still refused"** (`README.md:24`) — `state.rs:53`
    rejects a non-loopback `GIT_VISTA_BIND_ADDR`. Also refuses loopback and `0.0.0.0`
    for `GIT_VISTA_LAN_IP` (`state.rs:82-90`).
11. **"CI pins `CARGO_TERM_COLOR=always`"** (`README.md:368`) — `ci.yml:68`.
12. **The twice-run clippy rationale** (`README.md:362-365`) — matches `ci.yml:77-82`'s
    own comment: wasm pass is frontend-only because `git-vista-git` (gix) and
    `git-vista-server` (axum/tokio) are native-only.
13. **Every link resolves** — all 7 `docs/*.md` targets, `docs/adr/`, `DESIGN.md`,
    `rust-toolchain.toml`, `contrib/systemd/git-vista.service`, all 5
    `docs/screenshots/*.png`, and all 8 inline ADR links (0002, 0004, 0015, 0016, 0019,
    0021, 0046, 0049).
14. **`bin: git-vista-ui`** (`README.md:126`) — `crates/git-vista/Cargo.toml:12`.

---

## C. Unverifiable in this environment

- **"this repository is ~3,400 commits"** (`README.md:41`) — this clone is shallow
  (`.git/shallow` present; `git rev-list --count HEAD` = 209, earliest reachable commit
  2026-08-15). Neither confirmed nor refuted. It is a screenshot caption describing the
  repo at capture time, so it may well be accurate; it simply cannot be checked from here.

---

## D. Side findings (not README defects)

Surfaced while verifying; recorded so they are not lost.

1. **ADR 0049 has itself drifted.** `:56` records "M9 — 3 open (#130-#132, kept deferred)"
   and `:121` lists #136 (M9.07) among the cut. Today #131 is **closed** and #136 is
   **open** — the opposite of both. Since the README directs readers to the ADR index as
   the tiebreaker, an ADR that has silently gone stale is worth more than a stale README
   line.
2. **The MCP read tools are called "the six" but there are seven.** `tools.rs:74`
   ("The read-only six"), the comment at `:69`, and the test name
   `the_tool_catalog_lists_exactly_the_six_read_tools` (`:708`) all say six; `get_activity`
   made it seven. The test is *not* vacuous — it still pins all names in order, so a
   silent add/remove/rename fails it — only its name is wrong.
3. **The Git version floor is enforced but undocumented in the README.** `ci.yml:153-159`
   parses a floor out of `docs/SUPPORTED_VERSIONS.md` and fails the build if it cannot.
   `README.md:234` says only "A working `git` on `PATH`", and never links that doc.

---

## Recommended follow-up

Nothing in this document has been applied to `README.md` — this branch contains the
verification only. The corrections in section A are mechanical and individually cited,
so applying them is a contained follow-up. Order by cost of being wrong:

1. **A3** (agent write path understated) — a security-relevant misstatement.
2. **A5 / A6** (wrong LAN flag; two passages that contradict) — likewise.
3. **A7–A11** (Status section) — rewrite against the tracker; five of six milestone
   lines are wrong, and M5's "part-shipped" inverts the record.
4. **A4** (test commands) — say six steps, and name the four CI jobs the gate misses.
5. **A1 / A2** (crate and ADR counts) — and consider a CI check that fails when either
   number drifts, since both have now rotted at least once.
