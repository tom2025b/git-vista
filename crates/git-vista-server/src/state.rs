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
use git_vista_protocol::RepositoryDescriptor;

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

/// Loopback is the safe default. The launcher sets an explicit address only for
/// its opt-in, session-protected personal-LAN compatibility mode.
pub(crate) fn bind_addr() -> Result<SocketAddr, String> {
    match std::env::var("GIT_VISTA_BIND_ADDR") {
        Ok(value) => parse_bind_addr(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_bind_addr(None),
        Err(error) => Err(format!("could not read GIT_VISTA_BIND_ADDR: {error}")),
    }
}

fn parse_bind_addr(value: Option<&str>) -> Result<SocketAddr, String> {
    match value {
        Some(value) => value
            .parse()
            .map_err(|error| format!("invalid GIT_VISTA_BIND_ADDR '{value}': {error}")),
        None => Ok(LOOPBACK_ADDR),
    }
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

/// The capability view of the catalog for `GET /api/catalog`: the servable
/// repositories addressed by opaque id, with absolute paths included only when
/// the operator opted in ([`expose_paths`]).
pub(crate) fn catalog_descriptors() -> Vec<RepositoryDescriptor> {
    catalog()
        .read()
        .expect("catalog lock")
        .descriptors(expose_paths())
}

/// The repository the server is currently serving *by default* — the selection a
/// request with no explicit `?repo=` id acts on. Mutable at runtime (Phase 12):
/// starts at the CLI-arg repo (`read_only: false`, the user's own working repo),
/// and `POST /api/clone` swaps it for a throwaway clone (`read_only: true`).
struct Current {
    path: PathBuf,
    /// True for a cloned URL: a view-only snapshot, so the write endpoints refuse.
    read_only: bool,
    /// The opaque handle for this selection, when it registered in the catalog.
    /// `None` only in degraded mode (the path wouldn't classify as a repo), where
    /// the reads still run and surface git's own error.
    handle: Option<RepositoryHandle>,
}

static CURRENT: OnceLock<RwLock<Current>> = OnceLock::new();

fn set_current_resolved(path: PathBuf, read_only: bool, handle: Option<RepositoryHandle>) {
    let value = Current {
        path,
        read_only,
        handle,
    };
    if let Some(lock) = CURRENT.get() {
        *lock.write().expect("CURRENT lock not poisoned") = value;
    } else {
        CURRENT
            .set(RwLock::new(value))
            .unwrap_or_else(|_| unreachable!("CURRENT set once at startup"));
    }
}

/// Snapshot the current repo path and its read-only flag. Clones out of the lock
/// immediately so no guard is ever held across an `.await`.
pub(crate) fn current() -> (PathBuf, bool) {
    let g = CURRENT
        .get()
        .expect("CURRENT is set at startup")
        .read()
        .expect("CURRENT lock not poisoned");
    (g.path.clone(), g.read_only)
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
pub(crate) fn set_current(path: &Path, read_only: bool) {
    let registered = {
        let mut c = catalog().write().expect("catalog lock");
        // Trusted selection: allow its own root so a repo launched from anywhere
        // (or a fresh clone under the clones root) can register.
        if let Ok(facts) = git_vista_git::read_repo_facts(path) {
            c.allow_root(&facts.root);
        }
        c.register(path, read_only)
    };
    match registered {
        Ok(handle) => {
            // Use the catalog's canonical path for the selection, so `current()`
            // and an explicit `?repo=<this id>` resolve to the very same path.
            let (path, read_only) = match resolve_worktree(handle.worktree) {
                Some((canonical, ro, _)) => (canonical, ro),
                None => (path.to_path_buf(), read_only),
            };
            set_current_resolved(path, read_only, Some(handle));
        }
        Err(e) => {
            eprintln!(
                "git-vista: serving {} in degraded mode ({e}); \
                 /api/* reads will surface git's own error",
                path.display()
            );
            set_current_resolved(path.to_path_buf(), read_only, None);
        }
    }
}

/// Parent directory that holds every throwaway clone, under the OS temp dir. A
/// clone's temp dir is created here, and cleanup refuses to delete anything that
/// isn't under this root — so a bug can never `rm` a real repository.
pub(crate) fn clones_root() -> PathBuf {
    std::env::temp_dir().join("git-vista-clones")
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

/// Guard for the write endpoints (Phase 12): when the current repo is a read-only
/// clone, refuse the operation with `403` and a clear reason, since any change
/// would be thrown away with the clone. Returns `None` when writes are allowed.
pub(crate) fn reject_if_read_only() -> Option<(StatusCode, String)> {
    if current().1 {
        Some((
            StatusCode::FORBIDDEN,
            "This repository is a read-only clone opened from a URL. Open your own \
             repo to make changes."
                .to_string(),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_bind_addr, LOOPBACK_ADDR};

    #[test]
    fn bind_address_defaults_to_loopback() {
        assert_eq!(parse_bind_addr(None).unwrap(), LOOPBACK_ADDR);
    }

    #[test]
    fn bind_address_accepts_an_explicit_lan_listener() {
        assert_eq!(
            parse_bind_addr(Some("0.0.0.0:8080")).unwrap(),
            "0.0.0.0:8080".parse().unwrap()
        );
    }

    #[test]
    fn bind_address_rejects_invalid_configuration() {
        let error = parse_bind_addr(Some("not-an-address")).unwrap_err();
        assert!(error.contains("invalid GIT_VISTA_BIND_ADDR"));
    }
}
