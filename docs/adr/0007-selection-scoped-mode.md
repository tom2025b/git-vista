# ADR 0007 — Mode rides the current-repo selection (`POST /api/select`)

- **Status:** Accepted — implementation pending (`feature/repo-picker-modes`)
- **Date:** 2026-07-19
- **Milestone / issue:** Post-M1.05 feature set; design spec
  `docs/superpowers/specs/2026-07-19-repo-modes-lan-visualizer-design.md`
- **Supersedes / superseded by:** Supersedes the Phase-12 `RepoEntry.read_only`
  always-read-only clone flag. Deliberately does **not** front-run ADR 0003's
  reservation of per-request write addressing for M1.06.

## Context

The visualize/active choice (ADR 0006) has to live somewhere the server can
enforce it. The server already has a process-global "current repository"
selection: reads accept `?repo=<id>`, writes act on the current selection
only, and per-request write addressing is explicitly reserved for the M1.06
typed-operations milestone (ADR 0003). Clones today carry a hard-wired
`read_only=true` flag, which cannot express "clone opened in active mode".

## Decision

A new endpoint `POST /api/select { worktree: <id>, mode: "visualize" | "active" }`:

- Resolves the id through the fail-closed catalog; unknown/forged id → 404,
  the same contract as reads.
- Sets the process-global current selection to that repo **and** records the
  chosen mode as part of the selection.
- `reject_if_read_only()` becomes a mode check on the current selection:
  in `visualize` mode every write handler returns 403 `ErrorCode::ReadOnly`.
- Write handlers are otherwise untouched — they still act on the current
  selection only. `RepoEntry.read_only` is superseded: a clone opened in
  active mode accepts local writes (a push to a non-owned remote fails with
  git's own error, which is the honest answer).
- The frontend widens its single `read_only: bool` threading to a mode enum,
  audits every write affordance (the Activity-panel Undo buttons are a known
  ungated gap), and adds a defense-in-depth refusal at the `api.rs` write
  chokepoint. The server remains the actual boundary.

## Alternatives considered

- **Per-request repo + mode addressing on writes now.** Rejected: that is
  precisely the write-by-id surface ADR 0003 reserved for M1.06's typed,
  serialized operations. Front-running it here would bake an ad-hoc version
  of the M1.06 design into every write handler.
- **Session-scoped mode** (each session carries its own repo+mode). Rejected
  for now: the server's whole write model is a process-global selection; a
  per-session fork of that state is M1.06/M1.07 territory (serialized typed
  operations), not a picker feature.
- **Client-side-only gating.** Rejected outright: hiding buttons is UX, not
  enforcement; the 403 on the server is the control.
- **Delivery as one combined branch** (picker + clones + LAN in one PR).
  Rejected: three independently verifiable branches
  (`feature/repo-picker-modes`, `feature/persistent-clones`,
  `feature/lan-view-mode`) keep each PR reviewable and let the two-account
  workflow interleave.

## Consequences

- The process-global selection survives — an accepted, explicitly temporary
  posture until M1.06/M1.07 replace it with typed, serialized, id-addressed
  operations.
- New wire tests: visualize-mode write → 403; select with forged id → 404.
- `remote_web_url` and other new DTO fields ride the M1.02 versioned
  contract with `#[serde(default)]` / skip-serializing-if, so old clients
  keep parsing.
