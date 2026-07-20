# Persistent Multi-Clone Store (Issue #121, ADR 0008) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clones move from `$TMPDIR/git-vista-clones` (single, wiped at startup) to a persistent XDG store holding many clones, with startup re-registration and an explicit, guarded `POST /api/delete-clone`.

**Architecture:** All clone-lifecycle state stays where it lives today — `state.rs` (roots + globals), `catalog.rs` (registration), `handlers/clone.rs` (clone + the new delete handler). The catalog's `read_only` entry flag (true exactly for URL clones) becomes the wire marker the picker uses to offer Delete; no protocol field is renamed. The clone response upgrades from plain text to the clone's `RepositoryDescriptor` so the frontend jumps straight to the mode screen.

**Tech Stack:** Rust (axum server, Leptos CSR frontend), serde, tempfile in tests.

## Global Constraints

- Branch: `feature/persistent-clones`, off fresh `main` (issue #121 names the branch; do NOT use `./dev start`'s `mX.YY` naming).
- Commits: `git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit …`, or `./dev wip` for checkpoints.
- Clones root resolution order (ADR 0008): `GIT_VISTA_CLONES_ROOT` env override → `$XDG_DATA_HOME/git-vista/clones` → `~/.local/share/git-vista/clones`.
- Clone URL validation unchanged: `https://`/`http://`/`git://` only, `--` argv guard, `GIT_TERMINAL_PROMPT=0`.
- Delete guard (ADR 0008): refuse to remove anything that does not canonicalize inside the clones root.
- M1.02 wire contract: new DTO fields/endpoints are additive; `#[serde(deny_unknown_fields)]` on request bodies; never rename an existing wire field.
- ADR 0003 invariants: paths never on the wire; unknown/forged ids fail closed (404).
- Tests must not mutate process env, and must not touch the process-global `CURRENT`/`CATALOG` outside the single designated test fn `selection_flow_carries_mode_and_gates_writes` in `state.rs` (existing repo convention — parallel test threads).
- `./dev gate` green before the PR (fmt, clippy native + wasm, tests, trunk build). PR body: `Closes #121`. Never delete local or remote branches.

## Setup (before Task 1)

- [ ] `git checkout main && git pull`
- [ ] `git checkout -b feature/persistent-clones`
- [ ] Commit this plan file: `git add docs/superpowers/plans/2026-07-19-persistent-clones.md && git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "docs: implementation plan for persistent clones (#121)"`

---

### Task 1: XDG clones root + remove the startup wipe

The wipe and the root move ship in ONE commit so no intermediate commit ever wipes a persistent directory.

**Files:**
- Modify: `crates/git-vista-server/src/state.rs` (`clones_root()`, ~line 284-289, + tests)
- Modify: `crates/git-vista-server/src/main.rs` (delete wipe block, lines 122-135)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub(crate) fn clones_root() -> PathBuf` (same name/signature, new resolution); private `fn resolve_clones_root(override_root: Option<PathBuf>, xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf` for tests. Later tasks rely on `clones_root()` pointing at the persistent store.

- [ ] **Step 1: Write the failing tests** — in `state.rs`'s existing `mod tests`:

```rust
    // --- clones root resolution (ADR 0008) ---------------------------------

    #[test]
    fn clones_root_prefers_the_explicit_override() {
        assert_eq!(
            resolve_clones_root(
                Some(PathBuf::from("/custom/clones")),
                Some(PathBuf::from("/xdg")),
                Some(PathBuf::from("/home/u")),
            ),
            PathBuf::from("/custom/clones")
        );
    }

    #[test]
    fn clones_root_uses_xdg_data_home_when_set() {
        assert_eq!(
            resolve_clones_root(None, Some(PathBuf::from("/xdg")), Some(PathBuf::from("/home/u"))),
            PathBuf::from("/xdg/git-vista/clones")
        );
    }

    #[test]
    fn clones_root_falls_back_to_dot_local_share() {
        assert_eq!(
            resolve_clones_root(None, None, Some(PathBuf::from("/home/u"))),
            PathBuf::from("/home/u/.local/share/git-vista/clones")
        );
    }

    #[test]
    fn clones_root_treats_empty_values_as_unset() {
        assert_eq!(
            resolve_clones_root(
                Some(PathBuf::from("")),
                Some(PathBuf::from("")),
                Some(PathBuf::from("/home/u")),
            ),
            PathBuf::from("/home/u/.local/share/git-vista/clones")
        );
    }
```

Add `use std::path::PathBuf;` to the test module if not already in scope (it is via `use super::*;` — `PathBuf` is imported at file top).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p git-vista-server clones_root`
Expected: FAIL — `resolve_clones_root` not found.

- [ ] **Step 3: Implement** — in `state.rs`, replace the whole `clones_root()` fn (lines 284-289):

```rust
/// Parent directory that holds every persistent clone (ADR 0008):
/// `GIT_VISTA_CLONES_ROOT` override, else `$XDG_DATA_HOME/git-vista/clones`,
/// else `~/.local/share/git-vista/clones`. Clones live here across restarts;
/// deletion refuses anything that doesn't canonicalize inside this root — so a
/// bug can never `rm` a real repository.
pub(crate) fn clones_root() -> PathBuf {
    resolve_clones_root(
        std::env::var_os("GIT_VISTA_CLONES_ROOT").map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// The pure resolution behind [`clones_root`], parameterised so tests never
/// read or write process env — the same pattern as `parse_bind_addr`. Empty
/// values count as unset (a systemd unit with `Environment=X=` must not send
/// clones to `/git-vista/clones`).
fn resolve_clones_root(
    override_root: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    if let Some(root) = override_root.filter(|p| !p.as_os_str().is_empty()) {
        return root;
    }
    let base = xdg_data_home
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            home.filter(|p| !p.as_os_str().is_empty())
                .map(|h| h.join(".local/share"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("git-vista-data"));
    base.join("git-vista").join("clones")
}
```

- [ ] **Step 4: Delete the startup wipe** — in `main.rs`, remove lines 122-135 (the whole "Phase 13: clear any throwaway clones" block, from the comment through the closing `}` of `if clones.exists() {…}`). Remove `clones_root` from the `use state::{…}` import on lines 81-84 — it stays out of `main.rs` for good (Task 2's startup scan calls path-qualified `state::scan_clones_root()`).

- [ ] **Step 5: Run tests + clippy to verify green**

Run: `cargo test -p git-vista-server && cargo clippy -p git-vista-server -- -D warnings`
Expected: all PASS, no warnings (unused-import would fail here — confirms Step 4's import cleanup).

- [ ] **Step 6: Commit**

```bash
git add crates/git-vista-server/src/state.rs crates/git-vista-server/src/main.rs
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(server): clones root moves to XDG data dir; startup wipe removed (ADR 0008)"
```

---

### Task 2: Startup + rescan re-register surviving clones

**Files:**
- Modify: `crates/git-vista-server/src/catalog.rs` (`scan_direct_children`, ~line 194, + tests ~379-405)
- Modify: `crates/git-vista-server/src/state.rs` (`scan_repo_root` ~line 141; new `scan_clones_root`)
- Modify: `crates/git-vista-server/src/main.rs` (startup scan, after the repo-root scan at lines 117-120)
- Modify: `crates/git-vista-server/src/handlers/select.rs` (`rescan`, ~line 36)

**Interfaces:**
- Consumes: `clones_root()` from Task 1.
- Produces: `Catalog::scan_direct_children(&mut self, root: &Path, read_only: bool) -> (usize, usize)` (new `read_only` param); `pub(crate) fn scan_clones_root() -> (usize, usize)` in `state.rs`. Later tasks rely on: an entry with `read_only == true` ⟺ a URL clone (this is the picker's Delete marker).

- [ ] **Step 1: Write the failing test** — in `catalog.rs` `mod tests`, after `scan_of_a_missing_root_is_a_soft_zero_not_a_panic`:

```rust
    #[test]
    fn a_clone_survives_a_simulated_restart_scan() {
        // ADR 0008: a fresh process re-scans the clones root and re-registers
        // surviving clones, keeping the clone marker (`read_only`) the picker
        // uses to offer Delete.
        let clones = tempfile::tempdir().unwrap();
        init_repo(&clones.path().join("octocat"));

        // "Restart" = a brand-new catalog scanning the same directory.
        let mut catalog = Catalog::new();
        let (registered, skipped) = catalog.scan_direct_children(clones.path(), true);
        assert_eq!((registered, skipped), (1, 0));
        let d = catalog.descriptors(false);
        assert_eq!(d[0].name, "octocat");
        assert!(d[0].read_only, "re-registered clones keep the clone marker");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p git-vista-server a_clone_survives`
Expected: FAIL to compile — `scan_direct_children` takes 1 argument, 2 supplied.

- [ ] **Step 3: Implement** —

In `catalog.rs`, change the signature (line 194) and the register call (line 210):

```rust
    pub(crate) fn scan_direct_children(&mut self, root: &Path, read_only: bool) -> (usize, usize) {
```
```rust
            match self.register(&child, read_only) {
```
Extend the doc comment's first line: `…and register every valid git repository, allowing `root` first. `read_only` marks every registered child as a URL clone (the clones-root scan) or a normal repo (the configured repo root).`

Fix the two existing test callers: `catalog.scan_direct_children(root.path(), false)` in `scan_registers_direct_child_repos_and_skips_junk` and `catalog.scan_direct_children(Path::new("/no/such/dir"), false)` in `scan_of_a_missing_root_is_a_soft_zero_not_a_panic`.

In `state.rs`, update `scan_repo_root` (line 141-149) to pass `false`, and add below it:

```rust
/// Scan the clones root (ADR 0008) into the catalog, marking every entry as a
/// clone (`read_only: true` — the descriptor flag the picker keys Delete on).
/// Called at startup and by `POST /api/rescan`; a missing clones root is a soft
/// zero, not an error.
pub(crate) fn scan_clones_root() -> (usize, usize) {
    let root = clones_root();
    // Create it if this is a fresh install (no clone yet): scan_direct_children
    // logs a "not scanned" warning on a missing directory, worded for the
    // configured repo root, not the not-yet-created clones store — make sure
    // it exists rather than let that warning fire every startup/rescan.
    let _ = std::fs::create_dir_all(&root);
    catalog()
        .write()
        .expect("catalog lock")
        .scan_direct_children(&root, true)
}
```

In `main.rs`, directly after the repo-root scan block (lines 117-120):

```rust
    // ADR 0008: clones persist across runs. Re-register every clone surviving
    // under the clones root so the picker keeps offering it after a restart.
    let (clones_registered, _) = state::scan_clones_root();
    if clones_registered > 0 {
        println!("git-vista: {clones_registered} persistent clone(s) re-registered");
    }
```

In `handlers/select.rs`, replace `rescan` (and extend the module doc's rescan sentence to say "…the configured repo root and the clones root"):

```rust
/// Re-scan the configured repo root and the clones root (ADR 0009/0008).
/// Bodyless POST, like `rebase`. Registered entries and the current selection
/// are untouched; this only adds/refreshes entries.
pub(crate) async fn rescan() -> (StatusCode, String) {
    // Repo-root scan first, clones-root scan second — same order as startup,
    // so the clones-root scan wins any path both roots would register
    // (keeping the `read_only` clone marker accurate) on a rescan too.
    let repo_result = scan_repo_root();
    let (clones_registered, _) = scan_clones_root();
    let summary = match repo_result {
        Some((registered, skipped)) => format!(
            "Rescanned: {registered} repos registered, {skipped} skipped; \
             {clones_registered} clone(s) re-registered."
        ),
        None => format!(
            "No repo root configured; {clones_registered} clone(s) re-registered."
        ),
    };
    (StatusCode::OK, summary)
}
```
Update its import line: `use crate::state::{scan_clones_root, scan_repo_root, select_registered};`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p git-vista-server && cargo clippy -p git-vista-server -- -D warnings`
Expected: all PASS (including the new restart test).

- [ ] **Step 5: Commit**

```bash
git add crates/git-vista-server/src
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(server): startup and rescan re-register surviving clones (ADR 0008)"
```

---

### Task 3: Clone handler — no eviction, named dirs, descriptor response

**Files:**
- Modify: `crates/git-vista-server/src/handlers/clone.rs` (whole handler)
- Modify: `crates/git-vista-server/src/catalog.rs` (`descriptor_of` + refactor `descriptors`, ~line 225)
- Modify: `crates/git-vista-server/src/state.rs` (new `descriptor_for`; `set_current` now returns `Option<RepositoryHandle>`, ~line 237-268)
- Modify: `crates/git-vista-protocol/src/dto.rs` (`CloneRequest` doc, ~line 58-63)
- Modify: `crates/git-vista-protocol/src/version.rs` (bump `PROTOCOL_VERSION`/`MIN_CLIENT_PROTOCOL`/`MAX_CLIENT_PROTOCOL`, lines 25/31/37)
- Already written: `docs/adr/0013-clone-descriptor-protocol-bump.md` (ADR for the version bump + `set_current` change below — commit it with this task, not Task 6, since Task 6's ADR step is only the status flip)

**Interfaces:**
- Consumes: `set_current` (signature changes below), `clones_root`, `path_is_allowed`, `cleanup_clone` (all existing in `state.rs`).
- Produces: `Catalog::descriptor_of(&self, worktree: WorktreeId, expose_paths: bool) -> Option<RepositoryDescriptor>`; `pub(crate) fn descriptor_for(worktree: WorktreeId) -> Option<RepositoryDescriptor>` in `state.rs`; `set_current(path: &Path, mode: RepoMode) -> Option<RepositoryHandle>` (was `-> ()` — now returns the handle it just registered, `None` in degraded mode, so the clone handler builds its response from the clone it just made instead of re-reading the mutable `CURRENT` global); `clone_repo` now returns `Result<Json<RepositoryDescriptor>, (StatusCode, String)>` — success body IS the new clone's descriptor (Task 5's frontend contract). Private helpers `fn clone_dir_name(url: &str) -> Option<String>` and `fn unique_dest(root: &Path, name: &str) -> PathBuf`. `PROTOCOL_VERSION` bumps 1→2 (M1.02: the response body reshaping is a contract change, not an additive one).

- [ ] **Step 1: Write the failing tests** — new `mod tests` at the bottom of `handlers/clone.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_dir_name_takes_the_last_segment_and_strips_dot_git() {
        assert_eq!(
            clone_dir_name("https://github.com/octocat/Hello-World.git"),
            Some("Hello-World".to_string())
        );
        assert_eq!(
            clone_dir_name("https://github.com/octocat/Hello-World"),
            Some("Hello-World".to_string())
        );
        assert_eq!(
            clone_dir_name("https://gitlab.com/group/sub/repo.git/"),
            Some("repo".to_string())
        );
    }

    #[test]
    fn clone_dir_name_drops_query_fragment_and_unsafe_characters() {
        assert_eq!(
            clone_dir_name("https://host/repo.git?ref=main#frag"),
            Some("repo".to_string())
        );
        assert_eq!(clone_dir_name("https://host/we ird$name"), Some("weirdname".to_string()));
    }

    #[test]
    fn clone_dir_name_refuses_names_that_reduce_to_nothing_or_dots() {
        assert_eq!(clone_dir_name("https://host/"), None);
        assert_eq!(clone_dir_name("https://host/..."), None);
        assert_eq!(clone_dir_name("https://host/$$$"), None);
    }

    #[test]
    fn unique_dest_suffixes_until_free() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo"));
        std::fs::create_dir_all(root.path().join("repo")).unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo-2"));
        std::fs::create_dir_all(root.path().join("repo-2")).unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo-3"));
    }

}
```

(The delete-clone handler test arrives with Task 4; Task 3's test module holds only these four.)

Also add `descriptor_of` test in `catalog.rs` `mod tests`:

```rust
    #[test]
    fn descriptor_of_reports_one_entry_and_fails_closed_on_unknown_ids() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let handle = catalog.register(&repo, true).unwrap();

        let d = catalog.descriptor_of(handle.worktree, false).expect("known id");
        assert_eq!(d, catalog.descriptors(false)[0]);
        assert!(d.read_only);

        let stranger = WorktreeId::from_git_dir("/nowhere/.git/worktrees/ghost");
        assert!(catalog.descriptor_of(stranger, false).is_none());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p git-vista-server clone_dir_name; cargo test -p git-vista-server descriptor_of`
Expected: FAIL to compile — `clone_dir_name`, `unique_dest`, `descriptor_of` not found.

- [ ] **Step 3: Implement catalog side** — in `catalog.rs`, replace `descriptors` (lines 225-241) with the pair:

```rust
    /// The capability view of the catalog: one [`RepositoryDescriptor`] per entry,
    /// addressed by id. Absolute paths are included only when `expose_paths` is
    /// set (the operator's opt-in); otherwise the descriptors carry no path.
    /// Sorted by display name so the report is stable across calls.
    pub(crate) fn descriptors(&self, expose_paths: bool) -> Vec<RepositoryDescriptor> {
        let mut out: Vec<RepositoryDescriptor> = self
            .entries
            .values()
            .map(|e| Self::descriptor(e, expose_paths))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.worktree.cmp(&b.worktree)));
        out
    }

    /// The descriptor for one entry, or `None` when the catalog doesn't hold
    /// the id — the same capability view as [`descriptors`](Self::descriptors),
    /// for the single entry a fresh clone just registered (ADR 0008).
    pub(crate) fn descriptor_of(
        &self,
        worktree: WorktreeId,
        expose_paths: bool,
    ) -> Option<RepositoryDescriptor> {
        self.entries
            .get(&worktree)
            .map(|e| Self::descriptor(e, expose_paths))
    }

    /// One entry's wire form — shared by the list and single-entry views.
    fn descriptor(e: &RepoEntry, expose_paths: bool) -> RepositoryDescriptor {
        RepositoryDescriptor {
            repository: e.handle.repository.to_string(),
            worktree: e.handle.worktree.to_string(),
            name: e.name.clone(),
            kind: kind_to_protocol(e.kind),
            read_only: e.read_only,
            path: expose_paths.then(|| e.path.display().to_string()),
            remote_web_url: e.remote_web_url.clone(),
        }
    }
```

In `state.rs`, next to `catalog_descriptors`:

```rust
/// The capability descriptor for one registered worktree — the clone handler's
/// success body (ADR 0008) — or `None` for an id the catalog does not hold.
pub(crate) fn descriptor_for(worktree: WorktreeId) -> Option<RepositoryDescriptor> {
    catalog()
        .read()
        .expect("catalog lock")
        .descriptor_of(worktree, expose_paths())
}
```

Also in `state.rs`, change `set_current` to return the handle it just registered — the clone handler needs it directly rather than re-reading the mutable `CURRENT` global a second time (a concurrent `/api/select` landing in between the two reads could otherwise hand the clone response back someone else's repository). Replace the function (lines 237-268), keeping the doc comment and body logic as-is apart from the return type and the two `Some(handle)`/`None` additions:

```rust
pub(crate) fn set_current(path: &Path, mode: RepoMode) -> Option<RepositoryHandle> {
    let registered = {
        let mut c = catalog().write().expect("catalog lock");
        if let Ok(facts) = git_vista_git::read_repo_facts(path) {
            c.allow_root(&facts.root);
        }
        c.register(path, mode == RepoMode::Visualize)
    };
    match registered {
        Ok(handle) => {
            let path = match resolve_worktree(handle.worktree) {
                Some((canonical, _, _)) => canonical,
                None => path.to_path_buf(),
            };
            set_current_resolved(path, mode, Some(handle));
            Some(handle)
        }
        Err(e) => {
            eprintln!(
                "git-vista: serving {} in degraded mode ({e}); \
                 /api/* reads will surface git's own error",
                path.display()
            );
            set_current_resolved(path.to_path_buf(), mode, None);
            None
        }
    }
}
```

The two existing callers (`main.rs` startup, the `state.rs` test) ignore the return value — `Option` isn't `#[must_use]`, so this compiles with no `unused` warning.

- [ ] **Step 4: Implement the handler** — rewrite `handlers/clone.rs` body. Module doc becomes:

```rust
//! `POST /api/clone` and `POST /api/delete-clone` (Phase 12, reshaped by ADR
//! 0008): clone a public repo from a pasted URL into the persistent clones
//! store and hand its descriptor back so the browser can offer the mode
//! picker; delete a clone again on request, guarded to the clones root.
```

Imports become:

```rust
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{validate_clone_url, CloneRequest, RepositoryDescriptor};

use crate::state::{
    allow_repo_root, cleanup_clone, clones_root, descriptor_for, path_is_allowed, set_current,
};
```

Helpers above the handler:

```rust
/// A human-recognisable directory name for a clone of `url` — the URL's last
/// path segment, minus any `.git` suffix, restricted to safe filename
/// characters. `None` when nothing usable survives (the caller falls back to a
/// stamped name). The picker shows the directory base name (ADR 0008), so a
/// clone must not be called `clone-1721400000-0`.
fn clone_dir_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    // Strip the scheme+authority before taking the last segment — otherwise a
    // path-less URL like "https://host/" names the clone after the bare host.
    let path = path.split_once("://").map_or(path, |(_, rest)| rest);
    let path = path.trim_end_matches('/');
    let (_, tail) = path.split_once('/')?;
    let tail = tail.rsplit('/').next()?;
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let safe: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    // No hidden dirs, no "." / "..": dots can't lead or trail.
    let safe = safe.trim_matches('.').to_string();
    (!safe.is_empty()).then_some(safe)
}

/// First free directory under `root` for `name`: `name`, then `name-2`, `-3`, …
/// — clones persist (ADR 0008), so a second clone of the same repo needs its
/// own directory rather than evicting the first.
fn unique_dest(root: &Path, name: &str) -> PathBuf {
    let first = root.join(name);
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| root.join(format!("{name}-{n}")))
        .find(|p| !p.exists())
        .expect("some numeric suffix is free")
}
```

The handler keeps its structure but: signature `pub(crate) async fn clone_repo(Json(req): Json<CloneRequest>) -> Result<Json<RepositoryDescriptor>, (StatusCode, String)>`; every early `return (code, msg)` becomes `return Err((code, msg))`. The `dest` computation (replacing lines 46-54) becomes:

```rust
    // Recognisable, unique per-clone dir (ADR 0008): the repo's own name where
    // the URL yields one, a stamped name otherwise. Never collides — suffixed
    // (`-2`, `-3`, …) or stamped-and-countered.
    let dest = match clone_dir_name(&url) {
        Some(name) => unique_dest(&root, &name),
        None => {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            root.join(format!("clone-{stamp}-{n}"))
        }
    };
```

The tail (replacing lines 105-114 — the eviction block) becomes:

```rust
    // ADR 0008: clones persist — no eviction of any previous clone. Open the
    // fresh clone look-only (safe default); the browser follows up with the
    // Visualize/Active mode screen for it, using the descriptor we return.
    // Built from the handle `set_current` just gave back, not by re-reading
    // CURRENT — a concurrent /api/select landing in between must never hand
    // this response someone else's repository.
    let descriptor = set_current(&dest, git_vista_protocol::RepoMode::Visualize)
        .and_then(|h| descriptor_for(h.worktree));
    match descriptor {
        Some(d) => {
            println!("[/api/clone] now viewing {}", dest.display());
            Ok(Json(d))
        }
        // set_current fell to degraded mode (the clone didn't classify as a
        // repo) — surface it rather than handing back a phantom descriptor.
        None => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Clone finished but the repository could not be registered.".to_string(),
        )),
    }
```

Update the handler's doc comment: drop "the previous clone, if any, is deleted so at most one is kept", say "clones persist under the clones root (ADR 0008) until deleted via `/api/delete-clone`". Remove the now-unused `current` import (done in the import block above).

In `dto.rs`, update `CloneRequest`'s doc (lines 58-63): replace "into a throwaway temp directory and switch the server to viewing it, read-only" with "into the persistent clones store (ADR 0008) and open it look-only pending the operator's mode choice".

- [ ] **Step 5: Bump the wire-protocol version** — `/api/clone`'s success response changes shape (plain text → JSON `RepositoryDescriptor`), which an older cached client would misread; M1.02 requires that be signalled, not shipped silently. In `crates/git-vista-protocol/src/version.rs`, bump all three constants together (lines 25, 31, 37):

```rust
pub const PROTOCOL_VERSION: u32 = 2;
```
```rust
pub const MIN_CLIENT_PROTOCOL: u32 = 2;
```
```rust
pub const MAX_CLIENT_PROTOCOL: u32 = 2;
```

Nothing else references the literal version number — the frontend and the middleware both read these constants, and the one test that passes a literal `"1"` (`header_parses_a_plain_integer_and_rejects_junk` in `version.rs`) is testing the header parser, not the constant, so it's untouched. A stale cached client (still sending protocol `1`) now gets the existing "Update Required" screen instead of a JSON-parse failure against the new response body.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p git-vista-server && cargo test -p git-vista-protocol && cargo clippy -p git-vista-server -p git-vista-protocol -- -D warnings`
Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/git-vista-server/src crates/git-vista-protocol/src/dto.rs crates/git-vista-protocol/src/version.rs docs/adr/0013-clone-descriptor-protocol-bump.md docs/adr/README.md
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(server): persistent named clones; clone response carries the descriptor (ADR 0008)"
```

---

### Task 4: `POST /api/delete-clone`

**Files:**
- Modify: `crates/git-vista-protocol/src/dto.rs` (new `DeleteCloneRequest` after `SelectRequest`, ~line 88, + test)
- Modify: `crates/git-vista-protocol/src/lib.rs` (export, ONLY if dto items are re-exported by explicit list — check first; a `pub use dto::*;` needs nothing)
- Modify: `crates/git-vista-server/src/catalog.rs` (`Catalog::remove` + test)
- Modify: `crates/git-vista-server/src/state.rs` (`DeleteCloneOutcome`, `delete_clone`; extend the single global-driving test)
- Modify: `crates/git-vista-server/src/handlers/clone.rs` (handler + test)
- Modify: `crates/git-vista-server/src/main.rs` (route + import)

**Interfaces:**
- Consumes: `resolve_worktree`, `current`, `catalog()`, `Catalog::remove`, `clones_root` from earlier tasks.
- Produces: `pub struct DeleteCloneRequest { pub worktree: String }`; `pub(crate) enum DeleteCloneOutcome { NotFound, NotAClone, CurrentlyOpen, Deleted, DeleteFailed(String) }`; `pub(crate) fn delete_clone(worktree: WorktreeId, clones_root: &Path) -> DeleteCloneOutcome`; handler `pub(crate) async fn delete_clone_repo(Json(req): Json<DeleteCloneRequest>) -> (StatusCode, String)`; route `POST /api/delete-clone`. Task 5 relies on: 200 body `"Clone deleted."`, refusals 400/404/409 with text bodies.

- [ ] **Step 1: Write the failing DTO test** — in `dto.rs`'s test module, mirror the `SelectRequest` test (~line 294):

```rust
    #[test]
    fn delete_clone_request_round_trips_and_rejects_unknown_fields() {
        let req = DeleteCloneRequest {
            worktree: "11111111-2222-5333-8444-555555555555".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<DeleteCloneRequest>(&json).unwrap(), req);

        assert!(serde_json::from_str::<DeleteCloneRequest>(
            r#"{"worktree":"x","path":"/etc"}"#
        )
        .is_err());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p git-vista-protocol delete_clone_request`
Expected: FAIL to compile — `DeleteCloneRequest` not found.

- [ ] **Step 3: Implement the DTO** — in `dto.rs` after `SelectRequest` (line 88):

```rust
/// Body of `POST /api/delete-clone` (ADR 0008): delete the *clone* addressed by
/// the opaque `worktree` id — its catalog entry and its directory. The server
/// refuses any id whose path does not canonicalize inside the clones root, so
/// this can only ever remove server-made clones, never a user repository; it
/// also refuses the currently open repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteCloneRequest {
    pub worktree: String,
}
```

Check `crates/git-vista-protocol/src/lib.rs`: if it re-exports dto items by name, add `DeleteCloneRequest`; if `pub use dto::*;`, nothing to do.

Run: `cargo test -p git-vista-protocol` — expected PASS.

- [ ] **Step 4: Write the failing catalog test** — in `catalog.rs` tests:

```rust
    #[test]
    fn remove_drops_the_entry_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let handle = catalog.register(&repo, true).unwrap();

        assert!(catalog.remove(handle.worktree).is_some());
        assert!(catalog.resolve(handle.worktree).is_none(), "gone after remove");
        assert!(catalog.remove(handle.worktree).is_none(), "second remove is a no-op");
    }
```

Run: `cargo test -p git-vista-server remove_drops` — expected: FAIL to compile.

- [ ] **Step 5: Implement `Catalog::remove`** — in `catalog.rs`, after `resolve`:

```rust
    /// Drop the entry for `worktree`, returning it (`None` when not held). The
    /// allowed root it lived under stays — other clones share it.
    pub(crate) fn remove(&mut self, worktree: WorktreeId) -> Option<RepoEntry> {
        self.entries.remove(&worktree)
    }
```

Run: `cargo test -p git-vista-server remove_drops` — expected PASS.

- [ ] **Step 6: Extend the global-state test (failing first)** — in `state.rs`, append to the END of `selection_flow_carries_mode_and_gates_writes` (this is the ONE test allowed to drive `CURRENT`/`CATALOG`; `wt` is the project repo's worktree id already in scope, selection is currently Visualize on the project repo):

```rust
        // --- delete-clone (ADR 0008) ------------------------------------
        // A fake clones root holding one "clone"; the project repo above is
        // the guard's negative case (a real repo, not a clone).
        let clones = root.path().join("clones");
        let clone_dir = clones.join("octocat");
        std::fs::create_dir_all(&clone_dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&clone_dir)
            .status()
            .unwrap()
            .success());
        set_current(&clone_dir, RepoMode::Visualize); // registers, like /api/clone
        let clone_wt = current_handle().expect("clone registered").worktree;

        // The currently open clone is not deletable (the server would be
        // serving a removed directory).
        assert_eq!(
            delete_clone(clone_wt, &clones),
            DeleteCloneOutcome::CurrentlyOpen
        );
        // Move the selection off the clone; the project repo is outside the
        // clones root, so IT is refused as NotAClone…
        assert!(select_registered(wt, RepoMode::Active));
        assert_eq!(delete_clone(wt, &clones), DeleteCloneOutcome::NotAClone);
        // …and the clone itself now deletes: directory gone, id fails closed.
        assert_eq!(delete_clone(clone_wt, &clones), DeleteCloneOutcome::Deleted);
        assert!(!clone_dir.exists(), "the clone directory was removed");
        assert_eq!(delete_clone(clone_wt, &clones), DeleteCloneOutcome::NotFound);
