# Repo Picker + Visualize/Active Modes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every repo open asks Visualize / Active; a picker lists the launch repo, root-scanned local repos, and clones; visualize mode refuses writes server-side; forge links point out to the repo's web host.

**Architecture:** Selection-based modes (ADR 0007): `POST /api/select {worktree, mode}` moves the process-global current selection and records the mode; `reject_if_read_only()` becomes a mode check. Repo discovery is a direct-children scan of one configured root (ADR 0009) registered into the existing fail-closed catalog. Forge links ride a new `remote_web_url` (any-host normalization) beside the existing GitHub-only `repo_url` (ADR 0010). Per-request write addressing stays reserved for M1.06 (ADR 0003).

**Tech Stack:** Rust workspace — Axum server, Leptos 0.6 CSR/WASM frontend, `gloo-net` fetch, system git via typed argv, Trunk build. Tests: `cargo test --workspace`, wire tests via `tower::ServiceExt::oneshot` (pattern: `security.rs::wire_tests`).

## Global Constraints

- One branch `feature/repo-picker-modes`, PR says `Closes #<issue ①>`. Never delete branches.
- Commits: `git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit …` (or `./dev wip`).
- `./dev gate` must be green before the PR (fmt, clippy native+wasm, tests, trunk build).
- New DTO fields carry `#[serde(default)]` (+ `skip_serializing_if` for `Option`) — M1.02 versioned-contract rule. Request bodies carry `#[serde(deny_unknown_fields)]`.
- Paths never cross the wire; ids resolve only through the catalog (ADR 0003). No per-request write addressing (`?repo=` on writes) — reserved for M1.06.
- The server remains the write boundary; client-side gating is UX + defense in depth only.
- Wire names are contract: pin serde renames in tests like `repository_kind_uses_stable_snake_case_wire_names`.
- Comment style: explain *why*, reference issues/ADRs, match the existing prose density.

---

### Task 1: Protocol — `RepoMode` + `SelectRequest` DTOs

**Files:**
- Modify: `crates/git-vista-protocol/src/dto.rs` (add after `CloneRequest`, ~line 68)
- Modify: `crates/git-vista-protocol/src/lib.rs` (re-export)

**Interfaces:**
- Produces: `git_vista_protocol::{RepoMode, SelectRequest}`; `RepoMode::{Visualize, Active}` with wire names `"visualize"`/`"active"`; `SelectRequest { worktree: String, mode: RepoMode }`. Also `RepositoryDescriptor.remote_web_url: Option<String>`.

- [ ] **Step 1: Write the failing tests** (in `dto.rs::tests`)

```rust
#[test]
fn select_request_roundtrips_and_rejects_unknown_fields() {
    let req = SelectRequest {
        worktree: "22222222-2222-5222-8222-222222222222".into(),
        mode: RepoMode::Visualize,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert_eq!(serde_json::from_str::<SelectRequest>(&json).unwrap(), req);
    // No path smuggling on the select body either.
    assert!(serde_json::from_str::<SelectRequest>(
        r#"{"worktree":"w","mode":"active","path":"/etc"}"#
    )
    .is_err());
}

#[test]
fn repo_mode_uses_stable_snake_case_wire_names() {
    assert_eq!(serde_json::to_string(&RepoMode::Visualize).unwrap(), "\"visualize\"");
    assert_eq!(serde_json::to_string(&RepoMode::Active).unwrap(), "\"active\"");
}

#[test]
fn repository_descriptor_omits_remote_web_url_when_none() {
    // Extend the existing repository_descriptor_roundtrips… test's struct literal
    // with `remote_web_url: None` and add:
    // assert!(!json.contains("remote_web_url"));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p git-vista-protocol` → FAIL: `SelectRequest` not found.

- [ ] **Step 3: Implement**

```rust
/// Which experience a repository is opened in (ADR 0006/0007). `Visualize` is
/// look-only: the server refuses every mutation while it is the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoMode {
    Visualize,
    Active,
}

/// Body of `POST /api/select` (ADR 0007): make the repository addressed by the
/// opaque `worktree` id the current selection, opened in `mode`. The id resolves
/// only through the server-owned catalog — an unknown/forged id is a 404.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectRequest {
    pub worktree: String,
    pub mode: RepoMode,
}
```

On `RepositoryDescriptor` add (after `path`):

```rust
    /// The repo's `origin` remote normalized to a browsable https base URL
    /// (ADR 0010), e.g. `"https://github.com/owner/repo"`. `None` when there is
    /// no usable remote. Optional on the wire (M1.02 contract rule).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_web_url: Option<String>,
```

In `lib.rs`, add `RepoMode, SelectRequest` to the existing `pub use dto::{…}` list. Fix the two existing descriptor test literals by adding `remote_web_url: None`.

- [ ] **Step 4: Run** — `cargo test -p git-vista-protocol` → PASS (all, including existing descriptor tests updated with the new field).

- [ ] **Step 5: Commit** — `feat(protocol): RepoMode + SelectRequest DTOs, descriptor remote_web_url`

---

### Task 2: Forge URL normalization (git crate) + pure link builders (core)

**Files:**
- Modify: `crates/git-vista-git/src/github.rs` (generalize; keep `github_web_base`)
- Modify: `crates/git-vista-git/src/lib.rs` (re-export)
- Create: `crates/git-vista-core/src/forge.rs`
- Modify: `crates/git-vista-core/src/lib.rs` (declare `pub mod forge;`)

**Interfaces:**
- Produces: `git_vista_git::remote_web_base(path: &Path) -> Option<String>` (any host); `git_vista_core::forge::{commit_url(base: &str, id: &str) -> String, branch_url(base: &str, branch: &str) -> String, host_label(base: &str) -> String}`.
- Consumes: nothing new.

- [ ] **Step 1: Failing tests**

In `github.rs::tests`:

```rust
#[test]
fn any_host_remotes_normalize_to_a_web_base() {
    let f = any_web_base_from_remote;
    assert_eq!(f("git@gitlab.com:owner/repo.git"),
               Some("https://gitlab.com/owner/repo".into()));
    assert_eq!(f("https://codeberg.org/owner/repo.git"),
               Some("https://codeberg.org/owner/repo".into()));
    // Unknown host still yields the normalized base (ADR 0010: best-effort link).
    assert_eq!(f("ssh://git@git.example.net/owner/repo.git"),
               Some("https://git.example.net/owner/repo".into()));
    // Owner-only, local paths, empty: no link.
    assert_eq!(f("git@host.com:owner.git"), None);
    assert_eq!(f("/local/path/repo.git"), None);
    assert_eq!(f(""), None);
}
```

