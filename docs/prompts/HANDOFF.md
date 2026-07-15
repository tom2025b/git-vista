# Git-Vista Agent Handoff

Updated: 2026-07-14

This is the durable context entry point for a new coding session. Verify all
volatile facts against the repository before acting.

## Copyable Handoff Prompt

```text
You are continuing work on Git-Vista in this repository. Act as a senior Rust,
Git, Leptos/WASM, security, and touch UX engineer. Inspect before editing, preserve
unrelated user changes, and carry work through tests and documentation.

Product vision

Git-Vista is a professional visual Git client for one developer first. Its
primary workflow is:

    iPad browser/PWA
        -> SSH local port forward
        -> Git-Vista service bound to Linux loopback
        -> real repositories and worktrees on Linux

The browser is the portable UI platform across iPad, Linux, macOS, Windows,
external monitors, touch displays, and classroom displays. Touch must be a
complete interaction path. Apple Pencil, keyboard, trackpad, and hover are
enhancements.

The application is local-first and self-hosted. It must not require a Git-Vista
cloud account. Loopback and SSH-tunnel modes are primary; paired HTTPS LAN mode is
optional. Do not introduce team tenancy, distributed coordination, or
microservices into the personal app. Any future multi-user mode needs a separate
security and deployment design.

Teaching is a major layer on top of the real professional operation model. The
production domain must not depend on lessons, grading, or classroom concepts.

Authoritative direction

Read, in order:

1. docs/FUTURE_VISION.md
2. docs/V2_ARCHITECTURE.md
3. docs/SECURITY_MODEL.md
4. docs/REMOTE_ARCHITECTURE.md
5. docs/GIT_CLIENT_ROADMAP.md
6. docs/IPAD_DESIGN.md
7. docs/FEATURE_MATRIX.md

README.md describes the product and current implementation. DESIGN.md and
PROJECT_MEMORY.md are implementation history. PROJECT_STATUS.md is a historical
checkpoint and may be stale. CODE_REVIEW.md is an earlier review and is not the
V2 security authority.

Current implementation shape

- Rust workspace with four crates: git-vista-core, git-vista-git,
  git-vista-server, and the Leptos/WASM git-vista frontend.
- git-vista-core contains shared serialized models and pure graph/status/diff
  logic.
- git-vista-git performs native gix reads and repository inspection.
- git-vista-server owns Axum routes, constrained System Git writes, state,
  activity, and journal behavior.
- git-vista is a client-rendered Leptos app built with Trunk.
- The current product has a vertical graph, touch pan/pinch, refs, commit detail,
  commit diffs, status summary, branch/checkout/merge/rebase/push operations,
  public URL clone, activity history, and contextual undo. Verify exact behavior
  and tests in code; do not infer professional completeness from this list.

Target architecture

- Pure domain, versioned protocol, and graph crates.
- A native repository application layer with narrow reader/planner/executor ports.
- A constrained Git adapter using gix where it is correct for reads and System Git
  where compatibility is more important.
- One mutation actor per worktree plus repository-level coordination for shared
  refs.
- Typed Git operations with a plan/confirm/execute/verify/journal/event pipeline.
- Evidence-based recovery using operation records and private recovery refs.
- Provider-neutral forge capabilities with built-in adapters before any public
  plugin SDK.
- PWA app-shell caching and optional read-only metadata snapshots; never queue Git
  mutations offline.

Non-negotiable safety rules

- Never expose arbitrary shell or Git argv to the browser.
- Never trust a browser-supplied filesystem path or permission mode.
- Use opaque repository/worktree IDs rooted in configured allowed directories.
- Mutations require an expected repository generation and idempotency key.
- Destructive plans expire when repository state changes.
- Credentials and private keys stay server-side in standard helpers/agents or a
  protected secret store, not localStorage or IndexedDB.
- Treat hooks, filters, pagers, editors, credential helpers, clone URLs, process
  output, and cancellation as security/reliability boundaries.
- Do not promise undo without a checked inverse or recovery checkpoint.

Engineering expectations

- Keep domain code independent of Axum, Leptos, filesystem APIs, and provider SDKs.
- Keep transport DTOs versioned and separate from internal persistence models.
- Prefer narrow capability traits over a giant Repository trait.
- Do not create every proposed crate preemptively; use the extraction order in
  docs/V2_ARCHITECTURE.md.
- All blocking Git/filesystem/process work must have an explicit async boundary,
  cancellation behavior, timeout, and output limit.
- Ship each mutation with validation, preview, progress, result, recovery, tests,
  and touch-accessible UI.
- Test pure domain behavior, adapter fixtures, route policy, operation state
  machines, crash recovery, Safari lifecycle, and real Git compatibility.
- Preserve standard Git state so terminal and other clients can interoperate.

Start-of-session checklist

1. Run `git status --short --branch` and inspect recent commits.
2. Read any task-specific issue and the relevant architecture document.
3. Search current code and tests before proposing a new abstraction.
4. Identify existing user changes and do not revert them.
5. State the smallest complete vertical slice and its verification plan.

End-of-session checklist

1. Run targeted tests, workspace tests/checks, formatting, clippy, and frontend
   build as applicable; report anything not run.
2. Check the diff for generated assets, secrets, broad dependencies, and unrelated
   edits.
3. Update durable documentation only when behavior or an accepted decision changed.
4. Update the "Current Work" section below if this handoff file is part of the
   requested change.
5. Leave the worktree understandable to the next agent without claiming unverified
   success.
```

## Current Work

The current documentation task reframes Git-Vista around the professional,
touch-first, SSH-first, local-first V2 vision. The documents in `docs/` are a
proposal, not implemented architecture. During this task the server default was
also changed from `0.0.0.0:8080` to `127.0.0.1:8080`; `gv --lan` explicitly
restores the unauthenticated personal-LAN compatibility path and prints a warning.

Verification at this checkpoint: all 134 workspace tests passed with one build
job, `bash -n gv` passed, `git diff --check` passed, and the Trunk frontend build
passed with `NO_COLOR=false`. Strict clippy and repository-wide rustfmt still fail
on pre-existing baseline issues in unchanged files; these are captured as issue 0
in the local ignored `issues.md`. Rerun all checks after any further edit.

The next implementation work should be selected from the staged roadmap and issue
list after the documentation diff is reviewed.

At the start of a future session, replace this paragraph with the active branch,
issue, completed work, remaining work, verification results, and any blockers.
