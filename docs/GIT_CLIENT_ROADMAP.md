# Git-Vista Git Client Roadmap

Status: proposed

This roadmap treats Git-Vista as a professional Git client for one developer,
not as a teaching demo and not as a promise to reproduce every desktop feature.
The product question is: **which professional Git workflows become more useful
when they are rebuilt for touch, a browser, and repositories on remote Linux?**

## Product Priority

### Must Have

- A repository catalog with explicit allowed roots and opaque repository IDs.
- Fast, paged commit history with search, filters, ref visibility, and comparison.
- Working-tree status, file and hunk diff, partial staging, commit, and amend.
- Branch, tag, remote, fetch, pull, push, and upstream management.
- Operation preview, stale-state checks, progress, cancellation, and recovery.
- Stash and worktree workflows suitable for parallel tasks.
- Conflict detection, three-way context, resolution, continue, and abort.
- Responsive portrait, landscape, split-screen, keyboard, and screen-reader UX.
- A secure loopback/SSH-tunnel mode that needs no Git-Vista cloud account.

### Should Have

- Cherry-pick, revert, commit comparison, blame, and file history.
- Interactive rebase represented as a touch-friendly plan, not a text todo file.
- Bisect with a visual search path and a record of good/bad decisions.
- Signed commits, multiple remotes, protected-operation warnings, and reflog views.
- Pull/merge request summaries, checks, reviews, and provider deep links.
- An explain mode that describes the same operations used by professionals.
- Recovery checkpoints and a truthful, operation-specific undo experience.

### Nice to Have

- Apple Pencil range selection, graph annotation, and instructor markup.
- Sparse-checkout, LFS, submodule, and advanced signing workflows.
- Provider issue context, review comments, and CI log summaries.
- Web Push for completed long-running operations when the installed PWA is idle.
- Shareable and printable diagrams with intentional redaction controls.
- Out-of-process extensions for forge providers and teaching content.

### Avoid Completely

- An arbitrary shell or arbitrary Git-argument endpoint.
- A built-in terminal or an attempt to become a general-purpose IDE.
- Mandatory hosted accounts, cloud sync, or telemetry for local operation.
- Storing a user's private SSH keys or Git credentials in browser storage.
- Queuing repository mutations while offline and replaying them later.
- Destructive swipe gestures or pressure-only Apple Pencil commands.
- A fake Git implementation in the production operation path.
- One generic `undo` button that implies every Git operation is reversible.
- Microservices, tenancy, and distributed locks in the single-user product.
- Provider-specific types in the core repository domain.

## Feature Families and Build Order

Features should be delivered as complete workflows. Adding isolated buttons
before state validation, progress, recovery, and tests creates unsafe product
debt.

### Foundation: Trustworthy Application Platform

Build before expanding write operations:

- Versioned protocol and structured errors.
- Repository catalog, allowed roots, and stable repository/worktree identity.
- Session bootstrap, origin/CSRF checks, and SSH-tunnel deployment.
- Repository generation numbers and idempotency keys.
- Per-worktree mutation actor and long-operation event stream.
- Operation plan, confirmation token, journal, and recovery references.
- Paged history and bounded diff APIs.
- Adaptive application shell and reconnect/state-restoration behavior.

Exit criterion: a stale browser tab cannot silently execute an operation against
a repository state different from the state the user reviewed.

### Daily Work: Status to Push

Build as one vertical slice:

1. Working-tree status and refresh after external changes.
2. File and hunk diff with whitespace and binary-file handling.
3. Stage, unstage, discard with explicit risk treatment, and partial staging.
4. Commit, amend, author/signing diagnostics, and hook progress.
5. Fetch, pull strategy selection, push, upstream selection, and authentication.

Exit criterion: Git-Vista can be the primary client for an ordinary feature
branch without requiring a terminal for routine work.

### Parallel Work and Recovery

Stash and worktrees belong together because both answer "where should unfinished
work live?"

