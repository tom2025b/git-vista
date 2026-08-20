//! Recovery refs (M1.09, #62; M2.21a #235 and M3.24 #77 extended the object
//! kinds pinned): [`recovery_oid`] naming the right oid for every
//! `RecoveryStrategy` variant that carries one, and the end-to-end write/read
//! path — a real ref written under `refs/git-vista/recovery/`, proven never
//! to touch the working branch of the same name, and proven absent for a
//! strategy that names none. Extracted verbatim from `durable.rs`'s inline
//! `mod tests` (a `#[cfg(test)]` child module) so the parent file can be read
//! as production code — see its module doc comment, item 2 ("Recovery
//! refs"), for the subsystem these exercise. A child module of `durable`, so
//! it still reaches `durable.rs`'s private items (`recovery_oid`,
//! `write_recovery_ref`, `read_recovery_ref`) through `super::`.
//! `read_recovery_ref` itself stays in `durable.rs` rather than moving here —
//! see the extraction report for why. The journal and redaction tests that
//! shared this `mod tests` block but exercise different subsystems live
//! separately in `journal_suite.rs` and `redaction_suite.rs`.

use super::*;
use git_vista_protocol::{BranchName, RefName};

#[test]
fn recovery_oid_is_present_only_for_strategies_that_name_one() {
    let with = RecoveryStrategy::ResetRef {
        ref_name: RefName::new("refs/heads/main").unwrap(),
        to: CommitOid::new("c".repeat(40)).unwrap(),
    };
    assert!(recovery_oid(&with).is_some());
    // M2.21a (#235): `RecreateTag` names the pre-delete ref value, and
    // the pin *must* exist — it is what keeps a deleted annotated tag's
    // dangling tag object alive against gc (see recovery_oid's comment).
    let recreate_tag = RecoveryStrategy::RecreateTag {
        name: git_vista_protocol::TagName::new("v1.0.0").unwrap(),
        at: CommitOid::new("d".repeat(40)).unwrap(),
    };
    assert_eq!(
        recovery_oid(&recreate_tag).map(CommitOid::as_str),
        Some("d".repeat(40).as_str()),
        "RecreateTag's pin must be the carried pre-delete oid itself"
    );

    for without in [
        RecoveryStrategy::NotNeeded,
        RecoveryStrategy::DeleteCreatedBranch {
            name: BranchName::new("x").unwrap(),
        },
        RecoveryStrategy::DeleteCreatedTag {
            name: git_vista_protocol::TagName::new("v1.0.0").unwrap(),
        },
        RecoveryStrategy::CheckoutPrevious {
            branch: BranchName::new("x").unwrap(),
        },
        RecoveryStrategy::Irrecoverable,
    ] {
        assert!(recovery_oid(&without).is_none());
    }
}

/// The end-to-end recovery-ref path: write one, read it back, and confirm
/// the branch of the same working name is untouched — the namespace
/// prefix is what makes "never overwrites a user ref" true.
#[tokio::test]
async fn a_recovery_ref_is_written_and_never_touches_the_user_ref_it_pins() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let run = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@example.invalid"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run(&["add", "a.txt"]);
    run(&["commit", "-q", "-m", "seed"]);
    let before = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let before_oid = String::from_utf8_lossy(&before.stdout).trim().to_string();

    // A second commit, so refs/heads/main has since moved — the case a
    // recovery ref exists to answer "what was it before".
    std::fs::write(repo.join("a.txt"), "b\n").unwrap();
    run(&["commit", "-qam", "second"]);

    let id = OperationId::new("recovery-ref-test").unwrap();
    let recovery = RecoveryStrategy::ResetRef {
        ref_name: RefName::new("refs/heads/main").unwrap(),
        to: CommitOid::new(before_oid.clone()).unwrap(),
    };
    write_recovery_ref(&repo, &id, &recovery).await;

    let read = read_recovery_ref(&repo, &id).await;
    assert_eq!(read.as_deref(), Some(before_oid.as_str()));

    let heads_main = std::process::Command::new("git")
        .args(["rev-parse", "refs/heads/main"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_ne!(
        String::from_utf8_lossy(&heads_main.stdout).trim(),
        before_oid,
        "refs/heads/main must still be the SECOND commit — the recovery ref \
         pins the old tip without moving the real branch"
    );
}

#[tokio::test]
async fn strategies_with_no_oid_write_no_ref() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());

    let id = OperationId::new("no-oid-test").unwrap();
    write_recovery_ref(&repo, &id, &RecoveryStrategy::NotNeeded).await;
    assert_eq!(read_recovery_ref(&repo, &id).await, None);
}
