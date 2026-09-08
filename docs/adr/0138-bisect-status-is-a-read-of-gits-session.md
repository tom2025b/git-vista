# ADR 0138 — Bisect status is a read of git's session

- **Status:** Accepted
- **Date:** 2026-09-07
- **Issue:** #708; follows #87 and #695
- **Number claim:** `refs/adr-claims/0138`

## Context

ADR 0131's executors discover git's durable bisect state, but the browser
cannot read it. A restarted session retains its bisect and loses any visible
indication of it. Starting, marking or resetting is not an acceptable way to
ask whether a session exists.

## Decision

Add `GET /api/bisect/status`, using the shared `RepoQuery` and `resolve_repo`
selector. An omitted selector reads the session's selected worktree. An
explicit opaque id resolves once and fails closed with the existing 400/404
responses. Both listener profiles expose the authenticated read, including
Visualize mode, alongside working-tree status. Successful responses carry
`Cache-Control: no-store`.

`git-vista-protocol::dto::BisectStatus` and `BisectLogStep` are shared wire
DTOs, following the existing bisect requests' derives and
`#[serde(deny_unknown_fields)]` convention. The internal discovery types stay
server-private. The handler maps every field mechanically:

| Key | JSON type | Meaning |
|---|---|---|
| `in_progress` | boolean | Git has a nonempty BISECT_START |
| `current` | string or null | HEAD while bisecting, if readable |
| `started_from` | string or null | Git's reset destination |
| `bad` | string or null | Bad boundary oid |
| `good` | array of strings | Good boundary oids |
| `skipped` | array of strings | Skipped oids |
| `history` | array of objects | Ordered commands, each with `verb` string and `args` string array |
| `finished` | boolean | Git's candidate range has narrowed to the bad commit |

No active bisect returns false flags, null optional values and empty arrays.
A finished range still has `in_progress: true` until reset. Strings preserve
the discovery representation; converting read observations into validated
write arguments is not this boundary's job. This is an additive endpoint,
without changes to the three existing write contracts.

Mount a passive text indicator in the existing topbar. Fetch when an accepted
frame supplies its worktree id, on graph epoch changes, on reconnect, and
every five seconds to observe external changes that do not move HEAD. Tag
replies with their requested epoch and worktree, and reuse the shared
`current_reading` rule to reject old, missing and loading readings. The host
core decides the label. A finished session remains visible with reset guidance.
A permanently mounted `role="status"` wrapper receives the label for assistive
technology. No action or browser persistence is needed to restore it on load.

## Alternatives

Serializing the internal type would couple the public contract to executor
implementation. Mirroring session state in the browser would make restart
visibility depend on another persistence mechanism and miss external git.
Only querying after a write would leave the original defect intact.

## Verification and limits

The protocol test pins literal wire keys and types. Handler tests exercise the
production authenticated router on both profiles against a real git bisect,
recreate the router and session manager, check the fresh observation, and
verify HEAD, refs, log and index remain unchanged by reads. They cover external
reset, a finished-but-active session and fail-closed repository selection.
Host tests exercise active, inactive, finished, stale, failed and loading
indicator readings. A source census binds the browser mounting/fetch/display
seam to that decision; it supplements executable tests, not a replacement for
the wasm build or browser gate.

The route preserves `discover`'s existing best-effort behavior: unreadable
BISECT_START is indistinguishable from absence, and some failed subsidiary
reads produce empty fields. It is not an atomic snapshot across concurrent
git writes. Changing those semantics belongs to discovery, outside this
surface-only issue. Notes and reviewed automatic test adapters remain on #87.
