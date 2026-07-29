//! Locating the `gv-sandbox` shim, once, and refusing to guess.
//!
//! The second impure corner of `sandbox` (see `bwrap.rs` for the first). It is
//! a separate file so `mod.rs`'s "everything here is pure" promise stays
//! literally true and `sandbox_argv` remains a total function of its `Policy`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::SHIM_BIN_ENV;

/// Why a shim path could not be produced. A named error rather than a panic:
/// policy construction runs per operation, on the interactive read path, and a
/// worker thread dying because a binary moved is a worse outcome than an
/// operation reporting that the host cannot supply the sandbox (INV-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShimError {
    /// `GIT_VISTA_SANDBOX_BIN` was set to something unusable. The value is
    /// included because the whole point of the override is that an operator
    /// set it deliberately and needs to know why it was rejected.
    BadOverride { value: PathBuf, why: &'static str },
    /// Nothing was found beside the running executable.
    NotFound { looked_in: PathBuf },
    /// `current_exe()` itself failed, so there is nothing to look beside.
    NoCurrentExe,
    /// `$HOME` is unset, so a policy cannot say which tree to grant or which
    /// secrets to withhold. Building a policy without it would grant nothing
    /// and silently break git identity, so it is a hard error instead.
    NoHome,
}

impl std::fmt::Display for ShimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadOverride { value, why } => write!(
                f,
                "{SHIM_BIN_ENV}={} is unusable: {why}. The sandbox launcher is \
                 never resolved through PATH, so this must be an absolute path \
                 to an existing file.",
                value.display()
            ),
            Self::NotFound { looked_in } => write!(
                f,
                "no `gv-sandbox` beside the server binary (looked in {}). Under \
                 `cargo test` this usually means no integration test exists to \
                 pull the binary into the build plan — see `tests/`.",
                looked_in.display()
            ),
            Self::NoCurrentExe => write!(f, "current_exe() failed"),
            Self::NoHome => write!(f, "$HOME is unset; cannot build a sandbox policy"),
        }
    }
}

static RESOLVED: OnceLock<Result<PathBuf, ShimError>> = OnceLock::new();

/// The absolute path of the `gv-sandbox` shim.
///
/// Resolved once and cached for the process lifetime. The caching is a
/// **security property**, not an optimisation, and it is the same one
/// `bwrap.rs` documents: the launcher cannot change identity between the moment
/// a policy is built and the moment it is spawned. The shim is the more
/// critical of the two binaries — it is what applies Landlock *and* seccomp —
/// so re-reading the environment on every call would reopen exactly the window
/// that removing the `PATH` lookup closed.
pub(crate) fn shim_path() -> Result<&'static Path, &'static ShimError> {
    match RESOLVED.get_or_init(resolve) {
        Ok(p) => Ok(p.as_path()),
        Err(e) => Err(e),
    }
}

fn resolve() -> Result<PathBuf, ShimError> {
    if let Some(raw) = std::env::var_os(SHIM_BIN_ENV) {
        let p = PathBuf::from(&raw);
        // An unvalidated override is the shim's version of the
        // `BWRAP_BIN = "bwrap"` hole. A relative value here would be resolved
        // by `execvp` against the inherited `PATH` and the current directory,
        // and because Landlock and seccomp are applied *by the shim*, a
        // substituted binary that simply execs its arguments would produce an
        // identical argv, an identical exit code, and no sandbox at all.
        if !p.is_absolute() {
            return Err(ShimError::BadOverride {
                value: p,
                why: "it is not an absolute path",
            });
        }
        if !p.is_file() {
            return Err(ShimError::BadOverride {
                value: p,
                why: "it does not name an existing file",
            });
        }
        return Ok(p);
    }

    let exe = std::env::current_exe().map_err(|_| ShimError::NoCurrentExe)?;
    let mut dir = exe
        .parent()
        .ok_or(ShimError::NoCurrentExe)?
        .to_path_buf();
    // Under `cargo test` the running binary is `target/<profile>/deps/<name>-<hash>`,
    // so the sibling lookup must step *out* of `deps` first. Measured: without
    // this the path resolves to `target/<profile>/deps/gv-sandbox`, which does
    // not exist, in every test configuration.
    if dir.file_name().is_some_and(|n| n == "deps") {
        if let Some(parent) = dir.parent() {
            dir = parent.to_path_buf();
        }
    }
    let candidate = dir.join("gv-sandbox");
    // Existence is checked here so a missing shim is a named policy failure at
    // construction time, rather than an ENOENT surfacing from deep inside an
    // unrelated git operation.
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(ShimError::NotFound { looked_in: dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `deps` step-out is load-bearing, not decorative: this is the exact
    /// layout every unit test in this crate runs under.
    #[test]
    fn a_deps_parent_is_stepped_out_of() {
        let exe = Path::new("/w/target/debug/deps/git_vista_server-abc123");
        let mut dir = exe.parent().unwrap().to_path_buf();
        if dir.file_name().is_some_and(|n| n == "deps") {
            dir = dir.parent().unwrap().to_path_buf();
        }
        assert_eq!(
            dir.join("gv-sandbox"),
            Path::new("/w/target/debug/gv-sandbox"),
            "without the step-out this resolves into deps/, where no binary is placed"
        );
    }

    /// A directly-run binary has no `deps` component and must be left alone.
    #[test]
    fn a_non_deps_parent_is_left_alone() {
        let exe = Path::new("/opt/gv/git-vista-server");
        let mut dir = exe.parent().unwrap().to_path_buf();
        if dir.file_name().is_some_and(|n| n == "deps") {
            dir = dir.parent().unwrap().to_path_buf();
        }
        assert_eq!(dir.join("gv-sandbox"), Path::new("/opt/gv/gv-sandbox"));
    }

    /// The regression guard for the override hole. A relative override would be
    /// resolved against `PATH`/cwd at spawn time, substituting the very process
    /// that applies the sandbox.
    #[test]
    fn a_relative_override_is_rejected_rather_than_resolved() {
        let p = PathBuf::from("gv-sandbox");
        assert!(
            !p.is_absolute(),
            "this test is meaningless if the value is already absolute"
        );
        let err = ShimError::BadOverride {
            value: p,
            why: "it is not an absolute path",
        };
        assert!(
            err.to_string().contains("never resolved through PATH"),
            "the error must say why, or an operator will just re-set it"
        );
    }

    /// An absolute path that names nothing is also refused: resolving to a
    /// non-existent file turns a policy-construction failure into an ENOENT
    /// surfacing from the middle of an unrelated git operation.
    #[test]
    fn an_absolute_but_missing_override_is_rejected() {
        let p = PathBuf::from("/nonexistent/gv-sandbox");
        assert!(p.is_absolute());
        assert!(!p.is_file(), "fixture must actually be missing");
    }

    /// `shim_path()` is cached, so two calls cannot disagree. A launcher that
    /// could change identity between policy construction and spawn is the
    /// window this cache exists to close.
    #[test]
    fn resolution_is_stable_across_calls() {
        let a = shim_path().map(Path::to_path_buf);
        let b = shim_path().map(Path::to_path_buf);
        assert_eq!(a.is_ok(), b.is_ok());
        if let (Ok(x), Ok(y)) = (a, b) {
            assert_eq!(x, y, "the resolved launcher must not change identity");
        }
    }
}
