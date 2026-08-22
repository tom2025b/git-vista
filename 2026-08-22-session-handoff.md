# Handoff — paste this into the new session

Session ran 2026-08-21 22:06 to 2026-08-22 05:05, across a power cut at ~02:00.
Nothing below needs the old transcript.

## Start here: #428 is designed and unblocked

**Git-Vista issue #428 (M4.31a) — inspect a conflict, read-only.** The design is
settled and written into the issue as comments. Read those first; do not
re-derive them.

Three pieces to build:

1. `GET /api/conflicts` -> `Vec<ConflictedFile>` from `conflicts::scan()`.
   Metadata only, no content — the type's own comment says a caller fetches
   content independently.
2. `GET /api/blob/{oid}` -> bounded content by blob OID, reusing
   `git_cat_file_batch` + `FILE_CONTENT_CAP` + `truncate_at_line`, the same
   reader `file_at_commit_for_repo` uses. Serves base/ours/theirs.
3. Result pane: the worktree file, same bounded reader. **Read-only and
   labelled**, per the decision in the issue.

Then a client viewer doc with four panes. `Absent` must read as absent, never
as empty. `Unreadable` must say so.

**The authz decision is FIXED and must not be re-litigated:** all three reads go
in `full_routes` only, `Authz::SessionRequired`. They expose uncommitted
worktree/index state, which ADR 0005's LAN profile withholds. Do **not** try to
make `/api/blob` LAN-safe by checking OID reachability — `main.rs:510` rejects
exactly that pattern ("a security boundary inside a match arm").

**Do not fan this out.** One vertical slice touching `main.rs`, `read.rs`, the
viewer and `route_authz.rs`. Parallel agents would collide.

## Model and effort — and WHEN TO ASK TOM TO BUMP IT

**Start: Sonnet, medium.** The build is ~80% plumbing against patterns already
in the files (`file_at_commit_for_repo` is the template for two of the three
endpoints; `ViewerDoc::Spec` for the viewer). Medium handles that.

**Tom pays per token on a 5-hour bucket. Do not silently run hot, and do not
silently struggle at medium either.** Both waste his money. Say something.

### Stop and tell him to bump — say it plainly, then WAIT

Do not just push on at medium through any of these:

| Trigger | Say to Tom |
|---|---|
| **About to design a mutation** | "Next step is designing mutations — worth bumping to high. A mutation that cannot fail is worse than none." |
| **A mutation SURVIVED** | "Mutation survived. Working out whether the test is inert or the mutation was wrong needs judgment — bump to high." |
| **Any authz / security question NOT already answered in #428** | "This is a security-boundary call the handoff does not cover. Bump to the strong model before I decide it." |
| **The written design turns out wrong** (e.g. blob-by-OID does not behave as expected) | "The design assumed X and X is false. Redesigning needs judgment — bump." |
| **Third failed attempt at the same thing** | "Third attempt on the same problem. Either bump, or I am missing something and should stop and report." |
| **Anything that changes what reaches the LAN listener** | Stop. Strong model, every time. No exceptions. |

### Stay where you are — do NOT bump for these

Writing endpoints from an existing template · wiring a viewer doc · fixing a
clippy nit · a compile error · renaming · running the gate · reading code to
find something.

### Tell him to go CHEAPER too

The rule cuts both ways and is already in `~/.claude/CLAUDE.md`: before a
docs-only stretch — worklog, task summary, ADR prose, PDF render — say
"next stretch is docs only, good point to drop to a cheaper model," then wait a
beat. Do not spend Opus on prose.

### The tell he is watching for

If you claim something is verified without showing the actual numbers —
baseline vs mutated, `N passed` not `0 passed`, the real command output — that
is the failure mode this whole handoff is organised against. Show the output.

## Milestone state — verified, do not re-check

M4: **5 closed, 6 open.** The six are #84 and its five sub-issues, nothing else.

| Issue | State |
|---|---|
| #81 M4.28 cherry-pick/revert | CLOSED |
| #80 M4.27 compare two states | CLOSED |
| #85 M4.32 force-with-lease | CLOSED |
| #84 M4.31 conflict resolution | umbrella |
| #428 a — inspect | **next** |
| #429 b — whole-file resolution | ready, low risk |
| #430 d — binary/rename/delete UX | ready, low risk |
| #431 e — reconnect/crash tested | medium |
| #432 c — block/line + manual edit | **needs an ADR first** |

