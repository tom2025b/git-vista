//! #66 (M1.13b) Task 25, step 3: the anti-vacuity contract's harness and
//! source tripwires.
//!
//! See `docs/sandbox/escape-battery-anti-vacuity-contract.md` (the
//! specification this file implements) and `.claude/parallel/pro-task.md`
//! (why it must land before any case is rewritten). This file has two jobs,
//! deliberately in one place so they cannot drift apart:
//!
//! 1. **The harness.** `EscapeCase`, `Outcome`, the `Result`-returning
//!    probe-output parser, the nonce `GVPROBE … BEGIN/END` substitution,
//!    `production_env_profile()`, and `run_case()` — everything step 5's
//!    case rewrite needs and nothing it should have to reinvent per case.
//! 2. **The tripwires.** One `#[test]` per contract rule (R1, R2, R3, R4,
//!    R6, R7, R8, R10, R11), each scanning the *source* of the battery files
//!    (`escape_suite.rs`, `hook_mode_suite.rs`) rather than trusting a
//!    reviewer to notice a hand-written acceptance condition. R1 is why this
//!    works at all: once a battery file may contain nothing but `const
//!    CASE_X: EscapeCase` declarations and `run_case(&CASE_X)` bodies outside
//!    one `mod harness`, "does this file contain freeform Rust" becomes a
//!    grep, not a judgement call.
//!
//! # This file is meant to fail today
//!
//! `escape_suite.rs` has not been rewritten yet (that is step 5, blocked on
//! step 4). Several tripwires here — R1, R2, R4, R6, R7 — scan it for
//! properties it does not have yet and are *expected* to fail loudly until
//! the rewrite lands. That is the point: a tripwire that cannot reject the
//! current file has not been shown to reject anything. R8 and R10 do not
//! depend on the rewrite and pass today.

#![allow(dead_code)] // Nothing outside this file calls `run_case` until step 5. See the
                     // module doc above; remove once escape_suite.rs / hook_mode_suite.rs
                     // wire cases through it (mirrors the sandbox/mod.rs Task-1 precedent).

use super::*;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

// =========================================================================
// Part 1: the harness
// =========================================================================

/// A single named errno constant. Never a set, a predicate, or a string match
/// (R2) — the only operation ever performed on one is `assert_eq!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Errno(pub i32);

/// Kernel state captured by the probe beside the operation's errno.
///
/// `NotApplicable` is a typed claim, not a parser default. It is reserved for
/// the blocked-hooks functional case: the inside hook deliberately never runs,
/// so no paired kernel observation exists. A line carrying provenance-looking
/// text for such a case is rejected rather than promoted to evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provenance {
    Kernel { seccomp: i32, no_new_privs: i32 },
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Observation {
    errno: i32,
    provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Containment,
    Functional,
}

/// R9's mutant set (the mutation-matrix driver, step 6, is not this task —
/// this enum only needs to exist so `EscapeCase::dies_under` has a type to
/// name, per step 3's "land every mod declaration the later steps need"
/// instruction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutantId {
    M1,
    M2,
    M3,
    M4,
    M5,
    M6,
    M7,
    /// `ci/mutants/M8-remove-af-unix-socket-rule.patch` — removes *only* the
    /// Strict tier's `socket`/`socketpair` AF_UNIX rules from
    /// `seccomp_filter::rules_for`, leaving the rest of the filter installed.
    /// M1 (whole filter emptied) would kill an AF_UNIX case too, but only M8
    /// shows the case notices its **own** mechanism rather than the filter's
    /// existence.
    M8,
    /// `ci/mutants/M9-widen-af-unix-comparison.patch` — widens the AF_UNIX
    /// rules' arg0 comparison from `Dword` to `Qword`, exactly as M7 does to the
    /// sibling `prctl` rule.
    ///
    /// The width was measured correct when the rule landed, and a measurement
    /// only ever proves today's code. M7 exists because this project already
    /// shipped this defect once; without a mutant on the AF_UNIX rule the same
    /// class can reopen there with the whole battery green, since every existing
    /// AF_UNIX case constructs its family as a 32-bit `int` and cannot tell a
    /// `Dword` comparison from a `Qword` one. `high_bit_af_unix_denied` is the
    /// case that can.
    M9,
    /// `ci/mutants/M10-allow-io-uring.patch` — removes *only* the three
    /// `io_uring_setup`/`enter`/`register` entries from
    /// `seccomp_filter::denied_outright`, leaving every other denial — including
    /// the Strict tier's AF_UNIX `socket`/`socketpair` rules — installed.
    ///
    /// M1 (whole filter emptied) kills the io_uring cases too, and cannot tell
    /// the two mechanisms apart. M10 is what makes
    /// `uring_socket_bypass_denied`'s claim mechanical rather than editorial:
    /// with the AF_UNIX rules **demonstrably still in force**, the probe still
    /// obtains an `AF_UNIX` socket through `IORING_OP_SOCKET`. That is the whole
    /// sub-claim — a seccomp rule keyed on `socket(2)`'s first argument does not
    /// reach a socket io_uring creates on the process's behalf, so the io_uring
    /// denial, and not the AF_UNIX rule, is what closes that path.
    M10,
    /// `ci/mutants/M11-empty-ssh-known-hosts-carveout.patch` (#188) — empties
    /// *only* `sandbox::ssh_known_hosts_carveout`, leaving `secret_excludes`,
    /// Landlock enforcement and every other mechanism untouched.
    ///
    /// M2 (Landlock never restricted) and M3 (`secret_excludes_for_home`
    /// emptied) both also kill `ssh_known_hosts_carveout`'s case, but neither
    /// is specific to the #188 grant: M2 breaks every containment case in the
    /// battery, and M3 breaks every secret, not just `known_hosts`. M11 is
    /// what makes the case's claim mechanical rather than editorial — with
    /// `secret_excludes` and Landlock enforcement **demonstrably still
    /// intact**, only the one function that computes the carve-out is gone,
    /// and only a case whose `GRANTED` leg actually depends on that grant
    /// (not merely on secrets-in-general staying excluded) notices.
    M11,
}

/// R8: a case whose configuration production cannot build yet carries the
/// *named* blocker, checked against source so its disappearance forces the
/// exemption to be retired rather than quietly outliving its own reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exemption {
    None,
    NotProductionReachable { blocker: &'static str },
}

/// How a case relates to TCP port 9418 — the one port a Network-tier Landlock
/// connect grant covers (the only unprivileged entry in `DEFAULT_GIT_PORTS`),
/// and therefore the one port several unrelated tests in this binary must
/// share. The harness turns this into a `test_ports::PortClaim` held for
/// exactly one `execute` call; a case never touches the port itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitPortUse {
    /// The case never touches 9418.
    Unused,
    /// The case's probe needs 9418 to itself and binds nothing on the host
    /// side — e.g. a baseline leg that proves a TCP bind is possible by
    /// performing it. Claiming without binding is the point: a listener here
    /// would make the baseline fail with `EADDRINUSE`, which `run_case` would
    /// report as a silently-vacuous `CapabilityAbsent`.
    Exclusive,
    /// The case's probe connects to 9418, so the harness also holds a loopback
    /// listener there for the duration of the baseline leg.
    ExclusiveWithListener,
}

/// What `run_case` handed the case's `build_hook` function: everything it
/// needs to produce a hook body and nothing it could use to bypass the
/// harness (no policy, no shim path — those are the harness's job).
pub(crate) struct HarnessCtx<'a> {
    pub repo: &'a Path,
    pub nonce: &'a str,
    /// `Some` only for a `GitPortUse::ExclusiveWithListener` case: the port of
    /// the harness-owned loopback listener the probe should connect to. The
    /// harness binds and releases it, so no `build_hook` can outlive it.
    pub listener_port: Option<u16>,
}

/// One battery case. Every field is written out per case (no `Default`, no
/// `..Default::default()` — R1) so a reviewer sees every claim a case makes
/// in one literal, never inherits one silently.
pub(crate) struct EscapeCase {
    pub id: &'static str,
    pub class: Class,
    pub tier: Tier,
    /// Whether the policy under test blocks hooks (`HookMode::Blocked`) — the
    /// functional case, not a containment claim.
    pub hooks_blocked: bool,
    /// Build the pre-commit hook body for one leg. Already `GVPROBE <nonce>
    /// BEGIN`/`END`-wrapped; `run_case` does not touch its output before
    /// parsing. Case-specific setup (compiling a C probe, wiring a listener)
    /// belongs in the battery file's own `mod harness`, called from here —
    /// never in the case region a `const` sits in (R1).
    pub build_hook: fn(&HarnessCtx) -> String,
    /// The tag the denial/functional observation line carries, e.g.
    /// `"IOURING"`, `"HIGHBIT"`, `"CONNECT"`, `"SECRET"`. The paired positive
    /// (R3) always carries the fixed tag `"GRANTED"` — one convention, so
    /// `run_case` never needs a second per-case field to find it.
    pub probe_tag: &'static str,
    /// R4: the errno the **baseline** (outside the sandbox) leg must observe
    /// for the operation to count as possible on this host at all. A
    /// baseline that misses this becomes `Outcome::CapabilityAbsent` — a
    /// return value, never a skip decided by the case.
    pub expect_baseline: Errno,
    /// Acceptance evidence F, written as exact literals in every case rather
    /// than derived from the producer. Applicable baseline probes must report
    /// the unsandboxed kernel state (`Seccomp: 0`, `NoNewPrivs: 0`).
    pub expect_baseline_provenance: Provenance,
    /// The errno the **inside** leg must observe for containment to hold.
    pub expect_inside: Errno,
    /// Applicable inside probes must report the sandboxed kernel state
    /// (`Seccomp: 2`, `NoNewPrivs: 1`).
    pub expect_inside_provenance: Provenance,
    /// R3: the paired positive, mandatory on every case. A sibling operation,
    /// same run, same policy, same probe binary, that must still succeed —
    /// without this a denial claim is unattributable (see the contract's
    /// `enumerate()`-is-omission argument).
    pub expect_granted: Errno,
    /// The paired positive is emitted by the same inside process and therefore
    /// carries its own exact, mandatory provenance assertion.
    pub expect_granted_provenance: Provenance,
    /// R2: the commit's own exit status, asserted in both legs. Distinguishes
    /// "the hook ran and observed something" from "the commit never reached
    /// hook discovery at all."
    pub expect_carrier_code: i32,
    /// R9: at least one entry: the mutant(s) this case's mutation-matrix cell
    /// must go red under (step 6 wires the driver; the field exists now so
    /// step 5 can declare it without step 6 having landed).
    pub dies_under: &'static [MutantId],
    /// R8: `Exemption::None` for a production-constructible configuration;
    /// otherwise the named, source-checked blocker.
    pub exemption: Exemption,
    /// Whether this case needs exclusive use of TCP 9418, and whether the
    /// harness must hold a listener there. Written out per case like every
    /// other field (R1): a case that quietly inherited `Unused` while its probe
    /// touched the port would race the other holders in this binary, and the
    /// loser's symptom is a *passing* vacuous run, not a red test.
    pub git_port: GitPortUse,
}

/// R4's non-panicking, non-early-return outcome. `run_case` always computes
/// one of these and always records it (R5) before ever raising an assertion
/// failure for `Escaped` — the *tests* never decide severity (see the
/// contract's "Skip policy" section); this type is what lets them not have
/// to.
#[derive(Debug)]
pub(crate) enum Outcome {
    Contained,
    Escaped { detail: String },
    CapabilityAbsent { case: &'static str, missing: String },
}

/// R2: `Result`, never `Option<i32>` — a missing observation and "observed
/// zero" must be distinguishable types, not both foldable into `None`.
#[derive(Debug)]
pub(crate) struct MissingObservation {
    pub detail: String,
}

/// Parse `tag`'s `"<tag> rc=<n> errno=<n>"` line, but only from *inside* a
/// `GVPROBE <nonce> BEGIN` / `GVPROBE <nonce> END` pair — both markers with
/// the matching nonce are required before any expectation is evaluated (R2).
/// This is what removes the `None`-means-either-of-six-things ambiguity
/// `errno_for` had in the pre-contract battery, with no shim change: a
/// missing marker is a `MissingObservation`, not a silently-satisfied
/// `assert_ne!(.., Some(0))`.
pub(crate) fn parse_observation(
    out: &str,
    nonce: &str,
    tag: &str,
    provenance: Provenance,
) -> Result<Observation, MissingObservation> {
    let begin = format!("GVPROBE {nonce} BEGIN");
    let end = format!("GVPROBE {nonce} END");
    let Some(start) = out.find(&begin) else {
        return Err(MissingObservation {
            detail: format!("no `{begin}` in the combined output"),
        });
    };
    let Some(stop) = out.find(&end) else {
        return Err(MissingObservation {
            detail: format!("no `{end}` in the combined output"),
        });
    };
    if stop < start {
        return Err(MissingObservation {
            detail: format!("`{end}` precedes `{begin}` — markers out of order"),
        });
    }
    let block = &out[start + begin.len()..stop];
    for line in block.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(tag) else {
            continue;
        };
        let Some(after_rc) = rest.trim_start().strip_prefix("rc=") else {
            continue;
        };
        let Some(errno_part) = after_rc.split("errno=").nth(1) else {
            continue;
        };
        let Some(tok) = errno_part.split_whitespace().next() else {
            continue;
        };
        let Ok(errno) = tok.parse::<i32>() else {
            continue;
        };
        let provenance = match provenance {
            Provenance::Kernel { .. } => {
                let seccomp = parse_i32_field(line, tag, "Seccomp:")?;
                let no_new_privs = parse_i32_field(line, tag, "NoNewPrivs:")?;
                Provenance::Kernel {
                    seccomp,
                    no_new_privs,
                }
            }
            Provenance::NotApplicable => {
                if line.contains("Seccomp:") || line.contains("NoNewPrivs:") {
                    return Err(MissingObservation {
                        detail: format!(
                            "`{tag}` declares Provenance::NotApplicable but its observation \
                             contains provenance-looking text"
                        ),
                    });
                }
                Provenance::NotApplicable
            }
        };
        return Ok(Observation { errno, provenance });
    }
    Err(MissingObservation {
        detail: format!("no `{tag} rc=.. errno=..` line inside the marked block"),
    })
}

