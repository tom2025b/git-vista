# ADR 0003 — A server-owned, allowlisted repository catalog addressed by opaque id

- **Status:** Accepted
- **Date:** 2026-07-15
- **Milestone / issue:** M1.03 — Secure Repository Access with an Allowlisted Catalog (#56)
- **Supersedes / superseded by:** —

## Context

M1.01 (ADR 0001) gave repositories and worktrees opaque, path-independent
identity (`RepositoryId` / `WorktreeId`). M1.02 (ADR 0002) recorded the guarantee
that **no endpoint selects a repository by a request-supplied path**. This ADR is
where that guarantee grows teeth: it introduces the thing that turns an opaque id
back into a filesystem path, and makes that the *only* way a request reaches a
repository.

Before this work the server held a single "current" repository as a filesystem
path in process-global state. That was safe only because the browser had no way
to name a different one. As the roadmap moves toward selecting among several
repositories (and typed mutations that bind to one — M1.06+), the browser needs
to say *which* repository each request acts on. If it said so with a path, a
`../../etc` traversal or a symlink could point the server's git commands
anywhere. The identity types exist precisely so the browser never holds a path;
what was missing was the server-side registry that maps id → path under a policy.

## Decision

### 1. A catalog is the only path→id resolver

A new `git-vista-server::catalog` module owns a `Catalog`: a set of **allowed
roots** and a map from `WorktreeId` to a registered repository. It is the single
place that resolves an opaque id to a path, and it **fails closed** — an id it did
not itself register resolves to nothing.

- **Registration** (`Catalog::register`) admits a path only when the
  repository's **canonical** (symlink-resolved) root lies within an allowed root.
  A `../` traversal or a symlink escaping the allowlist canonicalises to its real
  location and is rejected there, so both fail closed. Containment is checked
  component-wise (`Path::starts_with`), so `/srv/repos-secret` is not treated as
  within `/srv/repos`.
- **Resolution** (`Catalog::resolve`) returns an entry only for a `WorktreeId`
  the catalog holds. Requests address repositories by this id; a malformed id is
  a `400` and an unknown-but-well-formed id is a `404`.

Keying by `WorktreeId` (not `RepositoryId`) is deliberate: the main working tree
and each linked worktree are distinct servable targets that share one
`RepositoryId`, so the worktree id is the correct request handle.

### 2. Repositories are classified, not assumed

The gix boundary crate gains `read_repo_facts`, which classifies a path as a
**bare** repository, the **main** worktree, or a **linked** worktree
(`WorktreeKind`) and returns the canonical root the allowed-root check runs
against. Bare repos (no working tree) and linked worktrees (`git worktree add`)
are first-class in the catalog and the capability report, rather than being
flattened into "one working tree per clone".

### 3. Capabilities are reported by id, without paths

`GET /api/catalog` returns a `RepositoryDescriptor` per entry: the opaque
repository/worktree ids, a short **non-path** display name (the directory base
name), the kind, and the read-only flag. Absolute paths are **omitted by
default** — the server's filesystem layout is not the browser's business. An
operator who wants paths for local diagnosis sets `GIT_VISTA_EXPOSE_PATHS`, which
also switches the graph's `repo_label` from the base name to the full path. The
graph now also carries the `repo_id` / `worktree_id` it was read for, so a client
can echo the id back to address the same repository.

### 4. Trusted launch vs. untrusted request

`set_current` — the server-initiated selection used at startup and after a clone —
allows the target's own canonical root before registering, so `gv` can be
launched inside any repository and a fresh clone under the clones root can
register. This does **not** widen what a *request* can reach: requests never call
`set_current`; they resolve ids against what is already registered. Clone
destinations are held under the clones root (an allowed root), and the clone
handler confirms the canonical destination is allowed before serving it.

### 5. Staged frontend adoption

The read endpoints accept an optional `?repo=<worktree-id>` today, defaulting to
the current selection when absent, so the existing single-repo frontend keeps
working unchanged. Full frontend adoption of id-addressed requests lands with the
frontend state refactor (M1.11), which depends on this contract.

## Alternatives considered

- **Resolve paths from the request, canonicalise, and check a prefix.** Rejected:
  it keeps a path on the wire, so every endpoint would have to get the check
  exactly right forever. Opaque ids remove the path from the client entirely; the
  one resolver is the only place that can get it wrong, and it is unit-tested.
- **Key the catalog by `RepositoryId`.** Rejected: it cannot distinguish a
  repository's linked worktrees, which share a `RepositoryId` but are distinct
  servable targets.
- **Persist an id→path table.** Unnecessary: ids are *derived* (v5) from the
  canonical git directory (ADR 0001), so registration is reproducible across
  restarts with no stored table.

## Consequences

- A request can only ever reach a repository the server registered, addressed by
  an id it cannot forge into a path. Traversal and symlink escapes fail closed at
  registration; unknown ids fail closed at resolution.
- The capability report never leaks the server's filesystem by default.
- `git-vista-git` remains the sole holder of gix; the server catalog does path
  policy and storage, never repository reading.
- The typed operations of M1.06 can bind to a `WorktreeId` and resolve it through
  the same catalog, inheriting the fail-closed guarantee.
