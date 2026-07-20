# Repo Modes, Persistent Clones, and LAN View — Design

Date: 2026-07-19
Status: Approved in brainstorming session; awaiting implementation plan.
Scope: three features, three branches/PRs, three new GitHub issues.

## Problem

1. Opening a repository that is not the user's own work (a third-party local
   repo or a cloned public repo) should offer a **visualizer** experience:
   look-only, with links out to the repo's forge.
2. Cloning any public repo should offer the **same visualize-or-active
   choice**, and clones must survive restarts so active work on them is not
   lost.
3. The SSH tunnel is the only remote path today and has failed in practice on
   the user's Linux browser and iPad. A **LAN backup** is wanted — without
   reintroducing the plain-HTTP LAN write surface removed in `ae28093`.

## Decisions (user-confirmed)

| Question | Decision |
|---|---|
| When to choose mode | **Ask every time** a repo is opened: Visualize / Active picker |
| LAN scope | **Visualizer only** — zero write routes served on LAN; active mode requires loopback or SSH tunnel |
| Clone lifecycle | **Persistent folder, multiple clones**, delete-from-app |
| Clones location | `$XDG_DATA_HOME/git-vista/clones` (fallback `~/.local/share/git-vista/clones`), override `GIT_VISTA_CLONES_ROOT` |
| Own-repo access | In-app picker over a configured root (e.g. `~/projects`) |
| Visualizer content | Existing read-only view (graph, detail, diffs, files) **plus forge links** |
| Architecture | **A: selection-based modes** — mode rides the current-repo selection; write-by-id stays reserved for M1.06 (ADR 0003) |

## Server design

### Repo roots and catalog

- New launcher flag `gv --root <dir>` and env `GIT_VISTA_REPO_ROOT` (env form
  for systemd units). At startup the server scans the root's **direct
  children**; each valid git repo registers in the existing fail-closed
  catalog (opaque `WorktreeId`s, paths never on the wire, symlink-escape
  rejection — ADR 0003 invariants unchanged).
- `POST /api/rescan` re-scans the configured root and the clones root without
  restart. Auth-gated like every mutation.
- No root configured → today's behavior (launch repo + clones only).

### Selection carries mode

- New endpoint `POST /api/select { worktree: <id>, mode: "visualize" | "active" }`.
  - Resolves the id through the catalog; unknown/forged id → 404 (same
    contract as reads).
  - Sets the process-global current selection to that repo **and** records the
    chosen mode.
- `reject_if_read_only()` becomes a mode check on the current selection:
  `visualize` → every write handler returns 403 `ErrorCode::ReadOnly`.
- Write handlers are otherwise untouched: they still act on the current
  selection only. Per-request write addressing (`?repo=`) remains reserved
  for the M1.06 typed-operations milestone (ADR 0003), which this design
  deliberately does not front-run.
