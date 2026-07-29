//! M1.13b (#66) Task 5: the two spawn wrappers that are the *only* way the
//! server starts a git process.
//!
//! Everything above this in `sandbox` is pure — it produces argv. This is where
//! that argv becomes a real `Command`, and it is deliberately the single
//! chokepoint: `argv_boundary.rs` proves no other file in the crate constructs
//! a git `Command` outside the allowlist, and Task 6 migrates the existing
//! spawn sites onto these two functions so that proof means "every git the
//! server runs is sandboxed."
//!
//! # Why two functions and not one
//!
//! The server runs git both ways: `git_stdout_capped` streams a child's stdout
//! under a cap on the async runtime, and a handful of helpers (`rev_parse`,
//! `is_ancestor`) want a simple blocking `output()`. Both must go through the
//! same policy, so the sandboxing cannot live in either call style — it lives
//! here, in `configure`, which both wrappers share. Neither needs a
//! `pre_exec` closure or a `block_on`, because the sandbox is *argv*: the shim
//! applies Landlock and seccomp in its own process, after this one has already
//! exec'd it.

use std::path::Path;

use super::{sandbox_argv, Policy};

/// Build the full argv for `git -C <repo> <args…>` under `policy`.
///
/// Split out from both wrappers so the argv they will run is testable without
/// spawning anything, and so the two wrappers cannot drift apart in how they
/// assemble it.
fn full_argv(policy: &Policy, repo: &Path, args: &[&str]) -> Vec<std::ffi::OsString> {
    let mut argv = sandbox_argv(policy);
    argv.push(std::ffi::OsString::from("-C"));
    argv.push(repo.as_os_str().to_os_string());
    for a in args {
        argv.push(std::ffi::OsString::from(*a));
    }
    argv
}

/// Configure a `std`-shaped command from an argv. The environment is not
/// touched here: the server's own environment is what git should see, minus
/// nothing — the sandbox is the boundary, not an env scrub. (Tests that need a
/// stripped environment do it themselves; production wants the real one so
/// `GIT_*` operational variables the server sets still reach git.)
fn split(argv: &[std::ffi::OsString]) -> (&std::ffi::OsString, &[std::ffi::OsString]) {
    (&argv[0], &argv[1..])
}

/// A composed launcher whose argv is **final**.
///
/// This is Task 5's half of C10 hazard #1. `command_async` used to hand back a
/// bare `tokio::process::Command`, and the crate's only production caller —
/// `git_cmd::sandboxed()` — built it with an *empty* arg slice and let each
/// caller append the real subcommand afterward with `.args(…)`. Whatever
/// `sandbox_argv` classified, it classified an argv the process never ran.
///
/// Threading the real args into `sandboxed(repo, args)` does not close that on
/// its own: a `-> Command` return still lets a caller append more. So the argv
/// is sealed by the *type* instead. There is deliberately no `arg`, no `args`
/// and no `env` here — only stdio configuration, which cannot change what runs.
/// `env` is excluded for the same reason as `arg`: `GIT_DIR`, `GIT_SSH_COMMAND`
/// and `GIT_EXTERNAL_DIFF` redirect or execute, so an environment appended
/// after classification is an argv change wearing a different hat.
///
/// The setters consume and return `Self` so a call site still reads as one
/// chain ending in `output()`/`spawn()`.
pub(crate) struct SandboxedCommand(tokio::process::Command);

impl SandboxedCommand {
    pub(crate) fn stdin(mut self, cfg: impl Into<std::process::Stdio>) -> Self {
        self.0.stdin(cfg);
        self
    }

    pub(crate) fn stdout(mut self, cfg: impl Into<std::process::Stdio>) -> Self {
        self.0.stdout(cfg);
        self
    }

    pub(crate) fn stderr(mut self, cfg: impl Into<std::process::Stdio>) -> Self {
        self.0.stderr(cfg);
        self
    }

    pub(crate) fn kill_on_drop(mut self, kill: bool) -> Self {
        self.0.kill_on_drop(kill);
        self
    }

    pub(crate) async fn output(mut self) -> std::io::Result<std::process::Output> {
        self.0.output().await
    }

    pub(crate) fn spawn(mut self) -> std::io::Result<tokio::process::Child> {
        self.0.spawn()
    }

