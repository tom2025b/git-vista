//! M1.13b (#66) Task 9, part 2: the **escape** half of the boot probe —
//! INV-13 / Global Constraint 15.
//!
//! `sandbox::capabilities` (Task 9, part 1) answers "could this host even
//! try?" by reading kernel facts. This file answers a different question:
//! "did the **composed** launcher actually contain a hostile child, on this
//! host, right now?" — proved by launching it, never by asking. The two
//! answers are never merged into one boolean: a host that *reports* bwrap
//! present can still fail to launch it, and only an execution proves which.
//!
//! **This probe GATES BOOT** (see `main.rs`). A verdict other than
//! [`ProbeVerdict::Contained`] refuses to start the server — no degrade, no
//! "run anyway with hooks blocked". ADR 0029 is the binding record of that
//! decision and of the distinction that matters most: a host that genuinely
//! *cannot* supply a capability ([`ProbeVerdict::CapabilityAbsent`]) must be
//! reported as exactly that, and must never be reported as
//! [`ProbeVerdict::FailOpen`] or silently folded into a weaker posture. An
//! operator reading `capability_absent missing=["bwrap"]` installs
//! bubblewrap; an operator reading `fail_open failed=[...]` has found a
//! security bug in git-vista. Collapsing the two would make the second look
//! like the first.
//!
//! **What a green verdict proves, and what it does not.** It proves *the
//! host* can compose bwrap + Landlock + seccomp into a boundary a hook cannot
//! cross — a fact that does not change between requests. It is **not** a
//! substitute for per-repository policy construction: the server is
//! multi-repository and dynamic (boot registers configured-root children and
//! persistent clones; `?repo=` addresses any catalog entry; rescan adds
//! repositories after launch; clone creates one at runtime), and
//! `policy_for`/`policy_for_repo` grants read-write to the **specific repo
//! argument** passed to it. Treating this boot probe as covering every later
//! operation — or "optimising away" per-operation policy construction because
//! this already ran — would grant every repository the boot scratch repo's
//! grants. Per-operation policy construction is Task 8's territory and is not
//! made redundant by anything here.
//!
//! # Observation is by marker file, not by a JSON self-probe
//!
//! There is no `--self-probe` route: the shim's `parse()` has no such arm and
//! `validate()` refuses any argv whose program is not literally `git`. So
//! this observes the way the escape battery observes: a hostile `pre-commit`
//! hook writes its one measured fact into a file under a tree the policy
//! already grants read-write, read back after the composed launcher exits. A
//! missing or unparsable marker is a hard failure, never a silent pass (R2 —
//! `None` is not a pass).
//!
//! # Deliberate deviations from the original plan sketch (`docs/superpowers/plans/2026-07-28-m1.13b-sandbox.md`, Task 9)
//!
//! Verified against current source rather than trusted from the plan's own
//! (marked-unreliable) line citations:
//!
//! * **No `GV_MARKERS` environment variable.** The plan's sketch set one on
//!   the launcher's command, but `sandbox::spawn::SandboxedCommand` —
//!   deliberately, per its own doc comment and a compile-time-checked test —
//!   exposes no way for *production* code to set an environment variable on
//!   the command it spawns (only a `#[cfg(test)]` setter exists, gated
//!   precisely so production cannot reopen C10 hazard #1). Adding one would
//!   mean editing `sandbox/spawn.rs`, which this task does not own. Instead,
//!   the fixture lays `markers/` out as a sibling of `repo/` and the hook
//!   reaches it with the plain relative path `../markers` — verified
//!   directly that a `pre-commit` hook's working directory is the
//!   repository's worktree root regardless of how git was invoked (`-C
//!   <repo>` included), so no environment plumbing is needed at all.
//! * **No `sandbox-negative-controls` feature / `DegradeMode` / `--degrade`.**
//!   The plan's Task 9 also specifies a test-only feature that weakens the
//!   shim on command, to prove each layer's absence is detectable. Wiring it
//!   requires: (a) a `DegradeMode` enum and a `degrade` field on `Policy` in
//!   `sandbox/mod.rs`, a file this task is authorised to touch for exactly
//!   one line (`pub(crate) mod probe;`) and no more; (b) promoting
//!   `escape_suite::probe_in_repo` to `pub(crate)` to reuse its `cc`-compiled
//!   io_uring probe, and `escape_suite.rs` is explicitly off-limits for this
//!   task. Both requirements cross into files this task does not own, so per
//!   this task's own instructions ("skip rather than reach across") the
//!   feature is not implemented here. The boot gate itself, and its refusal
//!   behaviour, are proved instead by unit-testing the decision functions
//!   below (`missing_capabilities`, `baseline_failed_verdict`,
//!   `evaluate_observation`, `to_boot_result`) against hand-built inputs that
//!   stand in for a host lacking a capability — see the test module.

