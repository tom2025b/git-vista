//! Locating the `gv-sandbox-reaper` binary, once, and degrading quietly if it
//! is absent (#728).
//!
//! Unlike `shim.rs`/`bwrap.rs`, absence here is **not** a policy-construction
//! failure. The reaper is defence in depth over a process-lifetime property —
//! "if my parent dies abruptly, tear down what it started" — layered on top of
//! Landlock, seccomp and (for `Tier::Strict`) the bwrap namespaces, never a
//! substitute for any of them. A host that cannot supply it still gets the
//! exact sandbox it got before #728: no capability is lost, only the extra
//! reaping guarantee. That is why this returns `Option`, not a named error
//! that would need threading through every `Policy` constructor the way
//! `ShimError::StrictUnavailable` does for a capability INV-13 actually
//! depends on.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The absolute path of the `gv-sandbox-reaper` binary beside the running
/// executable, or `None` if it is not there.
///
/// Resolved once and cached for the process lifetime, the same reason
/// `shim::shim_path` and `bwrap::bwrap_path` cache: the launcher must not be
/// able to change identity between the moment a spawn is composed and the
/// moment it runs.
pub(crate) fn reaper_path() -> Option<&'static Path> {
    RESOLVED.get_or_init(resolve).as_deref()
}

fn resolve() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    // Under `cargo test` the running binary is `target/<profile>/deps/<name>-<hash>`,
    // so the sibling lookup must step *out* of `deps` first — the same
    // `shim_path` measurement applies here, unchanged.
    if dir.file_name().is_some_and(|n| n == "deps") {
        if let Some(parent) = dir.parent() {
            dir = parent.to_path_buf();
        }
    }
    let candidate = dir.join("gv-sandbox-reaper");
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `deps` step-out is load-bearing here for the same reason it is in
    /// `shim::shim_path`: every unit test in this crate runs under exactly this
    /// layout.
    #[test]
    fn a_deps_parent_is_stepped_out_of() {
        let exe = Path::new("/w/target/debug/deps/git_vista_server-abc123");
        let mut dir = exe.parent().unwrap().to_path_buf();
        if dir.file_name().is_some_and(|n| n == "deps") {
            dir = dir.parent().unwrap().to_path_buf();
        }
        assert_eq!(
            dir.join("gv-sandbox-reaper"),
            Path::new("/w/target/debug/gv-sandbox-reaper"),
            "without the step-out this resolves into deps/, where no binary is placed"
        );
    }

    /// Resolution is stable across calls, same invariant `shim_path` pins:
    /// a launcher that could change identity mid-process is the window the
    /// cache exists to close.
    #[test]
    fn resolution_is_stable_across_calls() {
        let a = reaper_path().map(Path::to_path_buf);
        let b = reaper_path().map(Path::to_path_buf);
        assert_eq!(a, b, "the resolved reaper must not change identity");
    }
}
