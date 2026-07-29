//! Composition rule (verdict §5): these tests drive the **whole launcher**.
//!
//! Nothing here installs a Landlock ruleset or a seccomp filter itself. Each
//! test runs the shim exactly as production runs it — through `sandbox_argv`,
//! against a real repository. A test that builds a primitive itself is a defect
//! in the test even when it passes, because it proves a layer works in
//! isolation and then credits the composition with it.
//!
//! # Why the shim path is a parameter and not an environment variable
//!
//! `Policy::shim` is filled from an explicit argument here rather than by
//! setting `GIT_VISTA_SANDBOX_BIN`. Measured: `std::env::set_var` races under
//! `cargo test`'s default multi-threaded execution — three of four tests failed
//! when independent tests set the same variable — and serialising only the
//! *env-mutating* tests does not help, because every other test that builds a
//! policy reads the same process-global while they run. Only
//! `--test-threads=1` makes env mutation sound, and that penalises the whole
//! suite. Passing the path is free and has no shared state at all.
//!
//! `SHIM_BIN_ENV` remains the production override for a packaged install where
//! the shim does not sit beside the server binary; it is simply not the
//! mechanism tests use.

use super::*;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Everything a sandboxed git needs on this host, plus the repository.
///
/// `/dev` is read-write because it is the only configuration a real `git
/// commit` was measured to succeed under — `/dev/urandom` in particular, which
/// a `git status`-only probe never exercises and which was missing from the
/// original grant list.
pub(crate) fn workable(tier: Tier, repo: &Path, shim: &Path) -> Policy {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let (mut rw, mut ro) = default_system_trees(tier);
    rw.push(repo.to_path_buf());
    ro.push(home.clone());
    Policy {
        tier,
        shim: shim.to_path_buf(),
        bwrap: if tier == Tier::Strict {
            bwrap::bwrap_path().map(Path::to_path_buf)
        } else {
            None
        },
        rw_trees: rw,
        ro_trees: ro,
        secret_excludes: secret_excludes_for_home(&home),
        net_ports: if tier == Tier::Network {
            DEFAULT_GIT_PORTS.to_vec()
        } else {
            Vec::new()
        },
        hook_mode: HookMode::Run,
    }
}

/// Can this host actually run the strict tier?
///
/// Strict needs a resolved `bwrap` — `shim_argv` requires it and a `Policy`
/// without one is a programming error, not a runtime condition. Gating on this
/// keeps a host with no bwrap reporting "skipped" rather than panicking inside
/// the shared fixture that Tasks 4, 10 and 13 all build on.
pub(crate) fn strict_available() -> bool {
    bwrap::bwrap_path().is_some()
}

/// The path of the shim under test. Resolved through production code, so a
/// broken resolver fails the suite rather than being papered over by a
/// test-only shortcut.
pub(crate) fn shim() -> PathBuf {
    shim::shim_path()
        .expect("gv-sandbox must be built; tests/forces_shim_build.rs exists to ensure it")
        .to_path_buf()
}

/// Run the composed launcher and return `(exit code, stdout, stderr)`.
///
/// The environment is stripped to `PATH` and `HOME` deliberately: it stops a
/// caller's `GIT_AUTHOR_*`, `GIT_CONFIG_*` or `GIT_DIR` leaking in and
/// silently supplying something the sandbox was supposed to be the only source
/// of. Several acceptance claims depend on identity coming from `~/.gitconfig`
/// and nowhere else.
pub(crate) fn launch(policy: &Policy, repo: &Path, args: &[&str]) -> (i32, String, String) {
    let argv = sandbox_argv(policy);
    let home = std::env::var("HOME").expect("HOME is set");
    // The one non-literal `Command::new` in this crate. See the launcher-site
    // carve-out in `argv_boundary.rs`: the program is `policy.shim`, an
    // absolute path this crate resolved, never a name from the environment.
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home);
    let out = cmd.output().expect("the launcher runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A repository with a commit already in it, and **no local identity** — so
/// anything that needs an author must reach `~/.gitconfig` through the policy.
pub(crate) fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    let p = d.path();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["commit", "-q", "--allow-empty", "-m", "seed"],
    ] {
        let ok = Command::new("git")
            .args(&args)
            .current_dir(p)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "fixture setup failed: git {args:?}");
    }
    d
}

#[test]
fn the_shim_refuses_an_argv_it_does_not_recognise() {
    let out = Command::new(shim())
        .args(["--pwn", "--", "git", "status"])
        .output()
        .expect("shim runs");
    assert_eq!(
        out.status.code(),
        Some(90),
        "an unknown flag must be a hard argv error, never ignored"
    );
}