fn parse_i32_field(line: &str, tag: &str, field: &str) -> Result<i32, MissingObservation> {
    let Some((_, tail)) = line.split_once(field) else {
        return Err(MissingObservation {
            detail: format!(
                "`{tag}` observation is missing mandatory `{field} <n>` kernel provenance"
            ),
        });
    };
    let Some(token) = tail.split_whitespace().next() else {
        return Err(MissingObservation {
            detail: format!("`{tag}` observation has no value after mandatory `{field}`"),
        });
    };
    token.parse::<i32>().map_err(|_| MissingObservation {
        detail: format!("`{tag}` observation has non-integer `{field} {token}`"),
    })
}

#[test]
fn observation_parser_requires_kernel_provenance_on_the_observation_line() {
    let out = "\
Seccomp: 2 NoNewPrivs: 1
GVPROBE wanted BEGIN
SECRET rc=-1 errno=13
Seccomp: 2 NoNewPrivs: 1
GVPROBE wanted END
";
    let err = parse_observation(
        out,
        "wanted",
        "SECRET",
        Provenance::Kernel {
            seccomp: 2,
            no_new_privs: 1,
        },
    )
    .expect_err("stray and split provenance must not satisfy SECRET");
    assert!(
        err.detail.contains("Seccomp:"),
        "the missing field must be named: {}",
        err.detail
    );
}

#[test]
fn observation_parser_binds_provenance_to_the_matching_nonce_block() {
    let out = "\
GVPROBE wrong BEGIN
SECRET rc=-1 errno=13 Seccomp: 2 NoNewPrivs: 1
GVPROBE wrong END
GVPROBE wanted BEGIN
SECRET rc=-1 errno=13
GVPROBE wanted END
";
    let err = parse_observation(
        out,
        "wanted",
        "SECRET",
        Provenance::Kernel {
            seccomp: 2,
            no_new_privs: 1,
        },
    )
    .expect_err("another nonce's provenance must not satisfy this observation");
    assert!(
        err.detail.contains("Seccomp:"),
        "the missing field must be named: {}",
        err.detail
    );
}

#[test]
fn observation_parser_rejects_fabricated_provenance_when_not_applicable() {
    let out = "\
GVPROBE wanted BEGIN
HOOK rc=0 errno=0 Seccomp: 0 NoNewPrivs: 0
GVPROBE wanted END
";
    let err = parse_observation(out, "wanted", "HOOK", Provenance::NotApplicable)
        .expect_err("NotApplicable must reject provenance-looking text");
    assert!(
        err.detail.contains("NotApplicable"),
        "the typed exemption must be named: {}",
        err.detail
    );
}

/// A fresh per-invocation nonce. Generated by the harness and substituted
/// into both legs' hook bodies via `HarnessCtx::nonce` — never baked into a
/// case's `const` declaration, since a fixed nonce would let two concurrent
/// `cargo test` threads collide on the same marker text.
fn fresh_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}", std::process::id(), n)
}

/// The pinned, reviewed environment both legs run under (R7). Not the
/// developer's shell — `spawn.rs` deliberately does not touch env, so
/// something has to, and R7's whole argument is that "something" must be one
/// reviewed function rather than an inherited environment nobody chose. `HOME`
/// is read here, once, because every acceptance claim in this battery depends
/// on reaching `~/.gitconfig` through the enumerated `$HOME` grant — this is
/// harness code, not the battery region R4 restricts.
pub(crate) fn production_env_profile() -> Vec<(&'static str, String)> {
    let home = std::env::var("HOME").expect("HOME must be set to run the escape battery");
    vec![
        ("PATH", "/usr/bin:/bin".to_string()),
        ("HOME", home),
        // The same two GIT_* variables production sets globally (main.rs);
        // asserted to be the server's *entire* GIT_* surface by
        // `r7_both_legs_share_one_pinned_environment_profile` below.
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
        ("GIT_EDITOR", "true".to_string()),
    ]
}

struct HookRun {
    commit_code: i32,
    combined: String,
}

fn install_hook(repo: &Path, body: &str) {
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\n{body}\n")).expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod hook");
}

/// A repository with a seed commit and no local identity — so a hook that
/// needs an author must reach `~/.gitconfig` through the policy under test.
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    let p = d.path();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["commit", "-q", "--allow-empty", "-m", "seed"],
    ] {
        let ok = std::process::Command::new("git")
            .args(&args)
            .current_dir(p)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", std::env::var("HOME").expect("HOME"))
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "fixture setup failed: git {args:?}");
    }
    d
}

/// A throwaway repository whose `pre-commit` hook is `script` — the single
/// constructor the lifecycle (Task 12), non-coverage (Task 13) and
/// compatibility (Task 14) batteries all name.
///
/// **Composed, never duplicated.** It is exactly `fixture()` followed by
/// `install_hook()`, the same two pieces `execute` uses for both of its own
/// legs, so a hostile-hook repository built by another battery is
/// byte-for-byte the repository this one runs its containment claims against:
/// the same seed commit, the same deliberately absent local identity (a hook
/// that needs an author must therefore reach `~/.gitconfig` *through the policy
/// under test*), and the same `#!/bin/sh` + `0755` hook wrapper. A second
/// constructor that drifted on any of those would silently change what a
/// neighbouring battery's "same fixture" claim means.
///
/// Re-exported from `escape_suite` (`pub(crate) use`) so those tasks'
/// `use super::escape_suite::hostile_hook_repo;` resolves as written, without
/// any of them reaching into the harness for `fixture`/`install_hook`
/// separately and re-pairing them by hand.
pub(crate) fn hostile_hook_repo(script: &str) -> tempfile::TempDir {
    let repo = fixture();
    install_hook(repo.path(), script);
    repo
}

fn run_git_outside(repo: &Path, args: &[&str], env: &[(&str, String)]) -> (i32, String) {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git runs");
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Fire the hook **outside** the sandbox — proves the probe is possible on
/// this host at all (R4's capability signal).
fn commit_outside(repo: &Path) -> HookRun {
    let env = production_env_profile();
    std::fs::write(repo.join("payload.txt"), "x").expect("write payload");
    let _ = run_git_outside(repo, &["add", "-A"], &env);
    let (code, combined) = run_git_outside(repo, &["commit", "-q", "-m", "baseline"], &env);
    HookRun {
        commit_code: code,
        combined,
    }
}

/// Fire the hook **inside** the composed launcher, through the one production
/// seam (R6): `sandbox::spawn::command_async` — never `shim_cli::launch`.
/// (It used to say "never `command_sync`" too; that wrapper had no caller in
/// production or in tests and was deleted in Task 6, so `command_async` is now
/// the only seam there is.)
fn commit_inside(policy: &Policy, repo: &Path) -> HookRun {
    let env = production_env_profile();
    std::fs::write(repo.join("payload.txt"), "x").expect("write payload");
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let _ = super::spawn::command_async(policy, repo, &["add", "-A"])
            .pinned_env_for_test(&env)
            .output()
            .await;

        let out = super::spawn::command_async(policy, repo, &["commit", "-q", "-m", "inside"])
            .pinned_env_for_test(&env)
            .output()
            .await
            .expect("the launcher runs");
        HookRun {
            commit_code: out.status.code().unwrap_or(-1),
            combined: format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    })
}

/// R8: builds the policy for a case.
///
/// Production-constructible cases (no exemption) go through the **production
/// dispatch**, with the declared [`NetworkNeed`] taken from the tier the case is
/// written against. That mapping is not a harness knob — it is `tier_for`'s own:
/// a *local* operation on an untrusted repository is `Strict`, a *remote* one is
/// `Network`. Nothing here asserts the tier by construction; the `assert_eq!`
/// below re-reads it off the policy production actually returned, so an edit to
/// `tier_for` or to the trust lookup that re-tiered a case fails the run instead
/// of silently changing what that case proves.
///
/// # Why the Strict cases stopped being exempt (#206)
///
/// Until #197 this function could only reach `policy_for_repo(repo)` — a fixed
/// one-argument entry point with no way to say which tier the case wanted — and
/// before #197 that function *hard-coded* `Tier::Network` besides. Seven Strict
/// cases therefore carried `Exemption::NotProductionReachable`. #197 made the
/// tier a function of the declared need, which made production Strict genuinely
/// reachable; #206 retired those seven exemptions by teaching this function to
/// declare that need. The exemption was the only thing standing between those
/// cases and the production path, and it is gone rather than reworded.
///
/// Network-tier cases keep routing through `policy_for_repo`, deliberately. It
/// *is* `policy_for(repo, false, NetworkNeed::Remote)` plus a self-check (see
/// its own doc comment), so this is the same policy by construction — and
/// keeping the nine already-green cases on the entry point they have always used
/// means retiring the Strict exemptions changed nothing about them.
///
/// # The one exemption left
///
/// `hook_mode_suite`'s `blocked_hooks` is still built here, in the harness,
/// because **no production policy constructor yields `HookMode::Blocked`** —
/// `policy_for`, `policy_for_clone` and `probe::boot_probe_policy` all spell
/// `HookMode::Run`, and ADR 0029 rejects the degrade-and-block posture by name.
/// That is the blocker `r8_exemptions_expire_when_their_named_blocker_disappears`
/// checks, over production source, and it is the condition that has to disappear
/// before this last exemption can be retired.
///
/// This function contains a `Policy { .. }` literal on purpose — R6's ban on
/// that literal scopes to `escape_suite.rs`/`hook_mode_suite.rs`, not to the
/// harness that serves them, because the point of R6 is that the *battery*
/// cannot fabricate its own policy; the harness fabricating one for a shape
/// production cannot express, in one reviewed place, is exactly R8's expiring
/// exemption.
fn policy_for_case(case: &EscapeCase, repo: &Path) -> Policy {
    if case.exemption == Exemption::None {
        let policy = match case.tier {
            Tier::Network => policy_for_repo(repo)
                .expect("policy_for_repo must build for a case with no R8 exemption"),
            Tier::Strict => policy_for(repo, false, NetworkNeed::Local).unwrap_or_else(|e| {
                panic!(
                    "{}: production policy_for(.., NetworkNeed::Local) refused to build \
                     the Strict tier on this host ({e:?}). The CI preflight asserts \
                     bwrap, unprivileged user namespaces and the Landlock floor before \
                     any case runs, so this is a real refusal (INV-13 / ADR 0029), never \
                     a reason to fall back to a harness-built policy.",
                    case.id
                )
            }),
            Tier::Unsandboxed => panic!(
                "{}: no battery case may declare Tier::Unsandboxed — it installs no \
                 ruleset at all, so a containment claim written against it is vacuous \
                 by construction",
                case.id
            ),
        };
        assert_eq!(
            policy.tier, case.tier,
            "{}: the production dispatch returned a tier the case is not written \
             against — the case would have gone on passing while proving something \
             about a different sandbox",
            case.id
        );
        return policy;
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let (mut rw, mut ro) = default_system_trees(case.tier);
    rw.push(repo.to_path_buf());
    ro.push(home.clone());
    let bwrap = if case.tier == Tier::Strict {
        bwrap::bwrap_path().map(Path::to_path_buf)
    } else {
        None
    };
    let shim = shim::shim_path()
        .expect("gv-sandbox must be built; tests/forces_shim_build.rs ensures it")
        .to_path_buf();
    Policy {
        tier: case.tier,
        shim,
        bwrap,
        rw_trees: rw,
        ro_trees: ro,
        secret_excludes: secret_excludes_for_home(&home),
        // #188 is Network-tier only. The one case still built through this
        // harness branch (`hook_mode_suite`'s `blocked_hooks`) is
        // `Tier::Strict` — see this function's own doc comment — so there is
        // nothing to carve out for any configuration this branch builds
        // today; written as a real per-tier match rather than a bare
        // `Vec::new()` so a future Network-tier exemption does not silently
        // inherit an empty carve-out set the way a `..` default would.
        ro_carveouts: match case.tier {
            Tier::Network => ssh_known_hosts_carveout(&home),
            Tier::Strict | Tier::Unsandboxed => Vec::new(),
        },
        net_ports: if case.tier == Tier::Network {
            DEFAULT_GIT_PORTS.to_vec()
        } else {
            Vec::new()
        },
        hook_mode: if case.hooks_blocked {
            HookMode::Blocked {
                empty_dir: leaked_empty_dir(),
            }
        } else {
            HookMode::Run
        },
    }
}

/// A tempdir whose handle is intentionally leaked: `HookMode::Blocked` needs
/// a path that outlives the policy, and the process is short-lived per test.
fn leaked_empty_dir() -> PathBuf {
    let d = tempfile::tempdir().expect("empty dir");
    let p = d.path().to_path_buf();
    std::mem::forget(d);
    p
}

/// Append one line to `$GV_ESCAPE_REPORT` (R5). Silently does nothing if the
/// variable is unset — this is what makes an unset variable in CI fail
/// *closed*: the gating job's "the report file must exist" assertion is what
/// turns that into a red build, not an in-Rust severity switch. Embedded
/// newlines in `missing` are flattened so the file stays one record per line.
fn report(case: &EscapeCase, outcome: &Outcome) {
    let Some(path) = std::env::var_os("GV_ESCAPE_REPORT") else {
        return;
    };
    let class = match case.class {
        Class::Containment => "containment",
        Class::Functional => "functional",
    };
    let result = match outcome {
        Outcome::Contained => "contained".to_string(),
        Outcome::Escaped { .. } => "escaped".to_string(),
        Outcome::CapabilityAbsent { missing, .. } => {
            format!("capability-absent:{}", missing.replace('\n', " "))
        }
    };
    let line = format!(
        "GV-ESCAPE case={} result={} class={}\n",
        case.id, result, class
    );
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("open $GV_ESCAPE_REPORT ({path:?}): {e}"));
    f.write_all(line.as_bytes()).expect("write report line");
}

