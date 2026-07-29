//! Resolving the strict tier's `bwrap` launcher to an absolute path, once.
//!
//! This is the one impure corner of `sandbox`: it stats the filesystem. It is a
//! separate file precisely so `mod.rs`'s "everything here is pure" promise
//! stays literally true and `sandbox_argv` remains a total function of its
//! `Policy`.
//!
//! See `BWRAP_CANDIDATES` in `mod.rs` for why `PATH` is never consulted.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::BWRAP_CANDIDATES;

static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The absolute path of `bwrap` on this host, or `None` if it is not at any of
/// the reviewed locations.
///
/// Resolved on first call and cached for the process lifetime. Caching is not
/// only an optimisation: it means the launcher cannot change identity between
/// the moment a policy is built and the moment it is spawned, which is the
/// window a `PATH`-resolved bare name left open.
pub(crate) fn bwrap_path() -> Option<&'static Path> {
    RESOLVED
        .get_or_init(|| resolve(BWRAP_CANDIDATES))
        .as_deref()
}

/// Split out from `bwrap_path` so it can be tested against a candidate list
/// that is not the host's. A candidate qualifies only if it is an existing
/// **regular file** — a directory or a dangling symlink named `bwrap` must not
/// be accepted and then fail at `exec` time with a confusing error.
///
/// `is_file()` follows symlinks deliberately: a distribution shipping
/// `/bin/bwrap -> /usr/bin/bwrap` is normal and both are in the candidate list
/// anyway. What matters is that the final target exists and is a regular file.
fn resolve(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_picks_the_first_existing_regular_file() {
        // `/bin/sh` stands in for bwrap: guaranteed present, and a regular file
        // (or a symlink to one) on every host this server supports.
        let picked = resolve(&["/nonexistent/bwrap", "/bin/sh"]);
        assert_eq!(picked, Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn resolve_skips_directories() {
        // A directory named like the launcher must not be accepted: it would
        // pass a bare `exists()` check and then fail at exec with EACCES,
        // which reads like a permissions problem rather than a missing bwrap.
        assert_eq!(resolve(&["/tmp"]), None, "a directory is not a launcher");
    }

    #[test]
    fn resolve_returns_none_when_nothing_matches() {
        assert_eq!(resolve(&["/nonexistent/bwrap", "/also/missing"]), None);
    }

    /// The security property this module exists for: the candidate list is all
    /// absolute paths. A relative entry would be resolved against the process's
    /// current directory, reintroducing exactly the substitution hole that
    /// removing the `PATH` lookup closed.
    #[test]
    fn every_candidate_is_an_absolute_path() {
        for c in BWRAP_CANDIDATES {
            assert!(
                Path::new(c).is_absolute(),
                "{c} is not absolute: the launcher must never be resolved \
                 against PATH or the current directory"
            );
        }
    }
}
