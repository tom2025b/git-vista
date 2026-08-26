# 0084 — One error-rewrap mechanism, and it forwards what it cannot buffer

- **Status:** Accepted — implemented and tested; browser leg unrun
- **Date:** 2026-08-26
- **Issue:** [#336](https://github.com/tom2025b/git-vista/issues/336). A #323 successor.
- **Handoff:** `docs/handoffs/2026-08-26/CLOUD-1-issue-336-collapse-route-local.md`.
- **Extends:** ADR 0040 (`/api/amend-commit` as its own route with its own typed
  400 contract). Nothing 0040 decided is reversed — every 400 body from that
  route still parses as `AmendCommitError`; only *which layer* makes that true
  changes.

## Context

#323 was fixed twice, and both fixes shipped.

The general one lives in `middleware::rewrap_error`. A handler's typed error DTO
travels out of `planner::execute` through the same `(StatusCode, String)` shape
a plain-text refusal uses — that dispatcher's return type is shared by ~30
operation kinds and cannot vary per handler — and axum's blanket
`impl IntoResponse for String` always stamps `text/plain`. So the content-type
header cannot tell a pre-serialized DTO from prose; only the bytes can.
`rewrap_error` therefore sniffs the body: one that parses as a JSON *object* is
relabeled `application/json` and passed through; anything else is enveloped as
`ApiError`.

The route-local one lived on `/api/amend-commit` alone: `amend_refusal` returned
a `Response` with `Json`'s own content-type, `amend_refusal_body` kept the
`(StatusCode, String)` shape for the executor, and
`handlers::commit::amend_route_response` re-labeled that channel's output at the
final hop after its own sniff of the `String`.

Four other routes hand-serialize a typed DTO into the same channel with **no**
route-local layer at all — `/api/commit` (`commit_refusal_body`), `/api/tag`
(`sign_refusal_body`), `/api/fetch` (`fetch_error_body`) and `/api/pull`
(`pull_error_body`) — and both of the first two carry a doc comment saying, in
so many words, that they need no `Response`-returning sibling because the
general sniff already covers them. Amend was the lone holdout.

### The one thing the route-local layer really did cover

`rewrap_error` read the body with:

```rust
let bytes = to_bytes(response.into_body(), MAX_ERROR_BODY)
    .await
    .unwrap_or_default();
```

`to_bytes` is all-or-nothing. A body one byte past `MAX_ERROR_BODY` (64 KiB)
comes back as `Err`, `unwrap_or_default()` turns that into **no bytes at all**,
and the client receives an `ApiError` whose `message` is the status's canonical
reason — "Bad Request" — and none of what the server said. The route-local layer
escaped that only because sniffing the `String` *before* it became a body let
`is_json` return early, so `to_bytes` never ran.

That edge is reachable, and not narrowly:

- `git_cmd::git_output_bounded` — the spawn behind `run_git_hooked`, and so
  behind every amend and commit that runs hooks — captures output with a plain
  `cmd.output()`. It is bounded in *time*, not in bytes. A `pre-commit` hook
  that prints a megabyte of policy text produces a megabyte of stderr.
- `stderr_stdout_or` puts that text into the DTO's `message` verbatim.
- JSON escaping widens it: a newline doubles, and a control byte becomes six
  bytes of `\uXXXX`. Even the spawns that *are* capped are capped at
  `git_cmd::STDERR_CAPTURE_CAP` — 64 KiB, the same number as `MAX_ERROR_BODY` —
  so a stderr sitting exactly at its own cap serializes to a body over the
  middleware's.

So the honest scoring of the two mechanisms is not "one covers everything the
other does". It is: the general one covers five routes and loses over-cap
bodies on all five; the route-local one covered one route and kept over-cap
bodies on that one. Neither was complete.

## Decision

**Fix the ordering in `rewrap_error`, and delete the route-local layer.**

The owner's standing rule decides it: the thorough mechanism over the quick one,
and one mechanism that covers everything over two that each cover most. Keeping
the route-local layer would have documented an edge case for one route out of
five and left the other four broken in exactly the way the layer existed to
prevent.

`rewrap_error` no longer *collects* the body. It **splits** it:

```rust
let (head, rest) = split_at_limit(response.into_body(), MAX_ERROR_BODY).await;
```

`split_at_limit` reads frames until one crosses the limit, keeps the bytes up to
it, and hands back the remainder **as a body to forward** — not as a second
buffer. Nothing past the crossing frame is polled there. Two outcomes:

- **The body ended inside the cap** (every real refusal). Classification is
  exact, and identical to before: the full bytes either parse as a JSON object
  or they do not.
- **The body ran past the cap.** The prefix is classified by
  `json_object_or_prefix_of_one`, which separates a *truncated* JSON object from
  prose that merely starts with `{` by the kind of error `serde_json` returns —
  `Error::is_eof` for input that ended while more was expected, a syntax error
  for a token that is not JSON. A truncated object gets the
  `application/json` label and the whole body is streamed on untouched; anything
  else is prose, and the prefix is enveloped **with an explicit truncation
  marker** rather than silently replaced by "Bad Request".

### How it is bounded

Peak memory is `MAX_ERROR_BODY` plus the single frame that crossed it — never
the body, whatever a hook decides to print. The constant's meaning changes from
"the most we will deliver" to "the most we will hold", and its doc comment now
says so. The client receives the whole DTO because the bytes are *forwarded*,
which costs no buffer at all; the server never sees them together.

The sniff is done through `String::from_utf8_lossy` rather than the raw bytes:
the cut can land mid-character, which the byte parser reports as a syntax error
and the prefix sniff would then misread as prose. A replacement character is
still a legal JSON string character, so the lossy copy fails at end-of-input the
way the truncation actually did. Only the copy is inspected; the original bytes
are what get forwarded.

### What the collapse removes

- `planner::commit_exec::amend_refusal_body` — gone; `amend_refusal` is back to
  the plain `(StatusCode, String)` shape and is now the *one* constructor for
  this route's 400, used by the handler's request-shape refusals and by
  `exec_amend_commit`'s classified git outcomes alike.
- `handlers::commit::amend_route_response` — gone; `amend_commit` returns
  `(StatusCode, String)` directly, like every other write handler.

The wire contract is unchanged: same status, same `application/json`, same
bytes. Only the layer that produces the label moves.

## Alternatives weighed

**Keep the route-local layer and document the edge it exists for.** The option
the issue offers, and the one this rejects. It would leave `/api/commit`,
`/api/fetch`, `/api/pull` and `/api/tag` discarding over-cap refusals, and would
pin, as deliberate, a shape two sibling constructors already carry doc comments
explaining they do *not* need.

**Raise `MAX_ERROR_BODY`.** Moves the cliff; does not remove it, and buys the
move by holding more of a hostile body in memory. There is no size at which a
hook's output is guaranteed to fit, because nothing caps it.

**Cap the message where the DTO is built.** Genuinely closes the gap for the
paths that exist today — bound the message to a few KiB at `stderr_stdout_or`
and no body can reach the middleware's cap. Rejected for two reasons: it is a
user-visible truncation of every large refusal whether or not it needed one,
and it fixes the callers rather than the mechanism, so the next handler that
returns a large error body reintroduces the defect silently. The middleware is
where "any error, one shape" is promised; that is where totality belongs.

**Widen `plan_and_execute`'s return type to `Response` so handlers can say
"this is JSON" instead of the middleware guessing.** The cleanest answer to the
root ambiguity, and out of scope here: the return type is shared by ~30
operation kinds and the ripple reaches the whole pipeline for one route's
benefit. Recorded, not taken.

## Consequences

`http-body-util` becomes a direct dependency of `git-vista-server`. It was
already in the tree as axum's own dependency and compiles no new code; the crate
now names `BodyExt::frame` itself, so it declares it.

An over-cap **prose** refusal is still truncated at 64 KiB. That is a real
remaining bound and it is stated rather than hidden: the envelope now carries
what fits *and says it was truncated*, where before it carried the canonical
reason and said nothing. An over-cap **JSON** refusal is no longer truncated at
all.

The amend route loses a layer, and with it the `Response`/`(StatusCode, String)`
split that three separate doc comments elsewhere in the planner existed to
explain. Those comments are now one sentence each.

`middleware::MAX_ERROR_BODY` becomes `pub(crate)` so the `/api/pull` wire test
can build a fixture that is over the cap *by construction* rather than by a
magic number that would silently stop testing the edge if the cap ever moved.

## Verification

**Baseline.** ~320 `git-vista-server` tests fail in the cloud container on
unmodified `main` — the strict sandbox tier needs `landlock_abi>=6` plus
`bwrap`, and this kernel lacks Landlock. Measured on `405a764` with
`gv-sandbox` built first: **616 passed, 321 failed, 4 ignored**. After this
change: **622 passed, 321 failed, 4 ignored**. The failing sets are identical —
`comm` over the sorted names reports zero new failures and zero newly passing.
The +6 are exactly the six tests added here.

**Mutation-proof, three ways, each red at assertions the others do not reach.**

*A — the sniff removed entirely* (the relabel branch deleted). Five tests red:

```
crates/git-vista-server/src/handlers/fetch.rs:190:41:
body was not a bare FetchError: missing field `kind` at line 1 column 180
crates/git-vista-server/src/handlers/pull.rs:408:37:
body was not a bare PullError: missing field `kind` at line 1 column 517
crates/git-vista-server/src/middleware.rs:647:33:
body was not a bare AmendCommitError: missing field `kind` at line 1 column 176
```

*B — the sniff narrowed to bodies that fit the cap* (`rest.is_some()` ⇒ not
JSON: the old cap-then-sniff ordering, restored exactly). Two tests red, and the
three assertions above stay green:

```
crates/git-vista-server/src/handlers/pull.rs:517:13:
an over-cap refusal did not survive as a bare PullError (missing field `kind`
at line 1 column 65664): 65664 bytes, starting "{\"error\":{\"code\":\"bad_request\",…
crates/git-vista-server/src/middleware.rs:726:13:
an over-cap typed refusal did not survive as a bare AmendCommitError (missing
field `kind` at line 1 column 70904): 70904 bytes, starting "{\"error\":…
```

*C — the remainder dropped rather than forwarded* (`split_at_limit` returns
`None` at the crossing frame: the cap truncates silently). Four tests red, two
of them at assertions neither A nor B reaches:

```
crates/git-vista-server/src/middleware.rs:837:9:
one byte past the cap is a body with a remainder
crates/git-vista-server/src/middleware.rs:779:9:
a truncated message must say it was truncated rather than read as the whole
of the answer
```

Each mutation was reverted from a pre-mutation copy and the restore verified
byte-identical with `diff` and a clean `git diff`; the target tests were rerun
green after each.

**The incidental coverage, now deliberate.** The handoff recorded that neither
`/api/fetch` nor `/api/pull` had a wire-level test through the real router. That
is right for `/api/fetch` and wrong for `/api/pull`:
`the_strategy_mandate_is_a_400_through_a_real_router` already drove that route
through the real `api_contract` middleware and already parsed the body as a bare
`PullError` — it is one of the five reds under mutation A. What it could not
catch is a sniff *narrowed* rather than removed, because its bodies are small.
So:

- `/api/fetch` gains its first wire-level test —
  `a_refusal_reaches_the_client_as_a_bare_fetch_error_through_a_real_router` —
  with the read-only prose refusal on the same route as its paired negative, so
  it cannot be satisfied by a middleware that labeled everything JSON.
- `/api/pull` gains
  `an_over_cap_refusal_reaches_the_client_as_a_bare_pull_error_through_a_real_router`,
  whose >64 KiB body is produced by the endpoint's **own** refusal path rather
  than a fixture: `deny_unknown_fields` echoes the offending key into serde's
  message and `parse_request` quotes it into the `PullError`.

`middleware`'s own four new tests cover the mechanism route-agnostically,
including the reader's exactly-at-the-cap boundary and the negative for the
prefix sniff (prose that starts with `{` must not be forwarded as JSON — the
regression `amend_route_response`'s own doc warned is *worse* than the
double-encoding #323 set out to fix).

**Green.** `cargo fmt --all --check` clean; `cargo clippy --all-targets -- -D
warnings` clean over the workspace.

**Unrun here.** `ci/browser/run.sh` cannot run in the cloud container. The
frontend is untouched by this change and the wire bytes for `/api/amend-commit`
are unchanged — same status, same content-type, same body — but that is
reasoning, not a measurement, and the browser leg is what would measure it.