/// One case's exclusive hold on TCP 9418, scoped to one `execute` call.
///
/// Bounded lifetime is the whole point. The pre-contract battery bound its
/// listener through a `static OnceLock` and parked a thread in a blocking
/// `accept()`; under a *passing* denial case no connection ever arrives, so the
/// thread never returned, the listener was never dropped, and the port stayed
/// held for the rest of the process — which is what made this test and
/// `planner::contract_suite`'s `git daemon` push test mutually exclusive.
struct GitProtocolPort {
    /// `Some` when a listener is bound — the value handed to `HarnessCtx`.
    port: Option<u16>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    accepting: Option<std::thread::JoinHandle<()>>,
    /// Declared last so it is released only after `drop` has joined the accept
    /// thread: the next claimant must never see the listener still bound.
    _claim: crate::test_ports::PortClaim,
}

impl GitProtocolPort {
    /// The accept loop has **no wall-clock deadline**, deliberately.
    ///
    /// An earlier version gave it 60 s "so a forgotten thread cannot outlive the
    /// run." That was the wrong trade, and it recreated the disease this whole
    /// contract exists to prevent. Between the moment this binds and the moment
    /// the baseline leg connects, `execute` builds two fixture repositories,
    /// compiles a C probe, and runs a `git commit` — and on a loaded host (the
    /// mutation matrix rebuilds two crates seven times while this can be
    /// running) that window can exceed any constant someone picks. If the
    /// deadline fires first, the listener closes early, the baseline connect
    /// gets `ECONNREFUSED` instead of the expected `0`, the case reports
    /// `CapabilityAbsent`, and it proves nothing. A timeout that turns a slow
    /// machine into a silently vacuous security test is worse than no timeout.
    ///
    /// The thread's lifetime is already bounded by ownership, which is the
    /// honest mechanism: the lease is held for exactly the body of `execute`,
    /// and `Drop` sets the stop flag and **joins** the thread before releasing
    /// the port claim. A leaked lease is the only way this thread could outlive
    /// the run, and a leaked lease would hold the `PortClaim` mutex too — which
    /// the next claimant reports loudly rather than hanging on silently.
    const POLL: std::time::Duration = std::time::Duration::from_millis(10);

    fn claim(use_: GitPortUse) -> Option<Self> {
        use std::sync::atomic::{AtomicBool, Ordering};
        if use_ == GitPortUse::Unused {
            return None;
        }
        let claim = crate::test_ports::PortClaim::acquire();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, accepting) = if use_ == GitPortUse::ExclusiveWithListener {
            let listener =
                std::net::TcpListener::bind(("127.0.0.1", crate::test_ports::PortClaim::PORT))
                    .expect("bind git protocol listener");
            let port = listener.local_addr().expect("listener address").port();
            listener
                .set_nonblocking(true)
                .expect("non-blocking listener");
            let stop_thread = std::sync::Arc::clone(&stop);
            let accepting = std::thread::spawn(move || {
                while !stop_thread.load(Ordering::Relaxed) {
                    match listener.accept() {
                        // Serve exactly one connection, then drop the listener
                        // — the baseline leg's connect has landed and the port
                        // must be closed again, so the sandboxed inside leg
                        // observes a denial (EACCES) rather than a success, and
                        // a mutant that removes the denial observes
                        // ECONNREFUSED rather than errno 0.
                        Ok(_) => return,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Self::POLL);
                        }
                        Err(_) => return,
                    }
                }
            });
            (Some(port), Some(accepting))
        } else {
            (None, None)
        };
        Some(Self {
            port,
            stop,
            accepting,
            _claim: claim,
        })
    }
}

impl Drop for GitProtocolPort {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.accepting.take() {
            // Joining is what makes the release honest: the thread owns the
            // listener, so only its exit closes the port. `_claim` is dropped
            // after this body returns.
            let _ = t.join();
        }
    }
}

fn observation_mismatch(
    leg: &str,
    tag: &str,
    observed: Observation,
    expect_errno: Errno,
    expect_provenance: Provenance,
) -> Option<String> {
    if observed.errno != expect_errno.0 {
        return Some(format!(
            "{leg} {tag} wanted errno {} got {}",
            expect_errno.0, observed.errno
        ));
    }
    match (expect_provenance, observed.provenance) {
        (
            Provenance::Kernel {
                seccomp: expected, ..
            },
            Provenance::Kernel {
                seccomp: actual, ..
            },
        ) if expected != actual => Some(format!(
            "{leg} {tag} wanted provenance field `Seccomp:` {expected} got {actual}"
        )),
        (
            Provenance::Kernel {
                no_new_privs: expected,
                ..
            },
            Provenance::Kernel {
                no_new_privs: actual,
                ..
            },
        ) if expected != actual => Some(format!(
            "{leg} {tag} wanted provenance field `NoNewPrivs:` {expected} got {actual}"
        )),
        (expected, actual) if expected != actual => Some(format!(
            "{leg} {tag} wanted provenance {expected:?} got {actual:?}"
        )),
        _ => None,
    }
}

fn execute(case: &EscapeCase, nonce: &str) -> Outcome {
    // Held for the whole call — both `build_hook` invocations below bake the
    // port into their hook bodies, and both legs then run against it.
    let git_port = GitProtocolPort::claim(case.git_port);
    let listener_port = git_port.as_ref().and_then(|p| p.port);

    let base_repo = fixture();
    let inside_repo = fixture();

    install_hook(
        base_repo.path(),
        &(case.build_hook)(&HarnessCtx {
            repo: base_repo.path(),
            nonce,
            listener_port,
        }),
    );
    install_hook(
        inside_repo.path(),
        &(case.build_hook)(&HarnessCtx {
            repo: inside_repo.path(),
            nonce,
            listener_port,
        }),
    );

    let baseline = commit_outside(base_repo.path());
    let base_obs = parse_observation(
        &baseline.combined,
        nonce,
        case.probe_tag,
        case.expect_baseline_provenance,
    );
    let base_problem = match base_obs {
        Ok(observed) => observation_mismatch(
            "baseline",
            case.probe_tag,
            observed,
            case.expect_baseline,
            case.expect_baseline_provenance,
        ),
        Err(ref e) => Some(format!(
            "baseline {} observation missing: {}",
            case.probe_tag, e.detail
        )),
    };
    if let Some(missing) = base_problem {
        return Outcome::CapabilityAbsent {
            case: case.id,
            missing,
        };
    }
    assert_eq!(
        baseline.commit_code, case.expect_carrier_code,
        "{}: baseline commit's own exit status drifted from the declared carrier code",
        case.id
    );

    let policy = policy_for_case(case, inside_repo.path());
    let inside = commit_inside(&policy, inside_repo.path());
    assert_eq!(
        inside.commit_code, case.expect_carrier_code,
        "{}: inside-leg commit's own exit status drifted from the declared carrier code",
        case.id
    );

    // A blocked hook cannot emit an inside-leg observation by definition. The
    // functional case therefore observes the hook's exact filesystem effect:
    // ENOENT means the marker was never created, while M6 (ignoring the empty
    // hooks directory) runs the hook and yields errno 0. The already-asserted
    // inside commit status is its paired positive: Git still completed under
    // the same policy even though hook execution was suppressed.
    let (inside_obs, granted_obs) = if case.hooks_blocked {
        if case.expect_inside_provenance != Provenance::NotApplicable
            || case.expect_granted_provenance != Provenance::NotApplicable
        {
            return Outcome::Escaped {
                detail: "blocked hook cannot supply inside or GRANTED kernel provenance; \
                         both declarations must be Provenance::NotApplicable"
                    .to_string(),
            };
        }
        let marker = inside_repo.path().join(".git/gv_escape_hook_ran");
        let observed = match std::fs::metadata(marker) {
            Ok(_) => 0,
            Err(e) => e.raw_os_error().unwrap_or(-1),
        };
        (
            Observation {
                errno: observed,
                provenance: Provenance::NotApplicable,
            },
            Observation {
                errno: inside.commit_code,
                provenance: Provenance::NotApplicable,
            },
        )
    } else {
        let observed = parse_observation(
            &inside.combined,
            nonce,
            case.probe_tag,
            case.expect_inside_provenance,
        )
        .unwrap_or_else(|e| {
            panic!(
                "{}: inside-leg `{}` observation missing: {}",
                case.id, case.probe_tag, e.detail
            )
        });
        let granted = parse_observation(
            &inside.combined,
            nonce,
            "GRANTED",
            case.expect_granted_provenance,
        )
        .unwrap_or_else(|e| {
            panic!(
                "{}: inside-leg GRANTED observation missing (R3): {}",
                case.id, e.detail
            )
        });
        (observed, granted)
    };

    if let Some(detail) = observation_mismatch(
        "inside",
        case.probe_tag,
        inside_obs,
        case.expect_inside,
        case.expect_inside_provenance,
    ) {
        return Outcome::Escaped { detail };
    }
    if let Some(detail) = observation_mismatch(
        "inside",
        "GRANTED",
        granted_obs,
        case.expect_granted,
        case.expect_granted_provenance,
    ) {
        return Outcome::Escaped {
            detail: format!(
                "{detail} — R3's paired positive failed, the policy denied more than the claim"
            ),
        };
    }
    Outcome::Contained
}

/// The chokepoint (R11): every `#[test]` body in the battery is exactly this
/// call. It always records (R5) before ever failing loudly, and it fails loudly
/// for **both** ways a case can stop proving something.
///
/// # Why `CapabilityAbsent` panics too
///
/// It did not, and that silence hid a vacuous case for an unknown number of
/// runs. `strict_tcp_bind_denied`'s baseline leg binds a fixed port to
/// establish the capability; a TIME-WAIT socket left by *any* other test that
/// touched that port made the bind return `EADDRINUSE` instead of the expected
/// `0`, which `execute` reports as `CapabilityAbsent`. `run_case` then returned
/// it quietly, `cargo test` printed `ok`, and the case asserted **nothing about
/// containment** — the exact disease this contract exists to prevent, wearing
/// the costume of a skip.
///
/// The reason a skip looked defensible was "the host might not be able to
/// demonstrate this." That is no longer true here:
/// `ci_preflight_host_meets_the_declared_minimum` asserts the landlock floor,
/// `bwrap`, unprivileged userns, io_uring, and a runnable `cc` *before* any
/// case runs. Once the preflight passes, every capability the battery needs is
/// present — so a `CapabilityAbsent` from a case is a defect in the harness (a
/// contended port, a listener closed too early, a fixture that did not build),
/// not a fact about the host. A harness defect must fail the run, because a
/// green test that proved nothing is worse than a red one.
///
/// R4 is unaffected: capability is still established only by *executing* the
/// baseline leg, never by probing the host and branching. This changes what
/// happens when that execution does not establish it, not how it is
/// established. `GV_ESCAPE_REPORT` still records the outcome first, so the
/// report and the mutation matrix (which already scores `capability-absent` as
/// a non-PASS) keep seeing exactly what happened.
pub(crate) fn run_case(case: &EscapeCase) -> Outcome {
    let nonce = fresh_nonce();
    let outcome = execute(case, &nonce);
    report(case, &outcome);
    match &outcome {
        Outcome::Escaped { detail } => panic!("{}: ESCAPED — {detail}", case.id),
        Outcome::CapabilityAbsent { missing, .. } => panic!(
            "{}: proved NOTHING — {missing}.\nCheck `ci_preflight_host_meets_the_declared \
             _minimum` first: if it PASSED in this same run, the host supplied every \
             prerequisite the preflight knows to demand (Landlock ABI, bwrap, userns, \
             io_uring, cc, and the $HOME paths the fixtures read), and this is a harness \
             defect. If the preflight FAILED, read its message instead — this case is \
             downstream of an unprovisioned runner and fixing the harness would be fixing \
             the wrong thing.\nThat distinction is the whole point, and it has been got \
             wrong once: on run 30633319726 this message claimed the preflight already \
             covered everything while the runner simply had no ~/.ssh/known_hosts. If the \
             cause turns out to be a prerequisite the preflight does not yet name, add it \
             THERE — do not silence this. (See run_case's doc comment for why this is a \
             hard failure rather than a skip.)",
            case.id
        ),
        Outcome::Contained => {}
    }
    outcome
}