In `forge.rs` (new) tests:

```rust
#[test]
fn commit_and_branch_urls_use_each_forges_path_shape() {
    assert_eq!(commit_url("https://github.com/o/r", "abc"),
               "https://github.com/o/r/commit/abc");
    // GitLab inserts /-/ before commit/tree paths.
    assert_eq!(commit_url("https://gitlab.com/o/r", "abc"),
               "https://gitlab.com/o/r/-/commit/abc");
    assert_eq!(branch_url("https://gitlab.com/o/r", "main"),
               "https://gitlab.com/o/r/-/tree/main");
    assert_eq!(branch_url("https://codeberg.org/o/r", "dev"),
               "https://codeberg.org/o/r/tree/dev");
}

#[test]
fn host_label_is_the_bare_host() {
    assert_eq!(host_label("https://github.com/o/r"), "github.com");
    assert_eq!(host_label("not a url"), "remote");
}
```

- [ ] **Step 2: Run to fail** — `cargo test -p git-vista-git -p git-vista-core` → FAIL (functions missing).

- [ ] **Step 3: Implement**

`github.rs` — factor the existing host/path reduction (lines 22–34) into a helper both parsers share, then:

```rust
/// The web base URL for a repository's `origin` remote on ANY host (ADR 0010):
/// `https://<host>/<owner>/<repo>`, or `None` when there's no origin or the URL
/// has no owner/repo shape. Unlike [`github_web_base`] this does not filter by
/// host — unknown forges get a best-effort base link.
pub fn remote_web_base(path: &Path) -> Option<String> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).ok()?;
    let url = repo.config_snapshot().string("remote.origin.url")?;
    any_web_base_from_remote(&url.to_string())
}

/// Host-agnostic version of [`web_base_from_remote`]: same scheme/user@ stripping
/// and owner/repo requirement, but keeps whatever host it finds. Pure, unit-tested.
fn any_web_base_from_remote(remote: &str) -> Option<String> {
    let host_and_path = reduce_to_host_and_path(remote)?; // the shared helper
    let (host, path) = host_and_path.split_once('/')?;
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.splitn(3, '/');
    let owner = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    // Drop a port if present (host:8443) — web URLs on forges are https-standard.
    let host = host.split(':').next().unwrap_or(host);
    Some(format!("https://{host}/{owner}/{repo}"))
}
```

(`web_base_from_remote` then becomes `any_web_base_from_remote` + a `github.com` host check, so the GitHub matrix keeps passing unchanged.)

`lib.rs`: `pub use github::{github_web_base, remote_web_base};`

`crates/git-vista-core/src/forge.rs`:

```rust
//! Pure forge-URL builders (ADR 0010): given a normalized web base
//! (`https://host/owner/repo`), produce commit/branch page URLs. GitLab nests
//! repo pages under `/-/`; GitHub, Gitea/Codeberg and most others don't. Pure
//! string work, shared by the wasm frontend, so it's host-unit-tested here.

/// The commit page URL for `id` under `base`.
pub fn commit_url(base: &str, id: &str) -> String {
    if is_gitlab(base) { format!("{base}/-/commit/{id}") } else { format!("{base}/commit/{id}") }
}

/// The branch (tree) page URL for `branch` under `base`.
pub fn branch_url(base: &str, branch: &str) -> String {
    if is_gitlab(base) { format!("{base}/-/tree/{branch}") } else { format!("{base}/tree/{branch}") }
}

/// The bare host of `base` for UI labels ("View commit on github.com");
/// "remote" when the base doesn't parse.
pub fn host_label(base: &str) -> String {
    base.strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "remote".to_string())
}

fn is_gitlab(base: &str) -> bool {
    host_label(base) == "gitlab.com" || host_label(base).starts_with("gitlab.")
}
```

- [ ] **Step 4: Run** — `cargo test -p git-vista-git -p git-vista-core` → PASS.
- [ ] **Step 5: Commit** — `feat(git,core): any-host remote web base + pure forge URL builders`

---

### Task 3: Server state — mode replaces the read-only bool on the selection

**Files:**
- Modify: `crates/git-vista-server/src/state.rs`
- Modify: `crates/git-vista-server/src/catalog.rs` (`RepoEntry.remote_web_url`)
- Modify: `crates/git-vista-server/src/handlers/clone.rs:108` (call-site)
- Modify: `crates/git-vista-server/src/main.rs:112` (call-site)

**Interfaces:**
- Produces: `state::current_mode() -> RepoMode`; `state::set_current(path: &Path, mode: RepoMode)`; `state::select_registered(worktree: WorktreeId, mode: RepoMode) -> bool`. `state::current() -> (PathBuf, bool)` KEEPS its signature — the bool becomes `mode == Visualize` so every read handler stays untouched.
- Consumes: `git_vista_protocol::RepoMode` (Task 1).

- [ ] **Step 1: Failing test** (in `state.rs::tests` — one test fn drives the globals end-to-end so parallel tests never fight over `CURRENT`; no other test in the crate touches it)

```rust
#[test]
fn selection_flow_carries_mode_and_gates_writes() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("project");
    // init a repo like catalog.rs::tests::init_repo does
    std::fs::create_dir_all(&repo).unwrap();
    assert!(std::process::Command::new("git").args(["init", "-q"])
        .current_dir(&repo).status().unwrap().success());

    set_current(&repo, git_vista_protocol::RepoMode::Active);
    assert_eq!(current_mode(), git_vista_protocol::RepoMode::Active);
    assert!(reject_if_read_only().is_none(), "active mode allows writes");
    assert!(!current().1);

    let wt = current_handle().expect("registered").worktree;
    assert!(select_registered(wt, git_vista_protocol::RepoMode::Visualize));
    assert_eq!(current_mode(), git_vista_protocol::RepoMode::Visualize);
    let (status, msg) = reject_if_read_only().expect("visualize refuses writes");
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert!(msg.contains("Visualize"));
    assert!(current().1, "compat bool mirrors visualize");

    // A forged id changes nothing and reports failure (the 404 path).
    let stranger = git_vista_core::identity::WorktreeId::from_git_dir("/nowhere/.git");
    assert!(!select_registered(stranger, git_vista_protocol::RepoMode::Active));
    assert_eq!(current_mode(), git_vista_protocol::RepoMode::Visualize);
}
```

- [ ] **Step 2: Run to fail** — `cargo test -p git-vista-server selection_flow` → FAIL (functions missing).

- [ ] **Step 3: Implement** in `state.rs`:

```rust
use git_vista_protocol::RepoMode;

