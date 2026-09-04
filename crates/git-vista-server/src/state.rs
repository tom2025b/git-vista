//! Server state, configuration constants, and the read-only write guard.
//!
//! Split out of `main.rs`: the process-wide "which repo are we serving, and is it
//! writable?" state ([`Current`]/[`CURRENT`]), the small config constants, and the
//! [`reject_if_read_only`] guard the write handlers share. Everything here is
//! crate-internal (this is a binary — there is no public API surface); the items
//! the handlers and `main` reach for are `pub(crate)`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use axum::http::StatusCode;

use git_vista_core::identity::{RepositoryHandle, WorktreeId};
use git_vista_protocol::{RepoMode, RepositoryDescriptor};

use crate::catalog::{Catalog, CatalogError, RepoEntry};

// Which repository to visualise *initially*. Taken from the first CLI argument
// (`git-vista-server <path>`), falling back to the current working directory (`.`,
// canonicalised at startup) when none is given. The `gv` launcher always passes
// the directory you run it from, so this default only bites when the server is run
// directly with no argument. This is only the starting repo — Phase 12 lets the
// user switch to a cloned URL at runtime.
pub(crate) const DEFAULT_REPO: &str = ".";

// The wasm bundle Trunk emits next to the frontend crate. Resolved at compile
// time relative to this crate so the server runs from any working directory.
pub(crate) const DIST_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../git-vista/dist");
pub(crate) const PORT: u16 = 8080;
pub(crate) const LOOPBACK_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT);

