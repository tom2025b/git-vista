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
//! # Why two typed entry points still have one execution chokepoint
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
//!
//! #723 adds `checkout_command_async`, but it is not a second general-purpose
//! wrapper: its argument is the sealed `CheckoutPolicy`, and its sole purpose is
//! selecting the checkout-only seccomp profile. Both typed entry points still
//! converge on `command_from_argv`, the one place argv becomes a `Command`.

use std::path::Path;

use super::{checkout_sandbox_argv, sandbox_argv, CheckoutPolicy, Policy};

/// Build the full argv for `git -C <repo> <args…>` under `policy`.
///
/// Split out from both wrappers so the argv they will run is testable without
/// spawning anything, and so the two wrappers cannot drift apart in how they
/// assemble it.
pub(crate) fn full_argv(policy: &Policy, repo: &Path, args: &[&str]) -> Vec<std::ffi::OsString> {
    let mut argv = sandbox_argv(policy);
    argv.push(std::ffi::OsString::from("-C"));
    argv.push(repo.as_os_str().to_os_string());
    for a in args {
        argv.push(std::ffi::OsString::from(*a));
    }
    argv
}

/// The same sealed argv assembly for clone's untrusted checkout, whose type
/// selects the checkout-specific seccomp profile before any Git arguments are
/// appended (#723).
fn full_checkout_argv(
    policy: &CheckoutPolicy,
    repo: &Path,
    args: &[&str],
) -> Vec<std::ffi::OsString> {
    let mut argv = checkout_sandbox_argv(policy);
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
///   geometry. Ordinary commands retain user-git parity (`preview_suite.rs`'s
///   `fast_forward_shape` doc records that a developer's `~/.gitconfig`
///   reaches those git processes, and fixtures pin their own). The untrusted
///   clone-checkout path is the deliberate exception: it replaces the system
///   and global selectors at completion because fetched `.gitattributes` can
///   select executable filter commands from those scopes.
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

/// The **complete** set of environment variable names an untrusted checkout
/// child may inherit (#704). Everything not named here is absent from that
/// child by construction, not by having been remembered.
///
/// # Why this is an allowlist and #680's removal list was not enough
///
/// ADR 0128 closed clone's credential leak by *removing* three names from the
/// checkout child: `GIT_VISTA_CREDENTIAL_TOKEN`, `GIT_VISTA_GITHUB_TOKEN` and
/// `GH_TOKEN`. That is a denylist, and a denylist over an inherited
/// environment is only ever as complete as the last person to think about it.
/// Everything else the operator happened to export — `AWS_SECRET_ACCESS_KEY`,
/// `NPM_TOKEN`, a CI job's injected secret, `SSH_AUTH_SOCK` (#702) — reached
/// attacker-selected `post-checkout` hooks and `.gitattributes`-selected
/// filters untouched. Adding a fourth name rebuilds the same defect one
/// variable later.
///
/// So the child's environment is **built**, not filtered: start from nothing,
/// copy across only the names below. The Network harness additionally supplies
/// fixed, non-secret `GIT_ALLOW_PROTOCOL` and empty `GIT_PROXY_COMMAND` values
/// at spawn time (#779), never their inherited values. A credential nobody enumerated is absent
/// because it was never added, which is a property of the shape rather than of
/// anyone's diligence.
///
/// # Every entry, and why it survives the cut
///
/// * `PATH` — load-bearing twice over. `gv-sandbox` reaches git through
///   `Command::new("git").exec()`, which is a `PATH` lookup, so an empty
///   `PATH` does not run a reduced checkout, it runs none at all. Hooks and
///   filters are `#!/bin/sh` scripts that then need it themselves.
/// * `HOME` — checkout-time hooks and filter programs can use it for ordinary
///   operator-owned resources. It is a path, not a secret, and the paths
///   underneath it that *are* secrets (`~/.ssh`, `~/.config/gh`, …) stay
///   withheld by `secret_excludes` regardless of what this variable says.
///   Git itself does not load `$HOME/.gitconfig` on this path: the fixed
///   `GIT_CONFIG_GLOBAL=/dev/null` completion policy below prevents that.
/// * `XDG_CONFIG_HOME` — preserves the operator's conventional configuration
///   root for checkout-time child programs, subject to the same filesystem
///   grants and exclusions. Git's `$XDG_CONFIG_HOME/git/config` is likewise
///   bypassed by the fixed global-config selector.
/// * `LANG`, `LC_ALL`, `LC_CTYPE`, `LC_MESSAGES` — locale. They select message
///   text and character handling; they name no resource and grant no access.
///
/// # What was deliberately left out, and why each is a decision
///
/// * `SSH_AUTH_SOCK` — the whole of #702. See `sandbox::policy_for_clone`.
/// * `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` and inherited
///   `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`/`GIT_CONFIG_NOSYSTEM` — these can
///   carry arbitrary configuration, including a credential or executable
///   selector in the value itself. None is forwarded. At command completion,
///   checkout instead receives fixed `GIT_CONFIG_NOSYSTEM=1` and
///   `GIT_CONFIG_GLOBAL=/dev/null`: system and operator-global configuration
///   are unavailable, while the repository-local config Git created during
///   `clone --no-checkout` remains readable. This deliberately gives up
///   operator-level hooks and filters, including automatic LFS smudging when
///   its filter exists only in those scopes.
/// * `TMPDIR`, `TERM`, `TZ` — nothing in `git checkout -f` needs them, and
///   `/tmp` is not a grant this policy gives out in any case.
/// * The [`SCRUBBED_GIT_GEOMETRY_ENV`] family — already removed for every
///   spawn in the crate, and absent here for the stronger reason that they
///   were never added.
///
/// Adding a name here is a security decision. It must come with the sentence
/// saying what breaks without it, in this comment, in the same edit.
pub(crate) const UNTRUSTED_CHECKOUT_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "XDG_CONFIG_HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
];

