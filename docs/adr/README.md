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
| [0005](0005-lan-view-profile.md) | LAN view profile: a read-only second listener | Accepted — implementation pending |
| [0006](0006-ask-every-time-mode-picker.md) | Visualize / Active is chosen per open, every time | Accepted — implementation pending |
| [0007](0007-selection-scoped-mode.md) | Mode rides the current-repo selection (`POST /api/select`) | Accepted — implementation pending |
| [0008](0008-persistent-clones-xdg.md) | Persistent, multiple clones under the XDG data dir | Accepted — implementation pending |
| [0009](0009-configured-root-repo-discovery.md) | Local repos discovered from one configured root, direct children only | Accepted — implementation pending |
| [0010](0010-visualizer-forge-links.md) | Visualizer = existing read-only views plus forge deep links | Accepted — implementation pending |
| [0011](0011-pointer-type-gesture-slop.md) | Gesture slop is pointer-type-aware (touch 12px, mouse/pen 4px) | Accepted |
| [0012](0012-unscrollable-app-shell.md) | The app shell never scrolls; all scrolling is internal | Accepted |