// =========================================================================
// Part 2: the tripwires
// =========================================================================

/// R11: every rule above pairs with the name of the test that enforces it.
/// A rule whose enforcement is deleted — renamed, dropped, folded into
/// another test without the old name surviving — fails the build here,
/// rather than only being missed by whoever remembers to check.
const RULES: &[(&str, &str)] = &[
    (
        "R1-DECLARATIVE",
        "r1_case_region_has_no_freeform_control_flow_or_assertions",
    ),
    (
        "R2-EXACT-OBSERVATION",
        "r2_case_region_never_hand_writes_acceptance_conditions",
    ),
    (
        "R3-PAIRED-POSITIVE",
        "r3_every_case_declares_and_asserts_a_paired_positive",
    ),
    (
        "R4-CAPABILITY-BY-EXECUTION",
        "r4_capability_established_only_by_execution_never_by_probing_the_host",
    ),
    (
        "R5-REPORT-FILE-CENSUS",
        "r5_census_names_exactly_the_declared_cases",
    ),
    (
        "R6-PRODUCTION-SEAM",
        "r6_every_inside_leg_spawns_through_the_production_seam",
    ),
    (
        "R7-ONE-ENVIRONMENT",
        "r7_both_legs_share_one_pinned_environment_profile",
    ),
    (
        "R8-EXPIRING-EXEMPTION",
        "r8_exemptions_expire_when_their_named_blocker_disappears",
    ),
    (
        "R10-FLAG-ROUND-TRIP",
        "r10_every_flag_sandbox_argv_emits_has_a_shim_parser_arm",
    ),
    (
        "F-KERNEL-PROVENANCE",
        "f_every_observation_requires_typed_kernel_provenance",
    ),
    // Not an R-rule from the contract document — the same shape as
    // F-KERNEL-PROVENANCE above, an enforcement added after the fact and bound
    // here so it cannot be deleted quietly. Without this entry the CI-
    // environment tripwire is the one check in this file whose removal nothing
    // notices, which is precisely the failure mode it was written to close.
    (
        "CI-HOST-PROVISIONING",
        "every_ci_job_that_runs_this_crates_tests_provisions_the_host_capabilities_they_need",
    ),
];

const BATTERY_FILES: &[&str] = &[
    "src/sandbox/escape_suite.rs",
    "src/sandbox/hook_mode_suite.rs",
];

fn server_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read_rs(rel: &str) -> String {
    std::fs::read_to_string(server_root().join(rel))
        .unwrap_or_else(|e| panic!("{rel} must be readable: {e}"))
}

fn read_self_code_only() -> String {
    crate::argv_boundary::code_only(&read_rs("src/sandbox/escape_contract.rs"))
}

/// Same file, comments blanked but string-literal content intact — needed
/// wherever a check must see an actual quoted value (e.g. `"GRANTED"`) that
/// `code_only` would blank along with everything else inside the quotes.
fn read_self_comments_only() -> String {
    comments_only_blanked(&read_rs("src/sandbox/escape_contract.rs"))
}