use std::path::{Path, PathBuf};

use super::spawn::command_async;
use super::{default_system_trees, secret_excludes_for_home, HookMode, Policy, Tier};

// The capability half lives in `sandbox::capabilities` and is not redeclared
// here. Re-exported under the name the rest of the plan's batteries (Tasks
// 10/12/14, not built by this task) expect to import, so there is exactly one
// measurement in the crate.
pub(crate) use super::capabilities::{probe as capabilities, Capabilities};

/// **Only `Contained` permits boot.** The other two variants both refuse
/// (INV-13 / Global Constraint 15) — they are separate variants for the sake
/// of the *diagnosis*, not because the action differs. Collapsing them, or
/// mapping either to a "run anyway, maybe with hooks blocked" posture, is the
/// best-effort downgrade Global Constraint 1 (C5) forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeVerdict {
    /// Every check the boot probe made was closed, and its paired positive
    /// was granted in the same run.
    Contained,
    /// The composed launcher's cheapest real operation (`git --version`, no
    /// hook, no write) did not exit 0 — the host cannot supply the tier at
    /// all. **Not** a hole: a different state entirely, and the honest local
    /// result on a host without bubblewrap or without unprivileged user
    /// namespaces. `missing` is named from `Capabilities` *after* the launch
    /// failed, so the words come from the fact and the decision comes from
    /// the execution (R4).
    CapabilityAbsent { missing: Vec<&'static str> },
    /// The composed launcher ran, but a check that should have been closed
    /// was open, or a marker that should have been written was absent. This
    /// is a hole (or an unobservable probe, which must never be reported as
    /// green) — a git-vista bug, not a host configuration problem.
    FailOpen { failed_checks: Vec<String> },
}

/// INV-13's refusal. [`run_at_startup`] returns `Ok` only for
/// [`ProbeVerdict::Contained`]; every other verdict becomes this error, and
/// `main` exits rather than starting a degraded server.
#[derive(Debug)]
pub(crate) struct BootRefusal {
    pub verdict: ProbeVerdict,
}

impl std::fmt::Display for BootRefusal {
    /// Names what was missing (or what failed) and why the server will not
    /// start, so an operator reading stderr can act on it without reading
    /// this source file.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.verdict {
            ProbeVerdict::Contained => write!(
                f,
                "sandbox probe refused boot (BUG: Contained is not a refusal)"
            ),
            ProbeVerdict::CapabilityAbsent { missing } => write!(
                f,
                "sandbox unavailable: this host cannot supply the strict tier \
                 (missing: {missing:?}). Install bubblewrap and enable unprivileged \
                 user namespaces, then restart. (INV-13 — there is no degraded mode.)"
            ),
            ProbeVerdict::FailOpen { failed_checks } => write!(
                f,
                "sandbox self-test failed: the composed launcher did not contain a \
                 hostile hook (checks: {failed_checks:?}). This is a git-vista bug, \
                 not a host configuration problem — do not work around it."
            ),
        }
    }
}

