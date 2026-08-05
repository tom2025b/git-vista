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
| [0005](0005-lan-view-profile.md) | LAN view profile: a read-only second listener | Accepted — implemented |
| [0006](0006-ask-every-time-mode-picker.md) | Visualize / Active is chosen per open, every time | Accepted — implementation pending |
| [0007](0007-selection-scoped-mode.md) | Mode rides the current-repo selection (`POST /api/select`) | Accepted — implementation pending |
| [0008](0008-persistent-clones-xdg.md) | Persistent, multiple clones under the XDG data dir | Accepted |
| [0009](0009-configured-root-repo-discovery.md) | Local repos discovered from one configured root, direct children only | Accepted — implementation pending |
| [0010](0010-visualizer-forge-links.md) | Visualizer = existing read-only views plus forge deep links | Accepted — implementation pending |
| [0011](0011-pointer-type-gesture-slop.md) | Gesture slop is pointer-type-aware (touch 12px, mouse/pen 4px) | Accepted |
| [0012](0012-unscrollable-app-shell.md) | The app shell never scrolls; all scrolling is internal | Accepted |
| [0013](0013-clone-descriptor-protocol-bump.md) | Protocol v2 for the clone response; `set_current` returns its own handle | Accepted |
| [0014](0014-lan-view-read-only-affordances.md) | LAN-view sessions present read-only affordances only | Accepted |
| [0015](0015-typed-operation-vocabulary-and-plan-schema.md) | A closed Git-operation vocabulary and the reviewable Plan schema | Accepted |
| [0016](0016-shared-write-planner.md) | Every write action executes through one shared planner | Accepted |
| [0017](0017-no-arbitrary-argv-from-the-browser.md) | No arbitrary argv from the browser, held closed by a tripwire | Accepted |
| [0018](0018-plan-staleness-enforcement.md) | Stale, tampered or expired plans never execute | Accepted |
| [0019](0019-serialized-mutations-per-repository.md) | One mutation at a time per shared repository | Accepted |
| [0020](0020-idempotent-operation-lifecycles.md) | Idempotent operation lifecycles and reconnectable progress | Accepted |
| [0021](0021-durable-operation-journal-and-recovery-refs.md) | Durable operation journal and recovery references | Accepted |
| [0022](0022-paged-history-and-bounded-reads.md) | Paged history, signed cursors, and bounded reads | Accepted |
| [0023](0023-rehearsal-workspaces-and-atomic-promotion.md) | Rehearsal workspaces promote results, atomically | Proposed |
| [0024](0024-frontend-feature-boundaries.md) | Frontend overlay state moves into a `Dock`-keyed `OverlayStack` | Accepted |
| [0025](0025-hook-policy-and-disclosure.md) | Hook policy: a declared, disclosed value, not yet enforced | Accepted |
| [0026](0026-shell-mode-foundation.md) | `ShellMode`: Rust owns the layout mode, CSS keys off one class | Accepted |
| [0027](0027-landlock-enumerate-and-skip.md) | Secrets inside a granted `$HOME` are withheld by enumerate-and-skip, not a deny rule | Accepted |
| [0028](0028-network-tier-ports-not-hosts.md) | Accept that the network tier constrains ports, not hosts (Option A) | Accepted |
| [0029](0029-strict-tier-hard-fail-when-unavailable.md) | INV-13: hard-fail when the Strict tier is selected but unavailable | Accepted — implementation pending |
| [0030](0030-git-process-sandbox.md) | The git-process sandbox: a pure argv boundary, tiers by declared intent, and tests that prove their own premise | Accepted — core mechanism and INV-15 disclosure landed and tested |
| [0031](0031-adr-format-alternatives-and-rejection-reasoning.md) | Every ADR records its alternatives and why they lost | Accepted |
| [0032](0032-no-service-worker.md) | No service worker: offline is a failure to surface, not to mask | Accepted |
| [0033](0033-ssh-remote-carveout.md) | SSH remotes under the sandbox: a narrow, explicit carve-out through `secret_excludes` | Accepted — implemented and tested |
| [0034](0034-cat-file-batch-single-spawn-reads.md) | File-at-commit reads go through one long-lived `cat-file --batch` process | Accepted — implemented and tested |
| [0035](0035-inspector-bottom-sheet-wiring.md) | The inspector bottom sheet is wired to `ShellMode`, following the finger during drag | Accepted — implemented and tested |
| [0036](0036-network-tier-exec-harness-askpass-and-redaction.md) | The Network-tier exec harness: forced askpass hardening, byte-level redaction, and what stays open | Accepted — implemented and tested |
| [0037](0037-observe-state-not-git-prose.md) | Observe state, never parse git's prose: destructive operations report what the worktree proves | Accepted — implemented and tested |
| [0038](0038-worktree-destructive-operations.md) | Worktree-destructive operations: typed per-path impact, a preview that names every file, and one required control that cannot exist | Accepted — implemented and tested |
| [0039](0039-remote-operation-vocabulary.md) | The typed remote-operation vocabulary: `FetchRemote`, `PullBranch`, and a lease-guarded `PushBranch` | Accepted — typed contract implemented and tested; execution wired for all three by ADR 0043 (#229), ADR 0044 (#230) and ADR 0045 (#231) |
| [0040](0040-amend-execution.md) | Amend execution: its own route, an executor-level CAS, an advisory published-history flag, and typed failure kinds | Accepted — implemented and tested |
| [0041](0041-tag-operation-vocabulary.md) | The typed tag vocabulary: four variants, and an undo that restores the exact tag object | Accepted — typed contract implemented and tested; execution not yet wired (M2.21 slices of #74) |
| [0042](0042-planner-build-submit-split.md) | The planner's build / submit seam: two stages, one set of stage functions | Accepted — implemented and tested; not routed until #248/#249 |
| [0043](0043-fetch-execution.md) | Fetch execution: progress on the lifecycle that already exists, a cancel that kills the child, and an outcome read from refs rather than prose | Accepted — implemented and tested; its claim about *which host* a fetch may reach is corrected by ADR 0047 |
| [0044](0044-pull-execution.md) | Pull execution: one fetch in the server, a strategy the wire must state, and a conflict that is an outcome rather than an error | Accepted — implemented and tested |
| [0045](0045-push-execution.md) | Push execution: a force that cannot be built, a lease checked by two parties, and a cancel that refuses to reassure | Accepted — implemented and tested |
| [0046](0046-mcp-plan-tool-surface.md) | The MCP plan-tool surface: 23 build-only tools, one endpoint, and a variant that cannot be exposed | Accepted — implemented and tested |
| [0047](0047-remote-target-boundary.md) | Which host a fetch may contact: a name-shaped newtype *and* a precondition that refuses instead of being skipped | Accepted — implemented and tested |