- `RepoEntry.read_only` (the Phase-12 always-read-only clone flag) is
  superseded by selection mode; clones opened in active mode accept local
  writes (push to a non-owned remote simply fails with git's own error).

### Persistent clones

- Clones root moves from `$TMPDIR/git-vista-clones` to
  `$XDG_DATA_HOME/git-vista/clones` (override `GIT_VISTA_CLONES_ROOT`).
- Startup wipe and single-clone eviction are removed. Startup instead
  re-scans the clones root and re-registers surviving clones in the catalog.
- New `POST /api/delete-clone { worktree: <id> }`; keeps the existing
  delete-guard property — refuses to remove anything that does not
  canonicalize inside the clones root.
- Clone URL validation is unchanged: https/http/git schemes only, `--`
  argv guard, `GIT_TERMINAL_PROMPT=0`.
- After a successful clone the response carries the new repo's descriptor;
  the frontend then shows the mode picker for it.

### Forge links data

- `RepositoryDescriptor` and `Graph` gain optional `remote_web_url`: the
  `origin` remote URL normalized to a browsable https form (GitHub, GitLab,
  Codeberg patterns; unknown hosts get the normalized base URL only).
- New DTO fields use `#[serde(default)]` / skip-serializing-if to respect the
  M1.02 versioned protocol contract.

## Frontend design

### Flow

1. App load (after sign-in / protocol screens) → **repo picker**: a blocking
   full-screen overlay in the same iPad-proven pattern as
   `session.rs::not_connected_view`. Lists: launch repo, root-scanned repos,
   persistent clones, plus a "Clone URL…" action (existing open_url dialog).
2. Picking a repo → **mode screen**: two large touch buttons, Visualize /
   Active → `POST /api/select` → graph resources reload (existing `reload`
   signal; no page reload).
3. Topbar: a "Repos" button reopens the picker; a mode badge shows the
   current mode and reopens the mode screen for the current repo.
4. Frontend finally consumes `GET /api/catalog` (built in M1.03, currently
   unconsumed) via a new `fetch_catalog()` in `api.rs`.

### Mode gating

- The lone `read_only: bool` threading (Graph → canvas → menu) widens to a
  mode enum carried in the `state.rs` Copy-bundle style.
- Audit closes known gaps: Activity-panel Undo buttons (currently not gated
  client-side), commit dialog, confirm-op modals, every `PendingOp` path,
  topbar actions.
- Defense in depth at the `api.rs` single write chokepoint: in visualize mode
  write functions refuse before any network call. The server remains the
  actual boundary.

### Forge links UI

- Detail panel: "View commit on <host>". Topbar: repo link. Branch context
  menu: branch link. All `target="_blank" rel="noopener"`. Absent when the
  repo has no usable remote.

### LAN client behavior

- The session response tells the client which listener served it. Via the
  LAN listener the mode screen offers Visualize only (Active button absent);
  the server has no write routes there regardless.

## LAN view profile (security-sensitive)

- `./gv --lan-view [path]` starts the loopback server plus a **second
  listener** bound to one explicit LAN IP — auto-detected only when the
  machine has exactly one candidate, otherwise `--lan-ip <addr>` is required.
  `0.0.0.0` is never accepted; the removed non-strict Host escape does not
  return.
- The LAN listener serves a **separate router**: GET read routes plus
  `POST /api/session` and `DELETE /api/session` only. Write routes are not
  registered on it — structurally absent, not gated.
- Auth still required on LAN: same single-use bootstrap flow;
  `gv --token --lan-view` prints `http://<lan-ip>:8080/#s=…` (plain
  `gv --lan` remains a hard rejection, unchanged from `ae28093`). Sessions created via
  the LAN listener are view-scoped. Host header must exactly match the
  pinned LAN IP and port. Session sign-in on the LAN listener is
  rate-limited (SECURITY_MODEL.md requirement for beyond-loopback exposure).
- Accepted, documented risk: plain-HTTP transport means repo contents and the
  view-scoped cookie are readable by anyone on the same network. Suitable
  for a trusted home LAN, not guest/shared networks; the startup banner and
  docs say so explicitly.
- Governance: new **ADR 0005** plus a SECURITY_MODEL.md amendment defining
  the "LAN view" profile. The model's plain-HTTP-LAN non-goal covers *write*
  mode, which stays forbidden; paired HTTPS remains the future path for LAN
  writes. Startup displays bound interfaces (model requirement).
- `gv doctor` and the exposed-listener enforcement learn the sanctioned
  socket: a LAN listener is expected when `--lan-view` is active and still a
  SECURITY ERROR otherwise. Without the flag, behavior is exactly today's
  loopback-only enforcement, including the kill-on-exposed check.

## Error handling

- Bad/forged worktree id on select/delete-clone → 404 (catalog fail-closed).
- Write in visualize mode → 403 `ErrorCode::ReadOnly` (existing envelope).
- Write route on LAN listener → 404 (route absent).
- Root scan tolerates non-repo children (skipped, logged); missing root dir →
  startup warning, empty scan, server still healthy.
- Clone failures keep surfacing git's own stderr (B3 posture).
- Degraded launch (non-repo path) keeps working: picker still lists scanned
  repos and clones.

## Testing

- Wire tests: LAN router serves no write route (POST /api/commit → 404);
  Host pinning on the LAN listener (wrong Host → 403); visualize-mode write
  → 403 on loopback; select with forged id → 404; clone → survives simulated
  restart (re-scan re-registers); delete-clone refuses paths outside clones
  root; rate limit triggers on repeated LAN sign-in attempts.
- Unit tests: remote_web_url normalization matrix (github/gitlab ssh-form,
  https-form, unknown host, no remote); root-scan child classification.
- `./dev gate` green on every PR (fmt, clippy native+wasm, tests, trunk).
- Live verification per working agreement: real server; curl the LAN socket
  for a write route expecting 404; real iPad on LAN opening the view link;
  loopback active-mode smoke (branch + commit on a scratch repo).

## Delivery plan

Three new GitHub issues, three branches, in order:

1. `feature/repo-picker-modes` — select endpoint, root scan + rescan,
   picker + mode screens, gating audit, forge links.
2. `feature/persistent-clones` — clones root move, multi-clone retention,
   delete-clone, startup re-scan.
3. `feature/lan-view-mode` — second listener, view-scoped sessions, rate
   limit, ADR 0005 + SECURITY_MODEL amendment, gv doctor updates.

Each PR: `Closes #<issue>`, `./dev gate` green, live verification, merge to
main, never delete branches.

## Preconditions / risks

- PR #114 (M1.05 loopback enforcement) must merge first; branch ① starts
  from a main that contains it.
- `gh` auth for tom2025b is currently invalid (see handoff); Tom must
  `gh auth login` interactively before any push/PR.
- Two-account workflow: assign each new issue before starting it.
- The process-global current selection survives this design (accepted in
  Approach A); M1.06/M1.07 later replace it with typed, serialized,
  id-addressed operations.