/// One marker-file observation, keyed by check name. `String` values, not
/// booleans: the seccomp checks report the kernel's own numbers and the
/// procfs check reports a pid, and collapsing those to a bool would be how a
/// "family of accepted values" gets reintroduced.
pub(crate) type Observation = std::collections::BTreeMap<String, String>;

/// Why the composed launcher could not be observed. Kept as three named
/// cases rather than one `Option`/`bool`: "never spawned", "spawned but the
/// carrier failed", and "ran but wrote no markers" have three different
/// meanings, and only a caller who already knows capability was established
/// (the baseline leg already passed) can safely treat any of them as
/// `FailOpen` rather than `CapabilityAbsent`.
#[derive(Debug)]
pub(crate) enum LaunchFailed {
    Spawn,
    Carrier(Option<i32>),
    NoMarkers,
}

/// The path the hostile hook must never be able to write. It lives under
/// `$HOME`, which the policy grants **read-only** — so a denial here is
/// attributable to read-vs-write on a *granted* tree, not to the tree simply
/// never having been granted (the same EACCES the kernel reports either way,
/// which is the whole reason R3 pairs this with a mandatory positive
/// control below).
const ESCAPE_WITNESS: &str = ".gv-boot-probe-escape";

/// The boot probe's hostile `pre-commit` hook, built (not a bare string
/// literal) so [`ESCAPE_WITNESS`] cannot drift between the script and the
/// host-side check in [`verdict`]. Shell only — no `cc` — so the boot path
/// carries no toolchain dependency and costs one bwrap spawn plus a `git
/// init`.
///
/// See the module doc for why this reaches its markers via the plain
/// relative path `../markers` rather than an environment variable.
///
/// It always exits 0: the *carrier* (`git commit --allow-empty`) must land,
/// or the hook's own writes would be what failed rather than the boundary
/// under test. Three checks, one per layer of the composition:
///   * `fs_write_outside` / `fs_write_inside` — Landlock, paired (R3).
///   * `seccomp_mode` / `no_new_privs` — seccomp, read from the kernel's own
///     self-report **across the execve**, which is the one observation a
///     `--self-probe` route could not have produced even if it existed.
///   * `procfs_max_pid` — bwrap's pid namespace + fresh procfs (C3). This
///     layer has no `--degrade` control by design: bwrap's args come from
///     the `STRICT_BWRAP_ARGS` constant and there must be no runtime knob
///     that drops the namespace.
///
/// Deliberately **not** checked here: AF_UNIX, io_uring, secret reads, fd
/// inheritance, lifecycle. Those need `cc`, a listener, or wall-clock timing
/// and belong to the escape battery (which runs in CI), not a check that
/// must run cheaply on every boot.
fn boot_probe_hook_script() -> String {
    format!(
        r#"#!/bin/sh
m="../markers"
# Landlock: $HOME is granted read-only, so this write must fail.
if : > "$HOME/{ESCAPE_WITNESS}" 2>/dev/null; then echo OPEN; else echo DENIED; fi > "$m/fs_write_outside"
# R3's mandatory paired positive: a sibling write inside the repo's OWN
# granted rw tree (cwd is the repo's worktree root). If this is not OK, the
# denial above proves nothing about the boundary.
if : > payload.txt 2>/dev/null; then echo OK; else echo FAIL; fi > "$m/fs_write_inside"
# The kernel's own self-report, after execve — stronger provenance than any
# errno, and shell-readable.
awk '/^Seccomp:/{{print $2}}'    /proc/self/status > "$m/seccomp_mode"
awk '/^NoNewPrivs:/{{print $2}}' /proc/self/status > "$m/no_new_privs"
# C3: a fresh procfs shows a handful of small pids; the host's real procfs
# shows many more.
ls /proc | grep -E '^[0-9]+$' | sort -n | tail -1 > "$m/procfs_max_pid"
exit 0
"#
    )
}