Merged this session: #422 #423 #424 #425 #426 #427 #433. Every branch kept.

## #432 has an independent review already — read it before touching that issue

`~/projects/_claude-outputs/2026-08-22_fable-conflict-content-transport.md`
(Fable, read-only, verified by max against source — every claim held).

Headline: **`GitOperation::ResolveConflict` already exists** and already does the
load-bearing half — an executor re-read inside the coordinator lock. Content
transport extends that seam rather than inventing one.

Its sharpest finding: if the editor seeds from the **working-tree marker file**
rather than composing the three stage blobs, that input is invisible to both the
porcelain generation and the index checksum. It must be digested into a new
`conflict-v1:` token the way `diff-v1:` digests patch bytes.

**Measured follow-up (PR #433, merged):** `git status --porcelain=v2` is the
**single** input detecting a stage move. Removing the index-checksum slot
entirely breaks nothing. There is no redundancy — do not write an ADR claiming
the index slot backs it up.

## Infrastructure changed tonight — know this before debugging anything

- **Borg is four repos now**, not one: `borg-projects` (991 MB),
  `borg-system` (5.06 GB), `borg-docs` (181 MB), `borg-rest` (1.83 GB), all on
  BORGVAULT, all verified on MEGA. The old 75 GB `borg backup` repo and its
  30 GB replica are **deleted**. Tom will delete the old MEGA copy himself.
- **113 GB of cargo targets moved off the HDD** to the two SSDs, split by size.
  This **broke 10 MCP servers** whose binaries lived in `target/release`. All
  repaired and now installed to `~/.local/bin` — a cache purge cannot break them
  again. `ledger-mcp` is the exception, still running from its project `.venv`.
- **fstab now has `noauto,nofail,user,exec` entries** for the three SSDs by
  UUID. You can `mount /media/tom/borgbackup-home` with **no sudo**. Verified
  working after the reboot, when every device letter reshuffled.
- **GitHub MCP is alive again on podman** — rootless, no `docker` group. Every
  hardening flag verified by measurement. `codex-github-mcp` deliberately left
  on docker; note at
  `~/projects/_claude-outputs/NOTE-for-codex-podman-github-mcp.md`.
- **44 MCP servers connected, 1 failing** (`serena`, needs `uvx`, unrelated).

## The discipline that earned its keep, repeatedly

Tom's instruction, said many times tonight: **"check it in a different way."**
It changed the answer every time it was applied:

- A new test passed and was **inert** — mutation 1 survived, mutation 2 caught it.
- Three `cargo test` runs printed `ok` having run **zero tests** (bad filter,
  wasm-gated module, wrong module path). A pass on nothing is not a pass.
- `claude mcp list` reported `Connected` for a server this session could not use.
- A probe reported `mapmcp NO HANDSHAKE` — zsh does not word-split unquoted
  variables, so the arg was mangled. The binary was fine.
- `disksentinel` reported healthy for `/dev/sda` while the question was `sdb`.
- A cable replacement was recommended and **withdrawn** — 148 "unclean power
  losses" is normal for a USB-attached SSD, and `UDMA_CRC = 1` contradicted the
  theory. Tom caught it.

**Two mutations minimum, and they must fail differently.** This is now a rule in
`~/.claude/CLAUDE.md`; the story is `INCIDENTS.md §two-ways`.

Corollary that cost time tonight: **`mod menu` and `mod prefs` are
`#[cfg(target_arch = "wasm32")]`.** Anything there is invisible to `cargo test`.
Put testable logic in `features::graph::core`.

## Loose ends, none blocking

- Old MEGA folder `borg-repo-linux-home-2026-08-09` — Tom's to delete.
- `switchboard` category-consolidation design doc written and sent:
  `~/projects/mcp-fleet/design-docs/2026-08-22-category-consolidation.pdf`.
  Recommends merging only the cold half (133 of 238 tools). **Re-measure
  `/context` before acting** — the whole case rests on one measurement.
- `CLAUDE.md` is 1,121 lines / 63.6 KB. Over the 1,100-line budget, under the
  64 KB one.
- Session brief: `~/projects/_claude-outputs/2026-08-22_session-brief.pdf`.

**Signed:** max · 2026-08-22T05:05:00-04:00