struct Current {
    path: PathBuf,
    /// Visualize = look-only: every write handler refuses (ADR 0007). Replaces
    /// the old `read_only: bool` (Phase-12 clones), which this supersedes.
    mode: RepoMode,
    handle: Option<RepositoryHandle>,
}

pub(crate) fn current_mode() -> RepoMode { /* read lock, g.mode */ }

/// Snapshot (path, visualize?) — the bool keeps the old read_only meaning so the
/// many read-handler call sites stay untouched; writes should use current_mode().
pub(crate) fn current() -> (PathBuf, bool) {
    /* (g.path.clone(), g.mode == RepoMode::Visualize) */
}

pub(crate) fn set_current(path: &Path, mode: RepoMode) { /* as before; store mode */ }

/// `POST /api/select` (ADR 0007): move the current selection to an id the catalog
/// already holds, in `mode`. Returns false — and changes nothing — for an unknown
/// or forged id: the handler turns that into a 404, same contract as reads.
pub(crate) fn select_registered(worktree: WorktreeId, mode: RepoMode) -> bool {
    match resolve_worktree(worktree) {
        Some((path, _entry_read_only, handle)) => {
            set_current_resolved(path, mode, Some(handle));
            true
        }
        None => false,
    }
}

pub(crate) fn reject_if_read_only() -> Option<(StatusCode, String)> {
    if current_mode() == RepoMode::Visualize {
        Some((
            StatusCode::FORBIDDEN,
            "This repository is open in Visualize mode — look-only. Reopen it in \
             Active mode to make changes."
                .to_string(),
        ))
    } else {
        None
    }
}
```

`set_current_resolved` takes `mode: RepoMode` instead of `read_only: bool`. The `register()` second argument keeps feeding `RepoEntry.read_only` (clones default look-only) — pass `mode == RepoMode::Visualize` at the two `set_current` call sites' `register` step. Call sites: `main.rs:112` → `set_current(&repo, RepoMode::Active);` `clone.rs:108` → `set_current(&dest, RepoMode::Visualize);` (import `RepoMode` in both).

`catalog.rs`: add to `RepoEntry`:

```rust
    /// Normalized web base of the repo's origin remote (ADR 0010), read once at
    /// registration. `None` = no usable remote.
    pub(crate) remote_web_url: Option<String>,
```

populate in `register()` with `git_vista_git::remote_web_base(&facts.root)` and map it into `RepositoryDescriptor` in `descriptors()` (`remote_web_url: e.remote_web_url.clone()`). Update the catalog test struct expectations only where they construct descriptors.

- [ ] **Step 4: Run** — `cargo test -p git-vista-server` → PASS (including untouched wire_tests).
- [ ] **Step 5: Commit** — `feat(server): selection carries RepoMode; visualize gates writes (ADR 0007)`

---

### Task 4: Server — root scan (`GIT_VISTA_REPO_ROOT`) at startup

**Files:**
- Modify: `crates/git-vista-server/src/state.rs` (scan fn + env read)
- Modify: `crates/git-vista-server/src/catalog.rs` (scan core, testable on a local `Catalog`)
- Modify: `crates/git-vista-server/src/main.rs` (startup call, after `set_current`)

**Interfaces:**
- Produces: `catalog::Catalog::scan_direct_children(&mut self, root: &Path) -> (usize, usize)` (registered, skipped); `state::repo_root() -> Option<PathBuf>`; `state::scan_repo_root() -> Option<(usize, usize)>` (None = no root configured) — Task 6's rescan reuses it.

- [ ] **Step 1: Failing test** (in `catalog.rs::tests`, local `Catalog`, no globals)

```rust
#[test]
fn scan_registers_direct_child_repos_and_skips_junk() {
    let root = tempfile::tempdir().unwrap();
    init_repo(&root.path().join("repo-a"));
    init_repo(&root.path().join("repo-b"));
    std::fs::create_dir_all(root.path().join("not-a-repo")).unwrap();
    std::fs::write(root.path().join("stray-file.txt"), "x").unwrap();
    // A repo one level deeper must NOT register (direct children only, ADR 0009).
    init_repo(&root.path().join("not-a-repo/nested"));

    let mut catalog = Catalog::new();
    let (registered, skipped) = catalog.scan_direct_children(root.path());
    assert_eq!(registered, 2);
    assert_eq!(skipped, 1, "the non-repo directory is skipped (files don't count)");
    let names: Vec<String> = catalog.descriptors(false).iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, vec!["repo-a", "repo-b"]);
}

