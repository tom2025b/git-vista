# ADR 0008 — Persistent, multiple clones under the XDG data dir

- **Status:** Accepted — implemented 2026-07-19 (`feature/persistent-clones`, #121)
- **Date:** 2026-07-19
- **Milestone / issue:** Post-M1.05 feature set; design spec
  `docs/superpowers/specs/2026-07-19-repo-modes-lan-visualizer-design.md`
- **Supersedes / superseded by:** Supersedes the Phase-12 clone lifecycle
  (tmp dir, single clone, wiped at startup).

## Context

Clones today live in `$TMPDIR/git-vista-clones`, one at a time, wiped at
every server start. That was right when clones were disposable read-only
demos. With the visualize/active split (ADR 0006/0007), a clone can host
real work in active mode — and work that evaporates on restart (or on
cloning a second repo) is unacceptable. The whole point of the repo-modes
feature is that "clone a public repo and work on it" becomes a first-class
flow.

## Decision

- Clones move to `$XDG_DATA_HOME/git-vista/clones` (fallback
  `~/.local/share/git-vista/clones`), overridable via
  `GIT_VISTA_CLONES_ROOT`.
- The startup wipe and single-clone eviction are removed. Startup re-scans
  the clones root and re-registers surviving clones in the fail-closed
  catalog.
- Deletion becomes explicit and in-app: `POST /api/delete-clone
  { worktree: <id> }`, keeping the existing delete-guard property — it
  refuses to remove anything that does not canonicalize inside the clones
  root.
- Clone URL validation is unchanged (https/http/git schemes only, `--` argv
  guard, `GIT_TERMINAL_PROMPT=0`).

## Alternatives considered

- **Status quo: tmp, single clone, wipe on start.** Rejected: silently
  destroys active-mode work; contradicts the feature's purpose.
- **Persistent but single clone.** Rejected: "clone B evicts repo A's
  in-progress work" is the same data-loss bug with fewer steps.
- **A custom non-XDG directory** (e.g. `~/git-vista-clones`). Rejected in
  favor of the XDG convention — the data is app-managed state, and the env
  override covers anyone who wants it elsewhere.
- **Clones inside the project/launch directory.** Rejected: mixes
  app-managed state into a user worktree and complicates the catalog's
  containment guarantees.

## Consequences

- Disk usage grows until the user deletes clones; the picker lists them so
  they stay visible rather than accumulating in an invisible tmp dir.
- New wire tests: a clone survives a simulated restart (re-scan
  re-registers); delete-clone refuses paths outside the clones root.
- After a successful clone the response carries the new repo's descriptor
  and the frontend goes straight to the mode picker for it.
