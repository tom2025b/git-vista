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
| [0041](0041-tag-operation-vocabulary.md) | The typed tag vocabulary: four variants, and an undo that restores the exact tag object | Accepted — typed contract implemented and tested; local execution wired by [0048](0048-local-tag-execution.md), remote pair still refused (M2.21 slices of #74) |
| [0042](0042-planner-build-submit-split.md) | The planner's build / submit seam: two stages, one set of stage functions | Accepted — implemented and tested; not routed until #248/#249 |
| [0043](0043-fetch-execution.md) | Fetch execution: progress on the lifecycle that already exists, a cancel that kills the child, and an outcome read from refs rather than prose | Accepted — implemented and tested; its claim about *which host* a fetch may reach is corrected by ADR 0047 |
| [0044](0044-pull-execution.md) | Pull execution: one fetch in the server, a strategy the wire must state, and a conflict that is an outcome rather than an error | Accepted — implemented and tested |
| [0045](0045-push-execution.md) | Push execution: a force that cannot be built, a lease checked by two parties, and a cancel that refuses to reassure | Accepted — implemented and tested |
| [0046](0046-mcp-plan-tool-surface.md) | The MCP plan-tool surface: 23 build-only tools, one endpoint, and a variant that cannot be exposed | Accepted — implemented and tested |
| [0047](0047-remote-target-boundary.md) | Which host a fetch may contact: a name-shaped newtype *and* a precondition that refuses instead of being skipped | Accepted — implemented and tested |
| [0048](0048-local-tag-execution.md) | Local tag execution: the annotation that cannot be empty, and the pin that outlives the tag | Accepted — implemented and tested; signing and the two remote-reaching tag operations still refuse (M2.21e/f of #74) |
| [0049](0049-v1-scope-freeze.md) | V1 scope freeze: eighteen never-started issues closed as won't-do, M6/M7 retired, M8 deleted | Accepted — tracker changes only; no code or protocol touched |
| [0050](0050-operation-by-key-lookup.md) | Learning an operation's id before it finishes: an additive `GET /api/operations/by-key/{key}` | Accepted — implemented and tested |
| [0051](0051-intent-admission-after-the-await.md) | Intent admission belongs *after* every await, not once before them | Accepted — implemented, confirmed on a device |
| [0052](0052-explicit-repo-list.md) | An explicit repo list, because "these four" is not "this folder" | Accepted — implemented and tested |
| [0053](0053-cancellation-by-framework-not-by-our-own-tracker.md) | "Cancellable" is satisfied by the framework and an id echo, not by a tracker of our own | Accepted — implemented |
| [0054](0054-linux-desktop-browser-is-the-verification-target.md) | The Linux desktop browser is the verification target; iPad deferred to a VNC display | Accepted |
| [0055](0055-status-readings-carry-a-server-stamped-age.md) | Working-tree status readings carry a server-stamped `scanned_at`, additive to the v1 wire contract | Accepted — implemented |
| [0056](0056-gv-repo-uses-the-boot-time-catalog-not-live-clone.md) | `gv --repo` registers a catalog entry via the boot-time `GIT_VISTA_REPOS` unit var, not the live `/api/clone` endpoint | Accepted — implemented |
| [0057](0057-commit-draft-localstorage-and-restore-banner.md) | The commit draft moves to `localStorage`, offered back through an aged restore banner, never auto-filled | Accepted — implemented |
| [0058](0058-hooked-git-spawns-are-time-bounded.md) | Commit-path git spawns that run hooks are time-bounded, killed, and verified | Accepted — implemented |
| [0059](0059-commit-failure-classification.md) | Plain-commit failures get a typed `CommitFailureKind`, split finer than amend's on signing | Accepted — implemented |
| [0060](0060-stale-index-lock-liveness-check.md) | `refuse_if_git_busy` verifies liveness via `/proc` before trusting `index.lock`, and removes a lock confirmed stale | Accepted — implemented |
| [0061](0061-plans-carry-advisories.md) | `Plan` carries `advisories`; a force-with-lease names the default branch, says when it could not tell, and states that the remote cannot be undone | Accepted — implemented |
| [0062](0062-a-comparison-states-which-question-it-asks.md) | A two-endpoint `DiffSpec` carries an explicit `basis` (two-dot vs three-dot); reversal preserves it, and only a `Direct` reversal is an inverse | Accepted — implemented |
| [0063](0063-one-conflict-model-for-six-operations.md) | One conflict vocabulary for all six operations; each side is Present, Absent or Unreadable, and none may collapse | Accepted — implemented |
| [0064](0064-resolving-a-conflict-is-a-planned-operation.md) | Resolving a conflict is a `GitOperation`; no precondition can express "still conflicted", so the executor re-reads and refuses | Accepted — implemented |
| [0065](0065-the-gate-must-be-able-to-say-no.md) | `set +e` for recording left errexit off inside `gate_body`, so the gate could not fail for three days; errexit is re-armed and a test drives the real script to prove it | Accepted — implemented |
| [0066](0066-inspecting-a-conflict-is-three-reads-the-lan-never-sees.md) | Inspecting a conflict is three reads the LAN listener never sees; the pane mapping is host-tested so "absent" cannot quietly render as empty | Accepted — implemented |
| [0067](0067-a-resolution-names-one-path-and-the-refusal-is-the-answer.md) | One path per resolve, the refusal rendered inline as the answer, and a path validated by the wire format rather than by a handler that could forget | Accepted — implemented |
| [0068](0068-a-conflicts-shape-is-read-from-kind-not-inferred-from-flags.md) | A conflict's shape is read from `kind`, never inferred from the deletion flags — which conflate "never had it" with "deleted it"; controls that cannot succeed are replaced by their reason; rename detection recorded as unbuildable | Accepted — implemented |
| [0069](0069-a-conflict-content-token-pins-the-served-marker-file.md) | Block/line conflict resolution seeds its editor from the working-tree marker file, not composed panes, and a new `conflict-v1:` token pins exactly what was served — closing the one blind spot no existing staleness check covers | Accepted — design only, no code |
| [0070](0070-a-ref-capture-says-which-kinds-it-recorded.md) | A ref capture says which kinds it recorded, and stays silent about the rest — HEAD, tags and remote-tracking refs join the per-event snapshot, each new field `Option` so a pre-#449 line's silence is never read as "there were none" | Accepted — implemented and tested |
| [0071](0071-a-badge-is-a-claim-about-a-commit.md) | A badge is a claim about a commit — HEAD is badged only when it resolves, so two readers stop contradicting each other; a dangling HEAD is recorded as `Unresolvable` rather than drawn as a badge pointing at nothing | Accepted — implemented and tested |
| [0072](0072-head-state-is-said-on-the-wire.md) | HEAD's state is said on the wire, not inferred from an absent branch name — `head_branch: null` covers a healthy detached HEAD and a broken one alike, and the topbar drew nothing for both | Accepted — implemented and tested |
| [0073](0073-a-pasted-token-re-runs-startup.md) | A sign-in link pasted into a live tab reloads the page rather than re-bootstrapping in place — a fragment edit fires only `hashchange`, and per-tab session facts are fixed once `establish_session` resolves | Accepted — implemented and tested |
| [0074](0074-a-diagnostic-may-not-fabricate.md) | A diagnostic may not fabricate — `doctor`'s clones root is read from the running listener's own `/proc/<pid>/environ`, says `unknown` when it cannot be read, and says whether it exists; a fabricated line inherits the credibility of the measured one beside it | Accepted — implemented and tested |
| [0075](0075-a-wip-run-is-a-chain-not-a-neighbourhood.md) | A WIP-checkpoint run is identified by ancestry, never by proximity — a branch and its diverged twin interleave row for row, so a scan over display neighbours is only ever shown cross-chain pairs; relaxing the lane check to fix that was refused as a quiet lie | Accepted — implemented and tested |
| [0076](0076-one-fixture-catalogue-in-rust.md) | One fixture catalogue, in Rust, shelled out to by the browser harness — twenty hand-rolled `seeded_repo()` builders and three conflicted-repository builders collapse into one crate whose doc comments are the teaching material, so a lesson that drifts from the code fails a test | Accepted — implemented and tested; browser leg unrun |
| [0077](0077-a-pop-reports-only-what-was-checked.md) | A pop is two operations, and its report never outruns what was checked — the drop runs only on an applied stash plus a conflict scan that really ran; a refused apply is not proof nothing was applied, so the scan is consulted on both outcomes; exactly one of seven verdicts claims completion | Accepted — implemented and tested (frontend; browser leg written, not executed) |
| [0080](0080-a-journal-line-may-point-at-its-batchs-capture.md) | A journal line may point at its batch's capture, and nothing leaves the server as a pointer — one operation reads the refs once, the batch's last line stores them, and `assemble_feed` resolves the reference before the feed goes out; `bytes/line` at 500 refs falls from 29,350 to 225 | Accepted — implemented and tested |
