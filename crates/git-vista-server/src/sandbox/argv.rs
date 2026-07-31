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
        // #188: same per-tier shape as `net_ports` below — populated in
        // Network, empty everywhere else. `known_hosts_carveout_is_network_tier_only_in_the_argv`
        // is the test that exists specifically to check this fixture's
        // Strict/Network difference reaches `sandbox_argv`'s output.
        ro_carveouts: if tier == Tier::Network {
            vec![PathBuf::from("/home/tom/.ssh/known_hosts")]
        } else {
            Vec::new()
        },
        net_ports: if tier == Tier::Network {
            DEFAULT_GIT_PORTS.to_vec()
        } else {
            Vec::new()
        },
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
    assert!(!a
        .iter()
        .any(|s| s.contains("bwrap") || s == "--unshare-net"));
    assert!(a.iter().any(|s| s == "--net-allow"));
}

/// ADR 0028 (decision A): the network tier's permitted ports are part of the
/// reviewed launcher argv, not a list buried in the shim. A reviewer reading a
/// command line must be able to see every port the sandbox will allow.
#[test]
fn the_network_tier_names_every_permitted_port_in_the_argv() {
    let a = strs(&sandbox_argv(&policy(Tier::Network)));
    let w = pairs(&a);
    for port in DEFAULT_GIT_PORTS {
        assert!(
            w.contains(&("--net-port", port.to_string().as_str())),
            "port {port} must be visible in the argv, not hardcoded in the shim"
        );
    }
    assert!(
        a.iter().any(|s| s == "--net-allow"),
        "ports are meaningless without the tier flag that enables them"
    );
}

/// A tier with no network must not carry ports. `--net-deny` followed by a port
/// list is an argv that contradicts itself, and INV-16's whole purpose is that
/// the argv can be checked by eye.
#[test]
fn no_tier_without_network_ever_carries_a_port() {
    for tier in [Tier::Strict, Tier::Unsandboxed] {
        let a = strs(&sandbox_argv(&policy(tier)));
        assert!(
            !a.iter().any(|s| s == "--net-port"),
            "{tier:?}: a tier with no network must name no ports"
        );
    }
}

/// The strict tier reaches the network through no path at all — bwrap's
/// `--unshare-net` is the boundary there, not a port list (F3).
#[test]
fn the_strict_tier_denies_network_and_unshares_it() {
    let a = strs(&sandbox_argv(&policy(Tier::Strict)));
    assert!(a.iter().any(|s| s == "--net-deny"));
    assert!(
        a.iter().any(|s| s == "--unshare-net"),
        "the strict tier's network denial is the namespace, not the ruleset"
    );
}

