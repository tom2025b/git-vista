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
    is_trusted_in(&sandbox_trust_dir(), canonical_repo)
}

/// The whole implementation, with the trust directory explicit. The public
/// functions bind it to `sandbox_trust_dir()`; tests bind it to a temp dir.
/// This split exists so tests never have to redirect `XDG_STATE_HOME`/`HOME`
/// through the process environment — a previous version did, leaked the
/// mutation, and intermittently killed every parallel test that reads `$HOME`
/// (the whole escape battery among them).
fn is_trusted_in(trust_dir: &Path, canonical_repo: &Path) -> bool {
    let marker = trust_dir.join(marker_name(canonical_repo));
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
    grant_in(&sandbox_trust_dir(), canonical_repo)
}

fn grant_in(trust_dir: &Path, canonical_repo: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(trust_dir)?;
    let marker = trust_dir.join(marker_name(canonical_repo));
    std::fs::write(&marker, canonical_repo.as_os_str().as_encoded_bytes())
}

/// Remove trust from `canonical_repo` (an operator revoke). Idempotent — a
/// missing marker is success, because the desired state (not trusted) holds.
#[cfg_attr(not(test), allow(dead_code))] // wired to the operator-revoke handler in a later task
pub(crate) fn revoke(canonical_repo: &Path) -> std::io::Result<()> {
    revoke_in(&sandbox_trust_dir(), canonical_repo)
}

fn revoke_in(trust_dir: &Path, canonical_repo: &Path) -> std::io::Result<()> {
    let marker = trust_dir.join(marker_name(canonical_repo));
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests must not touch the real trust dir, and they must not redirect
    /// it through `XDG_STATE_HOME`/`HOME` either: process-environment mutation
    /// under parallel `cargo test` is a suite-wide race, and a previous
    /// version of this test proved it — its leaked `remove_var("HOME")`
    /// intermittently killed every concurrently-running test that reads
    /// `$HOME` (the escape battery among them). The `*_in` functions take the
    /// directory explicitly, so nothing here touches the environment at all.
    #[test]
    fn trust_is_fail_closed_and_only_grant_can_flip_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("trusted-repos");

        let repo = tmp.path().join("some/canonical/repo/.git");

        // 1. Absent marker → not trusted.
        assert!(
            !is_trusted_in(&dir, &repo),
            "a repo with no marker must not be trusted"
        );

        // 2. After an explicit grant → trusted.
        grant_in(&dir, &repo).expect("grant writes a marker");
        assert!(
            is_trusted_in(&dir, &repo),
            "an explicitly granted repo is trusted"
        );

        // 3. A *different* repo is still not trusted (no cross-contamination).
        let other = tmp.path().join("another/repo/.git");
        assert!(
            !is_trusted_in(&dir, &other),
            "granting one repo must not trust another"
        );

        // 4. Revoke returns to untrusted.
        revoke_in(&dir, &repo).expect("revoke");
        assert!(
            !is_trusted_in(&dir, &repo),
            "a revoked repo is no longer trusted"
        );

        // 5. Revoke is idempotent.
        revoke_in(&dir, &repo).expect("revoke of an already-untrusted repo is Ok");

        // 6. A marker whose *content* does not match the repo path is not
        //    trusted — the guard against a hash collision silently trusting the
        //    wrong repository.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(marker_name(&repo)), b"/some/other/path/.git").unwrap();
        assert!(
            !is_trusted_in(&dir, &repo),
            "a marker with a mismatched stored path must not confer trust"
        );
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

    /// The escalation this module's own doc claims is impossible, pinned.
    ///
    /// A marker is only unforgeable if a sandboxed repository cannot *write*
    /// one. That rests on the trust store being outside every grant — but the
    /// served repository is granted read-write, and nothing prevents an
    /// operator from serving a path that contains the state directory (or from
    /// pointing `XDG_STATE_HOME` inside a served tree). When that happens the
    /// grant covers the store, a hostile hook can write its own marker, and the
    /// *next* operation resolves `trusted = true` and lands on
    /// `Tier::Unsandboxed` — a total bypass reached entirely through
    /// sanctioned paths.
    ///
    /// The closing mechanism is the shim's exclude set, which outranks grants
    /// rather than competing with them, so this asserts the trust directory is
    /// actually in `secret_excludes` for a policy built over a repository that
    /// contains it. Asserting on the constructed `Policy` (rather than on a
    /// list of literals) is deliberate: the store's location follows
    /// `XDG_STATE_HOME`, so a test that hard-codes `$HOME/.local/state/…`
    /// would pass while protecting nothing on a host that sets it.
    #[test]
    fn the_trust_store_is_withheld_even_from_a_repo_that_contains_it() {
        let trust_dir = crate::state::sandbox_trust_dir();
        let home = std::env::var_os("HOME").expect("HOME set in tests");

        // Serve the state directory's own parent — the shape that turns the
        // repo's read-write grant into cover for the trust store.
        let served = trust_dir
            .parent()
            .expect("the trust dir has a parent")
            .to_path_buf();

        let policy = super::super::policy_for(
            &served,
            false, // writable: the grant that creates the hazard
            super::super::NetworkNeed::Local,
        )
        .expect("policy builds for a served path");

        assert!(
            policy.secret_excludes.contains(&trust_dir),
            "the trust store {} must be withheld from every grant, but the \
             policy's excludes were {:?}. Without it, a hostile hook in a repo \
             whose grant covers the store can forge its own marker and escalate \
             the next operation to Tier::Unsandboxed.",
            trust_dir.display(),
            policy.secret_excludes
        );
        // And the hazard this guards is real: the served tree really is granted
        // read-write, so the exclude is the only thing standing in the way.
        assert!(
            policy.rw_trees.contains(&served),
            "precondition: the served path is granted read-write, or this test \
             is not exercising the escalation it claims to"
        );
        let _ = home;
    }
}
