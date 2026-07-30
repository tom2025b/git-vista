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
    /// The errno the **inside** leg must observe for containment to hold.
    pub expect_inside: Errno,
    /// R3: the paired positive, mandatory on every case. A sibling operation,
    /// same run, same policy, same probe binary, that must still succeed —
    /// without this a denial claim is unattributable (see the contract's
    /// `enumerate()`-is-omission argument).
    pub expect_granted: Errno,
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
) -> Result<i32, MissingObservation> {
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
        if let Ok(v) = tok.parse::<i32>() {
            return Ok(v);
        }
    }
    Err(MissingObservation {
        detail: format!("no `{tag} rc=.. errno=..` line inside the marked block"),
    })
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
/// seam (R6): `sandbox::spawn::command_async` — never `command_sync`, never
/// `shim_cli::launch`.
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

/// R8: builds the policy for a case. Production-constructible cases (no
/// exemption) go through the real `policy_for_repo` — the same function
/// production calls, zero change. Exempted cases (Strict, blocked hooks) are
/// built directly here, in the harness, since `policy_for_repo` cannot
/// represent them yet (Task 8 is blocked). This function itself contains a
/// `Policy { .. }` literal on purpose — R6's ban on that literal scopes to
/// `escape_suite.rs`/`hook_mode_suite.rs`, not to the harness that serves
/// them, because the point of R6 is that the *battery* cannot fabricate its
/// own policy; the harness fabricating one for a not-yet-wired tier, in one
/// reviewed place, is exactly R8's expiring exemption.
fn policy_for_case(case: &EscapeCase, repo: &Path) -> Policy {
    if case.exemption == Exemption::None {
        return policy_for_repo(repo)
            .expect("policy_for_repo must build for a case with no R8 exemption");
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

fn execute(case: &EscapeCase, nonce: &str) -> Outcome {
    let base_repo = fixture();
    let inside_repo = fixture();

    install_hook(
        base_repo.path(),
        &(case.build_hook)(&HarnessCtx {
            repo: base_repo.path(),
            nonce,
        }),
    );
    install_hook(
        inside_repo.path(),
        &(case.build_hook)(&HarnessCtx {
            repo: inside_repo.path(),
            nonce,
        }),
    );

    let baseline = commit_outside(base_repo.path());
    let base_obs = parse_observation(&baseline.combined, nonce, case.probe_tag);
    let base_ok = matches!(base_obs, Ok(v) if v == case.expect_baseline.0);
    if !base_ok {
        return Outcome::CapabilityAbsent {
            case: case.id,
            missing: match base_obs {
                Ok(v) => format!(
                    "baseline {} wanted errno {} got {v}",
                    case.probe_tag, case.expect_baseline.0
                ),
                Err(e) => format!(
                    "baseline {} observation missing: {}",
                    case.probe_tag, e.detail
                ),
            },
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
        let marker = inside_repo.path().join(".git/gv_escape_hook_ran");
        let observed = match std::fs::metadata(marker) {
            Ok(_) => 0,
            Err(e) => e.raw_os_error().unwrap_or(-1),
        };
        (observed, inside.commit_code)
    } else {
        let observed =
            parse_observation(&inside.combined, nonce, case.probe_tag).unwrap_or_else(|e| {
                panic!(
                    "{}: inside-leg `{}` observation missing: {}",
                    case.id, case.probe_tag, e.detail
                )
            });
        let granted = parse_observation(&inside.combined, nonce, "GRANTED").unwrap_or_else(|e| {
            panic!(
                "{}: inside-leg GRANTED observation missing (R3): {}",
                case.id, e.detail
            )
        });
        (observed, granted)
    };

    if inside_obs != case.expect_inside.0 {
        return Outcome::Escaped {
            detail: format!(
                "{}: wanted inside errno {} got {inside_obs}",
                case.probe_tag, case.expect_inside.0
            ),
        };
    }
    if granted_obs != case.expect_granted.0 {
        return Outcome::Escaped {
            detail: format!(
                "GRANTED: wanted errno {} got {granted_obs} — R3's paired positive failed, \
                 the policy denied more than the claim",
                case.expect_granted.0
            ),
        };
    }
    Outcome::Contained
}

/// The chokepoint (R11): every `#[test]` body in the battery is exactly this
/// call. It always returns a value (R4 — no early `return`, no panic on
/// absence), always records (R5) before ever failing loudly, and only raises
/// an assertion failure for a genuine `Escaped` outcome.
pub(crate) fn run_case(case: &EscapeCase) -> Outcome {
    let nonce = fresh_nonce();
    let outcome = execute(case, &nonce);
    report(case, &outcome);
    if let Outcome::Escaped { detail } = &outcome {
        panic!("{}: ESCAPED — {detail}", case.id);
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
/// and the shared parser is `Result`, never `Option<i32>`.
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
        code.contains("fn parse_observation") && code.contains("Result<i32, MissingObservation>"),
        "R2: the probe-output parser must be `-> Result<i32, MissingObservation>`"
    );
    assert!(
        !code.contains("-> Option<i32>"),
        "R2: no probe-output parser in escape_contract.rs may return Option<i32>"
    );
}

/// R3: every case carries the mandatory paired-positive field, and `run_case`
/// actually asserts it rather than merely storing it.
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
    assert!(
        body.contains("expect_granted") && body.contains("\"GRANTED\""),
        "R3: expect_granted must be asserted inside the harness's per-case runner, \
         against a paired-positive observation, not merely stored on the case"
    );
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

/// R8: the named blocker for each exemption must still exist in production
/// source. `policy_for_repo` hard-coding `Tier::Network`/`HookMode::Run` is
/// today's blocker for the Strict and blocked-hooks exemptions (Task 8 is
/// what removes it) — when it goes, this fails the build and forces step 9.
#[test]
fn r8_exemptions_expire_when_their_named_blocker_disappears() {
    let code = crate::argv_boundary::code_only(&read_rs("src/sandbox/mod.rs"));
    let body = fn_body_in(&code, "policy_for_repo");
    assert!(
        body.contains("Tier::Network"),
        "R8: policy_for_repo no longer hard-codes Tier::Network — the Strict-tier \
         exemption in escape_suite.rs must be retired (step 9), not left standing"
    );
    assert!(
        body.contains("HookMode::Run"),
        "R8: policy_for_repo no longer hard-codes HookMode::Run — the blocked-hooks \
         exemption in hook_mode_suite.rs must be retired (step 9), not left standing"
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
    assert!(
        missing.is_empty(),
        "::error::sandbox CI preflight: host missing {missing:?} — the escape battery \
         cannot produce sound containment evidence on this runner"
    );
}