    /// Test-only: run with a stripped environment so a fixture's result does not
    /// depend on the developer's shell. Gated to `#[cfg(test)]` precisely so the
    /// production surface above stays free of environment control — see the type
    /// doc for why `env` is a hazard, not a convenience.
    #[cfg(test)]
    pub(crate) fn hermetic_env_for_test(mut self) -> Self {
        self.0
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", std::env::var("HOME").expect("HOME set in tests"));
        self
    }
}

/// The async wrapper: a [`SandboxedCommand`] whose argv is already complete.
/// Pipes and `kill_on_drop` are left to the caller, because the two async call
/// sites want different shapes (a capped stream vs a simple output) and both
/// are legitimate — but neither may touch the argv.
pub(crate) fn command_async(policy: &Policy, repo: &Path, args: &[&str]) -> SandboxedCommand {
    let argv = full_argv(policy, repo, args);
    let (program, rest) = split(&argv);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest);
    SandboxedCommand(cmd)
}

/// The sync wrapper, for the `#[cfg(test)]` fixture builders and any blocking
/// helper. Same argv, same policy, `std::process::Command`.
#[cfg_attr(not(test), allow(dead_code))] // Task 6 wires the production blocking callers.
pub(crate) fn command_sync(policy: &Policy, repo: &Path, args: &[&str]) -> std::process::Command {
    let argv = full_argv(policy, repo, args);
    let (program, rest) = split(&argv);
    let mut cmd = std::process::Command::new(program);
    cmd.args(rest);
    cmd
}

#[cfg(test)]
mod tests {
    use super::super::shim_cli::{fixture, production_policy};
    use super::*;

    /// The wrapper's argv is exactly the sandbox argv with `-C <repo> <args>`
    /// appended — no more, no less. If this drifts, a spawn site is no longer
    /// running the reviewed launcher.
    #[test]
    fn the_wrapper_argv_is_the_sandbox_argv_plus_the_repo_and_args() {
        let repo = std::path::PathBuf::from("/srv/repo");
        let policy = production_policy(&repo);
        let argv = full_argv(&policy, &repo, &["status", "--short"]);

        // ends with the appended tail
        let tail: Vec<String> = argv
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(|o| o.to_string_lossy().into_owned())
            .collect();
        assert_eq!(tail, vec!["-C", "/srv/repo", "status", "--short"]);

        // begins with the pure sandbox argv
        let pure = sandbox_argv(&policy);
        assert_eq!(
            &argv[..pure.len()],
            &pure[..],
            "the launcher prefix drifted"
        );
    }

    /// The composition test: a real git actually runs through the async wrapper
    /// under a real policy. This is what makes the wrapper more than argv
    /// assembly — it proves the process the server will spawn works.
    #[tokio::test]
    async fn a_real_git_runs_through_the_async_wrapper() {
        let repo = fixture().await;
        let policy = production_policy(repo.path());
        let out = command_async(&policy, repo.path(), &["status", "--short"])
            .hermetic_env_for_test()
            .output()
            .await
            .expect("git runs through the wrapper");
        assert!(
            out.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The Task 6 shape end to end: the **production** `policy_for_repo` drives
    /// real git through the wrapper. This is exactly what a migrated spawn site
    /// will do, so it proves the production policy path works before any live
    /// site depends on it — and it exercises `shim::shim_path` resolution, the
    /// enumerated `$HOME` grant, and the real secret excludes together.
    #[tokio::test]
    async fn the_production_policy_runs_real_git_and_denies_secrets() {
        let repo = fixture().await;
        let policy = production_policy(repo.path());

        // A granted operation succeeds: proves the policy is not denying all.
        let ok = command_async(&policy, repo.path(), &["status", "--short"])
            .hermetic_env_for_test()
            .output()
            .await
            .expect("git runs");
        assert!(
            ok.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&ok.stderr)
        );

        // A secret stays denied under the same production policy.
        let home = std::env::var("HOME").unwrap();
        let secret = format!("{home}/.ssh/known_hosts");
        if std::path::Path::new(&secret).exists() {
            let out = command_async(&policy, repo.path(), &["config", "-f", &secret, "--list"])
                .hermetic_env_for_test()
                .output()
                .await
                .expect("git runs");
            assert!(
                !out.status.success(),
                "the production policy let git read ~/.ssh: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }
}
