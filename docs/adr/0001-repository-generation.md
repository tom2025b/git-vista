# ADR 0001 — Repository identity and the repository-generation algorithm

- **Status:** Accepted
- **Date:** 2026-07-15
- **Milestone / issue:** M1.01 — Establish Stable Repository and Worktree
  Identity (#101)
- **Supersedes / superseded by:** —

## Context

The V2 foundation must guarantee that *a stale browser tab cannot silently
execute an operation against a repository state different from the state the user
reviewed* (see `docs/GIT_CLIENT_ROADMAP.md`, "Foundation" exit criterion). Two
problems stand in the way of that guarantee today:

1. **Identity is a filesystem path.** The server addresses "the repository" by a
   `PathBuf` in process-global state (`git-vista-server`'s `CURRENT`). A path is
   the wrong thing to hand a browser: it leaks the server's filesystem, it is not
   stable across a move, and — critically — it does not distinguish a *shared
   repository* from one of its *worktrees*, which the parallel-work features will
   need to treat as separate actors.

2. **There is no notion of "which state".** Nothing lets a client say "I reviewed
   this repository *as it was*", and nothing lets the server check that the state
   has not moved on before it acts.

This ADR records the value types introduced to fix (1) and the algorithm chosen
for (2). The types live in pure code (`git-vista-core::identity`) so they compile
for both the native backend and the wasm frontend and cross the JSON boundary
unchanged; the native derivation from a real repository lives in
`git-vista-git::identity`.

## Decision

### Identity types

- **`RepositoryId`** and **`WorktreeId`** are opaque RFC 4122 **v5 (name-based)**
  UUIDs. `RepositoryId` is derived from the repository's canonicalised *common
  directory*; `WorktreeId` from the worktree's canonicalised *git directory*.
- **`ObjectId`** is a git object hash *validated on construction* (length must
  match SHA-1 = 40 or SHA-256 = 64 hex characters; characters must be lowercase
  hex). It is distinct from the loose `model::Oid` string used on the
  graph-drawing hot path, and is used wherever a hash is *identity* (generation
  inputs, operation preconditions) rather than a value flowing to the renderer.
- **`RepositoryHandle`** bundles `{ repository, worktree }` — the ID-based
  address the API uses in place of a path.

**Why derived v5 rather than random v4.** A v5 UUID is a SHA-1 over a fixed
namespace plus a name. Deriving the ids from the repository's canonical git
directory makes them **stable** — the same repository yields the same id across
server restarts with no persisted lookup table — while keeping them **opaque and
path-independent** to clients: the id is a 128-bit hash, the path cannot be
recovered from it, and nothing above the backend ever learns the path. It is also
**pure and randomness-free**, which is what lets the type live in the wasm-safe
core crate; only the `v5` feature of the `uuid` crate is enabled, never `v4`
(which would pull `getrandom`). A random v4 id would have required a persisted
path→id table to stay stable, adding state and a failure mode for no benefit.

**Why separate namespaces.** `RepositoryId`, `WorktreeId`, and the generation
digest each use a distinct namespace UUID, so that an identical input string
(e.g. an ordinary repo whose common dir *equals* its git dir) can never collide
across the three kinds. This is what makes "worktree and shared-repository
identity are distinct" hold even in the degenerate single-worktree case.

### Repository-generation algorithm

A **`RepositoryGeneration`** is a **content digest of the observable state of a
worktree**, not a monotonic counter. It is computed as follows:

1. Collect the observable inputs into a `GenerationInputs` builder as keyed
   `(key, value)` string fields:
   - **HEAD** — key `head`, value = `"<symbolic-target>\0<resolved-oid>"`, where
     the symbolic target is e.g. `refs/heads/main` (empty when detached) and the
     resolved oid is the commit HEAD points at (empty for an unborn HEAD).
   - **Each ref** — key `ref:<full-name>` (e.g. `ref:refs/heads/main`), value =
     the object id the ref peels to. Every local branch, remote-tracking branch,
     and tag contributes.
   - **The index** — key `index`, value = the index file checksum. Any
     stage/unstage rewrites the index and changes this.
   - **The working tree** — key `worktree`, value = a digest of the unstaged
     working-tree status (tracked modifications + untracked files).
2. **Canonicalise:** sort the fields by key, so the order in which inputs are
   recorded does not affect the result. Writing the same key twice keeps the last
   value, so the input set is always well-defined.
3. **Encode unambiguously:** for each `(key, value)`, append
   `len(key) as u64 BE ‖ key ‖ len(value) as u64 BE ‖ value`. Length-prefixing
   means no two distinct input sets can produce the same byte stream (e.g.
   `("ab","c")` and `("a","bc")` differ).
4. **Hash:** compute a v5 UUID of the byte stream under the generation namespace
   and take the leading 64 bits as the `u64` generation.

**Which changes advance a generation.** By construction: any change to HEAD (a
checkout, commit, or reset), to any ref (create, delete, rename, or retarget a
branch/tag/remote-tracking ref), to the index (stage/unstage), or to the working
tree (edit/add/remove a file). Changes that are *not* observable in these inputs
— reflog growth, config edits, object repacking — deliberately do not advance it,
because they do not change the state a user reviews before a mutation.

**How staleness is detected.** The client records the generation it reviewed. A
mutation is admitted only while the current generation still *equals* the recorded
one. Equality is the entire contract: equal ⇒ same observable state, differ ⇒ the
state moved and the client must refresh.

### Where the inputs come from

`git-vista-git::read_generation_inputs` populates HEAD, refs, and the index from
a `gix` read. It intentionally leaves the `worktree` slot empty and returns the
builder, because reading the full working-tree status is the status subsystem's
job (porcelain v2) and the request path already holds that read; the caller adds
`inputs.worktree(digest)` before folding. Staged changes are already caught via
the index checksum, so `git add` advances the generation without the worktree
slot; the worktree slot is what additionally catches *unstaged* edits.

## Alternatives considered

- **A monotonic counter maintained by a per-worktree actor.** Rejected as the
  primary mechanism. A counter carries ordering ("how many changes, in what
  order") that stale-detection does not need, and buys that ordering at the cost
  of *state*: to stay stable across restarts it must be persisted, and a reset or
  lost counter file becomes a correctness hazard (a new low number could match a
  number a client still holds). A content digest is stateless, survives restarts
  for free, and cannot be spoofed by resetting a counter. If a future feature
  genuinely needs ordering (e.g. an operation log), a counter can be layered on
  *alongside* the digest without changing the staleness contract.
- **Hashing the raw index/HEAD/ref *files* on disk.** Rejected: file
  representations carry incidental noise (index extension ordering, packed-vs-
  loose refs) that would advance the generation without an observable change, and
  would couple the algorithm to on-disk formats rather than observable state.
- **A cryptographic collision-resistant token as the generation.** Not needed.
  Staleness is not an adversarial channel here — the client and server are the
  same trust domain over loopback/SSH — so a 64-bit digest of a SHA-1 hash is
  ample to make an accidental "same generation, different state" collision
  negligible. The full 128-bit UUID is available internally if a wider token is
  ever wanted.

## Consequences

- API-facing repository selection can move from paths to `RepositoryHandle`;
  `RepositoryId`/`WorktreeId` are ready for the versioned protocol boundary
  (#102) and the repository catalog to build on without re-deciding identity.
- The generation is cheap to compute (one `gix` read + one hash) and needs no
  persisted state, so it can be recomputed on every snapshot and re-checked at
  the point of every mutation.
- The digest carries no ordering. Any feature that needs "how many / in what
  order" must add its own counter; it must not infer ordering from the
  generation value.
- The 64-bit generation has a negligible but non-zero accidental-collision
  probability. Because collisions are accidental (not adversarial) and each
  compared pair is a single before/after of one worktree, this is acceptable; the
  wider 128-bit value is available if that ever needs tightening.
