# Read-only forge summaries — #89 first slice

The server calls `state::credential_token()` on a bounded blocking worker,
then sends the resolved token only to the fixed HTTPS GitHub API origin.
No browser token field, raw upstream response/error, provider URL, redirect,
or repository-controlled credential destination crosses this boundary.
Typed provider-neutral summaries cross into the protocol and UI; exact token
redaction covers upstream text even if the provider echoes a credential.

The resolver is synchronous on current main. This consumer needs bounded
waiting and bounded worker admission, supplied outside token_store through
its existing state interface. A timed-out worker retains its admission permit
until it actually exits. No local Git coordinator is held across provider I/O.

First slice: origin repository mapping, open PR titles/numbers/draft states,
canonical deep links, explicit page navigation, fixed/hedged failure states,
rate-limit cooldown, no-store responses, and capability UI. Checks and review
state are unavailable in this slice. Cache opt-in is deferred: no provider
payload is persisted or cached server-side. The browser retains only the
currently displayed page until it closes or repository selection changes.

Verification will cover hostile/realistic provider fixtures, token absence at
the serialized response, bounded pagination/rate errors, auth/listener scope,
and local Git completion during a pending provider request. Two disjoint
mutations, the host/wasm checks and repository gate are required before push.

Refs #89. This slice must not close the issue.