#[test]
fn the_shim_refuses_to_exec_anything_but_git() {
    let out = Command::new(shim())
        .args([
            "--abi-floor", "6", "--hooks-run", "--net-deny", "--", "sh", "-c", "id",
        ])
        .output()
        .expect("shim runs");
    assert_eq!(out.status.code(), Some(90));
}

/// C5: fail closed. A floor no kernel can meet asserts the refusal path without
/// depending on the host being old.
#[test]
fn a_floor_the_kernel_cannot_meet_is_a_loud_refusal_not_a_downgrade() {
    let out = Command::new(shim())
        .args([
            "--abi-floor", "999", "--hooks-run", "--net-deny", "--", "git", "--version",
        ])
        .output()
        .expect("shim runs");
    assert_eq!(
        out.status.code(),
        Some(91),
        "below-floor must exit 91, never run git under a weaker policy"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("Landlock ABI"));
}

/// A relative exclude matches nothing, and a secret set that matches nothing is
/// an empty secret set — `~/.ssh` readable with nothing to signal it.
#[test]
fn a_relative_exclude_is_refused_rather_than_silently_matching_nothing() {
    let out = Command::new(shim())
        .args([
            "--abi-floor", "6", "--hooks-run", "--net-deny", "--exclude", ".ssh", "--", "git",
            "--version",
        ])
        .output()
        .expect("shim runs");
    assert_eq!(out.status.code(), Some(90));
}

#[test]
fn the_abi_floor_is_required_and_never_defaults() {
    let out = Command::new(shim())
        .args(["--hooks-run", "--net-deny", "--", "git", "--version"])
        .output()
        .expect("shim runs");
    assert_eq!(
        out.status.code(),
        Some(90),
        "C5: the floor travels in the argv; a default nobody can see is not a policy"
    );
}

#[test]
fn git_status_works_through_the_composed_network_tier_launcher() {
    let repo = fixture();
    let s = shim();
    let (code, out, err) = launch(
        &workable(Tier::Network, repo.path(), &s),
        repo.path(),
        &["status", "--short"],
    );
    assert_eq!(code, 0, "stdout={out} stderr={err}");
}

/// The acceptance test that matters most, and the one a `git status`-only probe
/// cannot stand in for: a real commit, with no repo-local identity, reaching
/// `~/.gitconfig` through the enumerated `$HOME` grant. Nine of the
/// twenty-four repositories on the development host rely on the global config
/// for identity, so a policy that breaks this is unusable regardless of how
/// secure it is.
#[test]
fn a_real_commit_reaches_the_global_identity_through_the_policy() {
    let repo = fixture();
    let s = shim();
    let p = workable(Tier::Network, repo.path(), &s);
    std::fs::write(repo.path().join("f.txt"), "hello").expect("write");

    let (code, _, err) = launch(&p, repo.path(), &["add", "f.txt"]);
    assert_eq!(code, 0, "git add failed: {err}");
    let (code, _, err) = launch(&p, repo.path(), &["commit", "-q", "-m", "sandboxed"]);
    assert_eq!(code, 0, "git commit failed: {err}");

    let (code, out, _) = launch(&p, repo.path(), &["log", "-1", "--format=%ae"]);
    assert_eq!(code, 0);
    assert!(
        out.trim().contains('@'),
        "the author email must come from ~/.gitconfig through the policy, got {out:?}"
    );
}

/// The other half of the same policy: the identity file is readable and the
/// secrets beside it are not. Asserted in the same tier and the same fixture,
/// because "secrets denied" proves nothing if the whole policy is denying
/// everything.
#[test]
fn secrets_stay_denied_while_the_same_policy_serves_git() {
    let repo = fixture();
    let s = shim();
    let p = workable(Tier::Network, repo.path(), &s);
    let home = std::env::var("HOME").expect("HOME");

    // Liveness control: something granted must work in this same policy.
    let (code, _, err) = launch(&p, repo.path(), &["config", "--global", "--list"]);
    assert_eq!(code, 0, "the granted global config must be readable: {err}");

    let secret = format!("{home}/.ssh/known_hosts");
    if Path::new(&secret).exists() {
        let (code, _, err) = launch(&p, repo.path(), &["config", "-f", &secret, "--list"]);
        assert_ne!(code, 0, "~/.ssh must not be readable through the sandbox");
        assert!(
            err.contains("Permission denied"),
            "it must fail with EACCES, not for some unrelated reason: {err}"
        );
    }
}