/// A throwaway repository whose `pre-commit` hook is
/// [`boot_probe_hook_script`], plus a sibling `markers/` directory the
/// policy grants read-write. Markers live *beside* the repo, not inside it,
/// so the hook's own bookkeeping can never be mistaken for repository
/// content and `fs_write_inside`'s paired positive genuinely lands in the
/// repo tree proper.
///
/// The `TempDir` is held for the fixture's whole lifetime: dropping it
/// deletes everything, so callers must keep this alive until every marker
/// has been read.
struct BootProbeFixture {
    dir: tempfile::TempDir,
}

impl BootProbeFixture {
    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn markers(&self) -> PathBuf {
        self.dir.path().join("markers")
    }
}

/// `git init` + write the hook + make it executable, all **outside** the
/// sandbox: this is fixture construction, not the thing under test — the
/// same standing carve-out every other fixture builder in this crate relies
/// on (e.g. `shim_cli::fixture`). Running it unsandboxed also keeps
/// causality clean: if this step ever failed, the composed launcher's
/// baseline leg (the actual capability evidence, in [`verdict`]) would not
/// even have run yet, so a fixture failure can never be misreported as
/// capability absence.
fn boot_probe_fixture() -> std::io::Result<BootProbeFixture> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    let markers = dir.path().join("markers");
    std::fs::create_dir_all(&repo)?;
    std::fs::create_dir_all(&markers)?;

    let status = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&repo)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("git init failed for the boot probe fixture"));
    }

    let hooks_dir = repo.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("pre-commit");
    std::fs::write(&hook_path, boot_probe_hook_script())?;
    let mut perms = std::fs::metadata(&hook_path)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&hook_path, perms)?;

    Ok(BootProbeFixture { dir })
}

/// The Strict policy the boot probe runs the fixture under.
///
/// Hand-built rather than routed through `policy_for`/`policy_for_repo`:
/// those hard-code `Tier::Network` until Task 8 wires real per-operation tier
/// dispatch in (see `sandbox::policy_for`'s own doc comment), and the whole
/// point of this probe is to prove the **Strict** tier — the one that needs
/// bwrap and userns — actually composes. Written field-for-field in the same
/// order `policy_for` uses so the two stay diffable; when Task 8 lands this
/// should become `policy_for(scratch, false, ...)` with an explicit
/// `Tier::Strict` override rather than staying an independent constructor.
///
/// `HookMode::Run` — **not** `Blocked` — because the hook is the observer
/// here; blocking it would make the probe blind to everything it exists to
/// check.
fn boot_probe_policy(scratch: &Path, markers: &Path) -> Result<Policy, &'static str> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME")?);
    let (mut rw, mut ro) = default_system_trees(Tier::Strict);
    rw.push(scratch.to_path_buf());
    rw.push(markers.to_path_buf());
    ro.push(home.clone());
    Ok(Policy {
        tier: Tier::Strict,
        shim: super::shim::shim_path().map_err(|_| "shim")?.to_path_buf(),
        bwrap: Some(super::bwrap::bwrap_path().ok_or("bwrap")?.to_path_buf()),
        rw_trees: rw,
        ro_trees: ro,
        secret_excludes: secret_excludes_for_home(&home),
        net_ports: Vec::new(),
        hook_mode: HookMode::Run,
    })
}

/// Which named capability is absent, given a measured [`Capabilities`] — the
/// words [`ProbeVerdict::CapabilityAbsent`] and the boot log line use. Pure
/// and host-independent, so the property "a missing capability is named,
/// never silently folded into fail-open" is unit-testable without a broken
/// host — see the test module's `missing_capabilities_names_every_absent_knob`.
fn missing_capabilities(caps: &Capabilities) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !caps.landlock_meets_floor() {
        missing.push("landlock_abi>=6");
    }
    if !caps.seccomp_available {
        missing.push("seccomp");
    }
    if !caps.userns {
        missing.push("user_namespaces");
    }
    if !caps.bwrap_present {
        missing.push("bwrap");
    }
    if missing.is_empty() {
        // Every named knob said yes and the launch still did not happen —
        // capability absence with no more specific name to give it.
        missing.push("strict_launch");
    }
    missing
}

