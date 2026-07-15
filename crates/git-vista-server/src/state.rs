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
pub(crate) const LOOPBACK_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT);

/// Loopback is the safe default. The launcher sets an explicit address only for
/// its opt-in, unauthenticated personal-LAN compatibility mode.
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
