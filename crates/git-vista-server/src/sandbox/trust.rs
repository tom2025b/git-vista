//! M1.13b (#66) Task 7: the persisted per-repository sandbox trust flag.
//!
//! This module answers exactly one question — *has the operator explicitly
//! trusted this repository enough to run it with no sandbox?* — and answers it
//! **fail-closed**: every uncertainty is `false`. It is what supplies the
//! `trusted` argument to `sandbox::tier_for`, and `tier_for` returns
//! `Unsandboxed` only when that argument is `true`, so the correctness of the
//! whole no-sandbox path rests on this file never returning `true` by accident.
//!
//! # Where trust is stored, and why there
//!
//! Markers live under the server's own state directory
//! (`state::sandbox_trust_dir`, `~/.local/state/git-vista/trusted-repos`), never
//! inside a repository. The C10 audit's rule: a sandboxed repository is granted
//! `$HOME` **read-only**, so it can read this directory but cannot write a
//! marker into it — a hostile hook therefore cannot forge its own trust and
//! escalate to `Unsandboxed`. A path inside `.git/config` or the worktree,
//! which a hook *can* write, would have turned the flag into an escalation
//! path. Trust is granted only by `grant`, which is called from an explicit,
//! authenticated operator action — never derived from repository content.
//!
//! # Keyed by canonical path
//!
//! A repository is identified by its canonicalised absolute git dir. The marker
//! file is named by a hash of that path (so an arbitrary path becomes a safe
//! filename) and *contains* the canonical path verbatim, so `is_trusted`
//! re-checks the content and a hash collision cannot silently trust the wrong
//! repository.

use std::path::Path;

use crate::state::sandbox_trust_dir;

/// A stable, filesystem-safe name for a canonical repository path. FNV-1a over
/// the raw bytes — this is a filename derivation, not a security primitive (the
/// stored path is what is actually compared), so a fast non-cryptographic hash
/// is the right tool. The path is stored in the file for the real check.
fn marker_name(canonical: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.as_os_str().as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Has the operator trusted `canonical_repo` to run unsandboxed?
///
/// `canonical_repo` must already be canonicalised by the caller (the same
/// canonicalisation repositories are identified by elsewhere). Returns `false`
/// for every failure: no marker, an unreadable marker, or a marker whose stored
/// path does not match. There is no path through this function to `true` that
/// does not correspond to a marker `grant` wrote for this exact repository.
pub(crate) fn is_trusted(canonical_repo: &Path) -> bool {
    let marker = sandbox_trust_dir().join(marker_name(canonical_repo));
    let Ok(stored) = std::fs::read(&marker) else {
        return false; // no marker, or unreadable — untrusted
    };
    // The stored bytes are the canonical path verbatim. Compare as bytes so a
    // non-UTF-8 path still matches exactly, and so a hash collision (different
    // path, same filename) is caught here rather than silently trusted.
    stored == canonical_repo.as_os_str().as_encoded_bytes()
}

/// Record that the operator has trusted `canonical_repo`. Called only from an
/// explicit, authenticated operator action — never from request-handling that a
/// repository could influence.
///
/// Writing the marker is the *only* way `is_trusted` can later return `true`.
#[cfg_attr(not(test), allow(dead_code))] // wired to the operator-trust handler in a later task
pub(crate) fn grant(canonical_repo: &Path) -> std::io::Result<()> {
    let dir = sandbox_trust_dir();
    std::fs::create_dir_all(&dir)?;
    let marker = dir.join(marker_name(canonical_repo));
    std::fs::write(&marker, canonical_repo.as_os_str().as_encoded_bytes())
}

/// Remove trust from `canonical_repo` (an operator revoke). Idempotent — a
/// missing marker is success, because the desired state (not trusted) holds.
#[cfg_attr(not(test), allow(dead_code))] // wired to the operator-revoke handler in a later task
pub(crate) fn revoke(canonical_repo: &Path) -> std::io::Result<()> {
    let marker = sandbox_trust_dir().join(marker_name(canonical_repo));
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests must not touch the real trust dir. `sandbox_trust_dir` is
    /// derived from `XDG_STATE_HOME`/`HOME`; point them at a temp dir for the
    /// duration of one serialized test. Env mutation races under parallel
    /// `cargo test`, so every test that sets it runs inside this one function.
    #[test]
    fn trust_is_fail_closed_and_only_grant_can_flip_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: single-threaded within this test; no other test mutates these.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
            std::env::remove_var("HOME");
        }

        let repo = tmp.path().join("some/canonical/repo/.git");

        // 1. Absent marker → not trusted.
        assert!(!is_trusted(&repo), "a repo with no marker must not be trusted");

        // 2. After an explicit grant → trusted.
        grant(&repo).expect("grant writes a marker");
        assert!(is_trusted(&repo), "an explicitly granted repo is trusted");

        // 3. A *different* repo is still not trusted (no cross-contamination).
        let other = tmp.path().join("another/repo/.git");
        assert!(!is_trusted(&other), "granting one repo must not trust another");

        // 4. Revoke returns to untrusted.
        revoke(&repo).expect("revoke");
        assert!(!is_trusted(&repo), "a revoked repo is no longer trusted");

        // 5. Revoke is idempotent.
        revoke(&repo).expect("revoke of an already-untrusted repo is Ok");

        // 6. A marker whose *content* does not match the repo path is not
        //    trusted — the guard against a hash collision silently trusting the
        //    wrong repository.
        let dir = sandbox_trust_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(marker_name(&repo)), b"/some/other/path/.git").unwrap();
        assert!(
            !is_trusted(&repo),
            "a marker with a mismatched stored path must not confer trust"
        );

        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    /// The marker name is stable for a given path and differs across paths — the
    /// two properties the filename derivation needs.
    #[test]
    fn marker_names_are_stable_and_distinct() {
        let a = Path::new("/home/tom/projects/foo/.git");
        let b = Path::new("/home/tom/projects/bar/.git");
        assert_eq!(marker_name(a), marker_name(a), "stable for one path");
        assert_ne!(marker_name(a), marker_name(b), "distinct across paths");
    }
}
