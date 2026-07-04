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
// Bound on all interfaces so the iPad can reach it over the LAN.
pub(crate) const ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT);

// Upper bound on how much history to walk; plenty for now. Shared by the graph
// read (`handlers::read`) and the activity feed's remote-commit lookup.
pub(crate) const HISTORY_LIMIT: usize = 5_000;

/// The repository the server is currently serving. Mutable at runtime (Phase 12):
/// starts at the CLI-arg repo (`read_only: false`, the user's own working repo),
/// and `POST /api/clone` swaps it for a throwaway clone (`read_only: true`).
struct Current {
    path: PathBuf,
    /// True for a cloned URL: a view-only snapshot, so the write endpoints refuse.
    read_only: bool,
}

static CURRENT: OnceLock<RwLock<Current>> = OnceLock::new();

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

/// Point the server at a new repository (startup, or after a clone).
pub(crate) fn set_current(path: PathBuf, read_only: bool) {
    if let Some(lock) = CURRENT.get() {
        *lock.write().expect("CURRENT lock not poisoned") = Current { path, read_only };
    } else {
        CURRENT
            .set(RwLock::new(Current { path, read_only }))
            .unwrap_or_else(|_| unreachable!("CURRENT set once at startup"));
    }
}

/// Parent directory that holds every throwaway clone, under the OS temp dir. A
/// clone's temp dir is created here, and cleanup refuses to delete anything that
/// isn't under this root — so a bug can never `rm` a real repository.
pub(crate) fn clones_root() -> PathBuf {
    std::env::temp_dir().join("git-vista-clones")
}

/// Delete a previous clone's directory, best-effort. Guarded: only ever removes a
/// path under [`clones_root`], so it can't touch the user's own repo even if state
/// were somehow wrong.
pub(crate) fn cleanup_clone(path: &Path) {
    if path.starts_with(clones_root()) {
        if let Err(e) = std::fs::remove_dir_all(path) {
            eprintln!("git-vista: couldn't remove old clone {}: {e}", path.display());
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