/// `secret_excludes` is documented as absolute paths while
/// `DEFAULT_SECRET_EXCLUDES` is relative to `$HOME`. A policy site that passes
/// the constant verbatim gets a secret set that matches nothing, and `~/.ssh`
/// is silently readable again. This asserts the conversion helper is the thing
/// that closes that gap.
#[test]
fn secret_excludes_are_absolute_and_cover_every_default() {
    let home = PathBuf::from("/home/someone");
    let got = secret_excludes_for_home(&home);
    assert_eq!(got.len(), DEFAULT_SECRET_EXCLUDES.len());
    for p in &got {
        assert!(p.is_absolute(), "{p:?} must be absolute to match anything");
        assert!(p.starts_with(&home), "{p:?} must live under the given home");
    }
    for must in [".ssh", ".git-credentials", ".gnupg", ".claude.json"] {
        assert!(
            got.iter().any(|p| p == &home.join(must)),
            "{must} must be withheld: it holds credentials"
        );
    }
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

/// The production policy-building sites (Tasks 6, 7 and 9) must
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

/// The DNS grant, both halves. On a systemd-resolved host `/etc/resolv.conf` is
/// a symlink into `/run`, so without this the network tier cannot resolve a
/// hostname and every named remote fails `Could not resolve host` — measured, see
/// `NETWORK_ONLY_RO_TREES`. And it must stay out of the strict tier, whose
/// posture is no network at all: resolver state there is access granted for an
/// operation that tier does not permit.
#[test]
fn resolver_state_is_readable_in_the_network_tier_and_never_in_the_strict_one() {
    let (_, net_ro) = default_system_trees(Tier::Network);
    for t in NETWORK_ONLY_RO_TREES {
        assert!(
            net_ro.contains(&PathBuf::from(t)),
            "{t} must be readable in the network tier or DNS fails"
        );
    }

    let (_, strict_ro) = default_system_trees(Tier::Strict);
    for t in NETWORK_ONLY_RO_TREES {
        assert!(
            !strict_ro.contains(&PathBuf::from(t)),
            "{t}: the strict tier has no network (--net-deny + --unshare-net), so it \
             must not be granted resolver state"
        );
    }
    assert!(
        !strict_ro.iter().any(|p| p.starts_with("/run")),
        "no /run grant belongs in the strict tier"
    );
}

/// #188, structural half of "verified absent from a Strict policy's argv":
/// production `policy_for` itself — not the synthetic fixture above — must
/// populate `ro_carveouts` in the Network tier and leave it empty in Strict.
/// This is the claim the synthetic `policy()` fixture cannot make on its
/// own: it proves `sandbox_argv` can *represent* the carve-out, not that
/// production actually *builds* one. No env-var control needed: this grant
/// depends only on `$HOME`, which every test in this crate already assumes
/// is set.
#[test]
fn production_policy_for_carries_the_known_hosts_carveout_in_network_only() {
    let repo = tempfile::tempdir().expect("tempdir");

    let strict = policy_for(repo.path(), false, NetworkNeed::Local)
        .expect("a Strict policy must build on this host");
    assert_eq!(strict.tier, Tier::Strict);
    assert!(
        strict.ro_carveouts.is_empty(),
        "#188: the Strict tier must never carry a known_hosts carve-out, got {:?}",
        strict.ro_carveouts
    );
    let strict_argv = strs(&sandbox_argv(&strict));
    assert!(
        !strict_argv.iter().any(|s| s == "--ro-carveout"),
        "a Strict policy's composed argv must never contain --ro-carveout"
    );

    let network = policy_for(repo.path(), false, NetworkNeed::Remote)
        .expect("a Network policy must build on this host");
    assert_eq!(network.tier, Tier::Network);
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    assert_eq!(
        network.ro_carveouts,
        vec![home.join(".ssh/known_hosts")],
        "#188: production must carve out exactly ~/.ssh/known_hosts, nothing more, nothing less"
    );
    let network_argv = strs(&sandbox_argv(&network));
    let w = pairs(&network_argv);
    assert!(
        w.contains(&(
            "--ro-carveout",
            home.join(".ssh/known_hosts").to_str().expect("utf8 path")
        )),
        "the composed Network argv must carry --ro-carveout <known_hosts>"
    );
}

/// #188's other grant, tested directly against the one function that reads
/// `SSH_AUTH_SOCK` — the precise unit for "Network tier only, and only when
/// an agent is actually running."
///
/// The Strict/Unsandboxed claims below need no env-var control at all:
/// `ssh_agent_socket_grant` returns before ever reading `SSH_AUTH_SOCK` for
/// any tier but `Network` (see its own source), so it is `None` regardless
/// of what the ambient environment happens to hold — on this host, in CI, or
/// anywhere else.
#[test]
fn ssh_agent_socket_grant_is_network_tier_only_and_only_when_set() {
    assert_eq!(
        ssh_agent_socket_grant(Tier::Strict),
        None,
        "#188: Strict must never grant the agent socket, whatever SSH_AUTH_SOCK holds"
    );
    assert_eq!(
        ssh_agent_socket_grant(Tier::Unsandboxed),
        None,
        "Unsandboxed installs no ruleset at all; nothing here should be granted through it either"
    );

    let prior = std::env::var_os("SSH_AUTH_SOCK");
    // SAFETY: no other test in this crate reads or sets SSH_AUTH_SOCK
    // (grepped for both at the time this was written) — a key nothing else
    // touches has nothing to race with on this one. Restored via the same
    // setter, never merely removed, so a test order that runs this before
    // something reading a real ambient value is not left with a poisoned
    // environment.
    unsafe { std::env::remove_var("SSH_AUTH_SOCK") };
    assert_eq!(
        ssh_agent_socket_grant(Tier::Network),
        None,
        "no agent running (SSH_AUTH_SOCK unset) must mean no grant, not an invented path"
    );

    let sock = PathBuf::from("/tmp/gv188-argv-test-agent.sock");
    unsafe { std::env::set_var("SSH_AUTH_SOCK", &sock) };
    assert_eq!(
        ssh_agent_socket_grant(Tier::Network),
        Some(sock),
        "#188: Network tier must grant exactly the path SSH_AUTH_SOCK names"
    );

    match prior {
        Some(v) => unsafe { std::env::set_var("SSH_AUTH_SOCK", v) },
        None => unsafe { std::env::remove_var("SSH_AUTH_SOCK") },
    }
}

/// `policy_for_clone` is an **independent** `Policy` constructor (it does not
/// call `policy_for`) and is hard-coded to `Tier::Network` — the one
/// production site the issue text never names, and therefore the one a
/// build could easily fix for push/fetch/ls-remote while leaving
/// `git clone git@host:…` broken outright. Both #188 grants must land here
/// too.
#[test]
fn policy_for_clone_carries_both_188_grants() {
    let clones_root = tempfile::tempdir().expect("tempdir");
    let sock = PathBuf::from("/tmp/gv188-policy-for-clone-test-agent.sock");
    let prior = std::env::var_os("SSH_AUTH_SOCK");
    // SAFETY: see ssh_agent_socket_grant_is_network_tier_only_and_only_when_set.
    unsafe { std::env::set_var("SSH_AUTH_SOCK", &sock) };

    let policy = policy_for_clone(clones_root.path()).expect("policy_for_clone must build");
    assert_eq!(
        policy.tier,
        Tier::Network,
        "clone is always NetworkNeed::Remote"
    );

    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    assert_eq!(
        policy.ro_carveouts,
        vec![home.join(".ssh/known_hosts")],
        "policy_for_clone must carry the same known_hosts carve-out as policy_for"
    );
    assert!(
        policy.rw_trees.contains(&sock),
        "policy_for_clone must carry the same agent-socket grant as policy_for, got {:?}",
        policy.rw_trees
    );

    let argv = strs(&sandbox_argv(&policy));
    let w = pairs(&argv);
    assert!(w.contains(&(
        "--ro-carveout",
        home.join(".ssh/known_hosts").to_str().expect("utf8 path")
    )));
    assert!(w.contains(&("--rw", sock.to_str().expect("utf8 path"))));

    match prior {
        Some(v) => unsafe { std::env::set_var("SSH_AUTH_SOCK", v) },
        None => unsafe { std::env::remove_var("SSH_AUTH_SOCK") },
    }
}

/// The integration half: `policy_for` must actually wire
/// `ssh_agent_socket_grant`'s result into `rw_trees`, and `sandbox_argv` must
/// carry it through to a real `--rw` entry — the pure-function test above
/// proves the *gate* is correct; this proves production actually *uses* it.
#[test]
fn production_policy_for_wires_the_agent_socket_grant_into_the_network_argv() {
    let repo = tempfile::tempdir().expect("tempdir");
    let sock = PathBuf::from("/tmp/gv188-policy-for-test-agent.sock");
    let prior = std::env::var_os("SSH_AUTH_SOCK");
    // SAFETY: see ssh_agent_socket_grant_is_network_tier_only_and_only_when_set.
    unsafe { std::env::set_var("SSH_AUTH_SOCK", &sock) };

    let network = policy_for(repo.path(), false, NetworkNeed::Remote)
        .expect("a Network policy must build on this host");
    assert!(
        network.rw_trees.contains(&sock),
        "policy_for must add the agent socket to rw_trees in the Network tier, got {:?}",
        network.rw_trees
    );
    let argv = strs(&sandbox_argv(&network));
    let w = pairs(&argv);
    assert!(
        w.contains(&("--rw", sock.to_str().expect("utf8 path"))),
        "the composed Network argv must carry --rw <agent socket>"
    );

    let strict = policy_for(repo.path(), false, NetworkNeed::Local)
        .expect("a Strict policy must build on this host");
    assert!(
        !strict.rw_trees.contains(&sock),
        "#188: Strict must never receive the agent socket grant even with \
         SSH_AUTH_SOCK set, got {:?}",
        strict.rw_trees
    );

    match prior {
        Some(v) => unsafe { std::env::set_var("SSH_AUTH_SOCK", v) },
        None => unsafe { std::env::remove_var("SSH_AUTH_SOCK") },
    }
}