/// Blank comments only — never string-literal content. Unlike
/// `argv_boundary::code_only` (which blanks both, because its callers scan
/// for *structural* patterns), several tripwires here need to see actual
/// quoted text (`env::var("HOME")`, `Command::new("git")`) while still being
/// blind to a doc comment that merely *mentions* the same text in prose.
fn comments_only_blanked(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        if ch == '/' && next == Some('/') {
            while i < c.len() && c[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if ch == '/' && next == Some('*') {
            let mut depth = 0usize;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    continue;
                }
                if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                out.push(if c[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            continue;
        }
        if ch == '"' {
            out.push('"');
            i += 1;
            while i < c.len() {
                if c[i] == '\\' {
                    out.push(c[i]);
                    if i + 1 < c.len() {
                        out.push(c[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                if c[i] == '"' {
                    out.push('"');
                    i += 1;
                    break;
                }
                out.push(c[i]);
                i += 1;
            }
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Split a battery file's comment-blanked source at its `mod harness` block
/// and return everything **outside** that block — the region R1 restricts.
/// String-literal content is left intact (see `comments_only_blanked`),
/// because several rules need to see real quoted text, not the structural
/// blanking `argv_boundary::code_only` performs for its own callers.
/// Panics (deliberately: see the module doc) if the file has no such marker.
fn case_region(rel: &str) -> String {
    let code = comments_only_blanked(&read_rs(rel));
    let marker = "mod harness";
    let at = code.find(marker).unwrap_or_else(|| {
        panic!(
            "{rel} has no `mod harness` marker — R1 requires every battery file to \
             fence its setup code inside one, splitting it from the case region. \
             This file has not been rewritten onto the contract yet."
        )
    });
    let open = at
        + code[at..]
            .find('{')
            .unwrap_or_else(|| panic!("{rel}: `mod harness` has no body"));
    let mut depth = 0usize;
    let mut close = None;
    for (i, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close.unwrap_or_else(|| panic!("{rel}: unbalanced braces in `mod harness`"));
    format!("{}{}", &code[..at], &code[close..])
}

fn ident_tokens(code: &str) -> std::collections::HashSet<&str> {
    code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .collect()
}

/// The body of `fn <name>` in already-`code_only`'d `code`, matched
/// brace-for-brace. Simpler than `argv_boundary::production_body`: no
/// exactly-one-definition or before-`mod tests` requirement, because the
/// functions this extracts (`policy_for_repo`, `execute`) are not duplicated
/// under a same-named test helper.
fn fn_body_in<'a>(code: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}");
    let at = code
        .find(&marker)
        .unwrap_or_else(|| panic!("`{marker}` not found"));
    let open = at
        + code[at..]
            .find('{')
            .expect("a fn signature has a body brace");
    let mut depth = 0usize;
    for (i, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &code[open..open + i + 1];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces extracting `{marker}`");
}

/// R1: outside `mod harness`, a battery file may contain only `const CASE_X`
/// declarations and `#[test] fn .. { run_case(&CASE_X) }` bodies — no
/// `assert*!`, `return`, `if`/`match`, `||`/`&&`, `eprintln!`. Enforced as a
/// grep for "no such syntax exists here", per the contract's whole argument:
/// you cannot grep "this assertion accepts a family of values" out of
/// freeform Rust, but you can grep "there is no assertion here at all."
#[test]
fn r1_case_region_has_no_freeform_control_flow_or_assertions() {
    for rel in BATTERY_FILES {
        let region = case_region(rel);
        let tokens = ident_tokens(&region);
        for banned in ["if", "match", "return"] {
            assert!(
                !tokens.contains(banned),
                "{rel}: `{banned}` found outside `mod harness` (R1)"
            );
        }
        for needle in [
            "assert!",
            "assert_eq!",
            "assert_ne!",
            "eprintln!",
            "&&",
            "||",
        ] {
            assert!(
                !region.contains(needle),
                "{rel}: `{needle}` found outside `mod harness` (R1)"
            );
        }
        let tests = region.matches("#[test]").count();
        let runs = region.matches("run_case(").count();
        assert_eq!(
            tests, runs,
            "{rel}: every #[test] body must be exactly `run_case(&CASE_X)` (R1); \
             {tests} #[test] attributes but {runs} run_case( calls"
        );
    }
}

/// R2: no hand-written acceptance predicate survives outside `mod harness`,
/// and the shared parser is `Result`, never an optional/defaultable value.
#[test]
fn r2_case_region_never_hand_writes_acceptance_conditions() {
    for rel in BATTERY_FILES {
        let region = case_region(rel);
        for needle in [".contains(", ".is_some()", "assert_ne!", "!= Some("] {
            assert!(
                !region.contains(needle),
                "{rel}: `{needle}` found outside `mod harness` — R2 requires a single \
                 named errno compared with assert_eq!, never a predicate"
            );
        }
    }
    let code = read_self_code_only();
    assert!(
        code.contains("fn parse_observation")
            && code.contains("Result<Observation, MissingObservation>"),
        "R2: the probe-output parser must be `-> Result<Observation, MissingObservation>`"
    );
    assert!(
        !code.contains("-> Option<i32>"),
        "R2: no probe-output parser in escape_contract.rs may return Option<i32>"
    );
}

/// Acceptance evidence F is source-bound as well as parser-bound: every case
/// writes all three leg expectations explicitly, and the one functional case
/// opts out with a typed value rather than a fabricated kernel signature.
#[test]
fn f_every_observation_requires_typed_kernel_provenance() {
    let escape = read_rs("src/sandbox/escape_suite.rs");
    assert_eq!(
        escape.matches("expect_baseline_provenance:").count(),
        // #188 added a 17th case (CASE_SSH_KNOWN_HOSTS_CARVEOUT), up from 16.
        17,
        "F: every containment case must spell a baseline provenance expectation"
    );
    assert_eq!(
        escape.matches("expect_inside_provenance:").count(),
        17,
        "F: every containment case must spell an inside provenance expectation"
    );
    assert_eq!(
        escape.matches("expect_granted_provenance:").count(),
        17,
        "F: every containment case must spell a GRANTED provenance expectation"
    );
    let hooks = read_rs("src/sandbox/hook_mode_suite.rs");
    assert_eq!(
        hooks.matches("Provenance::NotApplicable").count(),
        3,
        "F: blocked_hooks must explicitly exempt each of its three legs"
    );
    assert!(
        !hooks.contains("Seccomp: 0 NoNewPrivs: 0"),
        "F: blocked_hooks must not fabricate a kernel signature in printf"
    );
    let contract = read_self_code_only();
    assert!(
        contract.contains("parse_i32_field")
            && contract.contains("Provenance::NotApplicable")
            && contract.contains("Observation"),
        "F: the shared parser must carry typed, mandatory provenance"
    );
}

/// R3: every case carries the mandatory paired-positive field, and `run_case`
/// actually asserts it rather than merely storing it.
///
/// # Why this checks a whole call and not two substrings
///
/// This test used to assert `body.contains("expect_granted") &&
/// body.contains("\"GRANTED\"")` over the whole `execute` body, which is not
/// the property it claims. Both substrings occur in `execute` for reasons that
/// have nothing to do with the assertion: `case.expect_granted_provenance`
/// *contains* `expect_granted`, and the literal `"GRANTED"` is the parse tag
/// handed to `parse_observation`. So an edit that kept the parse call — needed
/// for provenance bookkeeping — while dropping the `observation_mismatch`
/// comparison would silently turn R3's mandatory paired positive into a no-op,
/// and this tripwire would have kept passing. That is the exact "green test
/// that proves nothing" shape the whole contract exists to prevent, sitting
/// inside the contract itself.
///
/// The check below is structural instead: some `observation_mismatch(…)` call
/// must take **both** the `"GRANTED"` tag and `case.expect_granted` (trailing
/// comma required, so the unrelated `case.expect_granted_provenance` field
/// cannot satisfy it), and its result must be acted on rather than discarded.
#[test]
fn r3_every_case_declares_and_asserts_a_paired_positive() {
    let code = read_self_code_only();
    assert!(
        code.contains("expect_granted"),
        "R3: EscapeCase must carry a mandatory expect_granted field"
    );
    // Raw-ish (comments blanked, strings intact): the "GRANTED" tag this
    // checks for is itself a string literal, which `code_only` would blank.
    let comments_blanked = read_self_comments_only();
    let body = fn_body_in(&comments_blanked, "execute");

    let granted_calls: Vec<(usize, &str)> = call_args_in(body, "observation_mismatch")
        .into_iter()
        .filter(|(_, args)| args.contains("\"GRANTED\"") && args.contains("case.expect_granted,"))
        .collect();
    assert_eq!(
        granted_calls.len(),
        1,
        "R3: exactly one `observation_mismatch` call must compare the GRANTED observation \
         against `case.expect_granted`; found {}. Zero means the mandatory paired positive is \
         stored but never asserted (the failure this test exists to catch); more than one means \
         the assertion has been duplicated and this test can no longer say which one is \
         load-bearing.",
        granted_calls.len()
    );

    // The comparison must also be *consumed*, and it must be **that** call —
    // `execute` makes three `observation_mismatch` calls and the first is a
    // match arm, so searching for the first one would test the wrong site.
    // `observation_mismatch` returns `Option<String>`; a call whose result is
    // dropped asserts nothing at all and would otherwise satisfy every check
    // above.
    let (at, _) = granted_calls[0];
    let preceding = body[..at].rsplit(';').next().unwrap_or("");
    assert!(
        preceding.contains("if let Some("),
        "R3: the paired-positive comparison must be consumed by the caller (an \
         `if let Some(detail) = …` that returns `Outcome::Escaped`); a bare call would \
         compute the mismatch and throw it away"
    );
}

/// Return `(offset, argument text)` for every call to `name` in `src`,
/// paren-balanced. The offset is where the call's name begins, so a caller can
/// inspect the syntax the call sits in — whether its result is consumed, say.
///
/// Substring checks over a whole function body cannot tell "these two tokens
/// both appear somewhere" from "these two tokens are arguments to the same
/// call" — the distinction that makes `r3_every_case_declares_and_asserts_a_paired_positive`
/// mean anything. Written by hand rather than pulled in as a parser dependency:
/// the battery deliberately reads its own source with no build-time deps.
fn call_args_in<'a>(src: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let marker = format!("{name}(");
    let mut from = 0usize;
    while let Some(rel) = src[from..].find(&marker) {
        let open = from + rel + marker.len();
        let mut depth = 1usize;
        for (i, ch) in src[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        out.push(&src[open..open + i]);
                        break;
                    }
                }
                _ => {}
            }
        }
        from = open;
    }
    out
}

/// R4: no host-capability probing appears outside `mod harness`, and the
/// harness's outcome type can represent absence as a value.
#[test]
fn r4_capability_established_only_by_execution_never_by_probing_the_host() {
    for rel in BATTERY_FILES {
        let region = case_region(rel);
        for needle in [
            "strict_available",
            "bwrap_path",
            "capabilities::probe",
            ".exists()",
            ".is_dir()",
            "env::var(\"HOME\")",
            "var_os(\"HOME\")",
        ] {
            assert!(
                !region.contains(needle),
                "{rel}: `{needle}` found outside `mod harness` — R4 forbids querying \
                 the host to decide capability; only an executed baseline may"
            );
        }
    }
    let code = read_self_code_only();
    assert!(
        code.contains("CapabilityAbsent"),
        "R4: run_case must be able to return Outcome::CapabilityAbsent as a value"
    );
}

/// R5's source half: "a source tripwire asserts the census file's id set equals
/// the set of `EscapeCase` constants in the battery."
///
/// The shell half of R5 lives in the gating job, which diffs
/// `$GV_ESCAPE_REPORT`'s case-ids against `docs/sandbox/escape-census.txt` in
/// both directions. That check alone cannot tell a *rename* from a *deletion*:
/// both show up as a diff, in CI, after a full battery run, with no indication
/// of which file is wrong. This one makes the census's disagreement with the
/// source a **build** failure at the point of edit — which is the difference
/// R5's "a rename breaks the BUILD rather than emptying the GATE" is naming.
///
/// It is not hypothetical. When this test was written the census was missing
/// `high_bit_af_unix_denied` and `high_bit_io_uring_denied`: two cases had
/// landed, both green, both writing report records the gating job would have
/// diffed against a census that did not know about them. The gate would have
/// gone red on the next CI run for a reason that looks exactly like a security
/// regression. Nothing in the tree could catch that before this test existed —
/// the contract specified it, and it was the one R5 clause never built.
#[test]
fn r5_census_names_exactly_the_declared_cases() {
    let mut declared: BTreeSet<String> = BTreeSet::new();
    for rel in BATTERY_FILES {
        // Comments blanked, string content intact: the ids being collected are
        // themselves string literals, which `code_only` would blank away.
        let src = comments_only_blanked(&read_rs(rel));
        let mut rest = src.as_str();
        while let Some(at) = rest.find("const CASE_") {
            let after = &rest[at + "const CASE_".len()..];
            // `id` is the first field of every `EscapeCase` literal (R1 forbids
            // `..Default::default()`, so it is always written out), which is
            // what makes "the first `id: \"` after the const marker" exact.
            let Some(id_at) = after.find("id: \"") else {
                break;
            };
            let tail = &after[id_at + "id: \"".len()..];
            let Some(end) = tail.find('"') else { break };
            let id = &tail[..end];
            assert!(
                declared.insert(id.to_string()),
                "{rel}: duplicate EscapeCase id `{id}` — the report file is a \
                 multiset the gating job compares by id, so two cases sharing one \
                 id make both unattributable"
            );
            rest = &tail[end..];
        }
    }
    assert!(
        !declared.is_empty(),
        "no `const CASE_…: EscapeCase` declarations found in {BATTERY_FILES:?} — the scan broke"
    );

    let census_path = server_root().join("../../docs/sandbox/escape-census.txt");
    let census_text = std::fs::read_to_string(&census_path).unwrap_or_else(|e| {
        panic!(
            "{}: the R5 census must be readable: {e}",
            census_path.display()
        )
    });
    let census: BTreeSet<String> = census_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    assert_eq!(
        declared,
        census,
        "R5: docs/sandbox/escape-census.txt and the battery's EscapeCase ids must name \
         the same set in both directions. In source but not the census: {:?}. In the \
         census but not source: {:?}.",
        declared.difference(&census).collect::<Vec<_>>(),
        census.difference(&declared).collect::<Vec<_>>(),
    );
}

/// R6: the composed launcher is reached only through `spawn::command_async`;
/// no battery file builds its own `Policy` or calls the deleted
/// `shim_cli::launch`/`workable`; every `Command::new(` in `escape_suite.rs`
/// names `"cc"` or `"git"` immediately.
#[test]
fn r6_every_inside_leg_spawns_through_the_production_seam() {
    for rel in BATTERY_FILES {
        let region = case_region(rel);
        for needle in ["launch(", "workable(", "Policy {"] {
            assert!(
                !region.contains(needle),
                "{rel}: `{needle}` found outside `mod harness` — R6 requires the \
                 composed launcher to be reached only through spawn::command_async"
            );
        }
    }
    // Whole file, not just the case region: the legitimate `Command::new(`
    // sites (compiling a probe with `cc`, the plain baseline `git`) live
    // inside `mod harness`. String content must survive this scan, so it
    // uses `comments_only_blanked`, never `code_only`.
    let code = comments_only_blanked(&read_rs("src/sandbox/escape_suite.rs"));
    let spawn = ["Command", "::new("].concat();
    let mut i = 0usize;
    let mut sites = 0usize;
    while let Some(pos) = code[i..].find(&spawn) {
        let after = code[i + pos + spawn.len()..].trim_start();
        assert!(
            after.starts_with("\"cc\"") || after.starts_with("\"git\""),
            "escape_suite.rs: a Command::new( site must be immediately followed by \
             \"cc\" or \"git\" (R6)"
        );
        sites += 1;
        i += pos + spawn.len();
    }
    assert!(
        sites > 0,
        "escape_suite.rs: no Command::new( sites found — the scan broke"
    );
}

/// R7: both legs share exactly one env-building function; no `env_clear`/
/// `.env(` appears outside `mod harness`; production sets exactly the two
/// documented `GIT_*` variables (`main.rs`), so the pinned profile's
/// deliberately-hostile case has a fixed, known set of ambient variables to
/// contrast against.
#[test]
fn r7_both_legs_share_one_pinned_environment_profile() {
    for rel in BATTERY_FILES {
        let region = case_region(rel);
        for needle in ["env_clear", ".env("] {
            assert!(
                !region.contains(needle),
                "{rel}: `{needle}` found outside `mod harness` — R7 requires exactly \
                 one env-building function, production_env_profile()"
            );
        }
    }
    let code = read_self_code_only();
    assert!(
        code.contains("fn production_env_profile"),
        "R7: production_env_profile() must exist in escape_contract.rs"
    );

    let main_code = comments_only_blanked(&read_rs("src/main.rs"));
    let mut git_vars: BTreeSet<&str> = BTreeSet::new();
    let mut rest = main_code.as_str();
    while let Some(start) = rest.find("\"GIT_") {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        git_vars.insert(&after[..end]);
        rest = &after[end + 1..];
    }
    assert_eq!(
        git_vars,
        BTreeSet::from(["GIT_TERMINAL_PROMPT", "GIT_EDITOR"]),
        "R7: the server's own GIT_* surface (main.rs) drifted from the two variables \
         production_env_profile() mirrors — found {git_vars:?}"
    );
}

/// The one blocker any battery case may still name, spelled once. It must match
/// `hook_mode_suite.rs`'s `blocker:` string byte for byte — that equality is
/// half of R8, and it is what makes a *reworded* blocker fail the build instead
/// of quietly re-labelling an exemption whose reason has changed.
const CHECKED_BLOCKERS: &[&str] = &["no production policy constructor yields HookMode::Blocked"];

/// The index of the `}` that closes the `{` at byte `open`, on source that has
/// already been through [`crate::argv_boundary::code_only`] (so braces inside
/// string, raw-string and char literals are blanked and cannot unbalance this).
fn matching_brace(code: &str, open: usize) -> usize {
    debug_assert_eq!(code.as_bytes().get(open), Some(&b'{'));
    let mut depth = 0usize;
    for (i, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + i;
                }
            }
            _ => {}
        }
    }
    panic!("R8: unbalanced braces after byte {open} — the scan broke");
}

/// Every `Policy { … }` **construction** in already-`code_only`'d production
/// source, each returned as the text between its own braces.
///
/// # Why this is not `find("Policy {")` any more
///
/// It was, and the shortcut had two defects — one that fires on innocent code
/// (and has already made production code contort to avoid it), one that would
/// have let guilty code through.
///
///  * **It matched any identifier *ending* in `Policy`.** `-> HookPolicy {` is
///    `Policy {` as far as `str::find` is concerned; the scan then demanded a
///    `hook_mode` field of it and panicked with "the scan broke" on finding
///    none. Not hypothetical: `sandbox::hook_policy` introduced a `Disclosed`
///    type alias for its return type largely so its source would not spell those
///    two characters. A tripwire that makes production code contort around it
///    teaches people to route around the tripwire, and the next `…Policy {`
///    would have been a red build with a message accusing the scan rather than
///    the code. The left edge is a token boundary now, so `HookPolicy` is what
///    it actually is — a different type, with no `hook_mode` field to check.
///  * **It searched the rest of the *file* for `hook_mode:`, not the literal.**
///    A construction that omitted the field — `Policy { tier, ..base }`, whose
///    hook mode comes from wherever `base` came from — would have silently
///    borrowed the *next* literal's `hook_mode: HookMode::Run` and passed —
///    green for a reason unrelated to the property, which is the same shape of
///    failure #206 caught R8 in once already. Bodies are brace-matched here, so every
///    construction is judged on its own text and a `..base` literal now reaches
///    the caller's "no hook_mode field" arm instead of another literal's answer.
///
/// Nothing is skipped on a guess. Only contexts where `Policy {` provably is not
/// a construction are passed over — a `struct`/`enum`/`union`/`trait`
/// declaration, `impl … Policy {`, `… for Policy {`, and a `-> Policy {` return
/// type (optionally path-qualified, e.g. `-> super::Policy {`) — and skipping a
/// *signature* loses no coverage, because that function's body is ordinary code
/// scanned like any other. Everything else is returned for the caller to judge;
/// R8 must never silently skip something it cannot verify.
fn production_policy_literals(prod: &str) -> Vec<&str> {
    let needle = ["Policy", " {"].concat();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = prod[from..].find(&needle) {
        let at = from + rel;
        // The `{` is the needle's last byte; both preceding chars are ASCII.
        let open = at + needle.len() - 1;
        from = open + 1;

        let before = &prod[..at];
        // Token boundary on the left: `HookPolicy {`, `SessionPolicy {` and
        // friends name other types entirely.
        if before
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        // The token that introduces it, looking through a `::` path prefix so
        // `impl super::Policy {` is classified by `impl`, not by `super::`.
        let mut tokens = before
            .trim_end()
            .rsplit(char::is_whitespace)
            .filter(|t| !t.is_empty());
        let mut lead = tokens.next().unwrap_or("");
        if lead.ends_with("::") {
            lead = tokens.next().unwrap_or("");
        }
        if matches!(
            lead,
            "struct" | "enum" | "union" | "trait" | "impl" | "for" | "->"
        ) {
            continue;
        }
        out.push(&prod[open + 1..matching_brace(prod, open)]);
    }
    out
}

/// R8: every exemption's named blocker must still be a true statement about
/// production source — **and every blocker the battery names must be one this
/// test knows how to check.**
///
/// # Why the previous form had stopped checking anything (#206)
///
/// R8 used to grep `sandbox::policy_for_repo`'s body for the literal tokens
/// `Tier::Network` and `HookMode::Run`, because before #197 that function
/// hard-coded both and the two hard-codes *were* the declared blockers. #197
/// removed the hard-code — `policy_for` now derives the tier from the declared
/// `NetworkNeed`, so production Strict became reachable — but the two tokens
/// survived the change, having moved into a `debug_assert!` inside what is now a
/// `#[cfg(test)]` compatibility wrapper. The grep went on passing while the
/// condition it stood for no longer existed, and the eight exemptions were
/// reworded rather than retired. A tripwire anchored to a token in test-only
/// code cannot expire, which is the single thing R8 exists to do.
///
/// # What it is anchored to now
///
/// Two checks, both about the property rather than about a token:
///
///  1. **The declared blocker set equals `CHECKED_BLOCKERS`.** A new exemption
///     carrying a blocker nobody checks fails here — the old hard-coded pair
///     silently permitted exactly that — and a blocker this test still checks
///     after the last case naming it is gone fails here too.
///  2. **No production policy constructor yields `HookMode::Blocked`.** Every
///     production module under `src/sandbox` is walked (the test-only ones are
///     *derived* from `mod.rs`'s own `#[cfg(test)] mod …;` declarations, so a new
///     production module is scanned without anyone remembering to add it), each
///     file's pre-`mod tests` region is taken, and every `Policy { … }` literal
///     in it must spell the field `hook_mode: HookMode::Run` — a *literal*, so a
///     `hook_mode: hook_mode_for(x)` helper fails it too — with nothing anywhere
///     assigning `HookMode::Blocked`. Give any constructor a route to `Blocked`
///     and this goes red, which is precisely when `hook_mode_suite`'s exemption
///     must be retired.
///
/// Finding the literals is [`production_policy_literals`], which is token-exact
/// on the left (so a `-> HookPolicy {` signature is not mistaken for a `Policy`
/// construction and does not fire a "the scan broke" panic at the next person to
/// name a type that way) and brace-scoped on the right (so a construction that
/// omits `hook_mode` cannot borrow a later literal's field and pass). It has its
/// own tests — `the_r8_policy_scan_is_token_exact_and_brace_scoped` — because a
/// scanner nobody scanned is how a tripwire ends up green on a technicality.
#[test]
fn r8_exemptions_expire_when_their_named_blocker_disappears() {
    // (1) Every blocker string the battery declares. String content must
    // survive the blanking here — the blockers *are* string literals.
    let mut declared: BTreeSet<String> = BTreeSet::new();
    for rel in BATTERY_FILES {
        let src = comments_only_blanked(&read_rs(rel));
        let mut rest = src.as_str();
        let marker = ["blocker", ": \""].concat();
        while let Some(at) = rest.find(&marker) {
            let tail = &rest[at + marker.len()..];
            let end = tail
                .find('"')
                .unwrap_or_else(|| panic!("{rel}: unterminated blocker string literal"));
            declared.insert(tail[..end].to_string());
            rest = &tail[end..];
        }
    }
    let checked: BTreeSet<String> = CHECKED_BLOCKERS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        declared,
        checked,
        "R8: every exemption blocker a battery file names must have a source check in \
         this test, and every check here must still have a case naming it. Named but \
         unchecked: {:?}. Checked but no longer named: {:?}.",
        declared.difference(&checked).collect::<Vec<_>>(),
        checked.difference(&declared).collect::<Vec<_>>(),
    );

    // (2) `no production policy constructor yields HookMode::Blocked`, checked
    // over production source. The test-only module list is read out of
    // `mod.rs`'s own declarations rather than restated here, so this cannot
    // drift from the module list the crate actually compiles.
    let mod_code = crate::argv_boundary::code_only(&read_rs("src/sandbox/mod.rs"));
    let mut test_only: BTreeSet<String> = BTreeSet::new();
    let cfg_test = ["#[cfg", "(test)]"].concat();
    let mut rest = mod_code.as_str();
    while let Some(at) = rest.find(&cfg_test) {
        let after = &rest[at + cfg_test.len()..];
        let Some(semi) = after.find(';') else { break };
        let head = after[..semi].trim();
        let decl = head
            .strip_prefix("pub(crate) ")
            .or_else(|| head.strip_prefix("pub "))
            .unwrap_or(head);
        if let Some(name) = decl.strip_prefix("mod ") {
            let name = name.trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                test_only.insert(name.to_string());
            }
        }
        rest = &after[semi..];
    }
    for expected in ["escape_contract", "escape_suite", "hook_mode_suite"] {
        assert!(
            test_only.contains(expected),
            "R8: the test-only module scan of sandbox/mod.rs missed `{expected}` — the \
             scan broke, and everything below it would be scanning the wrong file set"
        );
    }

    let mut files = Vec::new();
    crate::argv_boundary::rs_files(&server_root().join("src/sandbox"), &mut files);
    files.sort();
    let mut sites = 0usize;
    for path in &files {
        let stem = path
            .file_stem()
            .expect("a .rs file has a stem")
            .to_string_lossy()
            .to_string();
        if test_only.contains(&stem) {
            continue;
        }
        let code = crate::argv_boundary::code_only(
            &std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("{}: must be readable: {e}", path.display())),
        );
        // Everything before the file's inline `#[cfg(test)] mod tests` block —
        // a production file's own unit tests may build whatever policy they
        // like, and often must.
        let prod = match code.find("mod tests") {
            Some(at) => &code[..at],
            None => code.as_str(),
        };
        assert!(
            !prod.contains("= HookMode::Blocked"),
            "R8: {stem}.rs assigns HookMode::Blocked in production code — the \
             blocked-hooks exemption in hook_mode_suite.rs must be retired, not left \
             standing"
        );
        let field = ["hook_mode", ":"].concat();
        for body in production_policy_literals(prod) {
            let f = body.find(&field).unwrap_or_else(|| {
                panic!(
                    "R8: a `Policy` construction in {stem}.rs spells no `hook_mode` field \
                     inside its own braces. R8 does not skip what it cannot verify, so \
                     this is a hard failure rather than a silent pass: write the field \
                     out literally. A `Policy {{ .., ..base }}` functional update hits \
                     this on purpose — its hook mode comes from `base`, which is exactly \
                     the indirection this check exists to refuse. Body was: `{body}`"
                )
            });
            let value = body[f + field.len()..].trim_start();
            assert!(
                value.starts_with("HookMode::Run"),
                "R8: a production `Policy` literal in {stem}.rs sets hook_mode to \
                 something other than the literal `HookMode::Run` — production can now \
                 express a policy that blocks hooks, so the blocked-hooks exemption in \
                 hook_mode_suite.rs must be retired, not left standing"
            );
            sites += 1;
        }
    }
    assert!(
        sites >= 3,
        "R8: found only {sites} production `Policy` construction sites under \
         src/sandbox — policy_for, policy_for_clone and probe::boot_probe_policy are \
         the three that must be there, so the scan broke rather than the code shrinking"
    );
}