```

Run: `cargo test -p git-vista-server selection_flow` — expected: FAIL to compile (`delete_clone`, `DeleteCloneOutcome` not found).

- [ ] **Step 7: Implement outcome + logic** — in `state.rs`, after `cleanup_clone`:

```rust
/// Outcome of a delete-clone attempt (ADR 0008); the handler maps each to an
/// HTTP status. Every refusal names why, so the picker can show the reason.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeleteCloneOutcome {
    /// Unknown/forged id — fail closed, the same contract as the reads (404).
    NotFound,
    /// The id resolves, but its path does not canonicalize inside the clones
    /// root: not a clone, never deletable through this endpoint (400).
    NotAClone,
    /// The clone is the current selection — deleting the repo being served
    /// would break every read. Open another repo first (409).
    CurrentlyOpen,
    /// Removed from disk and catalog (200).
    Deleted,
    /// Guards passed but `remove_dir_all` failed (500); carries the OS error.
    DeleteFailed(String),
}

/// Delete the clone addressed by `worktree` (ADR 0008): resolve fail-closed,
/// refuse anything that does not canonicalize inside `clones_root` (the delete
/// guard), refuse the current selection, then remove the directory and the
/// catalog entry — in that order, so a failed removal stays visible and
/// retryable. `clones_root` is a parameter so tests never touch process env.
pub(crate) fn delete_clone(worktree: WorktreeId, clones_root: &Path) -> DeleteCloneOutcome {
    let Some((path, _, _)) = resolve_worktree(worktree) else {
        return DeleteCloneOutcome::NotFound;
    };
    // A root that can't canonicalize (missing dir) can't contain anything:
    // fail closed. Re-canonicalize the entry's path fresh too, rather than
    // trusting the catalog's registration-time value — if the directory was
    // swapped out from under us since registration, the guard must see that.
    let root = match std::fs::canonicalize(clones_root) {
        Ok(root) => root,
        Err(_) => return DeleteCloneOutcome::NotAClone,
    };
    let path = match std::fs::canonicalize(&path) {
        Ok(path) => path,
        Err(_) => return DeleteCloneOutcome::NotFound,
    };
    if path == root || !path.starts_with(&root) {
        return DeleteCloneOutcome::NotAClone;
    }
    if current().0 == path {
        return DeleteCloneOutcome::CurrentlyOpen;
    }
    if let Err(e) = std::fs::remove_dir_all(&path) {
        return DeleteCloneOutcome::DeleteFailed(e.to_string());
    }
    catalog().write().expect("catalog lock").remove(worktree);
    DeleteCloneOutcome::Deleted
}
```

Run: `cargo test -p git-vista-server selection_flow` — expected PASS.

- [ ] **Step 8: Handler + route (failing test first)** — in `handlers/clone.rs` tests (these only *read* the globals with an id that is never registered — safe alongside the state.rs test, same pattern as `select.rs`):

```rust
    #[tokio::test]
    async fn delete_clone_refuses_a_malformed_and_an_unknown_id() {
        let (status, _) = delete_clone_repo(axum::Json(DeleteCloneRequest {
            worktree: "not-an-id".into(),
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, msg) = delete_clone_repo(axum::Json(DeleteCloneRequest {
            // Valid id shape, never registered → fail-closed 404.
            worktree: "99999999-9999-5999-8999-999999999999".into(),
        }))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(msg, "No such repository.");
    }
```

Run: `cargo test -p git-vista-server delete_clone_refuses` — FAIL to compile. Then implement in `handlers/clone.rs`:

```rust
/// `POST /api/delete-clone` (ADR 0008): remove a clone — catalog entry and
/// directory — addressed by opaque id. Malformed id → 400; unknown id → 404
/// (fail closed, like the reads); a repo that isn't a clone → 400; the
/// currently open repo → 409. The guard is [`crate::state::delete_clone`]:
/// nothing outside the canonical clones root is ever removed.
pub(crate) async fn delete_clone_repo(
    Json(req): Json<DeleteCloneRequest>,
) -> (StatusCode, String) {
    let worktree: WorktreeId = match req.worktree.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()),
    };
    match delete_clone(worktree, &clones_root()) {
        DeleteCloneOutcome::NotFound => {
            (StatusCode::NOT_FOUND, "No such repository.".to_string())
        }
        DeleteCloneOutcome::NotAClone => (
            StatusCode::BAD_REQUEST,
            "Not a clone — only cloned repositories can be deleted.".to_string(),
        ),
        DeleteCloneOutcome::CurrentlyOpen => (
            StatusCode::CONFLICT,
            "This repository is open right now. Open another repository first.".to_string(),
        ),
        DeleteCloneOutcome::DeleteFailed(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't delete the clone: {e}"),
        ),
        DeleteCloneOutcome::Deleted => (StatusCode::OK, "Clone deleted.".to_string()),
    }
}
```

Imports to extend in `handlers/clone.rs`: add `DeleteCloneRequest` to the `git_vista_protocol` use; add `use git_vista_core::identity::WorktreeId;`; add `delete_clone, DeleteCloneOutcome` to the `crate::state` use.

In `main.rs`: import becomes `use handlers::clone::{clone_repo, delete_clone_repo};` and after the `/api/clone` route (line 239):

```rust
        // ADR 0008: delete a persistent clone (catalog entry + directory),
        // guarded to paths that canonicalize inside the clones root.
        .route("/api/delete-clone", post(delete_clone_repo))