/// Build an untrusted checkout child's complete environment from `source` by
/// keeping only [`UNTRUSTED_CHECKOUT_ENV_ALLOWLIST`] names.
///
/// Free, pure and taking its source as a parameter rather than reading
/// `std::env` itself — so the decision ("which names survive") is host-testable
/// with an arbitrary synthetic environment, including canaries that no test may
/// safely set process-wide. `with_untrusted_checkout_env` is the one production
/// caller and supplies `std::env::vars_os()`.
///
/// A name that is allowlisted but absent from `source` stays absent: this
/// copies, it never fabricates a value git would then read as meaningful.
pub(crate) fn untrusted_checkout_env<I, K, V>(
    source: I,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<std::ffi::OsString>,
    V: Into<std::ffi::OsString>,
{
    source
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .filter(|(k, _)| {
            UNTRUSTED_CHECKOUT_ENV_ALLOWLIST
                .iter()
                .any(|allowed| k == std::ffi::OsStr::new(allowed))
        })
        .collect()
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
/// is sealed by the *type* instead. There is deliberately no `arg` and no
/// `args` here — only stdio configuration, which cannot change what runs.
/// `env` is excluded on the same reasoning as `arg`: `GIT_DIR`, `GIT_SSH_COMMAND`
/// and `GIT_EXTERNAL_DIFF` redirect or execute, so an environment appended
/// after classification is an argv change wearing a different hat — with
/// narrow, fixed exceptions: [`credential_env`](SandboxedCommand::credential_env)
/// supplies server-owned credential data (#582),
/// [`with_network_transport_policy`](SandboxedCommand::with_network_transport_policy)
/// selects a server-authored transport restriction (#779), and the untrusted
/// checkout builder selects fixed system/global Git-config restrictions.
/// None exposes an arbitrary environment name/value setter.
///
/// The *inherited* environment gets the complementary treatment:
/// [`command_async`] removes the fixed [`SCRUBBED_GIT_GEOMETRY_ENV`] family at
/// construction, so a variable the server's own parent exported cannot re-aim
/// the geometry the argv pinned either.
///
/// The setters consume and return `Self` so a call site still reads as one
/// chain ending in `output()`/`spawn()`.
pub(crate) struct SandboxedCommand {
    command: tokio::process::Command,
    restrict_network_transports: bool,
    restrict_checkout_git_config: bool,
}

/// The one environment variable a production caller may set on a
/// [`SandboxedCommand`] (M13.01, #582), via [`SandboxedCommand::credential_env`]
/// — the only caller-supplied environment value this type permits. The Network
/// protocol restriction separately sets a fixed server-authored value (#779);
/// neither method reopens a general environment setter.
///
/// # Why this one variable does not carry the hazard `env` was excluded for
///
/// The excluded cases — `GIT_SSH_COMMAND`, `GIT_EXTERNAL_DIFF`, the
/// [`SCRUBBED_GIT_GEOMETRY_ENV`] family — are all names **git itself**
/// interprets: setting one changes what git does, unconditionally, by git's
/// own design. This name means nothing to git. It only becomes meaningful
/// because this crate's own `-c credential.helper=` literal
/// (`network_exec::credential_helper_arg`) names it — a config value *this
/// crate authored*, never request data. Setting an inert, git-opaque
/// variable is data, not an argv change wearing a different hat, which is
/// what the type doc's exclusion is actually about.
///
/// # What this does NOT give you: the value is not isolated to our helper
///
/// An earlier version of this comment claimed a repo-local
/// `credential.helper` pointing at `printenv GIT_VISTA_CREDENTIAL_TOKEN`
/// "gets nothing", on the reasoning that the variable is set only on the
/// spawn that forces our own helper. **That is false, and it was corrected
/// after review (#668, grok, 2026-09-05).** The variable is set on the
/// *git* process; `gv-sandbox` `execve`s git without clearing the
/// environment, and git's credential helpers are children of that git. So
/// **every helper in the chain inherits this value**, not only ours. The
/// original append-never-clear rule (ADR 0122 decision 8) put an operator's or
/// repository's helper earlier in that chain. PR #775 superseded that rule:
/// the Network harness now resets configured helpers before appending exactly
/// Git-Vista's helper (ADR 0144).
///
/// Resetting the helper chain does not isolate a process environment value to
/// one descendant. Any other program the Git process runs still inherits it.
///
/// #680 showed why “no repo-local config exists yet” was not enough: global
/// `core.hooksPath` or filter configuration can let fetched content select a
/// descendant during clone's implicit checkout. Clone now uses this method
/// only for a `--no-checkout` transfer and lets that process exit before a
/// separately spawned, credentialless checkout runs hooks and filters (ADR
/// 0128). This method also removes the two ambient token-source variables so
/// an env-backed credential has only this internal name in the child.
///
/// **Read this before reusing `network_command_with_credential` on
/// fetch/push/pull.** The helper reset and transport pins constrain the named
/// transport selectors, but these operations may also run repository-selected
/// hooks. Those descendants can read this variable from their environment.
/// Reuse therefore still requires a separate credential-isolation decision,
/// not just relying on the configured helper chain being reset.
pub(crate) const CREDENTIAL_TOKEN_VAR: &str = "GIT_VISTA_CREDENTIAL_TOKEN";

impl SandboxedCommand {
    /// Seal Network transport selection to Git-Vista's supported protocols.
    /// No caller-supplied names or values: repository protocol rules must not
    /// reopen installed helpers. Apply at completion so checkout's env_clear
    /// and test-only environment replacement cannot erase this restriction.
    pub(crate) fn with_network_transport_policy(mut self) -> Self {
        self.restrict_network_transports = true;
        self
    }

    fn apply_network_transport_policy(&mut self) {
        if self.restrict_network_transports {
            // Unlike protocol.allow=never, this overrides specific repository
            // protocol.<name>.allow rules too. The empty proxy environment
            // override bypasses ALL core.gitProxy entries (first-match config
            // cannot be reset with a later -c), while retaining direct git://.
            self.command
                .env("GIT_ALLOW_PROTOCOL", "http:https:ssh:git:file");
            self.command.env("GIT_PROXY_COMMAND", "");
        }
    }

    /// Remove the two operator-controlled configuration scopes from an
    /// untrusted clone checkout. A fetched `.gitattributes` file can select an
    /// arbitrary `filter.<name>` configured in either scope, so filtering
    /// individual known keys would leave the same executable surface under a
    /// different name. Repository-local config remains enabled: a remote
    /// repository's `.git/config` is not copied by clone, and the destination's
    /// config is created locally by Git during the no-checkout transfer.
    ///
    /// Applied at completion, after test-only environment replacement, so a
    /// spawned regression test exercises the same final authority as
    /// production and cannot accidentally erase the restriction.
    fn apply_checkout_git_config_policy(&mut self) {
        if self.restrict_checkout_git_config {
            self.command.env("GIT_CONFIG_NOSYSTEM", "1");
            self.command.env("GIT_CONFIG_GLOBAL", "/dev/null");
        }
    }

    fn apply_completion_policies(&mut self) {
        self.apply_network_transport_policy();
        self.apply_checkout_git_config_policy();
    }

    /// Set [`CREDENTIAL_TOKEN_VAR`] to `token` on this command's environment —
    /// the one deliberate exception to "no `env`", see that constant's doc.
    /// Never call this with a value that did not come from Git-Vista's own
    /// token source; it is not a general secret-passing mechanism.
    pub(crate) fn credential_env(mut self, token: &str) -> Self {
        for var in crate::token_store::TOKEN_SOURCE_ENV_VARS {
            self.command.env_remove(var);
        }
        self.command.env(CREDENTIAL_TOKEN_VAR, token);
        self
    }

    /// Replace this command's environment with the allowlisted one an
    /// untrusted checkout child may have (#702, #704).
    ///
    /// This *supersedes* `without_credential_env`, which removed exactly the
    /// three names ADR 0128 enumerated and let everything else through. The
    /// method is gone rather than deprecated: while a "remove these names"
    /// builder exists on this type, the next credential-adjacent call site can
    /// reach for it and rebuild the same defect one variable later. See
    /// [`UNTRUSTED_CHECKOUT_ENV_ALLOWLIST`] for what survives and why.
    ///
    /// `env_clear()` first, then the allowlist: the child's inherited
    /// environment is the returned set. At spawn time the Network harness adds
    /// fixed, non-secret `GIT_ALLOW_PROTOCOL` and empty `GIT_PROXY_COMMAND`
    /// restrictions (#779), plus fixed system/global Git-config restrictions;
    /// no parent value is copied for any of those names. The three ADR 0128
    /// names are absent here because they were never copied in — a strictly
    /// stronger statement than the removals this replaces, and one that holds
    /// for every name nobody has thought of yet.
    pub(crate) fn with_untrusted_checkout_env(mut self) -> Self {
        self.command.env_clear();
        for (key, value) in untrusted_checkout_env(std::env::vars_os()) {
            self.command.env(key, value);
        }
        self.restrict_checkout_git_config = true;
        self
    }

    /// Test-only: what [`CREDENTIAL_TOKEN_VAR`] is set to on this composed
    /// command, if anything.
    ///
    /// This exists because the *spawn*-based proof cannot answer the
    /// question. `pinned_env_for_test` deliberately does `env_clear()` and
    /// then applies its profile wholesale, so any `credential_env` set
    /// before it is wiped and re-supplied by the profile itself — which
    /// means a spawned child observing the variable proves only that the
    /// test put it there. Measured, not reasoned: deleting
    /// `.credential_env(token)` from `network_exec::
    /// network_command_with_credential` left the real-spawn test green
    /// (`failure-atlas` mutation 332, `survived`), which is what this
    /// accessor and its test exist to fix.
    ///
    /// Reading the composed command is the weaker kind of evidence this
    /// crate normally avoids — but here it is the *only* view of the
    /// supply step that the test harness does not overwrite, and it is
    /// paired with, not a replacement for, the real spawn that reads
    /// `/proc/self/cmdline` back from the kernel.
    #[cfg(test)]
    pub(crate) fn credential_env_for_test(&self) -> Option<String> {
        self.command.as_std().get_envs().find_map(|(k, v)| {
            (k == std::ffi::OsStr::new(CREDENTIAL_TOKEN_VAR))
                .then(|| v.map(|v| v.to_string_lossy().into_owned()))
                .flatten()
        })
    }

    pub(crate) fn stdin(mut self, cfg: impl Into<std::process::Stdio>) -> Self {
        self.command.stdin(cfg);
        self
    }

    pub(crate) fn stdout(mut self, cfg: impl Into<std::process::Stdio>) -> Self {
        self.command.stdout(cfg);
        self
    }

    pub(crate) fn stderr(mut self, cfg: impl Into<std::process::Stdio>) -> Self {
        self.command.stderr(cfg);
        self
    }

    pub(crate) fn kill_on_drop(mut self, kill: bool) -> Self {
        self.command.kill_on_drop(kill);
        self
    }

    pub(crate) async fn output(mut self) -> std::io::Result<std::process::Output> {
        self.apply_completion_policies();
        self.command.output().await
    }

    pub(crate) fn spawn(mut self) -> std::io::Result<tokio::process::Child> {
        self.apply_completion_policies();
        self.command.spawn()
    }

    /// Test-only: exit status, for fixture setup that only needs "did it work".
    #[cfg(test)]
    pub(crate) async fn status(mut self) -> std::io::Result<std::process::ExitStatus> {
        self.apply_completion_policies();
        self.command.status().await
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
        self.command.env_clear();
        for (k, v) in profile {
            self.command.env(k, v);
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

/// The general policy wrapper: a [`SandboxedCommand`] whose argv is complete.
/// Pipes and `kill_on_drop` are left to the caller, because the call sites want
/// different shapes (a capped stream vs a simple output) and both are
/// legitimate — but none of them may touch the argv.
pub(crate) fn command_async(policy: &Policy, repo: &Path, args: &[&str]) -> SandboxedCommand {
    command_from_argv(full_argv(policy, repo, args))
}

/// The only spawn seam for [`CheckoutPolicy`]. Its distinct argument type is
/// what makes `--seccomp-checkout` mandatory for the phase that runs fetched
/// hooks and filters.
pub(crate) fn checkout_command_async(
    policy: &CheckoutPolicy,
    repo: &Path,
    args: &[&str],
) -> SandboxedCommand {
    command_from_argv(full_checkout_argv(policy, repo, args))
}

fn command_from_argv(argv: Vec<std::ffi::OsString>) -> SandboxedCommand {
    let argv = wrap_with_reaper(argv);
    let (program, rest) = split(&argv);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest);
    for var in SCRUBBED_GIT_GEOMETRY_ENV {
        cmd.env_remove(var);
    }
    SandboxedCommand {
        command: cmd,
        restrict_network_transports: false,
        restrict_checkout_git_config: false,
    }
}

/// #728: an abrupt death of the process that spawns `bwrap` must not leave it
/// reparented and running forever — see `sandbox::reaper`'s module doc for the
/// full account and ADR 0141 for the design. Layered here, at the one place
/// pure argv becomes a real process, rather than inside `sandbox_argv` itself:
/// INV-16's reviewed argv shapes stay exactly what they were before #728,
/// because the reaper decides how that argv is *launched*, not what it is.
///
/// **Scoped to `Tier::Strict` and `Tier::Network`** — detected structurally,
/// by comparing the composed program against the resolved `bwrap` path or the
/// resolved `gv-sandbox` shim path, rather than by threading a `Tier` through
/// this function (which would need to see through `CheckoutPolicy`'s private
/// field). Every sandboxed argv shape has exactly one of the two as its
/// program (`sandbox_argv_with_seccomp_profile`'s three shapes: `Strict`
/// starts with `bwrap`, `Network` — transfer or checkout, `policy.tier` is
/// `Network` either way — starts with the bare shim, `Unsandboxed` starts with
/// `git` and matches neither), so this covers both sandboxed tiers without
/// needing to see the tier itself.
///
/// #757: originally `Tier::Network`'s bare shim was left unwrapped here,
/// because it has no pid namespace — `killpg` is the *entire* reaping
/// mechanism there, not a backstop on top of one, and the PR that added the
/// reaper had not built a Network-tier acceptance test to justify it. That
/// test is `sandbox::lifecycle::a_reaper_process_reaps_a_network_tier_launcher_when_only_its_own_parent_is_sigkilled`,
/// and it also names the residual `killpg` does **not** close: a
/// double-forked, `setsid`-detached grandchild leaves the reaper's process
/// group entirely, on every tier, and only `Tier::Strict`'s pid namespace
/// closes that gap (killing the namespace's pid 1 makes the kernel tear down
/// every task inside it, regardless of what process group or session it
/// self-assigned) — see that test's module-level comparison against
/// `strict_reaps_a_double_forked_setsid_orphan_that_the_network_tier_does_not`.
/// `Tier::Network`'s *ordinary* orphan — the shim, `git`, and whatever `git`
/// spawned without deliberately detaching — is exactly what `killpg` reaches,
/// which is the shape #757 was filed about: a Network-tier operation had **no**
/// protection of any kind against its coordinating process dying abruptly, not
/// even the bounded, `killpg`-only guarantee this section now gives it.
///
/// Never applied to `Tier::Unsandboxed`'s bare `git` (INV-16 shapes 1/2): that
/// operation is already explicit, persisted, operator-trusted content flying a
/// permanent banner (INV-15).
///
/// The caller's own pid is prepended as an explicit argument, **not** left for
/// the reaper to discover via its own `getppid()` at some uncertain later
/// time. A self-observed baseline is racy: the OS-level fork that creates the
/// reaper process happens *before* a single line of the reaper's own code can
/// run, so if the real caller dies inside that window, the reaper's first
/// `getppid()` read already reflects the post-reparenting value, and the
/// reaper would treat an already-orphaned state as normal from the start,
/// never detecting anything wrong. `std::process::id()`, read here — in the
/// caller, before the reaper is even spawned — has no such window: this
/// process trivially knows its own pid before it exists to race against.
/// `sandbox::lifecycle::a_reaper_process_reaps_a_launcher_orphaned_before_it_ever_ran`
/// proves this deterministically (a SIGSTOP/SIGCONT fixture that forces the
/// exact ordering: caller dies, reparenting completes, *then* the reaper's
/// own code runs for the first time).
///
/// A host missing `gv-sandbox-reaper` gets `argv` back unwrapped: exactly the
/// sandbox it had before #728, no capability lost — see `reaper::reaper_path`
/// for why that absence is a soft condition, not a policy-construction
/// failure.
pub(crate) fn wrap_with_reaper(argv: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let is_strict_bwrap_launch = super::bwrap::bwrap_path()
        .is_some_and(|bwrap| argv.first().map(|p| p.as_os_str()) == Some(bwrap.as_os_str()));
    let is_network_shim_launch = !is_strict_bwrap_launch
        && super::shim::shim_path()
            .ok()
            .is_some_and(|shim| argv.first().map(|p| p.as_os_str()) == Some(shim.as_os_str()));
    if !is_strict_bwrap_launch && !is_network_shim_launch {
        return argv;
    }
    match super::reaper::reaper_path() {
        Some(reaper) => {
            let mut wrapped = Vec::with_capacity(argv.len() + 2);
            wrapped.push(reaper.as_os_str().to_os_string());
            wrapped.push(std::ffi::OsString::from(std::process::id().to_string()));
            wrapped.extend(argv);
            wrapped
        }
        None => argv,
    }
}

#[cfg(test)]
mod tests {
    use super::super::shim_cli::{fixture, production_policy};
    use super::*;

    /// #704's decision, tested where it is made: on an arbitrary synthetic
    /// environment rather than the process's own.
    ///
    /// The canary here is the point. A denylist can only be tested against
    /// the names its author already listed — a test that checks the three ADR
    /// 0128 variables are gone passes identically whether the mechanism is an
    /// allowlist or the three `env_remove` calls it replaced, so it cannot
    /// tell the two apart and cannot fail on the defect #704 reported. A name
    /// this crate has never heard of is the assertion that separates them.
    ///
    /// MUTATION 1 (remove the mechanism): drop the `.filter(…)` from
    ///   `untrusted_checkout_env` so it copies `source` wholesale. RED here —
    ///   every one of the four withheld names comes back.
    /// MUTATION 2 (weaken the mechanism): add `"SSH_AUTH_SOCK"` to
    ///   `UNTRUSTED_CHECKOUT_ENV_ALLOWLIST`. RED here on the `SSH_AUTH_SOCK`
    ///   leg alone, with the canary and the tokens still correctly withheld —
    ///   a different failure, from a list that still looks deliberate.
    #[test]
    fn an_untrusted_checkout_environment_is_built_by_allowlist_not_by_removal() {
        let source = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/operator"),
            ("LANG", "en_US.UTF-8"),
            // Withheld: the three ADR 0128 removals, the #702 agent socket,
            // and a name no list in this crate mentions anywhere.
            (CREDENTIAL_TOKEN_VAR, "internal-helper-token"),
            ("GIT_VISTA_GITHUB_TOKEN", "gv-source-token"),
            ("GH_TOKEN", "gh-source-token"),
            ("SSH_AUTH_SOCK", "/tmp/ssh-XXXX/agent.1"),
            ("AWS_SECRET_ACCESS_KEY", "a-credential-nobody-enumerated"),
        ];

        let built = untrusted_checkout_env(source);
        let names: Vec<String> = built
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();

        // The paired positive: the child is not simply empty. An allowlist
        // that returned nothing would satisfy every "must be absent" leg
        // below while producing a checkout that cannot even find git.
        assert_eq!(
            names,
            vec!["PATH", "HOME", "LANG"],
            "exactly the allowlisted names present in the source, in source order"
        );

        // And pin the constant itself, not just its effect on this source.
        // Reviewed on #720: the assertion above supplies only three allowed
        // names, so DELETING `XDG_CONFIG_HOME` or a locale variable changes
        // real behaviour — config resolution and message encoding — while
        // every leg here stays green. A list is a security decision; its
        // contents are pinned, in full, and a change to it has to be a
        // deliberate edit in two places.
        assert_eq!(
            UNTRUSTED_CHECKOUT_ENV_ALLOWLIST,
            [
                "PATH",
                "HOME",
                "XDG_CONFIG_HOME",
                "LANG",
                "LC_ALL",
                "LC_CTYPE",
                "LC_MESSAGES",
            ],
            "the allowlist is the boundary; adding a name is a security decision and \
             removing one is a behaviour change, so neither may happen silently"
        );
        for withheld in [
            CREDENTIAL_TOKEN_VAR,
            "GIT_VISTA_GITHUB_TOKEN",
            "GH_TOKEN",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(
                !names.iter().any(|n| n == withheld),
                "{withheld} reached an untrusted checkout child; the environment is being \
                 filtered by name rather than built by allowlist"
            );
        }
    }

    /// An allowlisted name that the source does not have must not be
    /// fabricated. Without this, an implementation that wrote every
    /// allowlisted name with an empty value would pass the test above while
    /// handing git `HOME=""` — which it reads as a real, and wrong, answer.
    #[test]
    fn an_allowlisted_name_absent_from_the_source_stays_absent() {
        let built = untrusted_checkout_env([("PATH", "/usr/bin")]);
        assert_eq!(built.len(), 1, "got {built:?}");
        assert_eq!(built[0].0, std::ffi::OsStr::new("PATH"));
    }

    /// The wiring half: the production builder really applies the allowlist to
    /// the composed command, and really clears first. Read off the composed
    /// `Command` for the reason `credential_env_for_test` documents — a
    /// spawned child cannot testify about a step a pinned test profile would
    /// overwrite.
    #[test]
    fn the_production_builder_clears_the_environment_before_applying_the_allowlist() {
        let repo = std::path::PathBuf::from("/srv/repo");
        let policy = production_policy(&repo);
        let command =
            command_async(&policy, &repo, &["checkout", "-f"]).with_untrusted_checkout_env();

        assert!(
            command.command.as_std().get_envs().all(|(key, value)| {
                value.is_some()
                    && UNTRUSTED_CHECKOUT_ENV_ALLOWLIST
                        .iter()
                        .any(|allowed| key == std::ffi::OsStr::new(allowed))
            }),
            "every override on an untrusted checkout command must be an allowlisted \
             name carrying a value; a `None` here would mean the builder is still \
             removing names from an inherited environment"
        );
        // `env_clear()` itself has no accessor on `Command`, so this test
        // deliberately stops short of claiming it. The half it cannot see is
        // proved by a real spawn instead:
        // `handlers::clone`'s `clone_checkout_runs_the_hook_with_only_an_allowlisted_environment`
        // reads the child's whole environment back out of a running hook.
        assert!(
            command
                .command
                .as_std()
                .get_envs()
                .any(|(key, _)| key == std::ffi::OsStr::new("PATH")),
            "PATH must be supplied explicitly, not left to inheritance — after \
             env_clear() an unsupplied PATH means the shim cannot exec git at all"
        );
    }

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

    /// #757: `wrap_with_reaper`'s own selection logic, pinned directly rather
    /// than only observed through the process-tree acceptance tests in
    /// `sandbox::lifecycle` (real but slow, and — as this test's own history
    /// shows — not actually able to see every way the selection could drift:
    /// a `failure-atlas` mutation that widened `is_network_shim_launch` to
    /// match ANY non-empty argv, wrapping `Tier::Unsandboxed`'s bare `git`
    /// too, survived every `sandbox::lifecycle` test, because none of them
    /// ever exercise that tier through `wrap_with_reaper` at all).
    ///
    /// Three raw argv shapes, matching INV-16's three exhaustive outputs
    /// (`sandbox_argv_with_seccomp_profile`'s own doc comment) rather than
    /// built through a real `Policy` — this test's whole claim is about the
    /// structural comparison inside `wrap_with_reaper` itself, which reads
    /// only `argv[0]`, so a raw argv is the more direct fixture, not a
    /// shortcut around one.
    ///
    /// MUTATION 1 (remove the mechanism): delete the `shim_path()` comparison
    /// (`is_network_shim_launch = false`). RED here on the network leg alone
    /// — `failure-atlas` confirmed this 2026-09-08 (verdict: caught).
    /// MUTATION 2 (weaken the mechanism): widen the comparison to
    /// `argv.first().is_some()`. RED here on the unsandboxed leg — this is
    /// the exact mutation that survived every `sandbox::lifecycle` test
    /// before this one existed (`failure-atlas`, 2026-09-08, verdict:
    /// survived), which is why this test exists rather than resting on that
    /// suite alone.
    #[test]
    fn wrap_with_reaper_recognizes_exactly_the_two_sandboxed_launcher_shapes() {
        let reaper = super::super::reaper::reaper_path().unwrap_or_else(|| {
            panic!(
                "gv-sandbox-reaper must be built and resolvable, or this test proves \
                 nothing about the mechanism under test — see tests/forces_reaper_build.rs"
            )
        });
        let bwrap = super::super::bwrap::bwrap_path().unwrap_or_else(|| {
            panic!("bwrap must be resolvable on this host, or the Strict leg proves nothing")
        });
        let shim = super::super::shim::shim_path().unwrap_or_else(|e| {
            panic!(
                "gv-sandbox must be built and resolvable, or this test proves nothing \
                 about the mechanism under test: {e}"
            )
        });

        let strict_argv = vec![
            bwrap.as_os_str().to_os_string(),
            std::ffi::OsString::from("--"),
            std::ffi::OsString::from("git"),
        ];
        let network_argv = vec![
            shim.as_os_str().to_os_string(),
            std::ffi::OsString::from("--"),
            std::ffi::OsString::from("git"),
        ];
        let unsandboxed_argv = vec![std::ffi::OsString::from("git")];

        assert_eq!(
            wrap_with_reaper(strict_argv.clone()).first(),
            Some(&reaper.as_os_str().to_os_string()),
            "a Strict (bwrap-prefixed) argv must be wrapped with the reaper"
        );
        assert_eq!(
            wrap_with_reaper(network_argv.clone()).first(),
            Some(&reaper.as_os_str().to_os_string()),
            "#757: a Network (bare-shim) argv must be wrapped with the reaper too"
        );
        assert_eq!(
            wrap_with_reaper(unsandboxed_argv.clone()),
            unsandboxed_argv,
            "Tier::Unsandboxed's bare `git` argv must never be wrapped with the \
             reaper (INV-16 shapes 1/2): that operation is already explicit, \
             persisted, operator-trusted content flying a permanent banner (INV-15)"
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
    /// are retained by this ordinary-command constructor for the documented
    /// user-git-parity decision. Untrusted clone checkout overrides the two
    /// config scopes later, at completion. A construction-time scrub that grew
    /// to swallow them from every command would be a different change than the
    /// checkout-specific policy reviewed here.
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
            .command
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_os_string())
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