/// [`production_policy_literals`] on its own terms: what it must find, what it
/// must not mistake for a construction, and — the half that matters — that each
/// construction is judged on *its own* braces.
///
/// R8 rests entirely on this function, and R8's job is to notice when an
/// exemption has outlived its blocker. A scanner that quietly finds nothing, or
/// that answers a question about literal A using literal B's text, makes R8 pass
/// for reasons unrelated to the property. Both were live defects in the previous
/// `str::find`-based form; each has a case below.
///
/// The snippets are written as ordinary string literals rather than assembled
/// from fragments: this module is `#[cfg(test)] mod escape_contract` (see
/// `sandbox/mod.rs`), so R8's own file walk skips it, and every self-scan in
/// this file reads through `code_only`, which blanks string-literal content.
#[test]
fn the_r8_policy_scan_is_token_exact_and_brace_scoped() {
    // An identifier that merely ENDS in `Policy` is a different type. This is
    // the false positive that made `sandbox::hook_policy` name its return type
    // through an alias to avoid tripping the old scan.
    assert!(
        production_policy_literals("fn f(t: Tier) -> HookPolicy { HookPolicy::Strict }").is_empty(),
        "`-> HookPolicy {{` is not a `Policy` construction"
    );

    // Declarations and impls are not constructions either — including through a
    // path prefix and through `for`.
    for src in [
        "pub(crate) struct Policy { pub tier: Tier }",
        "impl Policy { fn tier(&self) -> Tier { self.tier } }",
        "impl std::fmt::Debug for Policy { }",
        "impl super::Policy { }",
        "fn build() -> super::Policy { unreachable!() }",
    ] {
        assert!(
            production_policy_literals(src).is_empty(),
            "not a construction, but the scan claimed one: {src}"
        );
    }

    // A real construction inside a function whose return type is also `Policy`:
    // exactly one body, and it is the literal's, not the function's.
    let one =
        production_policy_literals("fn p() -> Policy { Policy { hook_mode: HookMode::Run } }");
    assert_eq!(
        one.len(),
        1,
        "expected exactly one construction, got {one:?}"
    );
    assert_eq!(one[0].trim(), "hook_mode: HookMode::Run");

    // Brace-scoped: a `..base` construction must NOT be able to answer with the
    // next literal's field. Under the old whole-file search the first body's
    // missing `hook_mode` was silently supplied by the second.
    let two = production_policy_literals(
        "let a = Policy { tier, ..base }; let b = Policy { hook_mode: HookMode::Run };",
    );
    assert_eq!(two.len(), 2, "expected two constructions, got {two:?}");
    assert!(
        !two[0].contains("hook_mode"),
        "a functional-update literal must not inherit the next literal's field: {:?}",
        two[0]
    );
    assert!(two[1].contains("hook_mode: HookMode::Run"));

    // Nested braces inside a body do not truncate it early.
    let nested =
        production_policy_literals("Policy { trees: Some(T { x }), hook_mode: HookMode::Run }");
    assert_eq!(nested.len(), 1);
    assert!(nested[0].contains("T { x }") && nested[0].contains("hook_mode: HookMode::Run"));

    // And the scan is not vacuous against the real tree: production source must
    // still yield the three known constructors.
    let mut found = 0usize;
    for rel in ["src/sandbox/mod.rs", "src/sandbox/probe.rs"] {
        let code = crate::argv_boundary::code_only(&read_rs(rel));
        found += production_policy_literals(&code).len();
    }
    assert!(
        found >= 3,
        "the scan found {found} production `Policy` constructions in mod.rs + probe.rs; \
         policy_for, policy_for_clone and boot_probe_policy are the three that must be \
         there"
    );
}

/// The raw (not `code_only`'d) body of `fn <name>` in `src`, matched
/// brace-for-brace. R10 needs actual string-literal *content* (the flag
/// text), which `code_only` deliberately blanks — so this walks raw source,
/// scoped to one function body, which is what keeps a doc comment mentioning
/// a flag in backticks (not a string literal) from being misread as a scan
/// hit.
fn fn_body_raw<'a>(src: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}");
    let at = src
        .find(&marker)
        .unwrap_or_else(|| panic!("`{marker}` not found"));
    let open = at
        + src[at..]
            .find('{')
            .expect("a fn signature has a body brace");
    let mut depth = 0usize;
    for (i, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open..open + i + 1];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces extracting `{marker}`");
}

/// R10: every `"--…"` literal `shim_argv` emits (`mod.rs`) has a matching arm
/// in the shim's `parse()` (`bin/gv-sandbox/main.rs`), and vice versa — a
/// dead sanctioned route (emitted, unparsed) is exactly what
/// `probe_argv`/`--self-probe` was, and an unreachable terminal mode (parsed,
/// never built) is its mirror image. Scoped to the two builder/parser
/// function bodies, on raw source, so a doc comment cannot pollute either
/// direction of the scan.
#[test]
fn r10_every_flag_sandbox_argv_emits_has_a_shim_parser_arm() {
    let mod_src = read_rs("src/sandbox/mod.rs");
    let argv_body = fn_body_raw(&mod_src, "shim_argv");

    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut rest = argv_body;
    while let Some(start) = rest.find("\"--") {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        let flag = &after[..end];
        // `"--"` is the separator, not a flag — it has its own arm in
        // `parse()` (`"--" => { … break; }`) rather than a flag arm, and
        // `arms` below excludes it for the same reason.
        if flag != "--" {
            emitted.insert(flag.to_string());
        }
        rest = &after[end + 1..];
    }
    assert!(
        !emitted.is_empty(),
        "flag scan of shim_argv found nothing — the scan broke"
    );

    let main_src = read_rs("src/bin/gv-sandbox/main.rs");
    let parse_body = fn_body_raw(&main_src, "parse");

    let mut arms: BTreeSet<String> = BTreeSet::new();
    for line in parse_body.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        let flag = &rest[..end];
        if flag.starts_with("--") && flag != "--" && rest[end + 1..].trim_start().starts_with("=>")
        {
            arms.insert(flag.to_string());
        }
    }

    assert_eq!(
        emitted, arms,
        "R10: sandbox_argv's emitted flags and the shim's parser arms must name exactly \
         the same set — a mismatch is either a dead sanctioned route (emitted, no arm) or \
         an unreachable terminal mode (an arm nothing builds)"
    );
}

/// R11: every rule in `RULES` names a test that still exists in this file.
#[test]
fn r11_every_rule_names_a_test_that_still_exists() {
    let code = read_self_code_only();
    for (rule, test_fn) in RULES {
        let marker = format!("fn {test_fn}(");
        assert!(
            code.contains(&marker),
            "R11: rule {rule} names `{test_fn}`, which no longer exists in \
             escape_contract.rs — its enforcement was deleted"
        );
    }
}

