//! M1.13b (#66) Task 5: the spawn wrapper that is the *only* way the server
//! starts a git process.
//!
//! Everything above this in `sandbox` is pure — it produces argv. This is where
//! that argv becomes a real `Command`, and it is deliberately the single
//! chokepoint: `argv_boundary.rs` proves no other file in the crate constructs
//! a git `Command` outside the allowlist, and Task 6 migrated the existing
//! spawn sites onto [`command_async`] so that proof means "every git the server
//! runs is sandboxed."
//!
//! # Why one function and not two
//!
//! This module shipped with a `command_sync` beside `command_async`, for
//! "blocking helpers" — and Task 6 then found there are none. Every production
//! git in the crate is reached from an `async fn` (`git_output`,
//! `git_stdout_capped`, `rev_parse`, `is_ancestor`, `git_ref_exists`, the
//! planner's `run_git`, and as of plan step 6.7 the clone handler), and every
//! remaining `std::process::Command` in the crate is `#[cfg(test)]` fixture
//! setup that deliberately spawns *unsandboxed* git to build a repository
//! before the sandbox is applied. `command_sync` had no caller at all — not
//! even a test — and was carrying an `allow(dead_code)` to say so, which is
//! exactly the kind of "someone will wire this up" placeholder that outlives
//! the reason it existed. It was deleted rather than left dead; if a genuinely
//! blocking call site ever appears, the four lines are cheap to write back with
//! a caller attached.
//!
//! Neither call style needs a `pre_exec` closure or a `block_on`, because the
//! sandbox is *argv*: the shim applies Landlock and seccomp in its own process,
//! after this one has already exec'd it.

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

/// Split an argv into program and arguments.
fn split(argv: &[std::ffi::OsString]) -> (&std::ffi::OsString, &[std::ffi::OsString]) {
    (&argv[0], &argv[1..])
}