/// The verdict when the composed launcher's baseline leg (the cheapest real
/// Strict operation there is — see [`verdict`]) did not exit 0. `caps`
/// supplies the *words* for `missing`; the deciding read was the launch
/// itself (R4), never this capability query.
///
/// Split out from `verdict` so INV-13's central property — "a baseline
/// failure becomes `CapabilityAbsent`, named, and never `FailOpen`" — is
/// unit-testable against a hand-built `Capabilities` standing in for a real
/// host that lacks a capability, without needing to actually break this
/// development host's sandbox. See the test module's
/// `a_missing_capability_reports_absent_never_fail_open`.
fn baseline_failed_verdict(caps: &Capabilities) -> ProbeVerdict {
    ProbeVerdict::CapabilityAbsent {
        missing: missing_capabilities(caps),
    }
}

/// Score one completed [`Observation`] against the declared-closed checks.
/// `Ok(())` only if every check is present and exactly the declared value —
/// a missing marker fails exactly like a wrong one (R2: a missing marker is
/// not a pass).
///
/// Split out from `verdict` so the "wrong-or-missing observation becomes
/// `FailOpen`" mapping is unit-testable against hand-built observations,
/// without needing a real degraded sandbox to produce one. See the test
/// module's `evaluate_observation_rejects_missing_and_wrong_markers`.
fn evaluate_observation(obs: &Observation) -> Result<(), Vec<String>> {
    let mut failed = Vec::new();
    for (check, want) in [
        ("fs_write_outside", "DENIED"), // Landlock
        ("fs_write_inside", "OK"),      // R3's mandatory paired positive
        ("seccomp_mode", "2"),          // SECCOMP_MODE_FILTER
        ("no_new_privs", "1"),
    ] {
        match obs.get(check) {
            Some(v) if v == want => {}
            Some(v) => failed.push(format!("{check}={v} want={want}")),
            None => failed.push(format!("{check}=<no marker>")),
        }
    }
    // C3: a fresh procfs (bwrap's pid namespace) shows a handful of small
    // pids; the host's real procfs shows many more. Same threshold the
    // escape battery uses elsewhere in this crate.
    match obs.get("procfs_max_pid").and_then(|v| v.parse::<i64>().ok()) {
        Some(p) if p > 0 && p < 100 => {}
        Some(p) => failed.push(format!("procfs_max_pid={p} want<100")),
        None => failed.push("procfs_max_pid=<no marker>".to_string()),
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed)
    }
}

/// The single observer. Runs the composed launcher **once**, through the one
/// sanctioned route (`sandbox_argv` + `sandbox::spawn::command_async`, which
/// `command_async` assembles internally), and returns the marker files the
/// hook managed to write. `repo`'s parent directory is where the fixture puts
/// `markers/` (see [`BootProbeFixture`]).
pub(crate) async fn observe(policy: &Policy, repo: &Path) -> Result<Observation, LaunchFailed> {
    // `-c user.*` because the server's git identity is not guaranteed and a
    // commit without one aborts before hook discovery — which would look
    // like containment when it is really "no identity configured". The
    // carrier must land.
    let out = command_async(
        policy,
        repo,
        &[
            "-c",
            "user.name=gv-boot-probe",
            "-c",
            "user.email=gv@localhost",
            "commit",
            "--allow-empty",
            "-m",
            "boot-probe",
        ],
    )
    .output()
    .await
    .map_err(|_| LaunchFailed::Spawn)?;
    if !out.status.success() {
        return Err(LaunchFailed::Carrier(out.status.code()));
    }
    let markers = repo
        .parent()
        .ok_or(LaunchFailed::NoMarkers)?
        .join("markers");
    let mut obs = Observation::new();
    for entry in std::fs::read_dir(&markers).map_err(|_| LaunchFailed::NoMarkers)? {
        let Ok(entry) = entry else { continue };
        let Ok(value) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        obs.insert(
            entry.file_name().to_string_lossy().into_owned(),
            value.trim().to_owned(),
        );
    }
    Ok(obs)
}

