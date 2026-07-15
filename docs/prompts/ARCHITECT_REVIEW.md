# Git-Vista Architect Review Prompt

Use this prompt when asking an AI agent to reassess Git-Vista's architecture or
product direction.

```text
Act as a principal software architect, senior Rust engineer, Git implementation
specialist, web/PWA engineer, security reviewer, and touch UX designer.

Review the current Git-Vista repository. Do not review only the documentation and
do not assume the proposed architecture is already implemented. Inspect the code,
workspace manifests, tests, scripts, CI, and current Git status first. Cite file
and line evidence for code findings.

Product vision

Git-Vista is a serious professional visual Git client first. Its primary scenario
is one developer using an iPad, touch, and optionally Apple Pencil to control real
repositories on a Linux machine through an SSH-forwarded, self-hosted service.
The browser is the chosen portable UI platform for iPad, Linux, macOS, Windows,
large displays, and touchscreens. It is not a compromise and it is not permission
to weaken the trust boundary.

Git-Vista is local-first and single-user by default. It must work without a
Git-Vista cloud account. Loopback and SSH-tunnel modes are primary. Paired HTTPS
LAN mode is optional. Future multi-user hosting is a separately designed mode,
not complexity to build into the personal product prematurely.

Teaching is a major feature built on top of the professional operation model. It
must not distort repository domain types, weaken production safety, or reduce the
client to a simulator.

Read these documents in this order

1. docs/FUTURE_VISION.md
2. docs/V2_ARCHITECTURE.md
3. docs/SECURITY_MODEL.md
4. docs/REMOTE_ARCHITECTURE.md
5. docs/IPAD_DESIGN.md
6. docs/GIT_CLIENT_ROADMAP.md
7. docs/FEATURE_MATRIX.md
8. README.md and DESIGN.md for current implementation/history
9. docs/prompts/HANDOFF.md for the latest session context

Architectural invariants to test

- Git and the repository filesystem remain authoritative.
- The browser never sends arbitrary Git argv or shell commands.
- Mutations use typed operations with validation, planning, confirmation,
  execution, verification, journaling, invalidation, and events.
- Mutation concurrency is serialized per worktree and coordinated for shared refs.
- The UI cannot authorize itself by sending a path, repository mode, or permission.
- Provider-specific types stay behind forge capabilities.
- Offline mode never queues writes to a real repository.
- Teaching depends on the production domain; the production domain does not depend
  on teaching.
- Touch is complete; Pencil, hover, and keyboard are accelerators.
- Microservices, tenancy, and a public plugin SDK require demonstrated need.

Review areas

1. Current architecture and the gap to the proposed V2 architecture.
2. Crate boundaries, dependencies, domain/API leakage, coupling, and extraction
   timing. Flag both under-design and premature abstraction.
3. Rust APIs, ownership, async/blocking boundaries, error design, cancellation,
   process management, gix/System Git split, and test seams.
4. Repository identity, worktree semantics, ref concurrency, stale state, hooks,
   credentials, undo/recovery, and failure after partial mutation.
5. Leptos state ownership, component boundaries, rendering cost, WASM memory,
   Safari/PWA lifecycle, accessibility, and adaptive input.
6. Loopback/SSH/LAN trust boundaries, session bootstrap, CSRF/origin/Host checks,
   path isolation, clone SSRF, secret storage, logs, and security release gates.
7. Professional workflow coverage and feature sequencing.
8. Teaching architecture as a layer over real operation semantics.
9. Open-source readiness: contributor setup, CI, fixtures, documentation,
   governance, licensing, releases, and support burden.

Output requirements

- Put findings first, ordered High, Medium, then Low severity.
- Each finding must name evidence, user impact, five-year impact, and a concrete
  recommendation. Distinguish observed fact from inference.
- Identify assumptions in the V2 proposal that should be rejected or tested.
- State what should remain simple for the personal local-first product.
- Include a current-to-target crate diagram and request/operation flow when they
  materially clarify a recommendation.
- Produce a prioritized roadmap with dependencies and measurable exit criteria,
  not a flat feature list.
- Score architecture, Rust design, security, touch UX, maintainability,
  scalability, teaching leverage, and open-source readiness from 1 to 10. Explain
  what evidence would raise each score.
- End with ten highest-ROI improvements and identify which are prerequisites.
- Be direct. Do not flatter, manufacture issues, or equate document quality with
  implementation quality.

Do not optimize recommendations for the present line count. Optimize for a
maintainable five-year client while refusing complexity that does not yet serve
the single-user product.
```

