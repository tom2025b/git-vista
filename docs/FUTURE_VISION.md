# Git-Vista Future Vision

Status: product direction

## Five-Year Thesis

Git-Vista should become the GitKraken-quality experience that ought to exist for
an iPad connected to Linux: a professional, touch-first, self-hosted Git client
that runs beside real repositories and uses the browser as its portable UI
platform.

The application remains local-first and useful to one developer without a
Git-Vista account, hosted control plane, or synchronized copy of the repository.
Classroom and teaching capabilities become a major differentiator because the
professional client already models operations, topology, consequences, and
recovery explicitly.

## Strategic Position

Git-Vista should not compete by cloning every menu in a desktop client. It should
own the intersection of:

- Touch-native Git interaction.
- Remote Linux repository execution through an SSH-first deployment.
- Transparent, typed, previewable, and recoverable operations.
- A consistent browser client across iPad, Linux, macOS, Windows, and displays.
- Self-hosting with no mandatory cloud dependency.
- Professional workflows and teaching experiences built on the same semantics.

This is a narrower initial market than "everyone who uses Git," but it provides a
coherent reason for the product and its architecture to exist.

## Product Principles

1. Real Git repositories and standard recovery mechanisms remain authoritative.
2. The default product serves one developer; team hosting is a separate mode and
   business problem.
3. The service owns credentials, authorization, process execution, and mutation
   safety. The browser owns presentation and interaction.
4. A professional workflow cannot depend on hover, right-click, or a keyboard.
5. Every destructive operation explains what will move and how recovery works.
6. Git-Vista adds abstractions to clarify Git, not to conceal incompatible magic.
7. Provider integration remains optional and subordinate to local Git workflows.
8. Offline means resilient UI and deliberate snapshots, never delayed writes to
   a real repository.
9. Extensions begin only after internal implementations prove a stable boundary.
10. Teaching content never gains access to production repositories implicitly.

## Evolution Horizons

### Horizon 1: Trustworthy Daily Driver

The immediate objective is not feature count. It is earning permission to modify
a user's repository. Establish the repository catalog, session boundary, typed
operation pipeline, concurrency model, journal, recovery references, paging, and
adaptive shell. Complete status-through-push daily work.

Success means one developer can use Git-Vista over an SSH tunnel for normal
feature-branch work and understand every failure without opening server logs.

### Horizon 2: Professional Depth

Add worktrees, stash, compare, cherry-pick, revert, conflict resolution, rebase,
blame, bisect, and forge summaries as related workflow families. Make operation
history and recovery a product surface rather than an implementation detail.

Success means Git-Vista is chosen because its touch and remote workflows are
better, not merely because it is the only available browser.

### Horizon 3: Learning on Real Semantics

Add explain mode, disposable repository scenarios, conflict and rebase trainers,
printable diagrams, assessment events, and instructor presentation. The simulator
implements the same typed operations against an isolated backend.

Success means a lesson learned in the sandbox maps directly to the same visual
operation in a real repository.

### Horizon 4: Ecosystem Without Core Erosion

Stabilize forge capabilities and then an out-of-process extension protocol.
Support lesson packs, new forges, and analysis tools without loading untrusted
native code into the repository service.

Only consider classroom coordination or multi-user hosting as separately named
modes with their own identity, authorization, isolation, and support model.

## Teaching Reconsidered

Teaching as a feature changes the architecture for the better:

- The professional operation planner produces explanations and diagrams.
- The operation journal becomes a learning timeline in an isolated lesson.
- Recovery previews teach reflog and ref movement through real concepts.
- Conflict and rebase tools are identical in training and production, except the
  training backend is disposable and deterministic.
- Instructors can present redacted or synthetic state without accessing student
  credentials or repositories.

Do not contaminate production domain types with grading fields or lesson steps.
Learning orchestration depends on the domain; the domain does not depend on
learning.

## Sustainable Open Source Direction

- Publish architecture decisions for security-sensitive and compatibility choices.
- Maintain a contributor-friendly simulator and fixture suite so contributors do
  not need private repositories or provider tokens.
- Keep the core client usable without commercial services.
- Define compatibility windows for API, journal schema, and extension protocol.
- Fund maintenance through optional support, managed deployment, or curriculum,
  not by making local repository access contingent on an account.

## Measures That Matter

- Percentage of ordinary feature-branch sessions completed without a terminal.
- Median and worst-case graph/diff interaction latency on a supported iPad.
- Rate of stale-state rejection, failed mutation recovery, and unrecoverable loss.
- Time for a first-time user to explain the refs changed by an operation.
- Accessibility task completion with finger-only and VoiceOver workflows.
- Number of provider-specific conditionals outside forge adapters: target zero.
- Number of mutation endpoints outside the typed operation pipeline: target zero.

## Assumptions to Challenge Regularly

- Browser delivery is valuable only if Safari constraints are tested continuously.
- `gix` is not automatically the right implementation for every read operation;
  correctness and compatibility decide per capability.
- System Git is not automatically safe because it is familiar; argv, environment,
  hooks, credentials, cancellation, and output remain attack surfaces.
- Touch-first does not mean touch-only; professional keyboard efficiency matters.
- Self-hosted does not mean unauthenticated.
- Local-first does not justify skipping repository isolation or concurrency control.
- Teaching differentiation will fail if the underlying client is not trustworthy.

## Non-Goals

Git-Vista should not become a source forge, cloud IDE, remote desktop, Git hosting
service, or proprietary replacement for standard Git. Those directions dilute
the product advantage and introduce operational burdens unrelated to excellent
touch-first version control.