/// Repository-geometry environment variables removed from **every** composed
/// command, before the type seals it.
///
/// The launcher passes the server's environment through otherwise — that is
/// still deliberate; `GIT_TERMINAL_PROMPT` and `GIT_EDITOR` are set by
/// `main.rs` and must reach git — but this family is different in kind: each
/// of these redirects *which repository geometry* git operates on, silently
/// overriding the `-C <repo>` / `--git-dir=<...>` this module composed and
/// `sandbox_argv` classified. `GIT_OBJECT_DIRECTORY` in particular names the
/// primary object database **regardless of `--git-dir`**, which turned the
/// preview's "writes only into its scratch store" into writes into the served
/// repository's own ODB (#576's audit, reproduced in
/// `preview_suite::a2_an_inherited_git_object_directory_cannot_redirect_preview_writes`).
/// None of this needs hostility: git itself exports `GIT_OBJECT_DIRECTORY`
/// and `GIT_ALTERNATE_OBJECT_DIRECTORIES` into hooks during its receive-pack
/// quarantine, and `GIT_DIR` into most of them, so a server launched from
/// inside a hook inherits the whole family by construction.
///
/// Variable by variable — every entry redirects a location the argv already
/// pinned:
///
/// * `GIT_DIR` / `GIT_COMMON_DIR` — override repository discovery itself;
///   every spawn would operate on some *other* repository than its `-C`.
/// * `GIT_OBJECT_DIRECTORY` / `GIT_ALTERNATE_OBJECT_DIRECTORIES` — re-aim
///   object reads and writes past the git dir the argv named.
/// * `GIT_INDEX_FILE` — redirects every index write.
/// * `GIT_WORK_TREE` — redirects the worktree.
/// * `GIT_NAMESPACE` — silently rewrites every ref name under
///   `refs/namespaces/`, so ref reads and updates target refs the caller
///   never named.
/// * `GIT_GRAFT_FILE` / `GIT_SHALLOW_FILE` — substitute ancestry: history
///   walks and reachability answers come from a file outside the repository
///   (`history.rs` manages `$GIT_DIR/shallow` deliberately, via git's own
///   commands, never via this variable).
///
/// Deliberately **kept**, each for a stated reason:
///
/// * `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` / `GIT_CONFIG_NOSYSTEM` and
///   the `GIT_CONFIG_COUNT` family — they select *configuration*, not
///   geometry: the same config the operator's own command-line git would
///   read. The server's posture is user-git parity (`preview_suite.rs`'s
///   `fast_forward_shape` doc records that a developer's `~/.gitconfig`
///   reaches every git the server runs, and fixtures pin their own), and
///   preview and execution inherit them identically, so neither can see a
///   config the other did not.
/// * `GIT_CEILING_DIRECTORIES` — can only make discovery *refuse*, never
///   land somewhere else; a loud failure is the fail-closed direction.
/// * `GIT_REPLACE_REF_BASE` / `GIT_NO_REPLACE_OBJECTS` — select a *view* of
///   objects the repository itself carries, applied identically to every
///   spawn; they redirect no write.
/// * Everything else (`PATH`, `HOME`, …) — the sandbox is the boundary for
///   those, exactly as before. A hostile parent environment is out of scope
///   here (it already owns `PATH`); an *ordinary* inherited geometry
///   variable breaking A2 is what this list closes.
///
/// The scrub happens at construction, as a fixed reviewed list — there is
/// still no caller-facing `env` surface on [`SandboxedCommand`], so the seal
/// argument is unchanged. `pinned_env_for_test`'s `env_clear()` wipes these
/// removals first, so the escape battery's pinned profiles remain in full
/// control of what their cases observe.
///
/// Pinned by `the_launcher_scrubs_gits_repository_geometry_environment`
/// (which carries its own literal copy of these names, deliberately) and
/// behaviourally by the preview suite's A2 environment test.
const SCRUBBED_GIT_GEOMETRY_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_WORK_TREE",
    "GIT_NAMESPACE",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
];

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
/// The *inherited* environment gets the complementary treatment:
/// [`command_async`] removes the fixed [`SCRUBBED_GIT_GEOMETRY_ENV`] family at
/// construction, so a variable the server's own parent exported cannot re-aim
/// the geometry the argv pinned either. Neither direction gives a caller an
/// environment surface.
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

    /// Test-only: exit status, for fixture setup that only needs "did it work".
    #[cfg(test)]
    pub(crate) async fn status(mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.status().await
    }

    /// Test-only: **replace** the environment with `profile`, wholesale.
    ///
    /// Deliberately not an incremental `env(k, v)`. The escape battery's R7 rule
    /// is that both legs of a case run under one *pinned* environment profile —
    /// pinned meaning the environment is known in full, not "inherited plus a
    /// few overrides". An incremental setter makes a half-pinned environment
    /// expressible, and a half-pinned environment is how a developer's stray
    /// `GIT_*` variable silently changes what a containment case observed.
    ///
    /// So this clears first and applies the profile as a unit: the same
    /// discipline the argv now has, for the same reason. Gated to `#[cfg(test)]`
    /// so the production surface stays free of environment control entirely —
    /// see the type doc for why `env` is a hazard rather than a convenience.
    #[cfg(test)]
    pub(crate) fn pinned_env_for_test<K, V>(mut self, profile: &[(K, V)]) -> Self
    where
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        self.0.env_clear();
        for (k, v) in profile {
            self.0.env(k, v);
        }
        self
    }

    /// Test-only: the minimal pinned profile for fixtures that only need git to
    /// run at all. Expressed through [`Self::pinned_env_for_test`] so there is
    /// exactly one way an environment is applied.
    #[cfg(test)]
    pub(crate) fn hermetic_env_for_test(self) -> Self {
        let home = std::env::var("HOME").expect("HOME set in tests");
        self.pinned_env_for_test(&[("PATH", "/usr/bin:/bin".to_string()), ("HOME", home)])
    }
}

