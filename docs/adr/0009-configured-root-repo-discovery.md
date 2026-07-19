# ADR 0009 — Local repos are discovered from one configured root, direct children only

- **Status:** Accepted — implementation pending (`feature/repo-picker-modes`)
- **Date:** 2026-07-19
- **Milestone / issue:** Post-M1.05 feature set; design spec
  `docs/superpowers/specs/2026-07-19-repo-modes-lan-visualizer-design.md`
- **Supersedes / superseded by:** Extends ADR 0003 (catalog); its invariants
  are unchanged.

## Context

Opening another local repo (e.g. `~/projects/linux-ops-suite` next to
`~/projects/Git-Vista`) currently requires restarting the server with a new
path. An in-app picker needs a repo list — but ADR 0003 is explicit that
paths never cross the wire and the catalog is server-owned and fail-closed,
so the client can never *name* a path; it can only pick from what the server
already registered.

## Decision

- A new launcher flag `gv --root <dir>` and env `GIT_VISTA_REPO_ROOT` (env
  form for systemd units) designate one repos root, e.g. `~/projects`.
- At startup the server scans the root's **direct children only**; each
  valid git repo registers in the existing catalog under an opaque
  `WorktreeId` (paths never on the wire, symlink-escape rejection — ADR 0003
  invariants unchanged).
- `POST /api/rescan` re-scans the configured root and the clones root
  without a restart; auth-gated like every mutation.
- No root configured → today's behavior (launch repo + clones only).
- Non-repo children are skipped and logged; a missing root dir is a startup
  warning with an empty scan, not a failure.

## Alternatives considered

- **No discovery** (restart with a different path, status quo). Rejected:
  it is exactly the friction the picker exists to remove, and it is
  unusable from the iPad.
- **Recursive scan.** Rejected: unbounded surprise surface (every repo under
  `$HOME`?), slow on big trees, and a wider symlink/containment audit for no
  demonstrated need. Direct children of one deliberate root is predictable.
- **Client-supplied paths** ("open /home/tom/…" from the browser). Never an
  option: it reverses ADR 0003's core rule that the server owns the
  allowlist and paths stay off the wire.
- **Multiple roots.** Deferred: one root covers the known layout
  (`~/projects`); the flag can grow a repeatable form later without wire
  changes, since the client only ever sees catalog entries.

## Consequences

- The picker lists: launch repo, root-scanned repos, persistent clones —
  all as opaque catalog entries.
- The frontend finally consumes `GET /api/catalog` (built in M1.03,
  unconsumed until now).
- Unit tests cover root-scan child classification; the scan must stay
  tolerant of junk directories.