#[test]
fn scan_of_a_missing_root_is_a_soft_zero_not_a_panic() {
    let mut catalog = Catalog::new();
    let (registered, skipped) = catalog.scan_direct_children(Path::new("/no/such/dir"));
    assert_eq!((registered, skipped), (0, 0));
}
```

- [ ] **Step 2: Run to fail** — `cargo test -p git-vista-server scan_` → FAIL.

- [ ] **Step 3: Implement** in `catalog.rs`:

```rust
/// Scan `root`'s DIRECT children (ADR 0009: one deliberate root, no recursion)
/// and register every valid git repository, allowing `root` first. Junk children
/// are skipped and logged; a missing/unreadable root is a warning and an empty
/// scan — the server stays healthy (spec: degraded, never fatal). Returns
/// (registered, skipped-directories).
pub(crate) fn scan_direct_children(&mut self, root: &Path) -> (usize, usize) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("git-vista: repo root {} not scanned: {e}", root.display());
            return (0, 0);
        }
    };
    self.allow_root(root);
    let (mut registered, mut skipped) = (0, 0);
    let mut children: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    children.sort(); // stable log/scan order
    for child in children {
        match self.register(&child, false) {
            Ok(_) => registered += 1,
            Err(e) => {
                skipped += 1;
                eprintln!("git-vista: skipping {} ({e})", child.display());
            }
        }
    }
    (registered, skipped)
}
```

`state.rs`:

```rust
/// The operator-configured repos root (ADR 0009): `GIT_VISTA_REPO_ROOT`, set by
/// `gv --root <dir>` (env form so systemd units can set it too). None = feature off.
pub(crate) fn repo_root() -> Option<PathBuf> {
    std::env::var_os("GIT_VISTA_REPO_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Scan the configured root into the catalog (startup and /api/rescan).
pub(crate) fn scan_repo_root() -> Option<(usize, usize)> {
    let root = repo_root()?;
    Some(catalog().write().expect("catalog lock").scan_direct_children(&root))
}
```

`main.rs`, right after `set_current(&repo, RepoMode::Active);`:

```rust
    // ADR 0009: register every direct-child repo of the configured root, so the
    // picker can offer them. No root configured → exactly the old behavior.
    if let Some((registered, skipped)) = state::scan_repo_root() {
        println!("git-vista: repo root scan: {registered} registered, {skipped} skipped");
    }
```

- [ ] **Step 4: Run** — `cargo test -p git-vista-server` → PASS.
- [ ] **Step 5: Commit** — `feat(server): direct-children repo-root scan into the catalog (ADR 0009)`

---

### Task 5: Server — `POST /api/select` + `POST /api/rescan` routes

**Files:**
- Create: `crates/git-vista-server/src/handlers/select.rs`
- Modify: `crates/git-vista-server/src/handlers/mod.rs` (declare `pub(crate) mod select;`)
- Modify: `crates/git-vista-server/src/main.rs` (two routes + imports)

**Interfaces:**
- Consumes: `SelectRequest`/`RepoMode` (Task 1), `state::{select_registered, scan_repo_root, clones_root, allow_repo_root}` (Tasks 3–4).
- Produces: routes `POST /api/select` (200 / 400 malformed id / 404 unknown id) and `POST /api/rescan` (200 with a summary line). Both are `/api/*` POSTs, so session + CSRF + Host/Origin gating apply automatically.

- [ ] **Step 1: Failing tests** (in `select.rs`; handler-level like the other handler tests, plus the flow test from Task 3 already covering state)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use git_vista_protocol::{RepoMode, SelectRequest};

    #[tokio::test]
    async fn select_refuses_a_malformed_and_an_unknown_id() {
        let (status, _) = select_repo(axum::Json(SelectRequest {
            worktree: "not-an-id".into(),
            mode: RepoMode::Visualize,
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = select_repo(axum::Json(SelectRequest {
            // Valid uuid shape, never registered → fail-closed 404.
            worktree: "99999999-9999-5999-8999-999999999999".into(),
            mode: RepoMode::Active,
        }))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
```

- [ ] **Step 2: Run to fail** — `cargo test -p git-vista-server select_refuses` → FAIL (module missing).

- [ ] **Step 3: Implement** `handlers/select.rs`:

```rust
//! `POST /api/select` (ADR 0007) and `POST /api/rescan` (ADR 0009).
//!
//! Select moves the process-global current selection to a repository the catalog
//! already holds — addressed by opaque id, resolved fail-closed — and records the
//! Visualize/Active mode the operator chose. Rescan re-reads the configured repo
//! root and the clones root without a restart. Both sit behind the full M1.04
//! auth gate like every other mutation.

use axum::http::StatusCode;
use axum::Json;

use git_vista_core::identity::WorktreeId;
use git_vista_protocol::SelectRequest;

use crate::state::{allow_repo_root, clones_root, scan_repo_root, select_registered};

pub(crate) async fn select_repo(Json(req): Json<SelectRequest>) -> (StatusCode, String) {
    let worktree: WorktreeId = match req.worktree.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()),
    };
    if select_registered(worktree, req.mode) {
        (StatusCode::OK, "Selected.".to_string())
    } else {
        // Same fail-closed contract as the reads: unknown/forged id → 404.
        (StatusCode::NOT_FOUND, "No such repository.".to_string())
    }
}

pub(crate) async fn rescan(_body: Option<Json<serde_json::Value>>) -> (StatusCode, String) {
    // NOTE: bodyless POST (the app sends no content type; the M1.04 gate allows it).
    let root = scan_repo_root();
    let clones = clones_root();
    if clones.exists() {
        allow_repo_root(&clones);
    }
    let summary = match root {
        Some((registered, skipped)) => {
            format!("Rescanned: {registered} repos registered, {skipped} skipped.")
        }
        None => "No repo root configured; nothing to rescan.".to_string(),
    };
    (StatusCode::OK, summary)
}
```

(If `rebase`'s handler shows the house pattern for bodyless POSTs — check `handlers/rebase.rs` — copy that signature style instead of the `Option<Json>` shim.)

`main.rs` routes, after `/api/clone`:

```rust
        // ADR 0007: pick the current repository + Visualize/Active mode by id.
        .route("/api/select", post(select_repo))
        // ADR 0009: re-scan the configured repo root without a restart.
        .route("/api/rescan", post(rescan))
```

with `use handlers::select::{rescan, select_repo};`.

- [ ] **Step 4: Run** — `cargo test -p git-vista-server` → PASS.
- [ ] **Step 5: Commit** — `feat(server): POST /api/select and /api/rescan (ADR 0007/0009)`

---

### Task 6: Server — stamp `remote_web_url` (+ mode) onto the graph read

**Files:**
- Modify: `crates/git-vista-core/src/model.rs` (Graph field)
- Modify: `crates/git-vista-server/src/handlers/read.rs:99-113`

**Interfaces:**
- Produces: `Graph.remote_web_url: Option<String>` (`#[serde(default)]`), populated for any host; `Graph.read_only` now means "current mode is Visualize" when the graph is the current selection.

- [ ] **Step 1: Failing test** — extend the existing model serde test (or add):

```rust
#[test]
fn graph_remote_web_url_defaults_when_absent_from_wire() {
    let g: Graph = serde_json::from_str(r#"{"rows":[],"edges":[],"lane_count":0}"#).unwrap();
    assert_eq!(g.remote_web_url, None);
}
```

- [ ] **Step 2: Run to fail** — `cargo test -p git-vista-core graph_remote` → FAIL.

- [ ] **Step 3: Implement** — `model.rs` after `repo_url`:

```rust
    /// The origin remote normalized to a browsable https base for ANY forge host
    /// (ADR 0010) — GitHub, GitLab, Codeberg, or a best-effort unknown host.
    /// Unlike [`repo_url`](Self::repo_url) (GitHub-only, drives the pushed-commit
    /// dot links) this powers the general "view on <host>" links. `None` => no
    /// usable remote; the links are simply absent.
    #[serde(default)]
    pub remote_web_url: Option<String>,
```

`read.rs::commits`, after the `repo_url` stamp (line 105):

```rust
    // Any-host web base (ADR 0010) for the general forge links; repo_url above
    // stays GitHub-only for the existing pushed-commit link behavior.
    graph.remote_web_url = git_vista_git::remote_web_base(repo);
```

`graph.read_only = read_only;` is already correct: `current()`'s bool now means Visualize (Task 3), and a `?repo=` read reports that entry's stored read-only flag as before.

- [ ] **Step 4: Run** — `cargo test --workspace` → PASS.
- [ ] **Step 5: Commit** — `feat(server,core): graph carries any-host remote_web_url (ADR 0010)`

---

### Task 7: Frontend api.rs — catalog/select calls + the visualize write chokepoint

**Files:**
- Modify: `crates/git-vista/src/api.rs`

**Interfaces:**
- Produces: `fetch_catalog() -> Result<Vec<RepositoryDescriptor>, String>`; `select_request(worktree: &str, mode: RepoMode) -> Result<(), String>`; `rescan_request() -> Result<String, String>`; `set_ui_mode(Option<RepoMode>)`. Every existing repo-write fn refuses early in Visualize.
- Consumes: `RepositoryDescriptor`, `RepoMode`, `SelectRequest` from the protocol crate.

- [ ] **Step 1: Implement** (wasm-side; no host test — the server 403 wire test in Task 3 covers the boundary; this is the ADR 0007 defense-in-depth layer)

Next to `CSRF_TOKEN`:

```rust
// The mode the current repo is open in (ADR 0006/0007), mirrored from the last
// graph load / selection. Purely defense in depth: in Visualize the write fns
// below refuse before any network call; the server's 403 is the real boundary.
thread_local! {
    static UI_MODE: RefCell<Option<RepoMode>> = const { RefCell::new(None) };
}

pub fn set_ui_mode(mode: Option<RepoMode>) {
    UI_MODE.with(|m| *m.borrow_mut() = mode);
}

fn refuse_if_visualize() -> Result<(), String> {
    let visualize = UI_MODE.with(|m| *m.borrow() == Some(RepoMode::Visualize));
    if visualize {
        Err("This repository is open in Visualize mode — look-only.".to_string())
    } else {
        Ok(())
    }
}
```

Add `refuse_if_visualize()?;` as the first line of: `create_branch_request`, `create_commit_request`, `stage_request`, `unstage_request`, `undo_request`, `rebase_request`, `reset_test_repo_request`, `branch_op_request`. (NOT `clone_request` — cloning isn't a repo write — and NOT the session/select/rescan calls.)

New calls (same shapes as the existing ones):

```rust
/// The servable repositories (`GET /api/catalog`, consumed at last — M1.03 built
/// it, the picker uses it). Cache-busted like every live read.
pub async fn fetch_catalog() -> Result<Vec<RepositoryDescriptor>, String> {
    let url = format!("/api/catalog?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<RepositoryDescriptor>>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Make `worktree` the current repo in `mode` (`POST /api/select`, ADR 0007).
pub async fn select_request(worktree: &str, mode: RepoMode) -> Result<(), String> {
    let body = SelectRequest { worktree: worktree.to_string(), mode };
    let resp = req_post("/api/select")
        .json(&body).map_err(|e| e.to_string())?
        .send().await.map_err(network_error)?;
    if resp.ok() { Ok(()) } else { Err(response_error(resp).await) }
}

/// Re-scan the configured repo root (`POST /api/rescan`, ADR 0009). Ok carries
/// the server's summary line for the picker to show.
pub async fn rescan_request() -> Result<String, String> {
    let resp = req_post("/api/rescan").send().await.map_err(network_error)?;
    if resp.ok() { Ok(resp.text().await.unwrap_or_default()) } else { Err(response_error(resp).await) }
}
```

Update the `use git_vista_protocol::{…}` import list accordingly.

- [ ] **Step 2: Verify it compiles for wasm** — `cargo clippy -p git-vista-ui --target wasm32-unknown-unknown` (or just `./dev gate` at the task's end) → clean.
- [ ] **Step 3: Commit** — `feat(ui): catalog/select/rescan calls + visualize write chokepoint`

---

### Task 8: Frontend — repo picker + mode screen overlays

**Files:**
- Create: `crates/git-vista/src/picker.rs`
- Modify: `crates/git-vista/src/main.rs` (declare `mod picker;` — match how `session`/`update_required` are declared)

**Interfaces:**
- Produces: `picker::picker_view(open: RwSignal<bool>, mode_for: RwSignal<Option<RepositoryDescriptor>>, open_url: RwSignal<bool>, clone_url: RwSignal<String>, open_opened_at: StoredValue<f64>, reload: RwSignal<u32>) -> impl IntoView` and `picker::mode_view(mode_for: RwSignal<Option<RepositoryDescriptor>>, picker_open: RwSignal<bool>, reload: RwSignal<u32>) -> impl IntoView`.
- Consumes: `fetch_catalog`, `select_request`, `rescan_request`, `set_ui_mode` (Task 7).

- [ ] **Step 1: Implement** — both views use the `not_connected_view` recipe exactly (inline-styled `position:fixed` full-screen panel, `z-index:900` — below the sign-in/protocol overlays at 1000, above everything else):

```rust
//! The repo picker and Visualize/Active mode screens (ADR 0006/0009/0010).
//!
//! Both are blocking full-screen overlays in the iPad-proven inline-style
//! pattern of `session::not_connected_view`. The picker lists what the server's
//! catalog offers — launch repo, root-scanned repos, clones — as opaque
//! descriptors (never paths); picking one opens the mode screen; choosing a mode
//! POSTs /api/select and bumps `reload` so the graph re-reads.

use leptos::*;

use git_vista_protocol::{RepoMode, RepositoryDescriptor, RepositoryKind};

use crate::api::{fetch_catalog, rescan_request, select_request, set_ui_mode};

pub fn picker_view(
    open: RwSignal<bool>,
    mode_for: RwSignal<Option<RepositoryDescriptor>>,
    open_url: RwSignal<bool>,
    clone_url: RwSignal<String>,
    open_opened_at: StoredValue<f64>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    // Refetch every time the picker opens (and after a rescan) — the catalog
    // changes at runtime (clones, rescans), so a cached list would mislead.
    let bump = create_rw_signal(0u32);
    let catalog = create_local_resource(
        move || (open.get(), bump.get()),
        |(is_open, _)| async move {
            if is_open { Some(fetch_catalog().await) } else { None }
        },
    );
    let rescan_msg = create_rw_signal(String::new());
    move || open.get().then(|| view! {
        <div style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                    z-index:900; display:flex; align-items:center; \
                    justify-content:center; background:rgba(1,4,9,0.85);">
            <div style="min-width:320px; max-width:90vw; max-height:85vh; \
                        overflow-y:auto; padding:24px; background:#161b22; \
                        border:1px solid #30363d; border-radius:10px; color:var(--fg);">
                <div style="font-weight:600; font-size:1.2em; margin-bottom:12px;">
                    "Open a repository"
                </div>
                {move || match catalog.get().flatten() {
                    None => view! { <p>"Loading repositories…"</p> }.into_view(),
                    Some(Err(e)) => view! { <p>{format!("Couldn't list repositories: {e}")}</p> }.into_view(),
                    Some(Ok(entries)) => entries.into_iter().map(|d| {
                        let label = match d.kind {
                            RepositoryKind::Bare => format!("{} (bare)", d.name),
                            RepositoryKind::LinkedWorktree => format!("{} (worktree)", d.name),
                            RepositoryKind::MainWorktree => d.name.clone(),
                        };
                        let pick = {
                            let d = d.clone();
                            move |_| mode_for.set(Some(d.clone()))
                        };
                        view! {
                            // Big touch row per repo: name + kind, tap → mode screen.
                            <button class="picker-row" style="display:block; width:100%; \
                                    text-align:left; padding:12px; margin:4px 0; \
                                    font:inherit; color:var(--fg); background:#0d1117; \
                                    border:1px solid #30363d; border-radius:6px;"
                                on:click=pick>
                                {label}
                            </button>
                        }
                    }).collect_view(),
                }}
                <div style="display:flex; gap:8px; margin-top:16px;">
                    <button style="padding:8px 16px; font:inherit;" on:click=move |_| {
                        clone_url.set(String::new());
                        open_opened_at.set_value(js_sys::Date::now());
                        open_url.set(true);
                    }>"Clone URL…"</button>
                    <button style="padding:8px 16px; font:inherit;" on:click=move |_| {
                        spawn_local(async move {
                            match rescan_request().await {
                                Ok(msg) => { rescan_msg.set(msg); bump.update(|n| *n += 1); }
                                Err(e) => rescan_msg.set(e),
                            }
                        });
                    }>"Rescan"</button>
                    // The picker blocks the app, so it must always be dismissable:
                    // Cancel keeps whatever repo/mode is already current.
                    <button style="margin-left:auto; padding:8px 16px; font:inherit;"
                        on:click=move |_| open.set(false)>"Cancel"</button>
                </div>
                {move || (!rescan_msg.get().is_empty()).then(|| view! {
                    <div style="margin-top:8px; font-size:0.85em; opacity:0.7;">{rescan_msg.get()}</div>
                })}
            </div>
        </div>
    })
}

pub fn mode_view(
    mode_for: RwSignal<Option<RepositoryDescriptor>>,
    picker_open: RwSignal<bool>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let busy = create_rw_signal(false);
    let err = create_rw_signal(String::new());
    move || mode_for.get().map(|d| {
        let choose = move |mode: RepoMode| {
            let worktree = d.worktree.clone();
            move |_| {
                if busy.get_untracked() { return; }
                busy.set(true);
                err.set(String::new());
                let worktree = worktree.clone();
                spawn_local(async move {
                    match select_request(&worktree, mode).await {
                        Ok(()) => {
                            set_ui_mode(Some(mode));
                            mode_for.set(None);
                            picker_open.set(false);
                            reload.update(|n| *n = n.wrapping_add(1));
                        }
                        Err(e) => err.set(e),
                    }
                    busy.set(false);
                });
            }
        };
        view! {
            // Sits over the picker (z 901): two large touch buttons (ADR 0006).
            <div style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                        z-index:901; display:flex; align-items:center; \
                        justify-content:center; background:rgba(1,4,9,0.85);">
                <div style="min-width:300px; max-width:90vw; padding:24px; \
                            background:#161b22; border:1px solid #30363d; \
                            border-radius:10px; color:var(--fg); text-align:center;">
                    <div style="font-weight:600; font-size:1.2em; margin-bottom:16px;">
                        {format!("Open ‘{}’ as…", d.name)}
                    </div>
                    <button style="display:block; width:100%; padding:16px; margin:8px 0; \
                            font:inherit; font-size:1.05em; color:#fff; background:#1f6feb; \
                            border:1px solid #388bfd; border-radius:8px;"
                        disabled=move || busy.get()
                        on:click=choose(RepoMode::Visualize)>
                        "Visualize — look only, with links out"
                    </button>
                    <button style="display:block; width:100%; padding:16px; margin:8px 0; \
                            font:inherit; font-size:1.05em; color:#fff; background:#238636; \
                            border:1px solid #2ea043; border-radius:8px;"
                        disabled=move || busy.get()
                        on:click=choose(RepoMode::Active)>
                        "Active — full git operations"
                    </button>
                    {move || (!err.get().is_empty()).then(|| view! {
                        <div style="margin-top:8px; color:#f85149;">{err.get()}</div>
                    })}
                    <button style="margin-top:12px; padding:8px 16px; font:inherit;"
                        on:click=move |_| mode_for.set(None)>"Back"</button>
                </div>
            </div>
        }
    })
}
```

(Exact `spawn_local` import path: `leptos::spawn_local`, as the dialogs use.)

- [ ] **Step 2: Compile** — `cargo clippy -p git-vista-ui --target wasm32-unknown-unknown` → clean.
- [ ] **Step 3: Commit** — `feat(ui): repo picker + Visualize/Active mode screens (ADR 0006)`

---

### Task 9: Frontend — App wiring: picker on load, Repos button, mode badge

**Files:**
- Modify: `crates/git-vista/src/app/mod.rs`

**Interfaces:**
- Consumes: `picker::{picker_view, mode_view}` (Task 8), `set_ui_mode` (Task 7).

- [ ] **Step 1: Implement**

In `App()` after `print_graph_open`:

```rust
    // ADR 0006: ask every time — the picker opens on load (over the graph; the
    // sign-in/protocol overlays sit above it when they apply) and from the
    // topbar "Repos" button. `mode_for` holds the repo awaiting a mode choice.
    let picker_open = create_rw_signal(true);
    let mode_for = create_rw_signal(None::<git_vista_protocol::RepositoryDescriptor>);
```

Mirror the graph's mode into the api chokepoint (next to the session effect):

```rust
    // Defense in depth (ADR 0007): mirror the loaded graph's mode into api.rs so
    // write calls refuse client-side too. The server 403 remains the boundary.
    create_effect(move |_| {
        if let Some(Ok(g)) = graph.get() {
            crate::api::set_ui_mode(Some(if g.read_only {
                git_vista_protocol::RepoMode::Visualize
            } else {
                git_vista_protocol::RepoMode::Active
            }));
        }
    });
```

Topbar, before the "Open URL…" button:

```rust
                <button
                    class="refresh"
                    on:click=move |_| picker_open.set(true)
                    title="Open another repository — the launch repo, a repo from \
                           the configured root, or a clone"
                >
                    "Repos"
                </button>
                // The mode badge: which experience the current repo is open in;
                // tapping it re-opens the mode screen for this repo (ADR 0006).
                {move || graph.get().and_then(|r| r.ok()).map(|g| {
                    let (label, class) = if g.read_only {
                        ("Visualize", "refresh mode-badge visualize")
                    } else {
                        ("Active", "refresh mode-badge active")
                    };
                    let descriptor_ids = (g.worktree_id.clone(), g.repo_label.clone());
                    view! {
                        <button class=class
                            title="This repo's mode — tap to change it"
                            on:click=move |_| {
                                // Re-open the mode screen for the current repo by
                                // synthesizing its descriptor from the graph stamp.
                                if let (Some(worktree), label) = descriptor_ids.clone() {
                                    mode_for.set(Some(git_vista_protocol::RepositoryDescriptor {
                                        repository: g.repo_id.clone().unwrap_or_default(),
                                        worktree,
                                        name: label.unwrap_or_else(|| "repository".into()),
                                        kind: git_vista_protocol::RepositoryKind::MainWorktree,
                                        read_only: g.read_only,
                                        path: None,
                                        remote_web_url: g.remote_web_url.clone(),
                                    }));
                                }
                            }>
                            {label}
                        </button>
                    }
                })}
```

Mount the overlays next to the dialogs (after `reset_repo_view`):

```rust
            {crate::picker::picker_view(picker_open, mode_for, open_url, clone_url, open_opened_at, reload)}
            {crate::picker::mode_view(mode_for, picker_open, reload)}
```

Add `.mode-badge.visualize { color: #58a6ff; }` / `.mode-badge.active { color: #3fb950; }` to `styles.css` next to the `.refresh` rules.

- [ ] **Step 2: Compile + eyeball** — `./dev gate`; then `./gv` + browser: picker appears on load, Cancel keeps current repo, Repos reopens it.
- [ ] **Step 3: Commit** — `feat(ui): picker on load, Repos button, mode badge`

---

### Task 10: Frontend — gating audit (Activity Undo + friends)

**Files:**
- Modify: `crates/git-vista/src/activity.rs` (`activity_panel_view` signature + undo button)
- Modify: `crates/git-vista/src/app/canvas.rs` (call site — pass `read_only`)

**Interfaces:**
- Produces: `activity_panel_view(overlays: Overlays, settings: Settings, read_only: bool)`.

- [ ] **Step 1: Audit checklist** (verify each, fix what fails):
  - Context-menu write items: already gated (`menu.rs:570` `(!read_only)`). ✔ no change.
  - Activity panel Undo buttons (`activity.rs:330` `event.undo.map(...)`): NOT gated — wrap: `let undo_btn = (!read_only).then(|| event.undo.clone().map(|u| { … })).flatten();` (thread `read_only` down from `activity_panel_view` through the row-builder fn — follow the existing parameter style).
  - Commit dialog / confirm modals: only reachable from gated menu items and the (now gated) undo buttons. ✔.
  - Topbar: "Reset Test Repo" gated by `resettable` (server sets it `!read_only`). ✔. "Open URL…" stays available in both modes (clone is not a repo write). ✔.
  - api.rs chokepoint (Task 7) is the backstop for anything missed.

- [ ] **Step 2: Implement** the `activity.rs` + `canvas.rs` changes (canvas already holds `let read_only = graph.read_only;` at line 53 — pass it through at the `activity_panel_view` call site).
- [ ] **Step 3: Compile** — `./dev gate` → green.
- [ ] **Step 4: Commit** — `fix(ui): gate Activity Undo buttons in visualize mode`

---

### Task 11: Frontend — forge links (topbar repo link, detail commit link, menu branch link)

**Files:**
- Modify: `crates/git-vista/src/app/mod.rs` (status line repo link)
- Modify: `crates/git-vista/src/detail.rs` (commit link in the panel head — put it beside the existing head controls; exact spot: where the panel renders the commit hash/header)
- Modify: `crates/git-vista/src/menu.rs` (branch link item)
- Modify: `crates/git-vista/src/state.rs` (`MenuData` gains `remote_web_url: Option<String>`; populated where `MenuData` is built — `render/nodes.rs` / `render/stubs.rs`, from `graph.remote_web_url`)

**Interfaces:**
- Consumes: `git_vista_core::forge::{commit_url, branch_url, host_label}` (Task 2), `Graph.remote_web_url` (Task 6).

- [ ] **Step 1: Implement**

Topbar status line (`app/mod.rs`, inside the `repo.map(...)` status block): when `g.remote_web_url` is `Some(base)` wrap/append a link:

```rust
{g.remote_web_url.clone().map(|base| {
    let host = git_vista_core::forge::host_label(&base);
    view! {
        <a class="repo-link" href=base target="_blank" rel="noopener"
           style="margin-left:8px; font-size:0.85em;">
            {format!("view on {host} ↗")}
        </a>
    }
})}
```

Detail panel (`detail.rs`): in the head area, when the panel's commit id and the graph's `remote_web_url` are both known:

```rust
// "View commit on <host>" (ADR 0010) — a live anchor like "Open on GitHub"
// (iOS blocks scripted window.open). remote_web_url reaches the panel the same
// way read_only reaches the menu: threaded from graph_canvas.
{remote_web_url.clone().map(|base| {
    let url = git_vista_core::forge::commit_url(&base, &id);
    let host = git_vista_core::forge::host_label(&base);
    view! {
        <a class="detail-forge-link" href=url target="_blank" rel="noopener">
            {format!("View commit on {host} ↗")}
        </a>
    }
})}
```

(Thread `remote_web_url: Option<String>` into `detail_panel_view` the same way its existing params arrive — check its signature at implementation time and mirror it.)

Menu (`menu.rs`): in the per-branch items block (where the PR item is built, ~line 446), add a forge branch link **only when the repo is NOT GitHub** (`m.repo_url.is_none()`), so it never duplicates the existing GitHub items:

```rust
if m.repo_url.is_none() {
    if let Some(base) = m.remote_web_url.as_ref() {
        let url = git_vista_core::forge::branch_url(base, &b);
        let host = git_vista_core::forge::host_label(base);
        items.push(view! {
            <a class="ctx-item" href=url target="_blank" rel="noopener"
               on:click=move |_| menu.set(None)>
                <span class="nf ctx-icon">{ic.github}</span>
                {format!("View ‘{b}’ on {host}")}
            </a>
        }.into_view());
    }
}
```

`MenuData` in `state.rs` gains:

```rust
    /// Any-host forge web base (ADR 0010), for the non-GitHub branch link items.
    /// `None` => no usable remote (or the GitHub items already cover this repo).
    pub remote_web_url: Option<String>,
```

populated wherever `MenuData` is constructed (grep `MenuData {` — `render/nodes.rs`, `render/stubs.rs`) from the graph's field.

- [ ] **Step 2: Compile + eyeball** — `./dev gate`; browser: on this repo (GitHub origin) the topbar shows "view on github.com ↗" and nothing duplicated in the menu.
- [ ] **Step 3: Commit** — `feat(ui): forge links — topbar, detail panel, non-GitHub branch items (ADR 0010)`

---

### Task 12: `gv --root` flag

**Files:**
- Modify: `gv` (arg loop ~line 272, help text ~line 11, env block ~line 349)

- [ ] **Step 1: Implement**

Help text: add `#   gv --root <dir> [path]   also serve every repo directly under <dir>`.

Arg loop (pattern-match the existing `--seed`/`--token` handling; it's a `case` over args):

```bash
    --root)
      shift
      [ -n "${1:-}" ] || { echo "gv: --root needs a directory" >&2; exit 2; }
      [ -d "$1" ] || { echo "gv: --root: no such directory: $1" >&2; exit 2; }
      REPO_ROOT="$1"
      ;;
```

Env block (next to `export GIT_VISTA_BIND_ADDR=…`):

```bash
if [ -n "${REPO_ROOT:-}" ]; then
  export GIT_VISTA_REPO_ROOT="$REPO_ROOT"
fi
```

(Adapt to the script's actual arg-parsing structure — read the loop first; `gv` may use a `while`/`case` with its own `shift` discipline. `bash -n gv` + `shellcheck gv` must stay clean, matching the M1.05 verification posture.)

- [ ] **Step 2: Verify** — `bash -n gv && shellcheck gv`; then `./gv --root ~/projects` → startup log shows `repo root scan: N registered, M skipped`.
- [ ] **Step 3: Commit** — `feat(gv): --root <dir> exports GIT_VISTA_REPO_ROOT (ADR 0009)`

---

### Task 13: Gate + live verification + PR

- [ ] **Step 1:** `./dev gate` → all five checks green.
- [ ] **Step 2: Live verification** (working agreement: drive it, don't just trust tests):
  - `./gv --root ~/projects` on this checkout; `gv doctor` healthy.
  - Fresh browser session via `gv --token` link (Playwright: go via `about:blank` first — a fragment-only navigation does NOT reload the SPA).
  - Picker lists Git-Vista + `~/projects` repos; pick another repo → Visualize → graph renders; topbar badge says Visualize.
  - Visualize write refusal at the wire: `curl -s -X POST http://127.0.0.1:8080/api/branch -H 'content-type: application/json' -d '{"name":"x","commit":"HEAD"}'` (unauthenticated → 401 proves the gate; the mode 403 is proven by the Task 3 unit test and by tapping a write in the UI with the chokepoint temporarily bypassed — or simplest: check the Activity panel shows no Undo buttons and the menu shows no write items).
  - Select back to Git-Vista → Active → menu write items return; a scratch `git branch` via the UI works.
  - Forged-id 404: covered by Task 5's test.
  - iPad over the SSH tunnel: picker + mode screens usable by touch (Tom).
- [ ] **Step 3:** Update `README`/docs mentions if any describe single-repo behavior (grep `Open URL` in README).
- [ ] **Step 4:** Push, PR `Closes #<issue ①>` with verification evidence; CI green; merge (never delete the branch); update `handoff.md`.

---

## Self-review notes

- Spec coverage: select endpoint ✔ (T1/T3/T5), root scan + rescan ✔ (T4/T5/T12), picker + mode screens ✔ (T8/T9), gating audit ✔ (T7/T10), forge links ✔ (T2/T6/T11), serde(default) contract ✔ (T1/T6), 404 fail-closed ✔ (T3/T5), 403 ReadOnly ✔ (T3), degraded root scan ✔ (T4). Deferred to ② per spec: persistent clones, delete-clone, clone-response descriptor. Deferred to ③: LAN listener.
- Types cross-checked: `RepoMode`/`SelectRequest` (T1) used in T3/T5/T7/T8/T9; `remote_web_base` (T2) used in T3(catalog)/T6; `forge::{commit_url,branch_url,host_label}` (T2) used in T11; `select_registered` (T3) used in T5; `scan_repo_root` (T4) used in T5.
- Honest placeholders: T11 threads `remote_web_url` into `detail.rs` "mirror its existing params" and T12 says "adapt to the script's actual arg loop" — both are read-the-file-first instructions at the exact named location, not TBDs.