- Stash list, inspect, create, apply, pop, branch-from-stash, and conflict flow.
- Worktree list, create from branch/commit, open, lock, prune, and remove.
- Shared-repository coordination across worktrees.
- Operation history, recovery references, and reflog-oriented recovery UI.

Exit criterion: switching tasks does not require hiding work behind unexplained
Git commands, and failed operations leave a visible recovery path.

### History Editing

Commit comparison, cherry-pick, revert, and rebase share commit selection,
operation planning, conflict handling, and continuation state.

1. Compare any two commits or refs.
2. Cherry-pick and revert one or a selected ordered range.
3. Interactive rebase plan with pick, reword, edit, squash, fixup, drop, reorder.
4. Rebase continue, skip, abort, and recovery checkpoint.

Do not implement drag-and-drop rebase as an immediate mutation. Dragging edits a
plan; a separate review step explains the resulting history rewrite.

### Investigation

- File history and rename-aware path traversal.
- Blame linked to commit details and comparisons.
- Bisect session with visual candidate range, notes, skip, run, reset, and resume.
- Reflog browser and "what changed my branch?" operation correlation.

These features share object lookup, comparison, and graph highlighting. They
should not each invent a separate history data path.

### Forge Integration

Add forge support only after local Git workflows are dependable:

1. Provider-neutral identity, repository, change-request, check, and review types.
2. GitHub adapter as the first interoperability reference.
3. Forgejo adapter to serve the self-hosted product direction.
4. GitLab adapter once the capability model survives two providers.
5. Create/update pull or merge requests only after read-only summaries are solid.

Provider capabilities must be detected. The UI should not render disabled
GitHub-shaped controls for a provider that uses different semantics.

### Teaching Layer

Teaching uses the production operation vocabulary and a separate sandbox
repository backend:

- Explain mode overlays preconditions, ref movement, worktree effects, and undo.
- Guided lessons compose typed operations and observable repository assertions.
- A simulator provides deterministic disposable repositories.
- Conflict and rebase trainers use the same plan UI as real work.
- Assessment mode records learning events, never private production repository
  content.

## Release Horizons

### V1: Safe Visual Client

- Preserve and harden the graph, commit detail, branch, merge, push, and clone.
- Establish protocol, session security, repository identity, operation journal,
  paging, and adaptive navigation.
- Add status, diffs, staging, commit, fetch/pull/push, and testable error flows.
- Support loopback and SSH-tunnel modes as first-class documented paths.

### V2: Professional Touch Client

- Deliver worktrees, stash, compare, cherry-pick, revert, conflict resolution,
  interactive rebase, blame, and bisect.
- Ship an installable PWA with safe read-only offline behavior.
- Add GitHub and Forgejo integration behind the forge capability boundary.
- Ship explain mode and the first interactive professional-workflow lessons.

### Later

- GitLab, richer reviews/checks, signed workflows, LFS, sparse checkout, and
  submodule support.
- Extension SDK after at least three internal adapters prove the boundary.
- Optional classroom coordination and a separately designed multi-user service.
- Offline simulation and lesson packs, not offline mutation of real repositories.

## Delivery Rules

- Each mutation ships with plan, execute, progress, result, recovery, tests, and
  documentation. A UI button alone is not a feature.
- Each touch workflow must also be keyboard and assistive-technology operable.
- Each networked feature documents its trust boundary and credential lifecycle.
- Performance budgets are measured on large repositories and real iPads.
- V2 scope is reduced by removing lower-priority features, not by bypassing the
  operation pipeline.

## External References

- Git's worktree model: <https://git-scm.com/docs/git-worktree.html>
- GitKraken interactive rebase: <https://help.gitkraken.com/gitkraken-desktop/interactive-rebase/>
- GitKraken worktrees: <https://support.gitkraken.com/gitkraken-desktop/worktrees/>
- LazyGit feature inventory: <https://github.com/jesseduffield/lazygit>