/// Every `.rs` file under `src/sandbox/` is declared in `sandbox/mod.rs`, and every
/// declaration names a file that exists — set equality, checked both directions.
///
/// **This closes the one miss of the six that no R-rule could see.** A module with
/// tests but no `mod` declaration is dead source: it compiles nowhere, its tests never
/// run, and no test count moves — the suite goes green having silently stopped checking
/// whatever that file checked. Every R1–R11 tripwire scans *content* (`BATTERY_FILES`
/// drives them through `read_rs`, which reads bytes off disk whether or not the
/// compiler ever saw them), so content scans are exactly the wrong instrument: they
/// pass happily on a file the build graph has dropped.
///
/// Membership, not content, is the property here, and the two sides are derived
/// independently — the filesystem walk cannot see `mod.rs`, and the declaration parse
/// cannot see the directory. A file added without a declaration fails the first
/// assertion; a declaration whose file was deleted or renamed fails the second.
///
/// **What would make this pass while the mechanism was broken?** Three things, each
/// handled: (a) an empty walk would make the first assertion vacuous, so the floor
/// below fails if the walk stops finding files; (b) `#[path = "..."]` would let a
/// declaration name a file outside this directory, so it is banned outright rather
/// than modelled — there is none today and this keeps it that way; (c) this test lives
/// in `escape_contract`, which is itself declared in `mod.rs`, so deleting *its*
/// declaration would take the check with it — that specific case is caught elsewhere
/// (`r8_exemptions_expire_when_their_named_blocker_disappears` asserts its derived
/// test-only module set contains `escape_contract`, `escape_suite` and
/// `hook_mode_suite` by name, so their declarations cannot vanish quietly). The live
/// exposure this closes is the *other* modules — `compat`, `documented_gaps`,
/// `hostile`, `lifecycle` and the rest — where deletion is silent today: no reference,
/// no warning, no compile error.
#[test]
fn every_sandbox_module_file_is_declared_and_every_declaration_has_a_file() {
    let dir = server_root().join("src/sandbox");
    let mut paths = Vec::new();
    crate::argv_boundary::rs_files(&dir, &mut paths);

    let on_disk: BTreeSet<String> = paths
        .iter()
        .filter(|p| p.parent() == Some(dir.as_path()))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
        .filter(|stem| *stem != "mod")
        .map(str::to_string)
        .collect();

    assert!(
        on_disk.len() >= 15,
        "only {} .rs files found under src/sandbox/ — the walk has lost the directory \
         and this whole check is now vacuous",
        on_disk.len()
    );

    let mod_rs = crate::argv_boundary::code_only(&read_rs("src/sandbox/mod.rs"));
    assert!(
        !mod_rs.contains("#[path"),
        "sandbox/mod.rs uses `#[path]`, which lets a `mod` declaration name a file \
         outside this directory — the stem-vs-declaration equality below would then \
         compare two sets that no longer describe the same thing. If a `#[path]` is \
         genuinely needed, this check must be taught about it deliberately"
    );

    let declared: BTreeSet<String> = mod_rs
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("pub(crate) mod ")
                .or_else(|| line.strip_prefix("pub mod "))
                .or_else(|| line.strip_prefix("mod "))?;
            rest.strip_suffix(';').map(str::to_string)
        })
        .collect();

    let undeclared: Vec<_> = on_disk.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "{undeclared:?} exist under src/sandbox/ but are declared in no `mod` statement \
         in sandbox/mod.rs — dead source. Their tests do not run and no test count \
         moves, so the suite goes green having stopped checking whatever they checked. \
         That is exactly how a whole module's tests were lost once already (#199)."
    );

    let dangling: Vec<_> = declared.difference(&on_disk).collect();
    assert!(
        dangling.is_empty(),
        "sandbox/mod.rs declares {dangling:?}, which no `.rs` file under src/sandbox/ \
         provides — a stale declaration (this direction would normally fail the build, \
         so if you are reading it, something is generating or conditionally including \
         sources and this check needs to be taught about it)"
    );
}

/// The CI gating job's preflight (contract, "Skip policy": "the job's first
/// step is a preflight … failing with `::error::` naming the missing field,
/// before any test runs"). Deliberately host-probing — that is its whole
/// job, and it is not part of the battery R4 restricts (it lives in this
/// file, not `escape_suite.rs`/`hook_mode_suite.rs`). A failing `#[test]`'s
/// output is *not* swallowed by libtest (only passing tests are, R5's whole
/// argument), so the `::error::` prefix reaches the CI log on failure.
#[test]
fn ci_preflight_host_meets_the_declared_minimum() {
    let caps = capabilities::probe();
    let mut missing = Vec::new();
    if !caps.landlock_meets_floor() {
        missing.push(format!(
            "landlock_abi={} below the declared floor",
            caps.landlock_abi
        ));
    }
    if !caps.bwrap_present {
        missing.push("bwrap not found at any BWRAP_CANDIDATES path".to_string());
    }
    if !caps.userns {
        missing.push("unprivileged user namespaces not usable".to_string());
    }
    let io_uring_disabled = std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    if io_uring_disabled != Some(0) {
        missing.push(format!(
            "io_uring_disabled={io_uring_disabled:?}, want Some(0)"
        ));
    }
    let cc_ok = std::process::Command::new("cc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !cc_ok {
        missing.push("cc not runnable".to_string());
    }

    // The $HOME prerequisites, which are host provisioning exactly as much as
    // bwrap is — and which this preflight did not check until they bit.
    //
    // Why this is here rather than left to the cases: on CI run 30633319726 this
    // preflight PASSED and `secret_read_denied` then died with "proved NOTHING —
    // baseline SECRET wanted errno 0 got 2", whose own text tells the reader
    // "the CI preflight already asserts this host supplies every capability the
    // battery needs, so this is a harness defect, not a host limitation." That
    // sentence was false, and it pointed the next debugger at the harness when
    // the real answer was an unprovisioned runner path (a fresh /home/runner has
    // no ~/.ssh at all). Naming a missing FILE here, before any case runs, is
    // the same distinction D6 draws for capabilities: "this runner was not set
    // up" must never arrive disguised as "the sandbox failed to contain
    // something".
    //
    // `fixture()` passes $HOME through to its git subprocesses deliberately, so
    // the identity must resolve from ~/.gitconfig (#203); `secret_read_probe`
    // opens ~/.ssh/known_hosts as its SECRET and ~/.gitconfig as its GRANTED
    // path, and declares `expect_baseline: Errno(0)` for both — a path that does
    // not exist returns ENOENT and the case can prove nothing either way. CI
    // provisions both in .github/actions/host-sandbox-setup; the tripwire
    // `every_ci_job_that_runs_this_crates_tests_provisions_the_host_capabilities_they_need`
    // keeps that action wired into every job that runs these tests.
    match std::env::var_os("HOME").map(PathBuf::from) {
        None => missing.push(
            "HOME is unset, so no $HOME-relative prerequisite can be \
                              resolved at all"
                .to_string(),
        ),
        Some(home) => {
            for (rel, why) in [
                (
                    ".gitconfig",
                    "the identity-free fixture repositories resolve their author through it (#203)",
                ),
                (
                    ".ssh/known_hosts",
                    "secret_read_denied reads it as its SECRET and declares expect_baseline \
                     Errno(0), so it must exist and be readable with no sandbox applied",
                ),
            ] {
                let path = home.join(rel);
                // Read a byte rather than stat: the baseline leg's `open` +
                // `read` is what must succeed, and an existing-but-unreadable or
                // empty file would satisfy `exists()` while still failing the
                // case. Probe the property the cases actually depend on.
                let readable = std::fs::read(&path).map(|b| !b.is_empty()).unwrap_or(false);
                if !readable {
                    missing.push(format!(
                        "{} is missing, empty or unreadable — {why}",
                        path.display()
                    ));
                }
            }
        }
    }

    assert!(
        missing.is_empty(),
        "::error::sandbox CI preflight: host missing {missing:?} — the escape battery \
         cannot produce sound containment evidence on this runner"
    );
}

// =========================================================================
// Part 3: the CI-environment tripwire
// =========================================================================

/// The workflow this tripwire reads, repo-root-relative.
const WORKFLOW_REL: &str = ".github/workflows/ci.yml";

/// The shared composite action every job that runs this crate's tests must
/// reference, repo-root-relative and spelled exactly once: a job's `uses:`
/// value is `./` + this, and the file on disk is this + `/action.yml`.
///
/// One constant, both halves, on purpose. Renaming or deleting the action
/// directory has to fail this test — and it fails it twice over: the
/// file-exists assertion stops matching the tree, and every job's `uses:`
/// stops matching the workflow. A check that only read ci.yml would go on
/// passing while the action it names had been deleted.
const HOST_SETUP_ACTION_DIR: &str = ".github/actions/host-sandbox-setup";

/// The three host capabilities the composite action exists to provide, each
/// named by a token that must appear **in the action** and — deliberately, in
/// the same test — **nowhere in ci.yml itself**.
///
/// Both directions are asserted because they are different claims. "The action
/// does all three" is what stops it being hollowed out to a no-op while every
/// job goes on referencing it and every job goes on being green. "ci.yml does
/// none of the three" is what stops the opposite repair: someone meeting a red
/// job and pasting the setup steps back inline. That inline copy is not
/// hypothetical — it is the exact state this change is fixing, where `sandbox`
/// carried the two setup steps, `core` and `contract` did not, and nothing in
/// the tree could tell the difference until 111 tests failed at once.
const HOST_SETUP_TOKENS: &[(&str, &str)] = &[
    (
        "bubblewrap",
        "installs bwrap; without it the Strict tier cannot be built and every production \
         git spawn is refused rather than downgraded (ADR 0029)",
    ),
    (
        "openssh-server",
        "installs sshd (#188): sandbox::ssh_remote's fixture spawns a real, throwaway, \
         loopback sshd to drive git ls-remote over ssh:// through the composed launcher. \
         ubuntu-latest ships the openssh-client tools (ssh/ssh-keygen/ssh-agent, a base-image \
         dependency for git itself) but not sshd, a separate package — without it every \
         sandbox::ssh_remote test fails at spawning sshd with a bare ENOENT that has \
         nothing to do with the sandbox",
    ),
    (
        "apparmor_restrict_unprivileged_userns",
        "unclamps unprivileged user namespaces (D6 Option A); ubuntu-latest ships the clamp \
         set to 1, under which bwrap cannot create its namespaces at all",
    ),
    (
        "user.email",
        "gives the runner a global git identity; a fresh /home/runner has none, so the \
         battery's deliberately identity-free fixture repository cannot make its seed \
         commit and no invariant is ever exercised (#203)",
    ),
    (
        "known_hosts",
        "materialises the path `ssh_known_hosts_carveout`'s GRANTED leg reads (#188). That \
         case's paired positive declares `expect_baseline: Errno(0)` — the file must be \
         readable with no sandbox applied — so a runner with no ~/.ssh would otherwise \
         return ENOENT for the baseline and the case would hard-fail having proved nothing \
         about the carve-out's own claim",
    ),
    (
        "id_ed25519",
        "materialises the path `secret_read_denied` (#188: repointed off `known_hosts`, \
         which became legitimately readable once the carve-out landed) and \
         `ssh_known_hosts_carveout`'s SSHKEY leg both use as their SECRET. Same reasoning \
         as `known_hosts` above: `expect_baseline: Errno(0)` requires the file to exist and \
         be readable outside the sandbox, or the baseline leg returns ENOENT and both cases \
         hard-fail having proved nothing",
    ),
];

/// The repository root: two levels above `server_root()`
/// (`crates/git-vista-server`).
fn repo_root_dir() -> PathBuf {
    server_root().join("../..")
}

fn read_repo_text(rel: &str) -> String {
    let path = repo_root_dir().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: must be readable: {e}", path.display()))
}