```

- [ ] **Step 9: Run the full server suite**

Run: `cargo test -p git-vista-server && cargo test -p git-vista-protocol && cargo clippy -p git-vista-server -p git-vista-protocol -- -D warnings`
Expected: all PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/git-vista-server/src crates/git-vista-protocol/src
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(server): POST /api/delete-clone with clones-root canonicalize guard (ADR 0008)"
```

---

### Task 5: Frontend — clone lands on the mode screen; picker deletes clones

No wasm test harness exists; the gate here is clippy-wasm + trunk build, plus Task 6's live drive.

**Files:**
- Modify: `crates/git-vista/src/api.rs` (`clone_request` ~line 205-226; new `delete_clone_request`; extend the `git_vista_protocol` import with `DeleteCloneRequest`, `RepositoryDescriptor` — check what's already imported at the top first)
- Modify: `crates/git-vista/src/dialogs/open_url.rs` (new `mode_for` param; success arm; hint text)
- Modify: `crates/git-vista/src/picker.rs` (Delete button on clone rows; `(clone)` label; `rescan_msg` doubles as the delete-feedback line)
- Modify: `crates/git-vista/src/app/mod.rs` (line 355 call site gains `mode_for`)

**Interfaces:**
- Consumes: server contract from Tasks 3-4 (`/api/clone` → `RepositoryDescriptor` JSON; `/api/delete-clone` → text; descriptor `read_only == true` ⟺ clone).
- Produces: `pub async fn clone_request(url: &str) -> Result<RepositoryDescriptor, String>`; `pub async fn delete_clone_request(worktree: &str) -> Result<String, String>`; `open_url_view(open_url, clone_url, cloning, open_opened_at, reload, mode_for: RwSignal<Option<RepositoryDescriptor>>)`.

- [ ] **Step 1: api.rs** — replace `clone_request`:

```rust
/// Ask the backend to clone a public URL into the persistent clones store
/// (`POST /api/clone`, ADR 0008). `Ok` carries the fresh clone's descriptor so
/// the caller can jump straight to the Visualize/Active mode screen for it. On
/// a non-2xx response the body is the server's / git's own error text (bad
/// URL, repo not found, …), returned as `Err`.
pub async fn clone_request(url: &str) -> Result<RepositoryDescriptor, String> {
    let body = CloneRequest {
        url: url.to_string(),
    };
    let resp = req_post("/api/clone")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        resp.json::<RepositoryDescriptor>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}
```

Add after `rescan_request`:

```rust
/// Delete a persistent clone by id (`POST /api/delete-clone`, ADR 0008). `Ok`
/// carries the server's confirmation line for the picker; refusals (not a
/// clone, currently open, unknown id) come back as `Err` with the reason.
pub async fn delete_clone_request(worktree: &str) -> Result<String, String> {
    let body = DeleteCloneRequest {
        worktree: worktree.to_string(),
    };
    let resp = req_post("/api/delete-clone")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(response_error(resp).await)
    }
}
```

- [ ] **Step 2: open_url.rs** — signature gains the mode-screen signal (append last):

```rust
pub fn open_url_view(
    open_url: RwSignal<bool>,
    clone_url: RwSignal<String>,
    cloning: RwSignal<bool>,
    open_opened_at: StoredValue<f64>,
    reload: RwSignal<u32>,
    mode_for: RwSignal<Option<RepositoryDescriptor>>,
) -> impl IntoView {
```

Add `use git_vista_protocol::RepositoryDescriptor;` at the top. Success arm becomes:

```rust
                Ok(descriptor) => {
                    cloning.set(false);
                    open_url.set(false);
                    clone_url.set(String::new());
                    // The server opened the clone look-only; the reload shows
                    // it, and the mode screen asks Visualize/Active (ADR 0008).
                    reload.update(|n| *n = n.wrapping_add(1));
                    mode_for.set(Some(descriptor));
                }
```

Hint line (was "Public https:// URLs only. Cloned repos are read-only."):

```rust
                    "Public https:// URLs only. Clones persist until you delete them from the picker."
```

In `app/mod.rs` line 355: `{dialogs::open_url_view(open_url, clone_url, cloning, open_opened_at, reload, mode_for)}` — `mode_for` is already in scope (line 162) and is declared BEFORE line 355, so no reordering needed.

- [ ] **Step 3: picker.rs** — extend imports: `use crate::api::{delete_clone_request, fetch_catalog, rescan_request, select_request, set_ui_mode};`. Replace the per-entry `.map(|d| { … })` body (lines 66-87) with:

```rust
                                .map(|d| {
                                    let is_clone = d.read_only;
                                    let label = match d.kind {
                                        RepositoryKind::Bare => format!("{} (bare)", d.name),
                                        RepositoryKind::LinkedWorktree => {
                                            format!("{} (worktree)", d.name)
                                        }
                                        RepositoryKind::MainWorktree if is_clone => {
                                            format!("{} (clone)", d.name)
                                        }
                                        RepositoryKind::MainWorktree => d.name.clone(),
                                    };
                                    let worktree = d.worktree.clone();
                                    let name = d.name.clone();
                                    let pick = move |_| mode_for.set(Some(d.clone()));
                                    // Delete a persistent clone (ADR 0008): native
                                    // confirm, then the guarded endpoint; feedback
                                    // reuses the status line under the buttons.
                                    let del = move |_| {
                                        let confirmed = web_sys::window()
                                            .map(|w| {
                                                w.confirm_with_message(&format!(
                                                    "Delete the clone ‘{name}’ from disk?"
                                                ))
                                                .unwrap_or(false)
                                            })
                                            .unwrap_or(false);
                                        if !confirmed {
                                            return;
                                        }
                                        let worktree = worktree.clone();
                                        spawn_local(async move {
                                            match delete_clone_request(&worktree).await {
                                                Ok(msg) => {
                                                    rescan_msg.set(msg);
                                                    bump.update(|n| *n = n.wrapping_add(1));
                                                }
                                                Err(e) => rescan_msg.set(e),
                                            }
                                        });
                                    };
                                    view! {
                                        // A big touch row per repo: tap → mode
                                        // screen; clones carry a Delete beside.
                                        <div style="display:flex; gap:4px; margin:4px 0;">
                                            <button
                                                style="flex:1; text-align:left; \
                                                       padding:12px; font:inherit; \
                                                       color:var(--fg); background:#0d1117; \
                                                       border:1px solid #30363d; \
                                                       border-radius:6px;"
                                                on:click=pick
                                            >
                                                {label}
                                            </button>
                                            {is_clone.then(|| view! {
                                                <button
                                                    style="padding:12px; font:inherit; \
                                                           color:#f85149; background:#0d1117; \
                                                           border:1px solid #30363d; \
                                                           border-radius:6px;"
                                                    on:click=del
                                                >
                                                    "Delete"
                                                </button>
                                            })}
                                        </div>
                                    }
                                })
```

Closure-capture order matters: `is_clone`, `label`, `worktree`, `name` are all taken BEFORE `d` moves into `pick`. `name` moves into `del`; `worktree` is cloned inside `del` before the `spawn_local` so `del` stays `Fn` (callable repeatedly).

Update the picker module doc's list line to mention delete: "…persistent clones (deletable in place, ADR 0008)".

- [ ] **Step 4: Compile-check both targets**

Run: `cargo clippy -p git-vista --target wasm32-unknown-unknown -- -D warnings && cargo clippy -p git-vista-server -- -D warnings`
Expected: clean. (If `confirm_with_message` is missing: `web-sys`'s `Window` feature is already on — `alert_with_message` in this same crate proves it — so a failure here means a typo, not a feature gap.)

- [ ] **Step 5: Commit**

```bash
git add crates/git-vista/src
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "feat(ui): clone flow lands on the mode screen; picker deletes persistent clones (ADR 0008)"
```

---

### Task 6: Docs, gate, live verification, PR

**Files:**
- Modify: `docs/adr/0008-persistent-clones-xdg.md` (status line)
- Modify: `docs/SECURITY_MODEL.md` (annotate the clone-lifecycle items, ~lines 217-222)
- Modify: `README.md` (stale "temporary/read-only clone" wording, if any — grep first)
- Modify: `handoff.md` (progress + next step)

- [ ] **Step 1: ADR status** — in `docs/adr/0008-persistent-clones-xdg.md` line 3:

```markdown
- **Status:** Accepted — implemented 2026-07-19 (`feature/persistent-clones`, #121)
```

Same flip in `docs/adr/0013-clone-descriptor-protocol-bump.md` line 3 (implemented alongside it in Task 3):

```markdown
- **Status:** Accepted — implemented 2026-07-19 (`feature/persistent-clones`, #121)
```

Update both status cells in `docs/adr/README.md`'s index table (rows for 0008 and 0013) from "Accepted — implementation pending" to "Accepted" to match.

- [ ] **Step 2: SECURITY_MODEL annotation** — read the section around lines 217-222 first and FOLLOW ITS EXISTING ANNOTATION STYLE (grep for `Implemented` / `ADR` markers elsewhere in the file). The substance to record against those bullets: clones now live under a managed root (`$XDG_DATA_HOME/git-vista/clones`, ADR 0008); deletion is explicit (`POST /api/delete-clone`) with a canonicalize-inside-clones-root guard; the currently open clone is never deleted (409). If the file annotates inline, add `*(Implemented: ADR 0008, #121 — persistent XDG clones root, guarded /api/delete-clone, currently-open clone refused.)*` after the relevant bullets; if it uses a different convention, match it.

- [ ] **Step 3: README + handoff** — `grep -n "clone" README.md`; update any "temporary", "single clone", or "wiped at startup" wording to the ADR 0008 behavior. Update `handoff.md`: #121 done pending merge, next `#122 lan-view-mode`.

- [ ] **Step 4: Full gate**

Run: `./dev gate`
Expected: fmt ✓, clippy native ✓, clippy wasm ✓, tests ✓, trunk build ✓. Fix anything red before proceeding.

- [ ] **Step 5: Live verification** (working agreement — drive it, don't just trust tests):
  1. Start the server (`./dev serve` or `./gv`), sign in via the printed link.
  2. Clone `https://github.com/octocat/Hello-World.git` from the picker's "Clone URL…". Expect: mode screen opens for "Hello-World"; graph loads after choosing a mode.
  3. `ls ~/.local/share/git-vista/clones` — expect `Hello-World`.
  4. Restart the server. Reopen the picker. Expect: "Hello-World (clone)" still listed (startup re-scan).
  5. Clone the same URL again. Expect a second entry (`Hello-World-2`) — multi-clone retention.
  6. With one of the clones NOT current: Delete it from the picker; confirm the row disappears and the directory is gone from disk.
  7. With a clone current: Delete it — expect the 409 refusal text in the picker's status line.
  8. Forged-id check from the browser devtools console (authenticated session, CSRF handled by the app's own fetch path is not reusable here — instead verify via the picker being the only caller and step 6/7 above, OR run the wire assertion as a handler test, which Task 4 already does).
  9. `./dev wip` after verification.

- [ ] **Step 6: Commit docs, push, PR, merge**

```bash
git add docs README.md
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com \
  commit -m "docs: ADR 0008 implemented; SECURITY_MODEL clone-lifecycle annotations (#121)"
git push -u origin feature/persistent-clones
gh pr create --title "Persistent multi-clone store under the XDG data dir (ADR 0008)" \
  --body "Closes #121 …" # summarize: XDG root, no wipe, startup re-scan, named multi-clones, descriptor response, guarded delete-clone
```

Merge after checks pass. **Never delete the branch.** Write the task summary to `~/projects/_claude-outputs/2026-07-19_persistent-clones_summary.md`.

---

## Design decisions locked in this plan (differ from or refine the ADR's letter)

1. **Clone marker = existing `read_only` descriptor field.** True exactly for URL clones (clone handler registers Visualize→true; clones-root scan passes true; repo-root scan and launch repo pass false). No wire rename — M1.02-safe. The UI marker is cosmetic; the server's delete guard independently canonicalizes.
2. **`/api/clone` still `set_current`s the new clone in Visualize** (safe look-only default, graph shows the clone even if the user backs out of the mode screen), AND returns the descriptor so the frontend opens the mode screen — the ADR's "straight to the mode picker" flow.
3. **Deleting the currently open clone → 409**, satisfying SECURITY_MODEL's "never delete a clone while an active repository handle references it".
4. **Clone dirs are named after the repo** (`Hello-World`, `Hello-World-2`, …) because the picker displays directory base names; stamped names remain the fallback for unusable URLs. Two *concurrent* clones of the same URL can race `unique_dest` and collide — git then fails the second with its own "destination path already exists" error, which the B3 posture surfaces verbatim; accepted.
5. **`delete_clone` takes `clones_root` as a parameter** so the state test never mutates process env; the handler passes the real root.
6. **`set_current` returns `Option<RepositoryHandle>`** (Task 3) so `/api/clone`'s response is built from the clone it just made, not a second read of the mutable `CURRENT` global — closes a race where a concurrent `/api/select` could hand the clone response back the wrong repo's descriptor.
7. **`PROTOCOL_VERSION` bumps 1→2** (Task 3) because `/api/clone`'s success body reshapes from plain text to JSON — a contract change, not an additive one, per M1.02. A stale cached client now gets the existing "Update Required" screen instead of a JSON-parse failure.
