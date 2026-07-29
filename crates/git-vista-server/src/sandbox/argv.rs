//! INV-16's value half: the reviewed structural shape of every launcher argv.
//! Pure — no process is spawned here. The *source*-level half of INV-16 lives
//! in `crate::argv_boundary`; this file pins the value the chokepoint produces.

use super::*;
use std::path::PathBuf;

/// A fixed, fake bwrap path. Fake on purpose: these tests pin the *shape* of
/// the argv, and pinning them to wherever bwrap really lives on the build host
/// would make them pass or fail for reasons that have nothing to do with the
/// chokepoint. `bwrap::resolve` is what tests the real lookup.
const FAKE_BWRAP: &str = "/usr/bin/bwrap";

fn policy(tier: Tier) -> Policy {
    Policy {
        tier,
        shim: PathBuf::from("/opt/gv/gv-sandbox"),
        bwrap: (tier == Tier::Strict).then(|| PathBuf::from(FAKE_BWRAP)),
        rw_trees: vec![PathBuf::from("/srv/repos/r")],
        ro_trees: vec![PathBuf::from("/usr"), PathBuf::from("/home/tom")],
        secret_excludes: vec![PathBuf::from("/home/tom/.ssh")],
        hook_mode: HookMode::Run,
    }
}

fn strs(argv: &[std::ffi::OsString]) -> Vec<String> {
    argv.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn pairs(argv: &[String]) -> Vec<(&str, &str)> {
    argv.windows(2)
        .map(|w| (w[0].as_str(), w[1].as_str()))
        .collect()
}

#[test]
fn every_sandboxed_argv_ends_in_dashdash_git() {
    for tier in [Tier::Strict, Tier::Network] {
        let a = strs(&sandbox_argv(&policy(tier)));
        assert_eq!(
            &a[a.len() - 2..],
            &["--".to_string(), "git".to_string()],
            "{tier:?}: launcher argv must end in `-- git`"
        );
    }
}

#[test]
fn unsandboxed_with_running_hooks_is_bare_git_and_nothing_else() {
    let a = strs(&sandbox_argv(&policy(Tier::Unsandboxed)));
    assert_eq!(a, vec!["git".to_string()]);
}

/// The combination that used to be silently dropped. `Unsandboxed` is an
/// operator-trusted repository and `Blocked` is a host that failed the declared
/// minimum (INV-13); together they are the one case where nothing in the argv
/// suppresses hooks unless this does. Returning a bare `["git"]` here meant a
/// policy that said "hooks blocked" ran them.
#[test]
fn unsandboxed_still_blocks_hooks_when_the_policy_says_blocked() {
    let mut p = policy(Tier::Unsandboxed);
    p.hook_mode = HookMode::Blocked {
        empty_dir: PathBuf::from("/var/lib/gv/no-hooks"),
    };
    let a = strs(&sandbox_argv(&p));
    assert_eq!(
        a,
        vec![
            "git".to_string(),
            "-c".to_string(),
            "core.hooksPath=/var/lib/gv/no-hooks".to_string(),
        ],
        "an unsandboxed policy with blocked hooks must still suppress them"
    );
    assert_eq!(a[0], "git", "the caller appends `-C <repo>` after this");
}

#[test]
fn strict_prefix_is_the_reviewed_constant_after_the_resolved_launcher() {
    let a = strs(&sandbox_argv(&policy(Tier::Strict)));
    let cut = a.iter().position(|s| s == "--").expect("a `--` separator");
    assert_eq!(
        a[0], FAKE_BWRAP,
        "the launcher must be the policy's resolved absolute path"
    );
    assert!(
        std::path::Path::new(&a[0]).is_absolute(),
        "a bare launcher name would be resolved against the inherited PATH"
    );
    assert_eq!(
        a[1..cut],
        STRICT_BWRAP_ARGS[..],
        "the bwrap arguments drifted from their reviewed constant"
    );
    assert_eq!(
        a[cut + 1],
        "/opt/gv/gv-sandbox",
        "the shim must follow bwrap's `--`"
    );
}

/// The launcher is never a bare name in any tier. This is the regression guard
/// for the `BWRAP_BIN = "bwrap"` hole: a `PATH`-resolved launcher could be
/// substituted wholesale, and because Landlock and seccomp are applied by the
/// *shim* that bwrap execs, the substitute would produce an identical argv and
/// an identical exit code with no namespaces at all.
#[test]
fn no_argv_entry_is_a_bare_bwrap_name() {
    for tier in [Tier::Strict, Tier::Network, Tier::Unsandboxed] {
        let a = strs(&sandbox_argv(&policy(tier)));
        assert!(
            !a.iter().any(|s| s == "bwrap"),
            "{tier:?}: the launcher must be an absolute path, never a bare name"
        );
    }
}

#[test]
fn network_tier_never_names_bwrap_and_never_unshares_net() {
    let a = strs(&sandbox_argv(&policy(Tier::Network)));
    assert_eq!(
        a[0], "/opt/gv/gv-sandbox",
        "the network tier launches the shim directly (F3: netns breaks push)"
    );
    assert!(!a.iter().any(|s| s.contains("bwrap") || s == "--unshare-net"));
    assert!(a.iter().any(|s| s == "--net-allow"));
}

#[test]
fn strict_tier_carries_c3_procfs_and_c4_devshm() {
    let a = strs(&sandbox_argv(&policy(Tier::Strict)));
    let w = pairs(&a);
    assert!(
        w.contains(&("--proc", "/proc")),
        "C3: a fresh procfs is mandatory"
    );
    assert!(
        w.contains(&("--tmpfs", "/dev/shm")),
        "C4: a private /dev/shm is mandatory"
    );
    assert!(a.iter().any(|s| s == "--net-deny"));
}

#[test]
fn grants_and_excludes_are_passed_through_as_separate_argv_entries() {
    let a = strs(&sandbox_argv(&policy(Tier::Network)));
    let w = pairs(&a);
    assert!(w.contains(&("--rw", "/srv/repos/r")));
    assert!(w.contains(&("--ro", "/home/tom")));
    assert!(
        w.contains(&("--exclude", "/home/tom/.ssh")),
        "D5 Option B: the secret list is explicit and auditable in the argv"
    );
    assert!(
        w.contains(&("--abi-floor", "6")),
        "C5: the floor travels in the argv, not a default"
    );
}

/// Tripwire for the corrected D5 Option B mechanism. `--deny` was the original
/// plan's flag and it named a Landlock counter-rule that does not exist: a
/// `path_beneath` rule with `allowed_access = 0` is rejected by the kernel
/// (`ENOMSG`), and a nested lower-privilege rule does **not** revoke rights an
/// ancestor rule granted (both measured on this host — see `sandbox/mod.rs`).
/// If `--deny` ever reappears in a launcher argv, someone has re-invented the
/// broken mechanism and the secret set is silently readable again.
#[test]
fn the_argv_never_names_a_deny_rule() {
    for tier in [Tier::Strict, Tier::Network] {
        let a = strs(&sandbox_argv(&policy(tier)));
        assert!(
            !a.iter().any(|s| s == "--deny"),
            "{tier:?}: denial is expressed by *not granting* (enumerate-and-skip), \
             never by a counter-rule; see the measured note in sandbox/mod.rs"
        );
    }
}

#[test]
fn blocked_hooks_name_the_empty_dir_and_running_hooks_do_not() {
    let mut p = policy(Tier::Strict);
    p.hook_mode = HookMode::Blocked {
        empty_dir: PathBuf::from("/var/lib/gv/no-hooks"),
    };
    let a = strs(&sandbox_argv(&p));
    let w = pairs(&a);
    assert!(w.contains(&("--hooks-blocked", "/var/lib/gv/no-hooks")));
    assert!(strs(&sandbox_argv(&policy(Tier::Strict)))
        .iter()
        .any(|s| s == "--hooks-run"));
}

#[test]
fn the_probe_argv_names_no_program_at_all() {
    let a = strs(&probe_argv(&policy(Tier::Strict)).expect("a strict policy is probeable"));
    assert_eq!(
        a.last().unwrap(),
        "--self-probe",
        "the probe must not exec anything"
    );
    assert!(
        !a.iter().any(|s| s == "git"),
        "the probe argv must never name git"
    );
}

/// There is no sandbox in the unsandboxed tier, so there is no argv that could
/// probe one. This used to hit `unreachable!()` inside `shim_argv`, which meant
/// probing an operator-trusted repository panicked a server worker thread — a
/// crash for what is simply a legitimate answer.
#[test]
fn probing_the_unsandboxed_tier_returns_none_instead_of_panicking() {
    assert!(probe_argv(&policy(Tier::Unsandboxed)).is_none());
}

/// The four policy-building sites (Tasks 6, 7, 9 and `shim_cli::workable`) must
/// not each hand-roll a system-tree list — the round-4 measured configuration
/// granted `/dev` **and** `/proc`, and every list in the original plan omitted
/// both. `/proc` is strict-tier-only on purpose: bwrap mounts a fresh procfs
/// for the child pid namespace (C3), whereas the network tier has no mount
/// namespace at all, so granting `/proc` there would grant the *host's* view.
#[test]
fn system_trees_grant_dev_rw_and_proc_only_in_the_strict_tier() {
    let (strict_rw, strict_ro) = default_system_trees(Tier::Strict);
    assert!(
        strict_rw.contains(&PathBuf::from("/dev")),
        "/dev is read-write in the only configuration git was measured to work under"
    );
    assert!(
        strict_ro.contains(&PathBuf::from("/proc")),
        "C3/A8: without /proc the shim cannot open /proc/self/ns/user and the probe lies"
    );
    for t in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
        assert!(
            strict_ro.contains(&PathBuf::from(t)),
            "{t} must be readable+executable"
        );
    }

    let (net_rw, net_ro) = default_system_trees(Tier::Network);
    assert!(net_rw.contains(&PathBuf::from("/dev")));
    assert!(
        !net_ro.contains(&PathBuf::from("/proc")),
        "the network tier has no mount namespace: /proc there is the host's procfs"
    );
}