/// The escape half in full. Builds a throwaway repository whose `pre-commit`
/// hook attempts one escape per layer, runs the **composed** launcher
/// against it, and reads back what the hook managed to do. Never installs a
/// primitive itself and never asks the host a question it then decides on
/// (R4 — capability is established by execution).
pub(crate) async fn verdict(caps: &Capabilities) -> ProbeVerdict {
    let Ok(fixture) = boot_probe_fixture() else {
        return ProbeVerdict::CapabilityAbsent {
            missing: vec!["scratch_dir"],
        };
    };
    let Ok(policy) = boot_probe_policy(&fixture.repo(), &fixture.markers()) else {
        return baseline_failed_verdict(caps);
    };

    // The baseline leg: `git --version` writes nothing, runs no hook, and
    // exercises the whole composition end to end. This — not any capability
    // query — is the capability evidence.
    let baseline_ok = command_async(&policy, &fixture.repo(), &["--version"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !baseline_ok {
        return baseline_failed_verdict(caps);
    }

    let obs = match observe(&policy, &fixture.repo()).await {
        Ok(o) => o,
        // Capability is already established by the baseline leg above, so a
        // launch failure HERE is not capability absence — it is an
        // unobservable probe, and an unobservable probe must never be
        // reported as green.
        Err(e) => {
            return ProbeVerdict::FailOpen {
                failed_checks: vec![format!("probe_unobservable:{e:?}")],
            }
        }
    };

    let mut failed = evaluate_observation(&obs).err().unwrap_or_default();

    // Belt and braces: the host-side witness. If Landlock were somehow off,
    // the hook's own self-report would not be the only evidence — the file
    // really is there. Cleaned up either way so a run never leaves it
    // behind.
    if let Some(h) = std::env::var_os("HOME") {
        let witness = PathBuf::from(h).join(ESCAPE_WITNESS);
        if witness.exists() {
            failed.push("fs_write_outside=host_witness_present".to_string());
            let _ = std::fs::remove_file(&witness);
        }
    }

    if failed.is_empty() {
        ProbeVerdict::Contained
    } else {
        ProbeVerdict::FailOpen {
            failed_checks: failed,
        }
    }
}

/// The INV-13/Global Constraint 15 mapping: [`ProbeVerdict::Contained`] is
/// the only verdict that permits boot. Split out from `run_at_startup` so it
/// is unit-testable against a constructed `ProbeVerdict` — proving the
/// refusal actually fires — without needing a real broken host or launching
/// any process. See the test module's
/// `only_contained_permits_boot_every_other_verdict_refuses`.
fn to_boot_result(verdict: ProbeVerdict) -> Result<ProbeVerdict, BootRefusal> {
    match verdict {
        ProbeVerdict::Contained => Ok(verdict),
        other => Err(BootRefusal { verdict: other }),
    }
}

/// Called once from `main`, **before anything spawns git** and before either
/// listener binds. Prints the measured capability booleans first, then the
/// verdict — in that order, so a log cut short still distinguishes the two.
///
/// **Returns `Ok` only for `Contained`.** INV-13 / Global Constraint 15: a
/// host that cannot supply the declared tier does not get a degraded server;
/// it gets no server. The cost is stated plainly rather than buried: **this
/// makes git-vista unusable on a host without bubblewrap.** That is the
/// accepted trade (ADR 0029).
pub(crate) async fn run_at_startup() -> Result<ProbeVerdict, BootRefusal> {
    let caps = capabilities();
    println!(
        "[sandbox] landlock_abi={} seccomp_available={} userns={} bwrap_present={}",
        caps.landlock_abi, caps.seccomp_available, caps.userns, caps.bwrap_present
    );
    let v = verdict(&caps).await;
    match &v {
        ProbeVerdict::Contained => {
            println!("[sandbox] verdict=contained — the strict tier composes on this host");
            // Said out loud on every boot, so nobody reads the green line as
            // "policy is settled": it is not. Per-repository policy
            // construction still runs for every operation (see module doc).
            println!(
                "[sandbox] this proves the HOST can supply the tier; per-repository \
                 policy is still built per operation"
            );
        }
        ProbeVerdict::CapabilityAbsent { missing } => {
            eprintln!("[sandbox] verdict=capability_absent missing={missing:?}");
            eprintln!(
                "[sandbox] refusing to start: this host cannot supply the strict tier \
                 (INV-13). Install bubblewrap and enable unprivileged user namespaces."
            );
        }
        ProbeVerdict::FailOpen { failed_checks } => {
            eprintln!("[sandbox] verdict=fail_open failed={failed_checks:?}");
            eprintln!(
                "[sandbox] refusing to start: the composed launcher did NOT contain a \
                 hostile hook. This is a git-vista bug, not a host problem — do not \
                 work around it."
            );
        }
    }
    to_boot_result(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INV-13's naming half: which capability is named absent, given a
    /// measured `Capabilities` — deterministic and host-independent, unlike
    /// the real end-to-end tests below.
    #[test]
    fn missing_capabilities_names_every_absent_knob() {
        let none = Capabilities {
            landlock_abi: -1,
            bwrap_present: false,
            userns: false,
            seccomp_available: false,
        };
        let missing = missing_capabilities(&none);
        for want in ["landlock_abi>=6", "seccomp", "user_namespaces", "bwrap"] {
            assert!(
                missing.contains(&want),
                "missing={missing:?} does not name {want}"
            );
        }
    }

    /// A host that reports every knob present but still failed to launch
    /// gets a name too — `missing` is never empty, or an operator reading
    /// `capability_absent missing=[]` in the log would have nothing to act
    /// on.
    #[test]
    fn a_launch_failure_with_every_knob_present_still_names_something() {
        let full = Capabilities {
            landlock_abi: 8,
            bwrap_present: true,
            userns: true,
            seccomp_available: true,
        };
        assert_eq!(missing_capabilities(&full), vec!["strict_launch"]);
    }

    /// INV-13's deciding half, and **the negative case this task's
    /// verification contract requires**: a simulated missing capability (a
    /// hand-built `Capabilities`, standing in for a real host that lacks
    /// userns/bwrap/seccomp/Landlock) is proven to produce
    /// `CapabilityAbsent`, never `FailOpen` — without launching any process
    /// or needing an actually-broken host.
    #[test]
    fn a_missing_capability_reports_absent_never_fail_open() {
        let caps = Capabilities {
            landlock_abi: -1,
            bwrap_present: false,
            userns: false,
            seccomp_available: false,
        };
        let v = baseline_failed_verdict(&caps);
        assert!(
            matches!(v, ProbeVerdict::CapabilityAbsent { .. }),
            "a baseline failure must report capability_absent, got {v:?}"
        );
        if let ProbeVerdict::CapabilityAbsent { missing } = &v {
            assert!(!missing.is_empty(), "missing must never be silently empty");
        }
    }

    /// The gate itself, proven by unit-testing the decision function rather
    /// than by launching a process (per this task's verification contract):
    /// every non-`Contained` verdict — including the simulated
    /// `CapabilityAbsent` above — becomes `Err`, which is exactly what
    /// `main`'s `Err(refusal) => { eprintln!(...); exit(1) }` arm acts on.
    /// `Contained` is the only verdict that becomes `Ok`.
    #[test]
    fn only_contained_permits_boot_every_other_verdict_refuses() {
        assert!(to_boot_result(ProbeVerdict::Contained).is_ok());

        let absent = to_boot_result(ProbeVerdict::CapabilityAbsent {
            missing: vec!["bwrap"],
        });
        assert!(absent.is_err(), "capability_absent must refuse boot");
        assert!(matches!(
            absent.unwrap_err().verdict,
            ProbeVerdict::CapabilityAbsent { .. }
        ));

        let fail_open = to_boot_result(ProbeVerdict::FailOpen {
            failed_checks: vec!["fs_write_outside=OPEN want=DENIED".to_string()],
        });
        assert!(fail_open.is_err(), "fail_open must refuse boot too");
        assert!(matches!(
            fail_open.unwrap_err().verdict,
            ProbeVerdict::FailOpen { .. }
        ));
    }

    /// The refusal message names what was missing (or what failed), so an
    /// operator can act on it without reading this source file — this
    /// task's stated requirement for the gate's message.
    #[test]
    fn the_refusal_message_names_what_is_missing() {
        let refusal = BootRefusal {
            verdict: ProbeVerdict::CapabilityAbsent {
                missing: vec!["bwrap", "user_namespaces"],
            },
        };
        let msg = refusal.to_string();
        assert!(msg.contains("bwrap"), "message does not name bwrap: {msg}");
        assert!(
            msg.contains("user_namespaces"),
            "message does not name user_namespaces: {msg}"
        );

        let refusal = BootRefusal {
            verdict: ProbeVerdict::FailOpen {
                failed_checks: vec!["seccomp_mode=0 want=2".to_string()],
            },
        };
        let msg = refusal.to_string();
        assert!(
            msg.contains("seccomp_mode=0"),
            "message does not name the failed check: {msg}"
        );
    }

    /// `evaluate_observation`'s `FailOpen` half: a missing marker fails
    /// exactly like a wrong one, and a fully-correct observation passes.
    /// Hand-built observations, so this does not depend on ever actually
    /// degrading the real sandbox.
    #[test]
    fn evaluate_observation_rejects_missing_and_wrong_markers() {
        let mut good = Observation::new();
        good.insert("fs_write_outside".into(), "DENIED".into());
        good.insert("fs_write_inside".into(), "OK".into());
        good.insert("seccomp_mode".into(), "2".into());
        good.insert("no_new_privs".into(), "1".into());
        good.insert("procfs_max_pid".into(), "7".into());
        assert_eq!(evaluate_observation(&good), Ok(()));

        // A wrong value.
        let mut open = good.clone();
        open.insert("fs_write_outside".into(), "OPEN".into());
        let err = evaluate_observation(&open).unwrap_err();
        assert!(err.iter().any(|f| f.contains("fs_write_outside=OPEN")));

        // A missing marker fails exactly like a wrong one (R2: `None` is not
        // a pass).
        let mut missing = good.clone();
        missing.remove("seccomp_mode");
        let err = evaluate_observation(&missing).unwrap_err();
        assert!(err.iter().any(|f| f.contains("seccomp_mode=<no marker>")));

        // procfs_max_pid out of the expected small-namespace range.
        let mut big_pid = good.clone();
        big_pid.insert("procfs_max_pid".into(), "50000".into());
        let err = evaluate_observation(&big_pid).unwrap_err();
        assert!(err.iter().any(|f| f.contains("procfs_max_pid=50000")));
    }

    /// The full, real thing: the composed launcher genuinely runs against a
    /// hostile hook on this development host (known Landlock ABI 8, bwrap,
    /// userns and seccomp — see `capabilities.rs`'s own smoke test) and the
    /// verdict is `Contained`. This is the happy-path proof the deterministic
    /// unit tests above cannot give on their own: it exercises the real
    /// fixture, the real composed launcher, and the real marker files, end
    /// to end.
    #[tokio::test]
    async fn the_real_launcher_contains_the_hostile_hook_on_this_host() {
        assert_eq!(verdict(&capabilities()).await, ProbeVerdict::Contained);
    }

    /// `run_at_startup` end to end: `Ok` on this host, carrying `Contained`.
    #[tokio::test]
    async fn run_at_startup_succeeds_on_this_host() {
        let v = run_at_startup()
            .await
            .expect("this host is known to compose the strict tier");
        assert_eq!(v, ProbeVerdict::Contained);
    }
}