/// Git-Vista is intentionally loopback-only. An explicit environment value is
/// accepted for service files only when it repeats the exact safe address; this
/// prevents a stale launcher or service override from exposing the server.
pub(crate) fn bind_addr() -> Result<SocketAddr, String> {
    match std::env::var("GIT_VISTA_BIND_ADDR") {
        Ok(value) => parse_bind_addr(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_bind_addr(None),
        Err(error) => Err(format!("could not read GIT_VISTA_BIND_ADDR: {error}")),
    }
}

fn parse_bind_addr(value: Option<&str>) -> Result<SocketAddr, String> {
    match value {
        Some(value) => {
            let addr: SocketAddr = value
                .parse()
                .map_err(|error| format!("invalid GIT_VISTA_BIND_ADDR '{value}': {error}"))?;
            if addr != LOOPBACK_ADDR {
                return Err(format!(
                    "refusing GIT_VISTA_BIND_ADDR '{value}': Git-Vista only listens on {LOOPBACK_ADDR}; use an SSH local-port forward for remote access"
                ));
            }
            Ok(addr)
        }
        None => Ok(LOOPBACK_ADDR),
    }
}

/// The optional second, LAN-facing listener (ADR 0005, `gv --lan-view`). `None`
/// means the feature isn't requested — the server then behaves exactly as
/// before this feature landed. `gv` is responsible for auto-detecting the LAN
/// IP or requiring `--lan-ip` before it ever sets this variable, so a parse
/// failure here means the launcher passed something bad — still handled as a
/// clean startup error, never a panic.
pub(crate) fn lan_bind_addr() -> Option<Result<SocketAddr, String>> {
    parse_lan_ip_env(std::env::var("GIT_VISTA_LAN_IP").ok().as_deref())
}

/// The pure resolution behind [`lan_bind_addr`], parameterised so tests never
/// read or write process env — the same pattern as `parse_bind_addr`. An empty
/// value counts as unset, matching `resolve_clones_root`'s convention (a
/// systemd unit with `Environment=X=` must not silently enable the feature).
fn parse_lan_ip_env(value: Option<&str>) -> Option<Result<SocketAddr, String>> {
    let value = value.filter(|v| !v.trim().is_empty())?;
    let ip: IpAddr = match value.trim().parse() {
        Ok(ip) => ip,
        Err(error) => return Some(Err(format!("invalid GIT_VISTA_LAN_IP '{value}': {error}"))),
    };
    if ip.is_loopback() {
        return Some(Err(format!(
            "refusing GIT_VISTA_LAN_IP '{value}': that is a loopback address, not a LAN interface"
        )));
    }
    if ip.is_unspecified() {
        return Some(Err(format!(
            "refusing GIT_VISTA_LAN_IP '{value}': 0.0.0.0 is never accepted — pass one explicit interface address"
        )));
    }
    Some(Ok(SocketAddr::new(ip, PORT)))
}

// Upper bound on how much history to walk; plenty for now. `/api/frame` and the
// paged `/api/commits` (M1.10, #63) no longer use this — paging has no
// whole-history cap, only a per-page `?limit=` clamped by `page_limit`. It
// remains the cap for the activity feed's remote-commit lookup
// (`activity::activity_feed`), which still needs one whole-history scan.
pub(crate) const HISTORY_LIMIT: usize = 5_000;

/// Environment variable that opts the operator into exposing absolute filesystem
/// paths to the browser (the graph's `repo_label` and the catalog descriptors).
/// Off by default (M1.03): the server's layout is not the browser's business, so
/// only a short base-name label is sent unless this is set to a truthy value.
const EXPOSE_PATHS_ENV: &str = "GIT_VISTA_EXPOSE_PATHS";

/// Whether the operator opted into exposing absolute paths (see
/// [`EXPOSE_PATHS_ENV`]). Any value other than empty/`0`/`false` counts as on.
pub(crate) fn expose_paths() -> bool {
    match std::env::var(EXPOSE_PATHS_ENV) {
        Ok(v) => !matches!(v.trim(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

/// A short, non-path label for `path` — its directory base name — unless the
/// operator opted into path exposure, in which case the full path is used. This
/// is what the UI header shows; it never leaks the server's layout by default.
pub(crate) fn repo_label(path: &Path) -> String {
    if expose_paths() {
        return path.display().to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The process-wide catalog of servable repositories (M1.03). Every path→id
/// mapping and every allowed root lives here; it is the only thing that turns an
/// opaque request id back into a filesystem path, and it fails closed on anything
/// it did not itself register. See [`crate::catalog`].
static CATALOG: OnceLock<RwLock<Catalog>> = OnceLock::new();

fn catalog() -> &'static RwLock<Catalog> {
    CATALOG.get_or_init(|| RwLock::new(Catalog::new()))
}

/// Permit repositories under `dir` to be registered in the catalog. Used for the
/// clones root at startup; server-initiated selections ([`set_current`]) also
/// allow their own root automatically.
pub(crate) fn allow_repo_root(dir: &Path) {
    catalog().write().expect("catalog lock").allow_root(dir);
}

/// Whether `canonical` (already canonicalised by the caller) lies within an
/// allowed root. The clone handler uses this to confirm a destination stays
/// inside the clones root before serving it.
pub(crate) fn path_is_allowed(canonical: &Path) -> bool {
    catalog()
        .read()
        .expect("catalog lock")
        .contains_path(canonical)
}

/// Resolve an opaque worktree id to `(canonical path, read_only, handle)`, or
/// `None` for any id the catalog does not hold — the fail-closed path a request
/// for an unknown or forged id takes. Clones the small result out of the lock at
/// once so no guard is held across an `.await`.
pub(crate) fn resolve_worktree(worktree: WorktreeId) -> Option<(PathBuf, bool, RepositoryHandle)> {
    let c = catalog().read().expect("catalog lock");
    c.resolve(worktree)
        .map(|e| (e.path.clone(), e.read_only, e.handle))
}

/// The operator-configured repos root (ADR 0009): `GIT_VISTA_REPO_ROOT`, set by
/// `gv --root <dir>` (env form so systemd units can set it too). None = the
/// feature is off and only the launch repo + clones are served.
pub(crate) fn repo_root() -> Option<PathBuf> {
    std::env::var_os("GIT_VISTA_REPO_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The operator-configured explicit repo list (ADR 0009's list form):
/// `GIT_VISTA_REPOS`, a `:`-separated list of absolute repository paths.
///
/// # Why a list as well as a root
///
/// [`repo_root`] answers "serve everything in this folder." That is the wrong
/// shape when the operator wants a handful of repositories that do not share a
/// parent — the only way to express it with a root alone is to point at a
/// common ancestor and serve every sibling too. This says exactly which ones.
///
/// `:` rather than `,` as the separator, matching `PATH` and every other
/// path-list variable on this platform: a comma is a legal character in a Unix
/// path and a colon is not meaningfully so, which makes the wrong-separator
/// mistake fail loudly instead of silently producing one absurd path.
///
/// Empty entries are dropped rather than treated as `.` — a trailing colon or
/// a doubled `::` is a typo, not a request to serve the process's working
/// directory.
pub(crate) fn repo_list() -> Vec<PathBuf> {
    std::env::var_os("GIT_VISTA_REPOS")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Register the explicit repo list into the catalog (startup and
/// `POST /api/rescan`). Empty list = the feature is off; `(0, 0)` then, not an
/// error — the same soft posture [`scan_clones_root`] takes for a missing
/// clones root.
pub(crate) fn register_repo_list() -> (usize, usize) {
    let paths = repo_list();
    if paths.is_empty() {
        return (0, 0);
    }
    catalog()
        .write()
        .expect("catalog lock")
        .register_explicit(&paths, false)
}

/// Scan the configured root into the catalog (startup and `POST /api/rescan`).
/// `None` = no root configured; `Some((registered, skipped))` otherwise.
pub(crate) fn scan_repo_root() -> Option<(usize, usize)> {
    let root = repo_root()?;
    Some(
        catalog()
            .write()
            .expect("catalog lock")
            .scan_direct_children(&root, false),
    )
}

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

/// The capability view of the catalog for `GET /api/catalog`: the servable
/// repositories addressed by opaque id, with absolute paths included only when
/// the operator opted in ([`expose_paths`]).
/// INV-15 (#66, #202): each descriptor also carries the hook policy that
/// repository's local operations actually run under, computed from the boot
/// sandbox verdict and the repository's own operator-trust state. The verdict is
/// read here — one process-global read at the production seam — rather than
/// inside [`Catalog::descriptors`], so the catalog's own unit tests are not
/// order-dependent on whether the boot probe happened to have run.
pub(crate) fn catalog_descriptors() -> Vec<RepositoryDescriptor> {
    catalog()
        .read()
        .expect("catalog lock")
        .descriptors(expose_paths(), crate::sandbox::probe::boot_verdict())
}

/// The capability descriptor for one registered worktree — the clone handler's
/// success body (ADR 0008) — or `None` for an id the catalog does not hold.
pub(crate) fn descriptor_for(worktree: WorktreeId) -> Option<RepositoryDescriptor> {
    catalog().read().expect("catalog lock").descriptor_of(
        worktree,
        expose_paths(),
        crate::sandbox::probe::boot_verdict(),
    )
}

/// The repository the server is currently serving *by default* — the selection a
/// request with no explicit `?repo=` id acts on. Mutable at runtime: starts at
/// the CLI-arg repo (Active — the user's own working repo), `POST /api/clone`
/// swaps it for a clone (Visualize), and `POST /api/select` moves it to any
/// catalog entry in the mode the operator chose (ADR 0007).
/// `pub(crate)` only so [`SelectionCell`] can name it — a session record holds
/// one of these but never looks inside. Its fields stay private to this module.
#[derive(Clone)]
pub(crate) struct Current {
    path: PathBuf,
    /// Visualize = look-only: every write handler refuses (ADR 0007). This
    /// supersedes the old per-selection `read_only: bool` (Phase-12 clones).
    mode: RepoMode,
    /// The opaque handle for this selection, when it registered in the catalog.
    /// `None` only in degraded mode (the path wouldn't classify as a repo), where
    /// the reads still run and surface git's own error.
    handle: Option<RepositoryHandle>,
}

/// The **launch** selection: the repository the operator started the server on.
///
/// Written once, by `main`, before any listener binds — and, since #588, never
/// again by a request. Every request-scoped write lands in that request's
/// session cell instead (see [`SELECTION`]), so this stays the fixed, defined
/// place a *fresh* session begins at rather than drifting to whatever the last
/// person happened to pick.
///
/// Since #614 that is **enforced rather than merely true**: the `OnceLock` is
/// never reopened for writing, so a second no-scope write is refused by the
/// lock itself and panics loudly instead of overwriting. See
/// [`write_launch_selection`] for why the refusal lives there and not behind a
/// flag `main` has to remember to set.
static CURRENT: OnceLock<RwLock<Current>> = OnceLock::new();

/// One session's selection, shared between the session record that owns it and
/// whatever task is currently serving a request for that session.
pub(crate) type SelectionCell = Arc<RwLock<Option<Current>>>;

/// A selection cell for a session that has not chosen anything yet. Empty
/// means "no choice made", which resolves to the launch selection — never to
/// another session's leftovers.
pub(crate) fn new_selection_cell() -> SelectionCell {
    Arc::new(RwLock::new(None))
}

tokio::task_local! {
    /// The selection belonging to whoever this task is serving (#588).
    ///
    /// Established by `security::guard` once per authenticated request, from
    /// the cell hanging off that session's record, and inherited by detached
    /// tasks through [`inherit_selection`]. A task with no scope — startup, and
    /// only startup — reads and writes the process-global [`CURRENT`].
    ///
    /// This began as a `#[cfg(test)]` task-local that existed so parallel tests
    /// could not replace one another's fixture repository. The hazard was real
    /// and the shape was right; it was only ever scoped too narrowly. It is now
    /// the production mechanism, and the test harness below is a thin wrapper
    /// over it rather than a shadow of it — which is what #588's last
    /// acceptance criterion asks for.
    static SELECTION: SelectionCell;
}

/// Serve `future` as the owner of `cell`.
///
/// A cell nothing has been chosen from yet is seeded with the **ambient**
/// selection first, so a brand-new session starts at a defined place rather
/// than at nothing.
///
/// "Ambient" is [`current_snapshot`] evaluated *before* entering the new scope,
/// which is the launch selection in production (no enclosing scope, so
/// [`CURRENT`]) and the harness's own scope under test. One expression covers
/// both, and the seed is therefore never another session's cell: a session
/// scope is only ever entered from the guard, which is not itself inside one.
///
/// Seeding happens here rather than at session creation because the launch
/// selection is not yet set when a `SessionManager` is built.
pub(crate) async fn with_selection<F: std::future::Future>(
    cell: SelectionCell,
    future: F,
) -> F::Output {
    if cell.read().expect("selection lock not poisoned").is_none() {
        if let Some(ambient) = current_snapshot() {
            *cell.write().expect("selection lock not poisoned") = Some(ambient);
        }
    }
    SELECTION.scope(cell, future).await
}

/// Run `future` in a fresh, isolated selection scope.
///
/// The test harness. Async tests that select a repository must use it, so a
/// parallel test cannot replace another's fixture — and so nothing in the test
/// binary writes the process-global launch selection.
#[cfg(test)]
pub(crate) async fn with_isolated_test_current<F: std::future::Future>(future: F) -> F::Output {
    SELECTION.scope(new_selection_cell(), future).await
}

/// Carry the caller's selection into a detached task.
///
/// The child shares the *same* cell, not a copy: a task spawned to serve a
/// request must see that session's repository, and a selection it makes must be
/// visible to the session that spawned it. Outside any scope (startup) the
/// future is returned unchanged and resolves against [`CURRENT`].
pub(crate) fn inherit_selection<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    // Capture synchronously: `tokio::spawn` first polls the returned future in
    // the child task, where the parent's task-local scope is no longer visible.
    let inherited = SELECTION.try_with(Arc::clone).ok();
    async move {
        match inherited {
            Some(cell) => SELECTION.scope(cell, future).await,
            None => future.await,
        }
    }
}

/// What a write that arrived with **no selection scope** was permitted to do.
///
/// Exactly one such write is ever legitimate — startup seeding the launch
/// selection, before any listener binds. See [`write_launch_selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchWrite {
    /// The launch selection was empty, so this write is the startup seed.
    Seeded,
    /// The launch selection was already seeded. The write was refused and
    /// `cell` still holds what startup put there.
    Refused,
}

/// Seed the launch selection if nothing has, refuse otherwise (#614).
///
/// # Why a refusal exists at all
///
/// Before this, the no-scope branch of [`set_current_resolved`] *overwrote*
/// `CURRENT` whenever it was already set, and the only thing stopping a
/// request from reaching it was a fact about the current call graph: the two
/// request-path writers (`POST /api/clone`, `POST /api/select`) both run
/// inside [`with_selection`], so they take the session branch. The `panic!`
/// that would have caught a writer arriving without a scope was
/// `#[cfg(test)]` — **it did not exist in the release binary**. A future
/// handler that spawned without [`inherit_selection`] would have written the
/// process-global in production, silently restoring the cross-session leak
/// #588 was filed to remove, while every test stayed green because under
/// `cfg(test)` that path panics instead of running.
///
/// # Why `OnceLock::set` *is* the enforcement
///
/// The rule that needs enforcing is "no-scope writes are legitimate only
/// before bind", and `CURRENT` is already a `OnceLock`: startup seeds it once,
/// so every later no-scope write is by definition the illegitimate one.
/// `set` refuses those itself, atomically — no `BOUND` flag to set from `main`
/// and forget to set in some other entry point, and no window where two
/// concurrent writers both observe an empty cell and both proceed. The loser
/// of that race is `Refused` for the same reason a post-startup writer is,
/// which is the honest answer in both cases.
///
/// Takes the cell rather than reading the static, so a host test can drive the
/// release path against its own `OnceLock` without writing the process-global
/// — the write this whole function exists to prevent.
fn write_launch_selection(cell: &OnceLock<RwLock<Current>>, value: Current) -> LaunchWrite {
    match cell.set(RwLock::new(value)) {
        Ok(()) => LaunchWrite::Seeded,
        Err(_) => LaunchWrite::Refused,
    }
}

fn set_current_resolved(path: PathBuf, mode: RepoMode, handle: Option<RepositoryHandle>) {
    let value = Current { path, mode, handle };
    if let Ok(()) = SELECTION.try_with(|cell| {
        *cell.write().expect("selection lock not poisoned") = Some(value.clone());
    }) {
        return;
    }
    // No scope. Legitimate exactly once: startup writing the launch selection
    // before any listener binds.
    #[cfg(test)]
    panic!("tests that select a repository must use with_isolated_test_current");
    // The release path the test-time panic above cannot cover (#614). The
    // write is refused by `OnceLock` either way; the panic is how the caller
    // finds out, because `set_current` and `select_registered` both report
    // success to their handler and a refusal they never hear about is the
    // quiet failure this issue is about. Contained: a panic in a request task
    // fails that one connection, and no session's selection has moved.
    #[cfg(not(test))]
    if write_launch_selection(&CURRENT, value) == LaunchWrite::Refused {
        panic!(
            "git-vista: a repository selection reached the launch selection with no session \
             scope, after startup had already set it. This is a bug in the caller, not in the \
             request: every request-path write must run inside `with_selection`, and a detached \
             task must be wrapped in `inherit_selection` to keep it. The write was refused; the \
             launch selection is unchanged."
        );
    }
}

/// Clone the current selection while holding its lock only briefly.
///
/// The session serving this task answers first (#588). Outside any session
/// scope — startup, and only startup — the launch selection answers.
fn current_snapshot() -> Option<Current> {
    if let Ok(Some(current)) =
        SELECTION.try_with(|cell| cell.read().expect("selection lock not poisoned").clone())
    {
        return Some(current);
    }

    CURRENT
        .get()
        .map(|current| current.read().expect("CURRENT lock not poisoned").clone())
}

/// Snapshot the current repo path and whether it is look-only. The bool keeps
/// the old `read_only` meaning (`mode == Visualize`) so the many read-handler
/// call sites stay untouched; write gating goes through [`current_mode`]/
/// [`reject_if_read_only`]. Clones out of the lock immediately so no guard is
/// ever held across an `.await`.
pub(crate) fn current() -> (PathBuf, bool) {
    let g = current_snapshot().expect("CURRENT is set at startup");
    (g.path.clone(), g.mode == RepoMode::Visualize)
}

/// The current selection's path, or `None` when nothing has been selected yet.
///
/// The non-panicking sibling of [`current`]. [`current`] is right for the
/// request handlers — by the time a request is served, `main` has long since
/// called [`set_current`], and a panic there would mean a broken invariant, not
/// a case to handle. This one exists for a caller that must produce an answer
/// even before startup has run: the session handler's INV-15 disclosure
/// (#202), which is reachable from `#[cfg(test)]` router tests that never touch
/// the process-wide selection, and whose honest answer in that case is "no
/// policy is known" rather than a panic or a fabricated value.
pub(crate) fn current_path_if_set() -> Option<PathBuf> {
    current_snapshot().map(|current| current.path)
}

/// The mode the current selection is open in (ADR 0006/0007).
pub(crate) fn current_mode() -> RepoMode {
    current_snapshot().expect("CURRENT is set at startup").mode
}

/// The opaque handle for the current default selection, or `None` in degraded
/// mode. Used to stamp the graph with the ids the client addresses it by.
pub(crate) fn current_handle() -> Option<RepositoryHandle> {
    current_snapshot()
        .expect("CURRENT is set at startup")
        .handle
}

/// D2 (#66, Task 7): whether `path` should get the sandbox's write grant
/// withheld — the signal `sandbox::policy_for` uses.
///
/// **Decision 2026-07-30 (design-docs/2026-07-30-read-only-vs-mode-conflict.md,
/// Option A):** this must agree with [`reject_if_read_only`], not diverge from
/// it. An earlier version of this function answered from the catalog's
/// static, registration-time `read_only` flag instead — reasoned as "defense
/// in depth" against a hypothetical bug in the app-level gate, but it silently
/// reintroduced exactly the always-read-only-clone posture ADR 0007 already
/// considered and rejected: *"a clone opened in active mode accepts local
/// writes... `RepoEntry.read_only` is superseded."* The visible bug that
/// exposed the divergence: reselecting a clone into Active mode passed
/// `reject_if_read_only` (mode says Active) and then failed writes two layers
/// down with a raw sandbox permission error, because this function still said
/// read-only. Mode is the single source of truth; there must be only one.
///
/// For `path` equal to the current selection **at the moment this is called**,
/// this is exactly `current_mode() == Visualize` — the same value [`current`]'s
/// compat bool already returns. For any other path (including when `CURRENT`
/// has not been initialized at all — most of this crate's ~40 spawn-focused
/// unit tests deliberately run git against their own throwaway `tempdir()`
/// with no `set_current`/catalog registration whatsoever, by design; see
/// `sandbox::policy_for`'s doc comment), this falls back to the catalog's
/// record, same as before D2. The grant only matters for writes, and reads
/// don't care whether they get an `rw_trees` or `ro_trees` grant.
///
/// **Known residual gap, narrowed by #588 but not closed (adversarial review,
/// 2026-07-30; re-stated 2026-09-01):** this reads the selection fresh at call
/// time, not at the moment a write request's target was resolved. Between
/// `state::resolve_target()` capturing "repo B, Active" for an in-flight
/// mutation and that mutation's eventual `git_cmd::sandboxed` spawn — real
/// `.await` points sit in between (durable persistence, task admission) — a
/// reselection can land in between. The in-flight write to B then finds
/// `path != B` here, falls through to the catalog, and can get spuriously
/// denied by a stale flag even though B was legitimately Active when the write
/// was authorized. This is **fail-closed only** (a legitimate write can be
/// wrongly refused; nothing insecure can succeed) — same-path mode flips, the
/// case that fix targets and its regression test proves, are unaffected.
///
/// What #588 changed: the reselection that can do this must now come from the
/// **same session**. It used to be any request on the server, because the
/// selection was one process-global value; it is now that session's own cell,
/// and a detached task inherits the cell of the request that spawned it
/// (`inherit_selection`) rather than falling through to a process global. A
/// second browser session, or a second device, can no longer perturb an
/// in-flight write it has nothing to do with.
///
/// What is still owed, and is deliberately NOT part of #588: closing it
/// properly still means `resolve_target` capturing `read_only` alongside the
/// path and threading that snapshot through to `sandbox::policy_for` instead
/// of re-deriving it here at spawn time. That crosses the planner/sandbox
/// boundary and is its own change — named so it isn't silently lost.
pub(crate) fn read_only_for_path(path: &Path) -> bool {
    if let Some(current) = current_snapshot() {
        if current.path == path {
            return current.mode == RepoMode::Visualize;
        }
    }
    catalog()
        .read()
        .expect("catalog lock")
        .read_only_for_path(path)
        .unwrap_or(false)
}

/// D2 (#66, Task 7): the single validated resolution every write handler and
/// the planner's execution entry point use in place of a raw
/// `current()`/`current_handle()` call.
///
/// Mutation endpoints take no `?repo=` selector — they always act on the
/// current default selection (unlike the read endpoints' `resolve_repo`,
/// which additionally accepts an explicit id) — so this resolves exactly
/// that selection, but does two things a bare `current()` never did:
///
/// 1. **Fails closed on degraded mode.** A selection with no catalog entry
///    (the path never classified as a git repository) refuses here rather
///    than handing back a raw, unvalidated path a mutating git argv would
///    then be built against. This is a deliberate asymmetry with the read
///    side: `resolve_repo`'s doc comment records that a degraded selection's
///    *reads* run and surface git's own "not a repository" error, and that
///    established, lenient read-side posture is untouched by this function —
///    a mutation simply has no legitimate reason to run against a directory
///    that was never a repository in the first place.
/// 2. **Re-validates the selection's `.git` geometry** via
///    `sandbox::repo_paths::resolve`, composed with the catalog's own
///    multi-root `path_is_allowed` exactly the way `sandbox::policy_for`'s
///    doc comment describes doing at the *request-resolution* layer rather
///    than inside policy construction itself (see that doc comment for why).
///    A hostile `.git` gitfile written since the selection was last read is
///    refused here, before any mutating argv is built — not merely when the
///    eventual git spawn's own policy gets around to it.
///
/// Returns the entry's own canonical path (not whatever spelling the
/// in-memory selection happened to hold) alongside the full [`RepoEntry`], so
/// a caller that needs `read_only`/`kind`/the handle has it without a second
/// lookup.
pub(crate) fn resolve_target() -> Result<(PathBuf, RepoEntry), (StatusCode, String)> {
    let handle = current_handle().ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            "The current selection isn't a recognised repository.".to_string(),
        )
    })?;
    let entry = catalog()
        .read()
        .expect("catalog lock")
        .resolve(handle.worktree)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "The current selection is no longer registered.".to_string(),
            )
        })?;
    let paths = crate::sandbox::repo_paths::resolve(&entry.path).map_err(|e| {
        eprintln!("git-vista: resolve_target refused a mutation target: {e}");
        (StatusCode::CONFLICT, e.to_string())
    })?;
    if !path_is_allowed(&paths.gitdir) || !path_is_allowed(&paths.commondir) {
        eprintln!(
            "git-vista: resolve_target refused {} — its git directory resolves outside \
             the server's managed root",
            entry.path.display()
        );
        return Err((
            StatusCode::CONFLICT,
            "This repository's git directory is outside the server's managed root.".to_string(),
        ));
    }
    Ok((entry.path.clone(), entry))
}

/// Point the server at a new repository (startup, or after a clone), registering
/// it in the catalog and making it the default selection.
///
/// This is the **trusted, server-initiated** path — the operator launched this
/// repo, or the server itself cloned it — so it allows the repository's own
/// canonical root before registering. That is what lets you launch `gv` inside
/// any repo; it does *not* widen what a *request* can reach, since requests never
/// call this and resolve ids only against what is already registered.
///
/// If the path won't classify as a git repository, the server drops to degraded
/// mode: the selection is still set (so the reads run and surface git's own
/// error, as before this module), but with no catalog entry and no handle.
pub(crate) fn set_current(path: &Path, mode: RepoMode) -> Option<RepositoryHandle> {
    let registered = {
        let mut c = catalog().write().expect("catalog lock");
        // Trusted selection: allow its own root so a repo launched from anywhere
        // (or a fresh clone under the clones root) can register.
        if let Ok(facts) = git_vista_git::read_repo_facts(path) {
            c.allow_root(&facts.root);
        }
        // The entry's read_only flag records "opened look-only" for the catalog
        // report; the live write gate is the selection's mode (ADR 0007).
        c.register(path, mode == RepoMode::Visualize)
    };
    match registered {
        Ok(handle) => {
            // Use the catalog's canonical path for the selection, so `current()`
            // and an explicit `?repo=<this id>` resolve to the very same path.
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

/// `POST /api/select` (ADR 0007): move the current selection to an id the
/// catalog already holds, in `mode`. Returns false — and changes nothing — for
/// an unknown or forged id: the handler turns that into a 404, the same
/// fail-closed contract as the reads.
pub(crate) fn select_registered(worktree: WorktreeId, mode: RepoMode) -> bool {
    match resolve_worktree(worktree) {
        Some((path, _, handle)) => {
            set_current_resolved(path, mode, Some(handle));
            true
        }
        None => false,
    }
}

/// Admit a linked worktree discovered by the census (M11.03, #548), so the
/// selection machinery can address it.
///
/// A thin wrapper over [`Catalog::register`], and deliberately nothing more:
/// it does **not** call `allow_root` first. That omission is the whole point.
/// `register_explicit` allows a root and then registers under it, which is
/// right for a path an operator named on the command line; doing the same here
/// would make "this worktree was discovered" sufficient to widen the fence,
/// and creating a worktree would become a way to make the app serve any
/// directory — the second of the three options
/// `docs/superpowers/specs/m3.23-worktrees.md` §1 weighs and rejects.
///
/// So this can only ever succeed for a path that was **already** inside an
/// allowed root, which is exactly what `Serviceable::Yes` means. The caller
/// has checked that; this checks it again, independently, in the function that
/// has always owned the check.
pub(crate) fn register_discovered_worktree(
    path: &Path,
    read_only: bool,
) -> Result<RepositoryHandle, CatalogError> {
    catalog()
        .write()
        .expect("catalog lock")
        .register(path, read_only)
}

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
/// The managed root every worktree this app creates lives under (M11.04,
/// #549, ADR 0118): `GIT_VISTA_WORKTREES_ROOT` override, else
/// `$XDG_DATA_HOME/git-vista/worktrees`, else
/// `~/.local/share/git-vista/worktrees`.
///
/// The exact shape of [`clones_root`], and deliberately so — it is the same
/// kind of thing (a directory this application owns, creates, and serves from)
/// and ADR 0008 already argued where such a directory belongs. Sharing the
/// resolver rather than the location keeps the two from ever nesting inside
/// one another, which would make `delete_clone`'s "canonicalizes inside the
/// clones root" guard start matching worktrees.
///
/// # Why a managed root rather than a sibling directory
///
/// ADR 0118, answering the spec's open question 2. A managed root is inside
/// the fence **by construction**: it is admitted to the allowed roots once, at
/// startup, so every child of it is servable without any per-path check. A
/// sibling-directory convention would need containment re-checked at every
/// site that picks a path, and "checked every time" is a rule that holds until
/// one code path forgets.
pub(crate) fn worktrees_root() -> PathBuf {
    resolve_managed_root(
        std::env::var_os("GIT_VISTA_WORKTREES_ROOT").map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        "worktrees",
    )
}

/// The shared resolver behind [`clones_root`] and [`worktrees_root`]: an
/// explicit override, else XDG, else `~/.local/share`, else a temp fallback —
/// with `leaf` naming which managed directory is wanted.
fn resolve_managed_root(
    override_root: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
    leaf: &str,
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
    base.join("git-vista").join(leaf)
}

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

/// This user's git-vista state directory — `$XDG_STATE_HOME/git-vista`, or
/// `~/.local/state/git-vista` when that isn't set. Matches the `gv` launcher's
/// `LOG_DIR`, so the server and `gv` agree on where the bootstrap token lives.
fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| std::env::temp_dir().join("git-vista-state"));
    base.join("git-vista")
}

/// Where the one-time session bootstrap token (M1.04) is written `0600` at
/// startup. `gv` reads this exact path to build the `#s=<token>` setup URL it
/// prints; nothing else — and no request — ever reads it.
pub(crate) fn bootstrap_token_path() -> PathBuf {
    state_dir().join("bootstrap.token")
}

/// Directory holding the per-repository sandbox trust markers (M1.13b, #66,
/// Task 7). It lives under the server's own state directory *on purpose*: a
/// sandboxed repository is granted `$HOME` read-only, so it can *read* this path
/// but cannot *write* a marker to grant itself trust — which is the property
/// that keeps the `Unsandboxed` tier reachable only by an explicit operator
/// action, never by a hostile hook. See `sandbox::trust`.
pub(crate) fn sandbox_trust_dir() -> PathBuf {
    state_dir().join("trusted-repos")
}

/// Where the durable operation journal's SQLite file lives (M1.09, #62).
/// Process-wide rather than per-repository: the operation registry already
/// addresses repositories by opaque token, not path, and one file keeps
/// startup recovery a single open instead of a scan of every served repo.
///
/// Only [`crate::durable::db_path`] calls this, and only outside `#[cfg(test)]`
/// (tests point at a throwaway file instead, see that function's docs) — so a
/// test build never references it, which `dead_code` would otherwise flag.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn operations_db_path() -> PathBuf {
    state_dir().join("operations.sqlite3")
}

/// Delete a previous clone's directory, best-effort. Guarded: only ever removes a
/// path under [`clones_root`], so it can't touch the user's own repo even if state
/// were somehow wrong.
pub(crate) fn cleanup_clone(path: &Path) {
    if path.starts_with(clones_root()) {
        if let Err(e) = std::fs::remove_dir_all(path) {
            eprintln!(
                "git-vista: couldn't remove old clone {}: {e}",
                path.display()
            );
        }
    }
}

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

/// Guard for the write endpoints: in Visualize mode (ADR 0006/0007) the current
/// selection is look-only, so every mutation is refused with `403` and a clear
/// reason. Returns `None` when writes are allowed (Active mode).
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::{parse_bind_addr, LOOPBACK_ADDR};

    fn selection(path: &str) -> Current {
        Current {
            path: PathBuf::from(path),
            mode: RepoMode::Active,
            handle: None,
        }
    }

    /// #614: the release path's no-scope write refuses once the launch
    /// selection is seeded, and leaves it holding what startup put there.
    ///
    /// This is the branch `set_current_resolved`'s `#[cfg(test)]` panic hides:
    /// under `cfg(test)` a scopeless write panics before it can reach the
    /// release code, so a test that went through `set_current_resolved` would
    /// prove the harness works and say nothing about the shipped binary. So it
    /// drives [`write_launch_selection`] — the release branch itself,
    /// compiled in both configurations — against its **own** `OnceLock`. Its
    /// own, for the same reason the harness exists: writing the real `CURRENT`
    /// from a test is the defect, not the test for it.
    ///
    /// The second assertion is the one that matters. A refusal that still
    /// mutated the cell would satisfy the verdict and lose the guarantee, and
    /// that is exactly the pre-#614 behaviour: the old branch overwrote
    /// `CURRENT` whenever it was already set.
    #[test]
    fn a_no_scope_write_after_startup_is_refused_and_leaves_the_launch_selection_alone() {
        let cell = OnceLock::new();

        assert_eq!(
            write_launch_selection(&cell, selection("/launch")),
            LaunchWrite::Seeded,
            "the first no-scope write is startup seeding the launch selection"
        );
        assert_eq!(
            write_launch_selection(&cell, selection("/hijacked")),
            LaunchWrite::Refused,
            "a second no-scope write is a post-bind writer and must be refused"
        );

        assert_eq!(
            cell.get()
                .expect("seeded above")
                .read()
                .expect("CURRENT lock not poisoned")
                .path,
            PathBuf::from("/launch"),
            "the refused write reached the launch selection anyway"
        );
    }

    /// #438: two test tasks that select different repositories must retain
    /// their own path and mode after both writes have happened.
    ///
    /// Mutation caught: bypassing the test-local selection and writing the
    /// process-global `CURRENT` makes exactly one task observe the other's
    /// literal selection after the barrier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_test_selections_do_not_overwrite_each_other() {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

        let observe = |path: &'static str, mode: RepoMode| {
            let barrier = std::sync::Arc::clone(&barrier);
            tokio::spawn(async move {
                with_isolated_test_current(async move {
                    let path = PathBuf::from(path);
                    set_current_resolved(path.clone(), mode, None);
                    barrier.wait().await;
                    assert_eq!(
                        current(),
                        (path, mode == RepoMode::Visualize),
                        "a concurrent test replaced this task's repository selection"
                    );
                })
                .await;
            })
        };

        let active = observe("/tmp/git-vista-current-active", RepoMode::Active);
        let visualize = observe("/tmp/git-vista-current-visualize", RepoMode::Visualize);
        let (active, visualize) = tokio::join!(active, visualize);
        active.expect("active selection task completes");
        visualize.expect("visualize selection task completes");
    }

    /// Detached operation tasks must see the selection of the request that
    /// spawned them, not a process-global or catalog fallback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detached_tasks_inherit_their_session_selection() {
        with_isolated_test_current(async {
            let expected = PathBuf::from("/tmp/git-vista-current-detached");
            set_current_resolved(expected.clone(), RepoMode::Active, None);

            let observed = tokio::spawn(inherit_selection(async { current() }))
                .await
                .expect("detached selection task completes");

            assert_eq!(observed, (expected, false));
        })
        .await;
    }

    /// Drive the selection/catalog flow inside an explicit test-local current
    /// scope. The production process global remains unchanged, while every
    /// await in this long test keeps resolving its own fixture repository.
    #[tokio::test]
    async fn selection_flow_carries_mode_and_gates_writes() {
        with_isolated_test_current(selection_flow_carries_mode_and_gates_writes_in_scope()).await;
    }

    async fn selection_flow_carries_mode_and_gates_writes_in_scope() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());

        set_current(&repo, RepoMode::Active);
        assert_eq!(current_mode(), RepoMode::Active);
        assert!(reject_if_read_only().is_none(), "active mode allows writes");
        assert!(!current().1);

        let wt = current_handle().expect("registered").worktree;
        assert!(select_registered(wt, RepoMode::Visualize));
        assert_eq!(current_mode(), RepoMode::Visualize);
        let (status, msg) = reject_if_read_only().expect("visualize refuses writes");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(msg.contains("Visualize"));
        assert!(current().1, "compat bool mirrors visualize");

        // A forged id changes nothing and reports failure (the 404 path).
        let stranger =
            git_vista_core::identity::WorktreeId::from_git_dir("/nowhere/.git/worktrees/ghost");
        assert!(!select_registered(stranger, RepoMode::Active));
        assert_eq!(current_mode(), RepoMode::Visualize);

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

        // --- 2026-07-30 (design-docs/2026-07-30-read-only-vs-mode-conflict.md, ---
        // --- Option A): reselecting a clone into Active mode must actually ------
        // --- make it writable — ADR 0007, not a stale catalog snapshot. --------
        assert!(
            read_only_for_path(&clone_dir),
            "a clone just registered in Visualize mode reads read-only"
        );
        assert!(select_registered(clone_wt, RepoMode::Active));
        assert!(
            !read_only_for_path(&clone_dir),
            "reselecting the SAME clone into Active mode must lift the sandbox's \
             write grant, not silently keep it read-only underneath a gate that \
             already says writes are allowed"
        );
        assert!(select_registered(clone_wt, RepoMode::Visualize)); // restore for the flow below

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
        assert_eq!(
            delete_clone(clone_wt, &clones),
            DeleteCloneOutcome::NotFound
        );

        // --- D2 (#66, Task 7): the real handler refuses an out-of-managed- --
        // --- root linked worktree, end to end -------------------------------
        //
        // Everything above already leaves the selection on `repo` (Active,
        // writable) — reused rather than building a third throwaway repo.
        // This section proves the managed-root check at the actual
        // request-shaped seam (a real handler function), not merely as a
        // unit test of `sandbox::repo_paths::resolve_and_validate` in
        // isolation: a REAL linked worktree (built with actual `git worktree
        // add`, not hand-rolled files) whose main repository sits entirely
        // outside the managed root satisfies `worktree.rs`'s own containment
        // rule completely — that module alone would grant it — and is
        // refused only because `state::resolve_target` additionally checks
        // containment against the catalog's allowed roots.
        let elsewhere = tempfile::tempdir().unwrap();
        let main = elsewhere.path().join("main-repo");
        std::fs::create_dir_all(&main).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "t"],
            vec!["commit", "-q", "--allow-empty", "-m", "seed"],
        ] {
            assert!(std::process::Command::new("git")
                .args(&args)
                .current_dir(&main)
                .status()
                .unwrap()
                .success());
        }
        // `linked` lands under the same `root` this test's other fixtures use
        // (no root needs pre-allowing here: `set_current` below allows the
        // linked worktree's own canonical directory the same way it already
        // did for `repo` and `clone_dir` above — that is what makes the
        // scenario realistic). `main` sits in a wholly separate `elsewhere`
        // tempdir that nothing in this test ever allows.
        let linked = root.path().join("linked-worktree");
        assert!(std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ])
            .current_dir(&main)
            .status()
            .unwrap()
            .success());

        set_current(&linked, RepoMode::Active);
        assert_eq!(
            current_mode(),
            RepoMode::Active,
            "fixture invariant: the write gate must be open, so only the D2 \
             managed-root check can be what refuses this"
        );
        // Sanity: `worktree.rs`'s own rule alone sees nothing wrong here —
        // proving the managed-root check is what does the refusing below,
        // not a rule that already existed before D2.
        assert!(crate::sandbox::worktree::linked_worktree_dirs(&linked)
            .expect("the geometry is a real, valid linked worktree")
            .is_some());

        // The real production handler — `POST /api/rebase`'s actual body.
        let (status, msg) = crate::handlers::rebase::rebase().await;
        assert_ne!(
            status,
            StatusCode::OK,
            "a mutation against a repo whose linked-worktree main lives \
             outside the managed root must be refused, not executed: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("managed root"),
            "the refusal should name why: {msg}"
        );

        // --- #323: the real HTTP path for `POST /api/amend-commit` ----------
        //
        // Everything above calls a handler directly or drives the planner's
        // own pipeline stages (`contract_suite::pipeline`) — never through
        // the axum `Router` + middleware stack, so nothing in the crate has
        // proven what a client actually receives on the wire for this route.
        // This test is the crate's one legitimate owner of `CURRENT` (see the
        // module comment at the top of this fn); a second test elsewhere
        // calling `set_current` would race it under cargo's default parallel
        // test execution, so the new checks live here instead of in
        // `middleware.rs` alongside its own router-level tests.
        use axum::{
            body::{to_bytes, Body},
            http::{header, Request as HttpRequest},
            middleware::from_fn,
            routing::post,
            Router,
        };
        use git_vista_protocol::{
            AmendCommitError, AmendCommitSuccess, AmendFailureKind, ApiError, CommitError,
            CommitFailureKind, IDEMPOTENCY_HEADER, PROTOCOL_HEADER, PROTOCOL_VERSION,
        };
        use tower::ServiceExt;

        fn amend_router() -> Router {
            Router::new()
                .route(
                    "/api/amend-commit",
                    post(crate::handlers::commit::amend_commit),
                )
                .layer(from_fn(crate::middleware::idempotency))
                .layer(from_fn(crate::middleware::api_contract))
        }

        fn amend_req(body: String, key: &str) -> HttpRequest<Body> {
            HttpRequest::post("/api/amend-commit")
                .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                .header(IDEMPOTENCY_HEADER, key)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        }

        async fn resp_body(resp: axum::response::Response) -> String {
            let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }

        // `repo` (this fn's own fixture, above) never grew a commit — an
        // unborn HEAD, which is exactly one of `exec_amend_commit`'s two
        // `StaleTip` sources ("There is no commit here to amend", the D5
        // case its own doc comment names). Reusing it drives a genuine
        // executor-side refusal with no extra fixture, through the real
        // handler and the real `state::CURRENT`.
        set_current(&repo, RepoMode::Active);
        let refusal_body = format!(
            r#"{{"message":"does not matter","allow_empty":false,"expected_tip":"{}"}}"#,
            "0".repeat(40)
        );
        let resp = amend_router()
            .oneshot(amend_req(refusal_body, "amend-refusal-323"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json")),
            "the refusal must be labeled JSON so `middleware::rewrap_error` \
             passes it through untouched instead of re-enveloping it"
        );
        let body = resp_body(resp).await;
        let refusal: AmendCommitError = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("400 body did not parse as AmendCommitError ({e}): {body}"));
        assert_eq!(refusal.kind, AmendFailureKind::StaleTip);
        assert!(
            serde_json::from_str::<ApiError>(&body).is_err(),
            "the refusal was rewrapped into an ApiError envelope — double-encoded: {body}"
        );

        // The success path (200): zero coverage before this test existed.
        let success_root = tempfile::tempdir().unwrap();
        let success_repo = success_root.path().join("repo");
        std::fs::create_dir_all(&success_repo).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "t"],
            vec!["commit", "-q", "--allow-empty", "-m", "seed"],
        ] {
            assert!(std::process::Command::new("git")
                .args(&args)
                .current_dir(&success_repo)
                .status()
                .unwrap()
                .success());
        }
        // Register the catalog entry as Visualize, then select the same
        // worktree as Active only in this test's current-selection scope. The
        // amend below runs through `plan_and_execute_tracked`'s real detached
        // task. If the planner stops inheriting TEST_CURRENT, its sandbox
        // policy falls back to the deliberately stale Visualize catalog mode
        // and this genuine write cannot return 200.
        let success_handle = set_current(&success_repo, RepoMode::Visualize)
            .expect("the amend fixture registers in the catalog");
        assert!(read_only_for_path(&success_repo));
        assert!(select_registered(success_handle.worktree, RepoMode::Active));
        assert!(!read_only_for_path(&success_repo));
        let tip_out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&success_repo)
            .output()
            .unwrap();
        let tip = String::from_utf8_lossy(&tip_out.stdout).trim().to_string();
        let success_body =
            format!(r#"{{"message":"amended","allow_empty":true,"expected_tip":"{tip}"}}"#);
        let resp = amend_router()
            .oneshot(amend_req(success_body, "amend-success-323"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json")),
            "the success body had zero content-type coverage before this test"
        );
        let body = resp_body(resp).await;
        let success: AmendCommitSuccess = serde_json::from_str(&body).unwrap_or_else(|e| {
            panic!("200 body did not parse as AmendCommitSuccess ({e}): {body}")
        });
        assert_eq!(success.message, "Amended commit.");

        // --- #72 (M2.19): the real HTTP path for `POST /api/commit` ---------
        //
        // The same #323 proof `amend_router` gave `/api/amend-commit` above,
        // now for `/api/commit`'s own typed `CommitError`: through the real
        // `Router` + `idempotency` + `api_contract` middleware stack, not a
        // direct handler or planner call — so this proves what a client
        // actually receives on the wire, not just what the executor returns.
        // Reuses `success_repo`, still `set_current` from the amend success
        // case above and left with a clean working tree (nothing staged) —
        // exactly `classify_commit_failure`'s `NothingStaged` case, with no
        // extra fixture needed.
        fn commit_router() -> Router {
            Router::new()
                .route("/api/commit", post(crate::handlers::commit::create_commit))
                .layer(from_fn(crate::middleware::idempotency))
                .layer(from_fn(crate::middleware::api_contract))
        }

        fn commit_req(body: String, key: &str) -> HttpRequest<Body> {
            HttpRequest::post("/api/commit")
                .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                .header(IDEMPOTENCY_HEADER, key)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        }

        let nothing_staged_body =
            r#"{"message":"nothing here to commit","allow_empty":false}"#.to_string();
        let resp = commit_router()
            .oneshot(commit_req(nothing_staged_body, "commit-nothing-staged-72"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json")),
            "the typed commit refusal must be labeled JSON so `middleware::rewrap_error` \
             passes it through untouched instead of re-enveloping it (#72, mirroring #323)"
        );
        let body = resp_body(resp).await;
        let refusal: CommitError = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("400 body did not parse as CommitError ({e}): {body}"));
        assert_eq!(refusal.kind, CommitFailureKind::NothingStaged);
        assert!(
            serde_json::from_str::<ApiError>(&body).is_err(),
            "the refusal was rewrapped into an ApiError envelope — double-encoded: {body}"
        );

        // A third case, proving the *other* direction of the fix: not every
        // `plan_and_execute` output from this route is JSON, and the ones that
        // are not must still be enveloped. `plan_and_execute` itself refuses
        // with plain English when the idempotency header is absent
        // (`middleware::idempotency` just forwards the request untouched when
        // the header is missing — see its own doc comment — so nothing
        // upstream of the handler catches this one). Anything that labeled
        // that prose `application/json` would make `middleware::rewrap_error`'s
        // `is_json` check pass it through untouched, and the client would
        // receive a raw English sentence that fails to parse as `ApiError`
        // despite the header claiming JSON — worse than the double-encoding
        // #323 set out to fix, not better.
        //
        // Until #336 this route ran its output through a local
        // `amend_route_response` sniff, and this case was that sniff's
        // negative. It is now the negative for `rewrap_error`'s own sniff,
        // which is the only one left (ADR 0084): the prose stays `text/plain`
        // out of the handler, and `api_contract` wraps it into a proper
        // envelope, same as any other route's plain refusal.
        let no_key_body =
            format!(r#"{{"message":"amended","allow_empty":true,"expected_tip":"{tip}"}}"#);
        let req_no_key = HttpRequest::post("/api/amend-commit")
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
            .header("content-type", "application/json")
            .body(Body::from(no_key_body))
            .unwrap();
        let resp = amend_router().oneshot(req_no_key).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp_body(resp).await;
        let err: ApiError = serde_json::from_str(&body).unwrap_or_else(|e| {
            panic!(
                "the idempotency-gate's plain-English refusal must reach the client as a \
                 correctly-enveloped ApiError, not a raw sentence mislabeled application/json \
                 ({e}): {body}"
            )
        });
        assert!(
            err.error.message.to_lowercase().contains("header"),
            "the enveloped message should still name what was missing: {}",
            err.error.message
        );
    }

    #[test]
    fn bind_address_defaults_to_loopback() {
        assert_eq!(parse_bind_addr(None).unwrap(), LOOPBACK_ADDR);
    }

    #[test]
    fn bind_address_accepts_the_explicit_loopback_service_value() {
        assert_eq!(
            parse_bind_addr(Some("127.0.0.1:8080")).unwrap(),
            LOOPBACK_ADDR
        );
    }

    #[test]
    fn bind_address_rejects_an_all_interface_listener() {
        let error = parse_bind_addr(Some("0.0.0.0:8080")).unwrap_err();
        assert!(error.contains("only listens on 127.0.0.1:8080"));
    }

    #[test]
    fn bind_address_rejects_a_lan_interface() {
        let error = parse_bind_addr(Some("192.168.1.5:8080")).unwrap_err();
        assert!(error.contains("only listens on 127.0.0.1:8080"));
    }

    #[test]
    fn bind_address_rejects_invalid_configuration() {
        let error = parse_bind_addr(Some("not-an-address")).unwrap_err();
        assert!(error.contains("invalid GIT_VISTA_BIND_ADDR"));
    }

    // --- LAN listener address resolution (ADR 0005) -------------------------

    #[test]
    fn lan_ip_is_none_when_unset() {
        assert!(parse_lan_ip_env(None).is_none());
    }

    #[test]
    fn lan_ip_is_none_when_empty() {
        assert!(parse_lan_ip_env(Some("")).is_none());
    }

    #[test]
    fn lan_ip_accepts_an_explicit_lan_address() {
        let addr = parse_lan_ip_env(Some("192.168.1.42")).unwrap().unwrap();
        assert_eq!(addr, SocketAddr::new("192.168.1.42".parse().unwrap(), PORT));
    }

    #[test]
    fn lan_ip_rejects_loopback() {
        let error = parse_lan_ip_env(Some("127.0.0.1")).unwrap().unwrap_err();
        assert!(error.contains("loopback"));
    }

    #[test]
    fn lan_ip_rejects_unspecified() {
        let error = parse_lan_ip_env(Some("0.0.0.0")).unwrap().unwrap_err();
        assert!(error.contains("0.0.0.0"));
    }

    #[test]
    fn lan_ip_rejects_invalid_input() {
        let error = parse_lan_ip_env(Some("not-an-address"))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("invalid GIT_VISTA_LAN_IP"));
    }

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
            resolve_clones_root(
                None,
                Some(PathBuf::from("/xdg")),
                Some(PathBuf::from("/home/u"))
            ),
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
}
