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

/// Build through the production policy seam, unmodified — the **Strict** tier,
/// because every command these tests run (`init`, `commit`, `status`, `config`)
/// is a local operation, and after Task 8's dispatch a local operation on an
/// untrusted repository is exactly what `Tier::Strict` means.
///
/// This is deliberately `policy_for(.., NetworkNeed::Local)` rather than the
/// one-argument `policy_for_repo`, which now declares `Remote` for the escape
/// battery's Network-tier cases (see its doc comment). Routing these tests
/// through `Local` is the point: they are the "drive it exactly like production
/// does" suite, and after Task 8 production drives `git commit` through bwrap's
/// namespaces. Before Task 8 they ran in the Network tier and proved nothing
/// about the tier real mutations now use.
///
/// This used to carry a retry loop that re-set `HOME` because `trust::tests`
/// removed it process-wide and never restored it. That disease is cured at the
/// source — the trust tests now take their directory explicitly and touch no
/// environment at all — and the repair is deliberately NOT kept as insurance:
/// if some future test poisons the environment again, the right outcome is a
/// loud `NoHome` failure naming the problem, not a helper that silently
/// launders it while every other `$HOME` reader in the suite still races.
pub(crate) fn production_policy(repo: &std::path::Path) -> Policy {
    match policy_for(repo, false, NetworkNeed::Local) {
        Ok(policy) => policy,
        Err(error) => panic!("production policy builds: {error}"),
    }
}

/// A repository with a commit already in it, and **no local identity** — so
/// anything that needs an author must reach `~/.gitconfig` through the policy.
pub(crate) async fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    let p = d.path();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["commit", "-q", "--allow-empty", "-m", "seed"],
    ] {
        let policy = production_policy(p);
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
    let policy = production_policy(repo.path());
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
    let policy = production_policy(repo.path());
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

/// A policy entry naming a **regular file** must actually grant that file.
///
/// This is the regression test for the silent file-grant no-op. The shim's
/// non-enumerated fast path handed `landlock_add_rule` the directory-only right
/// `READ_DIR` for every `--ro`/`--rw` entry regardless of what the entry named;
/// the kernel answered `EINVAL` for a regular file, the shim mapped that to
/// `false`, discarded it, and reported `0 granted` with no diagnostic. So a
/// policy naming a file was indistinguishable — in behaviour and in output —
/// from a policy naming nothing, which is how a *weaker* sandbox comes to look
/// like a configured one.
///
/// Both legs matter and neither is redundant. The control leg proves the file is
/// unreachable without the grant (otherwise the second leg would pass on a
/// sandbox that grants everything); the granted leg proves the same file is
/// readable with it. `production_policy` builds both, and the only difference
/// between them is one pushed `ro_trees` entry, so nothing else can account for
/// the change in outcome.
#[tokio::test]
async fn a_read_only_grant_naming_a_regular_file_is_honoured() {
    let repo = fixture().await;
    // Deliberately outside every default tree and outside the repository: the
    // only thing that can make this readable is the grant under test.
    let outside = tempfile::tempdir().expect("tempdir");
    let file = outside.path().join("granted.cfg");
    std::fs::write(&file, "[gv]\n\tmarker = present\n").expect("write the granted file");
    let arg = file.to_string_lossy().to_string();
    let read_it = ["config", "-f", arg.as_str(), "--list"];

    let ungranted = production_policy(repo.path());
    let out = spawn::command_async(&ungranted, repo.path(), &read_it)
        .output()
        .await
        .expect("git runs through the production seam");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "control leg: an ungranted file must not be readable, or this test proves nothing"
    );
    assert!(
        stderr.contains("Permission denied"),
        "control leg must fail with EACCES, not for some unrelated reason: {stderr}"
    );

    let mut granted = production_policy(repo.path());
    granted.ro_trees.push(file.clone());
    let out = spawn::command_async(&granted, repo.path(), &read_it)
        .output()
        .await
        .expect("git runs through the production seam");
    assert!(
        out.status.success(),
        "a `--ro <regular file>` grant must be honoured: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "gv.marker=present",
        "the granted file must be readable through the sandbox"
    );
}

/// The other half of the same policy: the identity file is readable and the
/// secrets beside it are not. Asserted in the same tier and the same fixture,
/// because "secrets denied" proves nothing if the whole policy is denying
/// everything.
#[tokio::test]
async fn secrets_stay_denied_while_the_same_policy_serves_git() {
    let repo = fixture().await;
    let policy = production_policy(repo.path());
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

    // Asserted, not skipped — see the identical note in `sandbox::spawn`'s
    // `the_production_policy_runs_real_git_and_denies_secrets`. Guarding this
    // block with `if Path::new(&secret).exists()` silently deleted the whole
    // secret-denial check on any host lacking the file, which an adversarial
    // review reproduced under a runner-shaped `$HOME`. A missing premise is a
    // hard failure here, exactly as it is inside the escape battery.
    let secret = format!("{home}/.ssh/known_hosts");
    assert!(
        std::path::Path::new(&secret).exists(),
        "{secret} does not exist, so this test cannot show the sandbox denies it — the read \
         would fail because the path is absent, not because the policy refused, and the \
         EACCES assertion below would be checking the wrong thing. Any non-empty \
         owner-readable file will do; CI writes a placeholder in \
         .github/actions/host-sandbox-setup."
    );
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