/// Blank whole-line comments, keeping the line count intact.
///
/// Line-level rather than token-level because both file kinds scanned here —
/// YAML and `#!/usr/bin/env bash` — use `#` to end-of-line, and because the
/// only thing that must be excluded is *prose*: ci.yml's comment blocks discuss
/// `cargo test`'s exit behaviour and name the apparmor sysctl, and neither
/// mention is an invocation or a provisioning step. A trailing `#` inside a
/// `run: |` body is shell, not YAML, so it is deliberately left alone; nothing
/// below cares what follows a command on the same line.
fn without_full_line_comments(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim_start().starts_with('#') {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Split ci.yml into `(job name, job body)` pairs, hand-rolled.
///
/// # The indentation contract, stated because everything below rests on it
///
///  * `jobs:` is a **column-0** key.
///  * The jobs mapping runs to the next column-0 key, or to end of file.
///  * Each job is a key at **exactly two spaces** of indent.
///  * Everything belonging to a job is indented **deeper** than two spaces.
///
/// The last two are what make a block scalar unable to masquerade as a job key:
/// YAML requires a `run: |` body to be indented deeper than its own key, and
/// every key inside a job sits at four spaces or more, so no line of shell can
/// land at exactly two. That is an argument about YAML's own rules, not a
/// guess about this file's current contents.
///
/// A line at two spaces that is not `<identifier>:` therefore means the file's
/// shape has changed out from under this parser, and it **panics** rather than
/// dropping the line — silently finding fewer jobs is the one failure mode that
/// would leave every assertion below trivially satisfied.
fn workflow_jobs(yaml: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = yaml.lines().collect();
    let jobs_at = lines
        .iter()
        .position(|l| l.trim_end() == "jobs:")
        .unwrap_or_else(|| {
            panic!(
                "{WORKFLOW_REL} has no column-0 `jobs:` line. This parser is hand-rolled — \
                 the workspace has no YAML crate and a tripwire must not be the reason one \
                 gets added — so it fails here instead of returning an empty job list that \
                 every assertion downstream would pass over vacuously."
            )
        });

    let end = lines
        .iter()
        .enumerate()
        .skip(jobs_at + 1)
        .find(|(_, l)| {
            !l.trim().is_empty()
                && !l.starts_with(' ')
                && !l.starts_with('\t')
                && !l.starts_with('#')
        })
        .map_or(lines.len(), |(i, _)| i);

    let mut keys: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(jobs_at + 1) {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.chars().take_while(|c| *c == ' ').count();
        if indent != 2 {
            continue;
        }
        let rest = line[indent..].trim_end();
        if rest.starts_with('#') {
            continue;
        }
        let name = rest.strip_suffix(':').unwrap_or_else(|| {
            panic!(
                "{WORKFLOW_REL}:{}: `{rest}` sits at exactly two spaces of indent inside \
                 `jobs:` but is not a `<name>:` job key. Two-space indent meaning \"a job \
                 starts here\" is this parser's whole contract (see its doc comment); if \
                 the workflow's shape has genuinely changed, teach the parser deliberately \
                 rather than letting it degrade into finding no jobs.",
                i + 1
            )
        });
        assert!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-'),
            "{WORKFLOW_REL}:{}: `{name}` is not a plain job identifier — the two-space \
             indentation contract this parser depends on no longer describes the file",
            i + 1
        );
        keys.push((i, name.to_string()));
    }

    keys.iter()
        .enumerate()
        .map(|(n, (at, name))| {
            let stop = keys.get(n + 1).map_or(end, |(next, _)| *next);
            (name.clone(), lines[*at..stop].join("\n"))
        })
        .collect()
}

/// Does this `cargo test` invocation reach `git-vista-server`'s tests?
///
/// Three shapes, and only three, because a fourth must not be guessed at:
/// `--workspace` reaches every member and so reaches this crate; an explicit
/// `-p git-vista-server` reaches it; an explicit `-p` list naming other crates
/// does not. Anything else — a bare `cargo test`, `--all`, a cargo alias — is a
/// shape this classifier has not been taught, and it fails loudly instead of
/// picking a default. Defaulting to "no" would silently exempt a job from the
/// entire check, which is the class of bug this test exists to prevent;
/// defaulting to "yes" would be right today and wrong the first time someone
/// runs a frontend-only suite. Neither is a call a scanner should make alone.
fn cargo_test_line_reaches_server(origin: &str, line: &str) -> bool {
    if line.contains("--workspace") {
        return true;
    }
    if line.contains("-p git-vista-server") {
        return true;
    }
    assert!(
        line.contains("-p "),
        "{origin}: `{}` runs cargo test in a shape this tripwire cannot classify — it \
         names neither `--workspace` nor any `-p <crate>`, so whether it reaches \
         git-vista-server's tests (and therefore needs the sandbox host capabilities) \
         would be a guess. Teach `cargo_test_line_reaches_server` the new shape.",
        line.trim()
    );
    false
}

/// Repo-relative shell scripts a job's steps actually run.
///
/// Without this the check has a hole with a name on it: `ci/mutation-matrix.sh`
/// runs `cargo test -p git-vista-server` twice, so a job can reach this crate's
/// tests without ci.yml containing the string `cargo test` at all. Scripts are
/// resolved against the tree rather than pattern-matched, so a `.sh` token that
/// names no file in this repository (a path on the runner, a value in a shell
/// variable) is ignored instead of being read.
fn referenced_repo_scripts(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for token in body.split_whitespace() {
        let token = token.trim_matches(|c: char| !(c.is_alphanumeric() || "./_-".contains(c)));
        if !token.ends_with(".sh") {
            continue;
        }
        let rel = token.trim_start_matches("./");
        if repo_root_dir().join(rel).is_file() {
            out.insert(rel.to_string());
        }
    }
    out
}

/// Whether a job body contains a `uses:` step naming exactly this local action.
/// Value-exact, not a substring search over the body: a `uses:` naming a
/// *different* local action whose path happens to contain this one's, or a
/// comment quoting the path, must not count as provisioning.
fn job_uses_local_action(body: &str, action_dir: &str) -> bool {
    body.lines().any(|line| {
        let line = line.trim();
        let line = line.strip_prefix("- ").unwrap_or(line);
        let Some(value) = line.strip_prefix("uses:") else {
            return false;
        };
        let value = value.trim().trim_matches(|c: char| c == '"' || c == '\'');
        value.trim_start_matches("./") == action_dir
    })
}

/// **Every CI job that runs this crate's tests provisions the host capabilities
/// those tests need.** The gap this closes cost a full red build: M1.13b routed
/// every production git spawn through the sandbox chokepoint, so `core` and
/// `contract` began constructing the Strict tier too — but the host setup lived
/// as two hand-written steps inside the `sandbox` job only, and 111 tests died
/// with "this operation runs in the strict sandbox tier and this host cannot
/// provide it (missing: bwrap, user_namespaces)". Nothing in the tree could see
/// that a test-running job and its provisioning had come apart.
///
/// The property is a set equation over ci.yml: the jobs whose steps reach
/// `git-vista-server`'s tests are exactly the jobs that reference the shared
/// host-setup composite action.
///
/// # Equality, not a subset
///
/// The bug only needed `needs_setup ⊆ has_setup` — every testing job provisioned.
/// This asserts equality anyway, so a job that provisions without testing fails
/// too, for two reasons. First, the action does not merely install a package: it
/// writes `kernel.apparmor_restrict_unprivileged_userns=0`, deliberately
/// weakening the runner, and a job with no reason to do that should not. Second
/// and mainly, an unexplained extra on either side *is* the drift signal — the
/// whole failure mode here was two sides of one relationship being maintained by
/// hand and diverging unnoticed, and a subset relation only ever notices one
/// direction of divergence. Widening to a subset should take a deliberate edit
/// with a reason written next to it, not be the default.
///
/// # What would make this pass while the mechanism was broken?
///
/// Seven ways, each closed here:
///
///  1. **The parse finds no jobs** — every "for each testing job" assertion is
///     then vacuously true. Closed twice: `workflow_jobs` panics if there is no
///     column-0 `jobs:` key or if a two-space line is not a job key, and the
///     floor below fails if fewer than five jobs come back (the workflow's own
///     header documents seven).
///  2. **The parse finds jobs but classifies none as test-running** — same
///     vacuity one level down. Closed by two independent floors: at least four
///     `cargo test` invocations must be seen across the workflow, and at least
///     three jobs must classify as reaching this crate (`core`, `contract`,
///     `sandbox` today).
///  3. **A `cargo test` shape the classifier does not recognise is quietly
///     treated as not reaching this crate** — closed by
///     `cargo_test_line_reaches_server`, which fails loudly on an unclassifiable
///     invocation instead of returning a default.
///  4. **Tests reached through a shell script rather than a `cargo test` line in
///     ci.yml** — `ci/mutation-matrix.sh` really does this. Closed by
///     `referenced_repo_scripts`, which resolves `.sh` tokens against the tree
///     and scans them too.
///  5. **Prose counted as machinery** — ci.yml's comment blocks quote
///     `cargo test` and name the apparmor sysctl. Closed by
///     `without_full_line_comments`: every scan below runs on comment-stripped
///     text, in both directions.
///  6. **The action is renamed or deleted while ci.yml still names something** —
///     closed by asserting `action.yml` exists at the exact path, from the same
///     constant the `uses:` comparison uses.
///  7. **The action is reduced to a stub** — a `uses:` that resolves to an empty
///     composite action provisions nothing while every job still "references the
///     shared setup". Narrowed, **not closed**, by requiring the action to be a
///     composite action whose comment-stripped text still spells every capability
///     in `HOST_SETUP_TOKENS`, plus `sysctl`, a `user.name`/`user.email` git
///     identity, and the `::error::` fail-loud posture.
///
///     Be precise about what that buys, because an earlier revision of this
///     comment claimed more than the code delivers and an adversarial review
///     caught it: the enforcement is `action.contains(token)`, a substring scan.
///     It reliably catches **deletion** — the realistic regression, where someone
///     trims a step they think is redundant. It does **not** catch a hollowed
///     step, and this was demonstrated, not theorised: replacing the whole file
///     with a stub whose only step is `run: echo "bubblewrap
///     apparmor_restrict_unprivileged_userns user.email user.name sysctl git
///     config ::error::"` leaves this test green. No static scan of a shell
///     script can do better; proving a script *provisions* something requires
///     running it.
///
/// The hole knowingly left open, and the mechanism that actually closes it: this
/// test cannot prove the action's steps *succeed* on the runner — or that they do
/// anything at all — only that they are declared. That is
/// `ci_preflight_host_meets_the_declared_minimum`'s job. It measures the real
/// host, fails with `::error::` naming each missing capability, and it now runs
/// in every job that needs it precisely because of the equality asserted here.
/// A stubbed action therefore still produces a red build; it just fails one step
/// later, at the preflight, rather than here. Treat that preflight as the
/// backstop and this test as the thing that keeps the preflight wired in.
#[test]
fn every_ci_job_that_runs_this_crates_tests_provisions_the_host_capabilities_they_need() {
    let yaml = read_repo_text(WORKFLOW_REL);
    let jobs = workflow_jobs(&yaml);
    assert!(
        jobs.len() >= 5,
        "only {} job(s) parsed out of {WORKFLOW_REL} — its own header documents seven, so \
         the hand-rolled parser has lost the file's shape rather than CI having shrunk. \
         Every assertion below would be vacuous on an empty or near-empty job list.",
        jobs.len()
    );

    let mut needs_setup: BTreeSet<String> = BTreeSet::new();
    let mut has_setup: BTreeSet<String> = BTreeSet::new();
    let mut invocations = 0usize;

    for (name, raw_body) in &jobs {
        let body = without_full_line_comments(raw_body);

        // The job's own steps, plus any repo script those steps run: a job can
        // reach this crate's tests either way, and only one of them is visible
        // in ci.yml.
        let mut sources: Vec<(String, String)> =
            vec![(format!("{WORKFLOW_REL} job `{name}`"), body.clone())];
        for script in referenced_repo_scripts(&body) {
            let text = without_full_line_comments(&read_repo_text(&script));
            sources.push((format!("{script} (run by job `{name}`)"), text));
        }

        for (origin, text) in &sources {
            for line in text.lines() {
                if !line.contains("cargo test") {
                    continue;
                }
                invocations += 1;
                if cargo_test_line_reaches_server(origin, line) {
                    needs_setup.insert(name.clone());
                }
            }
        }

        if job_uses_local_action(&body, HOST_SETUP_ACTION_DIR) {
            has_setup.insert(name.clone());
        }
    }

    assert!(
        invocations >= 4,
        "the scan saw only {invocations} `cargo test` invocation(s) across {WORKFLOW_REL} \
         and the scripts it runs. There are at least four (core's workspace sweep, the \
         contract suites, the sandbox preflight, the escape battery), so this is the scan \
         breaking, not CI dropping its tests — and a scan that sees no invocations \
         classifies no job as needing setup and passes having checked nothing."
    );
    assert!(
        needs_setup.len() >= 3,
        "only {} job(s) classified as running git-vista-server's tests ({needs_setup:?}). \
         `core`, `contract` and `sandbox` all do, so fewer than three means the \
         classification broke; with an empty set the equality below would be satisfied by \
         a workflow that provisions nothing at all.",
        needs_setup.len()
    );

    assert_eq!(
        needs_setup,
        has_setup,
        "every {WORKFLOW_REL} job that runs git-vista-server's tests must reference the \
         shared host-setup action `{HOST_SETUP_ACTION_DIR}`, and only those jobs may. \
         Runs this crate's tests without provisioning the host: {:?} — those jobs \
         construct the Strict tier, so every git spawn in them is refused rather than \
         downgraded (ADR 0029) and their failures look like product bugs. Provisions the \
         host without running this crate's tests: {:?} — the action weakens the runner \
         (it clears kernel.apparmor_restrict_unprivileged_userns), so a job with no reason \
         to need it should not carry it. Equality rather than a subset is deliberate: the \
         defect being fixed was two hand-maintained sides of one relationship drifting, \
         and a subset check only ever sees one direction of that.",
        needs_setup.difference(&has_setup).collect::<Vec<_>>(),
        has_setup.difference(&needs_setup).collect::<Vec<_>>(),
    );

    // The action itself: referenced by every testing job above, which proves
    // nothing at all if the file is missing or has been emptied.
    let action_rel = format!("{HOST_SETUP_ACTION_DIR}/action.yml");
    assert!(
        repo_root_dir().join(&action_rel).is_file(),
        "{action_rel} does not exist, yet {has_setup:?} name it in a `uses:` step. A local \
         composite action is resolved from the checked-out tree, so this is a workflow that \
         cannot start — and if the directory was renamed, rename it in \
         HOST_SETUP_ACTION_DIR too so both halves of this test move together."
    );
    let action = without_full_line_comments(&read_repo_text(&action_rel));
    assert!(
        action.contains("using:") && action.contains("composite"),
        "{action_rel} does not declare `runs.using: composite`. Only a composite action can \
         be referenced with `uses: ./...` from three jobs the way this one is; anything else \
         means the jobs above reference something that will not run."
    );
    for (token, why) in HOST_SETUP_TOKENS {
        assert!(
            action.contains(token),
            "{action_rel} no longer mentions `{token}` — the action {why}. Reducing the \
             shared setup to a stub leaves every job's `uses:` in place and every check \
             above green while the hosts go unprovisioned, which is exactly the state this \
             test exists to make impossible."
        );
    }
    for (token, why) in [
        (
            "sysctl",
            "the userns unclamp has to actually write the sysctl",
        ),
        (
            "git config",
            "the git identity has to actually be configured",
        ),
        (
            "user.name",
            "git needs a name as well as an email to author a commit",
        ),
        (
            "::error::",
            "D6 Option A requires the unclamp to fail LOUDLY if the write does not take — a \
             step that silently falls through to a degraded run is indistinguishable from \
             one that worked, and would hand the battery back the vacuity it was built to \
             remove",
        ),
    ] {
        assert!(
            action.contains(token),
            "{action_rel} no longer contains `{token}`: {why}"
        );
    }

    // The other direction: the provisioning lives in the action and only in the
    // action. Re-inlining it into a job is how three copies of a
    // security-relevant preflight drifted in the first place.
    let workflow_steps = without_full_line_comments(&yaml);
    for (token, why) in HOST_SETUP_TOKENS {
        assert!(
            !workflow_steps.contains(token),
            "{WORKFLOW_REL} spells `{token}` in a step of its own — the action {why}, and it \
             must be the only place that does. A per-job copy is what produced this bug: \
             `sandbox` carried the setup, `core` and `contract` did not, and the three were \
             supposed to share one preflight. Put it in {HOST_SETUP_ACTION_DIR} and reference \
             it. (Prose is fine — this scan reads comment-stripped text.)"
        );
    }
}
