# ADR 0149 — An as-of selector names a replayable observation

**Status:** Proposed — implemented in the #136 branch; awaiting review
**Date:** 2026-09-12
**Issue:** [#136](https://github.com/tom2025b/git-vista/issues/136)
**Follows:** [0107](0107-an-activity-cursor-names-one-fold.md) (exact activity folds), [0070](0070-a-ref-capture-says-which-kinds-it-recorded.md) (capture completeness), [0080](0080-a-journal-line-may-point-at-its-batchs-capture.md) (batch resolution)
**Supersedes:** nothing
**Superseded by:** nothing

## Context

The journal already records HEAD, branches, peeled tags, and remote-tracking
refs. `HistorySnapshot` and the paged graph engine already accept arbitrary
canonical tips. The missing boundary was selecting exactly one replayable
journal observation and supplying its captured refs to that engine.

An activity row is not a durable event ID. Two real rows can have identical
fields; reflog attribution and burst folding can replace rows. Folded fetch,
pull, and push bursts have no capture. Failed captures, unknown formats,
orphaned batches, partial older captures, and truncated maps cannot establish a
historical repository state. Externally noticed deletions intentionally carry
only an earlier branch snapshot, not HEAD/tags/remotes from the later discovery.

There is another missing fact: old captures say nothing about shallow status.
A repository that is unshallow today might have been shallow when captured.
Neither today's shallow file nor today's complete object database can recover
that historical boundary. The coordinator confirmed that refusing those old
captures and recording the fact for new captures is the intended first slice.

## Decision

### 1. Select an absolute row inside one signed fold

Activity responses add a flattened `ActivityObservation<ActivityEvent>` wrapper.
Existing event fields remain unchanged. Its `as_of` is either
`available { token }` or `unavailable { reason }`; the unavailable variant has
no token field. Older clients ignore the additive response field, and newer
clients reading an older response default to unavailable. The protocol window
and journal event envelope stay unchanged.

`CursorCodec` signs `(scope, activity_generation, AsOfPosition { event_index })`
with its existing process key, format version, input bound, HMAC, and opaque
repository/worktree binding. The position has a different required field from
activity and history cursors and denies unknown fields. No second signing
scheme, durable event ID, or server-held snapshot cache is introduced.

The server authenticates and checks scope before reading repository sources.
It rebuilds the exact fold, compares its generation, and only then indexes the
row. A changed fold returns **409**. Invalid, forged, foreign, out-of-range, or
pre-restart tokens return **400**. Display timestamps, OIDs, and summaries are
never identities. Tokens are response decoration only: they are neither
journaled nor included in the fold hash.

### 2. Record shallow status; never invent the missing past

`RefsAtEvent::Captured` gains additive `shallow: Option<bool>`:

| Recorded value | Meaning | Replay policy |
|---|---|---|
| absent / `None` | No reliable shallow-status observation | Refuse with `shallow_state_not_recorded` |
| `Some(true)` | A nonempty shallow file was observed | Refuse with `shallow_repository` |
| `Some(false)` | Unshallow status was observed around the ref capture | Continue validating |

The journal reads shallow status immediately before and after its one
centralized `read_refs_at` call. Both readings must agree. A failure,
unsupported Git-directory indirection, or intervening status change records
unknown; it does not discard otherwise readable refs. Only two observed
unshallow readings can produce `Some(false)`. Batch anchors retain this field
and resolved rows inherit the same captured fact.

This is a conservative status observation, not a transaction over Git's ref
store and shallow file. A change and reversal between the readings cannot be
observed. The existing centralized ref read has the same external-writer
limitation; no new claim of atomicity is made.

No old line is rewritten. Field absence cannot default to false: that would
assert a past observation that never occurred. The current journal only writes
to ordinary `.git` directories; linked worktrees remain unsupported by that
writer, and this change does not expand it. The shallow-status reader refuses
indirection, unreadable metadata, and special files. At redemption the server
also refuses a currently shallow or unreadable repository, without ever using
its current boundaries as historical topology.

### 3. Validate before issuing a selector, and again when redeeming it

The resolved capture must contain HEAD, tags, and remotes, with no truncation
in any map. HEAD must be attached and consistent with the branch map, detached
and resolving, or genuinely unborn. Unreadable/unresolvable HEAD and invalid
ref names or OIDs are explicit refusals. Recorded empty maps and unborn HEAD
remain valid, including a real empty graph.

The existing `walk_history_topo` validates complete commit ancestry, including
unreachable captured tips, before a token is issued. A request-local memo lets
older captures reuse ancestry checks that already succeeded. A failed walk
certifies nothing. Objects are checked again when redeeming: a tip or ancestor
lost after issuance returns **422** with an explicit object-unavailable reason,
not an empty graph. Errors during page traversal also remain failures. There
is no global or persisted commit side table.

### 4. One graph engine with a source-specific consistency check

The existing `GET /api/frame` and `GET /api/commits` accept `?as_of=<token>`.
They keep their session-required GET classification on loopback and LAN. No
new timeline endpoint or second authorization path exists.

Accepted captures become `HistoryMaterials`: full ref names, display refs,
both HEAD halves, and an explicitly observed empty shallow-boundary set.
They pass through the same `snapshot_from_materials` canonicalization and the
same frame/page construction as live reads. Historical page generations bind
both canonical topology and the exact selected observation, so live cursors
and cursors from another observation cannot be spent against this one, even
when the refs are identical.

`SnapshotOrigin` chooses the final consistency check. Live reads retain their
existing current-ref re-read and generation comparison. Captured reads instead
rebuild and compare the signed activity fold after construction. Applying the
live check to captured refs would reject precisely the older state the user
asked to see. A fold change during construction still returns 409.

Remote-membership badges are derived from captured remote tips through the
existing walker. They cannot come from today's remote refs. A historical
remote scan failure is fatal rather than a silent false badge. Historical
frames report `read_only: true`, `resettable: false`, and omit links whose
configuration was not captured.

### 5. An explicit, static client mode

`GraphCore` owns `HistoryView::Live` or `AsOf { token, time, repo }`. Entry and
return both advance the graph epoch. Frame and page requests carry the same
selected token; late replies cannot replace a newer epoch. Refresh retains
the selection. Background operation invalidations do not refresh or replace
a historical epoch, and selector errors never trigger a fallback to live.

The Activity row offers “View graph at this activity” only for an available
selector. The app mounts a persistent “Historical view · after activity at
<time> · view only” banner before loading, through successful rendering, and
through refusal. It offers one “Return to live” action. Live status/toolbars,
repository selection, settings, Activity writes, and mutable dialogs are absent;
read-only graph inspection remains available. Entering closes stale write
dialogs. The API write guard synchronously reads the same graph state, including
while its seed is pending and before retrying a write.

This mode is a browser viewing state, not a change to the server session's
permissions. Explicit return starts a fresh live epoch. Nothing is persisted
across a full page reload, and no stepping or animation is included.

## Alternatives and costs

A timestamp/OID selector would confuse distinct identical events. A raw index
would splice changing folds. A separate signing key or graph implementation
would duplicate boundaries already present. Substituting live refs or shallow
state would render a plausible moment that the journal never recorded.

Every historical request refolds the bounded source windows and rechecks the
fold after construction. Validation can walk full ancestry before the first
frame, so it is more expensive than the live frame's ref-only read. The
request-local memo reduces repeated checks for shared ancestry, without a new
cache lifetime or object-retention promise. Busy repositories can invalidate a
selector before it is clicked or while it is paged. Legacy captures are
unavailable until the repository has new complete observations. These are
explicit first-slice limits, not reasons to return a best-effort graph.

## Verification

The server acceptance tests drive real temporary repositories and the
production collector, signer, redeemer, and frame/page builders. They cover
incomplete/failed/unknown/orphaned captures, all three truncation maps, unreadable
and inconsistent HEAD, missing objects, recorded and current shallow status,
identical rows across activity pages, signature/scope/type/index rejection,
head insertion and in-place fold changes, and changes during construction.

The graph comparison records a merge graph, moves HEAD and every kind of live
ref, then compares historical refs, colors, every row, edges, stubs, and remote
membership with the original live rendering. Cross-observation/live cursor
refusals are checked before traversal. Browser tests exercise the real journal
writer and signed endpoints, multi-page requests, missing live branch badges,
hidden write affordances, the persistent 409 state, and return during a pending
historical request. Host tests pin mode/epoch transitions independently of DOM
wiring.

Mutation results and final validation commands are recorded below after the
committed baseline is exercised.

**Signed:** codex · 2026-09-12