/// The one wrapper: a [`SandboxedCommand`] whose argv is already complete.
/// Pipes and `kill_on_drop` are left to the caller, because the call sites want
/// different shapes (a capped stream vs a simple output) and both are
/// legitimate — but none of them may touch the argv.
pub(crate) fn command_async(policy: &Policy, repo: &Path, args: &[&str]) -> SandboxedCommand {
    let argv = full_argv(policy, repo, args);
    let (program, rest) = split(&argv);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest);
    for var in SCRUBBED_GIT_GEOMETRY_ENV {
        cmd.env_remove(var);
    }
    SandboxedCommand(cmd)
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
        //
        // The premise is ASSERTED, never skipped. This was previously
        // `if Path::new(&secret).exists() { … }`, which meant that on any host
        // without `~/.ssh/known_hosts` — a fresh CI runner, for one — the entire
        // secret-denial assertion vanished and this test passed green having
        // checked nothing about secrets at all. That is failure shape #1 from
        // this milestone's own list ("a green test that proves nothing is worse
        // than a red one"), and an adversarial review reproduced it by running
        // the suite under a runner-shaped `$HOME`.
        //
        // The escape battery already takes this posture deliberately: a case
        // that cannot demonstrate its own premise is a HARD FAILURE, not a skip
        // (see `run_case` in escape_contract.rs). This now matches it. CI
        // materialises the path in `.github/actions/host-sandbox-setup`.
        let home = std::env::var("HOME").unwrap();
        let secret = format!("{home}/.ssh/known_hosts");
        assert!(
            std::path::Path::new(&secret).exists(),
            "{secret} does not exist, so this test cannot show that the production policy \
             denies it: git would fail to read an absent path for the wrong reason entirely, \
             and a pass would mean nothing. Any non-empty owner-readable file will do — CI \
             writes a placeholder in .github/actions/host-sandbox-setup."
        );
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

    /// The launcher scrubs git's repository-geometry environment from every
    /// composed command — the fixed, reviewed list applied at construction,
    /// variable by variable.
    ///
    /// The expected names are **written out here as literals**, deliberately
    /// not read from `SCRUBBED_GIT_GEOMETRY_ENV`: a test that iterated the
    /// same constant it verifies would follow a deletion silently and stay
    /// green — asserting a mapping by calling the function that defines it,
    /// which this repository has paid for before.
    ///
    /// The kept set is asserted too. `GIT_TERMINAL_PROMPT` and `GIT_EDITOR`
    /// are set by `main.rs` and must reach git; `GIT_CONFIG_GLOBAL`/`_SYSTEM`
    /// are the documented user-git-parity decision (`preview_suite.rs`'s
    /// `fast_forward_shape` doc records that a developer's own config reaches
    /// every git the server runs, and the fixtures pin their own). A scrub
    /// that grew to swallow those would be a different change than the one
    /// reviewed here.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M1 — REMOVES the mechanism where it bites.** Delete
    ///   `"GIT_OBJECT_DIRECTORY"` from the production list: red here on that
    ///   name, and red in `preview_suite`'s
    ///   `a2_an_inherited_git_object_directory_cannot_redirect_preview_writes`
    ///   at its object-count assertion — the behavioural half of the pair.
    /// * **M2 — WEAKENS the family.** Delete `"GIT_INDEX_FILE"`: red here
    ///   only, because no preview touches an index. The two failure surfaces
    ///   are what stop the family eroding one unexercised variable at a time.
    #[test]
    fn the_launcher_scrubs_gits_repository_geometry_environment() {
        let repo = std::path::PathBuf::from("/srv/repo");
        let policy = production_policy(&repo);
        let cmd = command_async(&policy, &repo, &["status", "--short"]);

        let removed: std::collections::BTreeSet<std::ffi::OsString> = cmd
            .0
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| v.is_none().then(|| k.to_os_string()))
            .collect();

        for var in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_INDEX_FILE",
            "GIT_WORK_TREE",
            "GIT_NAMESPACE",
            "GIT_GRAFT_FILE",
            "GIT_SHALLOW_FILE",
        ] {
            assert!(
                removed.contains(std::ffi::OsStr::new(var)),
                "{var} is not scrubbed from the launched environment — an \
                 inherited value redirects the repository geometry the argv \
                 pinned (git exports GIT_OBJECT_DIRECTORY itself into \
                 receive-pack hooks, so this is an ordinary inheritance, not \
                 an attack)"
            );
        }

        for var in [
            "GIT_TERMINAL_PROMPT",
            "GIT_EDITOR",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
        ] {
            assert!(
                !removed.contains(std::ffi::OsStr::new(var)),
                "{var} is scrubbed, but it is deliberately kept: the first two \
                 are set by main.rs for every child, and the config pair is \
                 the documented user-git-parity decision"
            );
        }
    }

    /// C10 hazard #1, as a tripwire rather than a review convention.
    ///
    /// `SandboxedCommand` exists so an argv cannot change after `sandbox_argv`
    /// classified it. Rust has no stable negative-impl assertion, and a plain
    /// "we just won't add it" comment is exactly the kind of reviewer-enforced
    /// invariant this milestone keeps finding holes in — so assert it against
    /// the source text: the production `impl` block must expose no `arg`,
    /// `args` or `env` method. A future edit that adds one fails here with the
    /// reason, instead of silently reopening the hazard.
    ///
    /// The `#[cfg(test)]` escape hatch is matched deliberately and allowed:
    /// test fixtures may strip the environment, production may not.
    #[test]
    fn the_sandboxed_command_exposes_no_way_to_change_what_runs() {
        let src = include_str!("spawn.rs");
        let start = src
            .find("impl SandboxedCommand {")
            .expect("the impl block moved or was renamed");
        let block = &src[start..];
        let end = block.find("\n}\n").expect("unterminated impl block");
        let block = &block[..end];

        for forbidden in ["fn arg", "fn args", "fn env"] {
            for (i, line) in block.lines().enumerate() {
                let line = line.trim();
                if !line.starts_with("pub(crate) fn ") {
                    continue;
                }
                // The one sanctioned exception, gated so production cannot reach it.
                if line.contains("hermetic_env_for_test") {
                    continue;
                }
                assert!(
                    !line.contains(forbidden),
                    "SandboxedCommand line {i} exposes `{forbidden}`: {line}\n\
                     That reopens C10 hazard #1 — a caller could change the argv or \
                     environment after `sandbox_argv` already classified it. If a spawn \
                     site genuinely needs different arguments, pass them to \
                     `command_async` so the classified argv is the executed argv."
                );
            }
        }
    }
}
