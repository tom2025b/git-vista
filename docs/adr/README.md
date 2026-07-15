# Architecture Decision Records

Short, dated records of decisions that shape Git-Vista's architecture — the kind
of thing that is expensive to reverse and easy to forget the *why* of. Each ADR
captures the context, the decision, the alternatives weighed, and the
consequences, so a later reader (or a later us) can see not just what was chosen
but what it was chosen over.

One file per decision, numbered in order: `NNNN-short-slug.md`. ADRs are
append-only history — supersede an old one with a new one rather than rewriting
it, and note the link in both.

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-repository-generation.md) | Repository identity and the repository-generation algorithm | Accepted |
| [0002](0002-versioned-api-contract.md) | A versioned API contract: protocol negotiation, structured errors, and a transport crate | Accepted |
| [0003](0003-repository-catalog.md) | A server-owned, allowlisted repository catalog addressed by opaque id | Accepted |
| [0004](0004-loopback-sessions.md) | Protecting loopback sessions and mutating requests | Accepted |
