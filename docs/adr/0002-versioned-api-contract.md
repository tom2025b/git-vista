# ADR 0002 — A versioned API contract: protocol negotiation, structured errors, and a transport crate

- **Status:** Accepted
- **Date:** 2026-07-15
- **Milestone / issue:** M1.02 — Establish a Versioned API Contract (#102)
- **Supersedes / superseded by:** —

## Context

The V2 foundation must guarantee that *a cached browser tab cannot silently talk
to a server it no longer agrees with* (see `docs/GIT_CLIENT_ROADMAP.md`,
"Foundation"). Two problems stood in the way:

1. **The API had no version.** The frontend is a PWA; iOS can keep a tab (and its
   cached wasm bundle) alive across a server redeploy. A stale client would issue
   requests against a server whose contract had moved on, and misread the
   responses with no signal that anything was wrong.

2. **Transport and domain were the same types.** The request/response DTOs lived
   in `git-vista-core` alongside the repository/graph/identity *domain* model.
   Letting one set of structs be both the wire format and the internal model
   couples contract compatibility to internal evolution — the coupling
   `docs/V2_ARCHITECTURE.md` flags as a scaling failure mode — and there was no
   consistent error shape or request correlation id across the surface.

M1.01 (#101, ADR 0001) established that the API must address repositories by
opaque identity, never by a filesystem path. This ADR builds the versioned
contract that identity-based addressing will travel over, and records the
guarantee — verified during this work — that **no endpoint selects a repository
by a request-supplied path**, so path-based selection cannot be reintroduced as a
fallback.

## Decision

### 1. A new `git-vista-protocol` crate

Transport is not domain. The wire types move to a new pure, wasm-safe crate that
both the server and the frontend depend on, and that depends on neither them nor
`git-vista-core`:

```text
  git-vista-server ─┐
                    ├─► git-vista-protocol      git-vista-core ─► (no transport)
  git-vista (wasm) ─┘
```

It owns: the protocol version + negotiation payload, the structured error
envelope + request id, and the shared request/response DTOs (`CreateBranchRequest`,
`CreateCommitRequest`, `BranchRequest`, `CloneRequest`, `RebaseStatus`, and the
`validate_clone_url` gate). The domain/graph model (`Graph`, `CommitDetail`, `Oid`,
…) stays in `git-vista-core` — it is produced by core's own layout engine and is
*not* moved in this issue. No Axum or Leptos type enters either pure crate.

### 2. Version negotiation: a header, plus one unversioned endpoint

The URLs stay `/api/*` (no `/api/v2/*`; URL versioning is reserved for a future
*incompatible* generation that must run beside this one). Instead:

- `GET /api/protocol` — the one endpoint exempt from the protocol header — returns
  `{ protocol_version, min_client_protocol, max_client_protocol, server_version }`.
  A client hits it before it trusts the rest of the API.
- Every *other* `/api/*` request must carry `X-Git-Vista-Protocol: <client version>`.
  A middleware checks it against the server's inclusive `[min, max]` window. A
  missing, unparseable, or out-of-window value is refused with `426 Upgrade
  Required` and the structured error envelope; the frontend turns that (and its
  own startup `/api/protocol` check) into a blocking **"Update Required"** screen.

The negotiated number is the *wire-protocol* version, not the server semver;
compatibility never turns on the semver. It starts at `1` (`min = max = 1`).

### 3. Errors: one structured envelope + a request id, on every failure

Every `/api/*` failure returns
`{ error: { code, message }, request_id, protocol }`. `code` is a stable
machine-readable `ErrorCode` (snake_case wire form — part of the contract); a
client switches on it, never on the message. One response layer guarantees the
shape across the whole surface: handlers may still return a bare
`(StatusCode, String)` and the layer wraps it (and a caught panic's 500) into the
envelope. Success bodies are **not** wrapped — only their headers carry the
protocol version and request id. Every response (success or error) carries
`X-Git-Vista-Protocol` and `X-Request-Id`.

### 4. No path-based repository selection — verified and guarded

The repository is process-global (`state::CURRENT`), set only at startup (CLI arg)
and by `POST /api/clone` — whose body is a URL only, with the destination chosen
by the *server*. No handler reads a repo/path from a request. To keep it that way,
the request DTOs carry `#[serde(deny_unknown_fields)]`, so a body smuggling a
`repo`/`path` key is rejected outright rather than silently dropped, and a wire
test pins that behaviour.

## Alternatives considered

- **`/api/v2/*` URL versioning now.** Rejected: renaming ~20 routes with no shape
  change is churn; URL generations are for running two *incompatible* contracts at
  once, which we don't need yet. Reserved for when we do.
- **A `protocol` module inside `git-vista-core`.** Rejected: it leaves transport
  and domain co-located — the split this milestone exists to make — even though
  core is pure today.
- **Wrapping every response (success too) in an envelope.** Rejected for now: it
  rewrites every frontend `json::<T>()` call and every handler success path for no
  behavioural gain. The errors-only envelope + response headers meet the need.

## Consequences

- A redeployed, incompatible server is caught by the client instead of silently
  misread; a user sees "Update Required" and reloads.
- Every failure is one shape with a code and a correlatable request id.
- `git-vista-core` is now free of transport types; `git-vista-protocol` is the
  single place the wire contract versions. The workspace grows to five crates.
- Requests cannot carry a repository path; the guarantee is enforced at the wire.
- Later work (the #103 catalog) will thread opaque `RepositoryHandle` addressing
  *through* this contract; the versioning + envelope are already in place for it.
