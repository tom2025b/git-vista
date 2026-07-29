//! Composition rule (verdict §5): these tests drive the **whole launcher**.
//!
//! Nothing here installs a Landlock ruleset or a seccomp filter itself. Each
//! test runs the shim exactly as production runs it — through `sandbox_argv`,
//! against a real repository. A test that builds a primitive itself is a defect
//! in the test even when it passes, because it proves a layer works in
//! isolation and then credits the composition with it.
//!
//! The tests use the production policy builder and async spawn seam. Keeping a
//! second test-only policy builder or launcher here would reopen the only hole
//! in the argv-boundary tripwire.

use super::*;

/// A repository with a commit already in it, and **no local identity** — so
/// anything that needs an author must reach `~/.gitconfig` through the policy.
pub(crate) async fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    let p = d.path();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["commit", "-q", "--allow-empty", "-m", "seed"],
    ] {
        let policy = policy_for_repo(p).expect("production policy builds");
        let ok = spawn::command_async(&policy, p, &args)
            .status()
            .await
            .expect("git runs through the production seam")
            .success();
        assert!(ok, "fixture setup failed: git {args:?}");
    }
    d
}

#[tokio::test]
async fn git_status_works_through_the_composed_network_tier_launcher() {
    let repo = fixture().await;
    let policy = policy_for_repo(repo.path()).expect("production policy builds");
    let out = spawn::command_async(&policy, repo.path(), &["status", "--short"])
        .output()
        .await
        .expect("git runs through the production seam");
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The acceptance test that matters most, and the one a `git status`-only probe
/// cannot stand in for: a real commit, with no repo-local identity, reaching
/// `~/.gitconfig` through the enumerated `$HOME` grant. Nine of the
/// twenty-four repositories on the development host rely on the global config
/// for identity, so a policy that breaks this is unusable regardless of how
/// secure it is.
#[tokio::test]
async fn a_real_commit_reaches_the_global_identity_through_the_policy() {
    let repo = fixture().await;
    let policy = policy_for_repo(repo.path()).expect("production policy builds");
    std::fs::write(repo.path().join("f.txt"), "hello").expect("write");

    let out = spawn::command_async(&policy, repo.path(), &["add", "f.txt"])
        .output()
        .await
        .expect("git add runs through the production seam");
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = spawn::command_async(&policy, repo.path(), &["commit", "-q", "-m", "sandboxed"])
        .output()
        .await
        .expect("git commit runs through the production seam");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = spawn::command_async(&policy, repo.path(), &["log", "-1", "--format=%ae"])
        .output()
        .await
        .expect("git log runs through the production seam");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().contains('@'),
        "the author email must come from ~/.gitconfig through the policy, got {stdout:?}"
    );
}

/// The other half of the same policy: the identity file is readable and the
/// secrets beside it are not. Asserted in the same tier and the same fixture,
/// because "secrets denied" proves nothing if the whole policy is denying
/// everything.
#[tokio::test]
async fn secrets_stay_denied_while_the_same_policy_serves_git() {
    let repo = fixture().await;
    let policy = policy_for_repo(repo.path()).expect("production policy builds");
    let home = std::env::var("HOME").expect("HOME");

    // Liveness control: something granted must work in this same policy.
    let out = spawn::command_async(&policy, repo.path(), &["config", "--global", "--list"])
        .output()
        .await
        .expect("git config runs through the production seam");
    assert!(
        out.status.success(),
        "the granted global config must be readable: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let secret = format!("{home}/.ssh/known_hosts");
    if std::path::Path::new(&secret).exists() {
        let out = spawn::command_async(&policy, repo.path(), &["config", "-f", &secret, "--list"])
            .output()
            .await
            .expect("git config runs through the production seam");
        assert!(
            !out.status.success(),
            "~/.ssh must not be readable through the sandbox"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Permission denied"),
            "it must fail with EACCES, not for some unrelated reason: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
