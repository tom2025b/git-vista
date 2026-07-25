//! Server state, configuration constants, and the read-only write guard.
//!
//! Split out of `main.rs`: the process-wide "which repo are we serving, and is it
//! writable?" state ([`Current`]/[`CURRENT`]), the small config constants, and the
//! [`reject_if_read_only`] guard the write handlers share. Everything here is
//! crate-internal (this is a binary — there is no public API surface); the items
//! the handlers and `main` reach for are `pub(crate)`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use axum::http::StatusCode;

use git_vista_core::identity::{RepositoryHandle, WorktreeId};
use git_vista_protocol::{RepoMode, RepositoryDescriptor};

use crate::catalog::Catalog;

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

// Upper bound on how much history to walk; plenty for now. Shared by the graph
// read (`handlers::read`) and the activity feed's remote-commit lookup.
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
pub(crate) fn catalog_descriptors() -> Vec<RepositoryDescriptor> {
    catalog()
        .read()
        .expect("catalog lock")
        .descriptors(expose_paths())
}

/// The capability descriptor for one registered worktree — the clone handler's
/// success body (ADR 0008) — or `None` for an id the catalog does not hold.
pub(crate) fn descriptor_for(worktree: WorktreeId) -> Option<RepositoryDescriptor> {
    catalog()
        .read()
        .expect("catalog lock")
        .descriptor_of(worktree, expose_paths())
}

/// The repository the server is currently serving *by default* — the selection a
/// request with no explicit `?repo=` id acts on. Mutable at runtime: starts at
/// the CLI-arg repo (Active — the user's own working repo), `POST /api/clone`
/// swaps it for a clone (Visualize), and `POST /api/select` moves it to any
/// catalog entry in the mode the operator chose (ADR 0007).
struct Current {
    path: PathBuf,
    /// Visualize = look-only: every write handler refuses (ADR 0007). This
    /// supersedes the old per-selection `read_only: bool` (Phase-12 clones).
    mode: RepoMode,
    /// The opaque handle for this selection, when it registered in the catalog.
    /// `None` only in degraded mode (the path wouldn't classify as a repo), where
    /// the reads still run and surface git's own error.
    handle: Option<RepositoryHandle>,
}

static CURRENT: OnceLock<RwLock<Current>> = OnceLock::new();

fn set_current_resolved(path: PathBuf, mode: RepoMode, handle: Option<RepositoryHandle>) {
    let value = Current { path, mode, handle };
    if let Some(lock) = CURRENT.get() {
        *lock.write().expect("CURRENT lock not poisoned") = value;
    } else {
        CURRENT
            .set(RwLock::new(value))
            .unwrap_or_else(|_| unreachable!("CURRENT set once at startup"));
    }
}

/// Snapshot the current repo path and whether it is look-only. The bool keeps
/// the old `read_only` meaning (`mode == Visualize`) so the many read-handler
/// call sites stay untouched; write gating goes through [`current_mode`]/
/// [`reject_if_read_only`]. Clones out of the lock immediately so no guard is
/// ever held across an `.await`.
pub(crate) fn current() -> (PathBuf, bool) {
    let g = CURRENT
        .get()
        .expect("CURRENT is set at startup")
        .read()
        .expect("CURRENT lock not poisoned");
    (g.path.clone(), g.mode == RepoMode::Visualize)
}

/// The mode the current selection is open in (ADR 0006/0007).
pub(crate) fn current_mode() -> RepoMode {
    CURRENT
        .get()
        .expect("CURRENT is set at startup")
        .read()
        .expect("CURRENT lock not poisoned")
        .mode
}

/// The opaque handle for the current default selection, or `None` in degraded
/// mode. Used to stamp the graph with the ids the client addresses it by.
pub(crate) fn current_handle() -> Option<RepositoryHandle> {
    CURRENT
        .get()
        .expect("CURRENT is set at startup")
        .read()
        .expect("CURRENT lock not poisoned")
        .handle
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

    /// One test fn drives the CURRENT/CATALOG globals end-to-end — keeping every
    /// global mutation in a single test means parallel test threads never fight
    /// over the process-wide selection (no other test touches it).
    #[test]
    fn selection_flow_carries_mode_and_gates_writes() {
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
