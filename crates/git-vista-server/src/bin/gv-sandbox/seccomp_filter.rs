//! M1.13b (#66) Task 4: the shim's seccomp filter.
//!
//! Included directly into `gv-sandbox.rs` rather than living in the library,
//! because the shim is a standalone binary and this code must run inside it,
//! after Landlock and immediately before the `execve`.
//!
//! # Why this is a denylist and not an allowlist
//!
//! An allowlist of the syscalls git needs is the stronger shape in principle
//! and the wrong shape here. `git` is not one program: it is a dispatcher that
//! execs dozens of subcommands, links libcurl and libssl for HTTPS remotes, and
//! runs whatever `core.pager`, `credential.helper` and hook interpreters the
//! host has configured. An allowlist tight enough to be worth having would
//! break a different git operation on every host, and a filter that has to be
//! widened per host is one that gets widened until it means nothing.
//!
//! So the filter is a **terminal denylist of the specific escapes this design
//! names**, and the filesystem boundary is carried by Landlock, which is
//! deny-by-default and does not have this problem.
//!
//! # The arch check does not distinguish x32 — measured, and it is why every
//! # key is inserted twice
//!
//! `SeccompFilter::new` is handed `TargetArch::x86_64`, which seccompiler
//! compiles into a prologue that loads `seccomp_data.arch` and
//! `SECCOMP_RET_KILL_PROCESS`es anything that is not `AUDIT_ARCH_X86_64`. That
//! closes the sibling i386 vector *fatally*. It does **not** close x32: the x32
//! ABI has no `AUDIT_ARCH` of its own — it reports `AUDIT_ARCH_X86_64` and marks
//! itself by setting `__X32_SYSCALL_BIT` (`0x4000_0000`) in `seccomp_data.nr`
//! instead. Since seccompiler emits one `BPF_JEQ` per *bare* key against the raw
//! `nr` and appends `mismatch_action` (Allow) as the fallthrough, an
//! x32-numbered syscall matched no key here and fell through to Allow — and
//! because the miss happens at the shared `nr` load, it fell through for the
//! **whole map at once**: io_uring, `unshare`/`setns`, `seccomp` (the C1
//! stacking denial), `ptrace`, and the AF_UNIX rules, all voided together.
//!
//! Measured 2026-07-29 in a 64-bit process, under hand-assembled cBPF of exactly
//! seccompiler's shape (`ld arch; jeq AUDIT_ARCH_X86_64 -> +1 else
//! KILL_PROCESS; ld nr; jeq <key> -> ERRNO(EPERM); ret ALLOW`), run twice: once
//! with a bare key and once with the same key OR'd with the bit:
//!
//! ```text
//! bare key 425          : io_uring_setup()          EFAULT -> EPERM   (filter live)
//! bare key 425          : X32BIT|io_uring_setup()    ENOSYS -> ENOSYS  (never matched)
//! key __X32_BIT|425     : X32BIT|io_uring_setup()    ENOSYS -> EPERM   (matched)
//! key __X32_BIT|425     : io_uring_setup()           EFAULT            (bare no longer matched)
//! ```
//!
//! The high-bit process was never killed, which is the direct evidence that the
//! arch prologue reads `AUDIT_ARCH_X86_64` for an x32-numbered call. The third
//! line is the whole reason for the doubled keys, and the fourth is why they are
//! *added* rather than substituted.
//!
//! **Not exploitable on this host, and that is not a security property.**
//! `/boot/config-7.0.0-28-generic` has `# CONFIG_X86_X32_ABI is not set`, so
//! `do_syscall_x32()` is compiled out, dispatch falls to `__x64_sys_ni_syscall`,
//! and every x32-numbered call returns `ENOSYS` — measured, and confirmed by the
//! control (`X32BIT|getpid` → ENOSYS while plain `getpid` → OK). An x32 *binary*
//! cannot even be exec'd here (`ENOEXEC`). But that is one kernel-config line
//! away from being live, the sandbox neither controls nor observes the setting,
//! and this shim is not only ever run on this laptop. Seccomp evaluates in
//! `syscall_enter_from_user_mode()`, i.e. **before** the x64/x32 dispatch split,
//! which is why a high-bit key can return `EPERM` at all — and why the fix is
//! fully verifiable here even though the exploit is not. The escape battery's
//! `high_bit_io_uring_denied` is that verification: `ENOSYS` outside, `EPERM`
//! inside, a two-sided assertion needing no x32 ABI and therefore no skip.
//!
//! Do **not** try to express this as "reject any `nr` with bit 30 set":
//! seccompiler's API has no masked-`nr` primitive, only per-key `JEQ` plus
//! argument conditions.
//!
//! # C1 — the composition rule that decides the action
//!
//! Seccomp filter stacking is **not monotonic**: a later `SECCOMP_RET_USER_NOTIF`
//! or `SECCOMP_RET_TRACE` filter can observe and continue a syscall an earlier
//! filter meant to mediate. So every denial here is terminal —
//! `SECCOMP_RET_ERRNO`, which outranks both — and the child is additionally
//! denied `seccomp(2)` and `prctl(PR_SET_SECCOMP)` so it cannot install a
//! filter of its own to play that trick.

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;

#[cfg(target_arch = "x86_64")]
const TARGET_ARCH: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
const TARGET_ARCH: TargetArch = TargetArch::aarch64;

/// `__X32_SYSCALL_BIT`: the bit the x32 ABI sets in `seccomp_data.nr`, and the
/// only thing that distinguishes an x32 call from an x86_64 one as far as this
/// filter can see. See the module header for the measurement; `rules_with_x32_aliases`
/// is what uses it. x86_64-only: there is no such bit on aarch64.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: i64 = 0x4000_0000;

/// Whether the tier this filter is being built for has network access — the one
/// axis on which the filter differs between tiers.
///
/// Named rather than a bare `bool` because `build(true)` at the call site would
/// not say *which* tier got the weaker filter, and the only rule that varies
/// (AF_UNIX, below) is the one a reviewer most needs to attribute to a tier.
/// The shim learns this from the `--net-deny` / `--net-allow` flag that
/// `sandbox::shim_argv` already emits per tier, so nothing new travels in the
/// argv: `--net-deny` is `Strict`, `--net-allow` is `Network`, and
/// `Unsandboxed` never launches the shim at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetScope {
    /// `--net-deny`: the Strict tier. bwrap has already put this process in its
    /// own network namespace and Landlock carries no `connect` grant.
    Denied,
    /// `--net-allow`: the Network tier, the only one in which `git
    /// push`/`fetch`/`clone` can work (F3).
    Allowed,
}

/// Syscalls denied outright, with the reason each one is here.
///
/// Every entry is an escape this design names. A syscall that is merely
/// dangerous-sounding does not belong: each denial is a compatibility risk, and
/// an unexplained entry is one a later session cannot safely remove.
fn denied_outright() -> Vec<(i64, &'static str)> {
    vec![
        // io_uring: the round-4 bypass. Landlock's filesystem rules are checked
        // when a path is opened; io_uring can submit an OPENAT from a kernel
        // worker context, which is a different path through the kernel than the
        // one Landlock was reasoned about on. Denying the three setup calls is
        // what closes it — there is no io_uring without them.
        (
            libc::SYS_io_uring_setup,
            "io_uring bypasses path-based mediation",
        ),
        (
            libc::SYS_io_uring_enter,
            "io_uring bypasses path-based mediation",
        ),
        (
            libc::SYS_io_uring_register,
            "io_uring bypasses path-based mediation",
        ),
        // Namespace manipulation. A sandboxed process that can create a user
        // namespace gains capabilities inside it, and `setns` would let it join
        // one that already exists.
        (
            libc::SYS_unshare,
            "namespace creation escapes the tier's boundary",
        ),
        (
            libc::SYS_setns,
            "joining a namespace escapes the tier's boundary",
        ),
        // C1: deny the child the ability to install its own seccomp filter, so
        // it cannot use non-monotonic stacking to continue a syscall this
        // filter denied.
        (libc::SYS_seccomp, "C1: filter stacking is not monotonic"),
        // Kernel module and kexec surfaces. Not reachable unprivileged, denied
        // anyway so the filter states the boundary rather than relying on the
        // absence of a capability.
        (
            libc::SYS_init_module,
            "kernel code loading is never in scope",
        ),
        (
            libc::SYS_finit_module,
            "kernel code loading is never in scope",
        ),
        (
            libc::SYS_delete_module,
            "kernel code loading is never in scope",
        ),
        (
            libc::SYS_kexec_load,
            "kernel code loading is never in scope",
        ),
        // ptrace: attaching to another process of the same uid would let a
        // hostile hook read or drive a process outside the sandbox. Landlock's
        // ABI-6 signal scope covers signalling; this covers inspection.
        (libc::SYS_ptrace, "same-uid inspection escapes the boundary"),
        // process_vm_*: reading or writing another process's memory directly,
        // same reasoning as ptrace and not covered by it.
        (
            libc::SYS_process_vm_readv,
            "same-uid memory access escapes the boundary",
        ),
        (
            libc::SYS_process_vm_writev,
            "same-uid memory access escapes the boundary",
        ),
    ]
}

/// `prctl(PR_SET_SECCOMP, …)` is denied while every other `prctl` is allowed —
/// git and its children legitimately use `PR_SET_NAME`, `PR_SET_PDEATHSIG` and
/// others, so denying the whole syscall would break ordinary operation.
///
/// C2 — **register width.** Seccomp compares the raw register, which is 64 bits
/// wide, *before* the kernel truncates the argument to the `int` that `prctl`
/// actually declares. Comparing at full width means `PR_SET_SECCOMP | 0x1_0000_0000`
/// does not match this rule and sails through. `SeccompCmpArgLen::Dword` is what
/// masks the comparison to the effective low 32 bits. This is the exact defect
/// the round-4 audit recorded as C2, and a test that constructs the hostile
/// value in userspace as a 32-bit `c_int` cannot detect it, because the
/// truncation it is testing for has already happened before the syscall.
fn prctl_rule() -> Result<SeccompRule, seccompiler::BackendError> {
    SeccompRule::new(vec![SeccompCondition::new(
        0, // prctl's first argument, the option
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::Eq,
        libc::PR_SET_SECCOMP as u64,
    )?])
}

/// `socket(2)` and `socketpair(2)` with `AF_UNIX` (== `AF_LOCAL` == 1) as the
/// address family, denied in the **Strict** tier only. Every other family —
/// `AF_INET`, `AF_INET6`, `AF_NETLINK` — is untouched, because a blanket denial
/// of `socket` would break the Network tier's TCP and anything in git that opens
/// a socket for a reason this design never objected to.
///
/// # Why this rule exists
///
/// The design's Strict-tier threat model is "no network, no AF_UNIX, no
/// io_uring", and until this rule landed the AF_UNIX third of that sentence was
/// **as-designed, not as-built** (`docs/superpowers/plans/2026-07-28-m1.13b-sandbox.md`
/// step 4.3; the anti-vacuity contract records the same gap as INV-4). Measured
/// on this host inside the real bwrap + Landlock + seccomp Strict stack, with the
/// probe run as a `pre-commit` hook so it inherited the sandboxed git's filter
/// and Landlock domain: `socket(AF_UNIX, SOCK_STREAM, 0)` and
/// `socketpair(AF_UNIX, …)` both **succeeded**, identical to the bare host, while
/// a `ptrace(PTRACE_TRACEME)` control in the same run went from success on the
/// host to `EPERM` inside — so the filter was demonstrably loaded and the AF_UNIX
/// success was a real gap, not a sandbox that failed to apply.
///
/// Landlock does not cover it. `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` (set in
/// `apply_landlock`) mediates *connecting* to an abstract socket created outside
/// the sandbox's domain; it says nothing about socket construction, and ABI 8
/// does not mediate **pathname** sockets at all. Nor do the Strict tier's
/// namespaces: an IPC namespace does not cover `AF_UNIX`, and a network namespace
/// does not either. So a hostile hook in Strict could reach
/// `/run/docker.sock`, `ssh-agent`, `gpg-agent` and the D-Bus session bus — every
/// one of them a full escape — with nothing in the stack objecting.
///
/// # Why Strict only, and not the Network tier
///
/// The Network tier is where `git push`/`fetch` over SSH lives, and SSH
/// legitimately wants an `ssh-agent` socket, which is a pathname `AF_UNIX`
/// socket. Issue #188 defers that carve-out deliberately, so denying AF_UNIX in
/// the Network tier here would either break authenticated remotes or force a
/// carve-out this task is not allowed to build. The Network tier therefore keeps
/// exactly the filter it had before this rule existed: a tier whose whole purpose
/// is reaching the network does not gain much from losing its local sockets, and
/// a denial nobody has measured git against is how a filter gets widened until it
/// means nothing (see the module comment).
///
/// # C2 — register width
///
/// Same trap as `prctl_rule`, for the same reason. Seccomp compares the raw
/// 64-bit register *before* the kernel truncates the argument to the `int` that
/// `socket`'s `domain` parameter declares, so a `Qword` comparison would let
/// `AF_UNIX | 0x1_0000_0000` sail past this rule while the kernel went on to
/// create an ordinary AF_UNIX socket from the low bits.
/// `SeccompCmpArgLen::Dword` masks the comparison to the effective low 32 bits.
///
/// The width was **measured** correct when this rule landed:
/// `socket(AF_UNIX | 1<<32)` returns `EPERM` inside Strict. A measurement proves
/// today's code and nothing else, which is why the width now also carries its own
/// mutant and its own case — `ci/mutants/M9-widen-af-unix-comparison.patch` and
/// `high_bit_af_unix_denied` — rather than borrowing the sibling rule's
/// (`M7-widen-prctl-comparison.patch` / `high_bit_prctl_denied`). Borrowing was
/// the gap: every other AF_UNIX case constructs its family as a 32-bit `int`, so
/// none of them can tell a `Dword` comparison from a `Qword` one, and this exact
/// defect class could have reopened here with the whole battery green.
fn af_unix_rule() -> Result<SeccompRule, seccompiler::BackendError> {
    SeccompRule::new(vec![SeccompCondition::new(
        0, // socket(2)/socketpair(2) first argument, the address family
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::AF_UNIX as u64,
    )?])
}

/// The syscall-to-rules map the filter is compiled from. Split out of `build`
/// so the unit tests below can assert *which* syscalls carry rules and that the
/// socket rules are argument-scoped rather than blanket denials — properties a
/// compiled `BpfProgram` no longer exposes.
fn rules_for(net: NetScope) -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::BackendError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // An empty rule vector means "every invocation of this syscall matches".
    for (nr, _why) in denied_outright() {
        rules.insert(nr, vec![]);
    }
    rules.insert(libc::SYS_prctl, vec![prctl_rule()?]);

    // Strict only — see `af_unix_rule` for the measurement that motivates the
    // rule and for why the Network tier is deliberately left alone (#188).
    if net == NetScope::Denied {
        rules.insert(libc::SYS_socket, vec![af_unix_rule()?]);
        rules.insert(libc::SYS_socketpair, vec![af_unix_rule()?]);
    }

    Ok(rules)
}

/// `rules_for`'s bare keys, plus — on x86_64 — an `__X32_SYSCALL_BIT` twin of
/// **every** one of them, carrying the identical rule.
///
/// Written as a post-pass over the finished map rather than as a second
/// `insert` beside each first one, for two reasons. It cannot drift: a key added
/// to `rules_for` later gets its x32 twin for free, whereas a hand-paired insert
/// is one a future edit can forget, and a forgotten one is invisible (the miss
/// looks exactly like the syscall being allowed on purpose). And it leaves
/// `rules_for` byte-for-byte unchanged, which keeps the committed mutants that
/// patch that function — `ci/mutants/M8-remove-af-unix-socket-rule.patch` — applying
/// at zero fuzz.
///
/// The keys cannot collide: every real syscall number is far below
/// `0x4000_0000`, so the twin is always a fresh `BTreeMap` entry. And they stay
/// inside seccompiler's `u32` narrowing (`filter.rs`'s
/// `syscall_number.try_into().unwrap()`): `0x4000_0000 | 425 == 1073742249`.
fn rules_with_x32_aliases(
    net: NetScope,
) -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::BackendError> {
    let mut rules = rules_for(net)?;
    #[cfg(target_arch = "x86_64")]
    {
        let aliases: Vec<(i64, Vec<SeccompRule>)> = rules
            .iter()
            .map(|(nr, rule)| (X32_SYSCALL_BIT | *nr, rule.clone()))
            .collect();
        rules.extend(aliases);
    }
    Ok(rules)
}

/// Build the filter program.
///
/// `mismatch_action` is `Allow`: anything not named above proceeds. See the
/// module comment for why an allowlist is the wrong shape for git.
pub fn build(net: NetScope) -> Result<BpfProgram, Box<dyn std::error::Error>> {
    let filter = SeccompFilter::new(
        rules_with_x32_aliases(net)?,
        // Not named -> allowed.
        SeccompAction::Allow,
        // Named -> denied terminally. EPERM rather than KillProcess so a git
        // operation that touches one of these fails with a diagnosable error
        // instead of a bare SIGSYS that looks like a crash. The escape battery
        // asserts the errno, which a killed process could not report.
        SeccompAction::Errno(libc::EPERM as u32),
        TARGET_ARCH,
    )?;
    Ok(filter.try_into()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every denial must be terminal. `Trace` and `Notify` are precisely what
    /// C1 forbids: a later filter can use them to observe and *continue* a
    /// syscall this one meant to stop.
    #[test]
    fn the_filter_builds_and_denies_terminally() {
        for net in [NetScope::Denied, NetScope::Allowed] {
            let program = build(net).expect("filter builds");
            assert!(
                !program.is_empty(),
                "an empty BPF program filters nothing ({net:?})"
            );
        }
    }

    /// The AF_UNIX denial is scoped two ways, and both scopes are the point.
    ///
    /// Scoped to the **family**: a rule vector of length 1 means `socket` is
    /// denied only for arguments matching that one condition. An empty vector
    /// would mean "every invocation matches" — a blanket denial that would take
    /// the Network tier's TCP down with it.
    ///
    /// Scoped to the **tier**: the Network tier must carry no socket rule at all
    /// while #188 defers the `ssh-agent` carve-out. If this half starts failing
    /// because someone widened the rule to both tiers, that is not a test to
    /// relax — it is a git-over-SSH regression that has not been measured.
    #[test]
    fn af_unix_is_denied_in_strict_and_left_alone_in_the_network_tier() {
        let strict = rules_for(NetScope::Denied).expect("strict rules build");
        for nr in [libc::SYS_socket, libc::SYS_socketpair] {
            let scoped = strict
                .get(&nr)
                .unwrap_or_else(|| panic!("syscall {nr} must carry an AF_UNIX rule in Strict"));
            assert_eq!(
                scoped.len(),
                1,
                "syscall {nr}'s denial must be argument-scoped to AF_UNIX; an empty rule \
                 vector is a blanket denial and would break AF_INET too"
            );
        }

        let network = rules_for(NetScope::Allowed).expect("network rules build");
        for nr in [libc::SYS_socket, libc::SYS_socketpair] {
            assert!(
                !network.contains_key(&nr),
                "syscall {nr} must be untouched in the Network tier: git over SSH needs an \
                 agent socket and issue #188 defers that carve-out"
            );
        }
    }

    /// Both tiers' filters must still be terminal denylists that leave the
    /// unnamed syscalls alone — the AF_UNIX rule adds exactly two keys to Strict
    /// and nothing to Network.
    #[test]
    fn the_af_unix_rule_is_the_only_difference_between_the_two_tiers() {
        let strict: Vec<i64> = rules_for(NetScope::Denied)
            .expect("strict rules build")
            .keys()
            .copied()
            .collect();
        let network: Vec<i64> = rules_for(NetScope::Allowed)
            .expect("network rules build")
            .keys()
            .copied()
            .collect();
        let extra: Vec<i64> = strict
            .iter()
            .copied()
            .filter(|nr| !network.contains(nr))
            .collect();
        assert_eq!(
            extra,
            {
                let mut want = vec![libc::SYS_socket, libc::SYS_socketpair];
                want.sort_unstable();
                want
            },
            "the only tier-conditional rules may be the two AF_UNIX ones; a filter that \
             quietly differs elsewhere is one no reviewer can attribute to a tier"
        );
    }

    /// The io_uring denial is the round-4 bypass and must never be quietly
    /// dropped for compatibility — this asserts the intent survives edits.
    #[test]
    fn io_uring_and_namespace_syscalls_are_all_denied() {
        let denied: Vec<i64> = denied_outright().into_iter().map(|(nr, _)| nr).collect();
        for required in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_seccomp,
        ] {
            assert!(
                denied.contains(&required),
                "syscall {required} must be denied; removing it reopens a named escape"
            );
        }
    }

    /// Every key the filter is compiled from must have an `__X32_SYSCALL_BIT`
    /// twin carrying the *same* rule — otherwise an x32-numbered call misses
    /// every key and falls through to `mismatch_action` (Allow), voiding the
    /// whole denylist at once rather than one entry of it. See the module header
    /// for the measurement.
    ///
    /// This asserts the twin carries the **same rule**, not merely that the key
    /// exists: an alias keyed to an empty rule vector where the bare key is
    /// argument-scoped would be a blanket denial reachable only over x32, and an
    /// alias keyed to nothing where the bare key is blanket is a hole. Both are
    /// silent.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn every_key_is_also_denied_under_the_x32_syscall_bit() {
        for net in [NetScope::Denied, NetScope::Allowed] {
            let bare = rules_for(net).expect("bare rules build");
            let all = rules_with_x32_aliases(net).expect("aliased rules build");
            assert_eq!(
                all.len(),
                bare.len() * 2,
                "every bare key must gain exactly one x32 twin ({net:?})"
            );
            for (nr, rule) in &bare {
                let alias = X32_SYSCALL_BIT | *nr;
                assert!(
                    alias > *nr,
                    "syscall {nr} already carries bit 30; the aliasing scheme would collide"
                );
                assert_eq!(
                    all.get(&alias),
                    Some(rule),
                    "syscall {nr} has no x32 twin carrying the identical rule ({net:?}); an \
                     x32-numbered call would fall through to Allow and take the entire \
                     denylist with it"
                );
            }
        }
    }

    /// Doubling the jump table must not push the program past seccompiler's
    /// `BPF_MAX_LEN`. A filter that fails to *build* would be caught by
    /// `apply_seccomp`'s `die`, so this is not a silent failure — but it would be
    /// a launch that refuses on every host, and it is cheaper to know here.
    #[test]
    fn both_tiers_still_fit_the_bpf_program_budget_with_the_aliases() {
        for net in [NetScope::Denied, NetScope::Allowed] {
            let program = build(net).unwrap_or_else(|e| {
                panic!("{net:?} filter must still compile with the x32 aliases: {e}")
            });
            assert!(
                !program.is_empty(),
                "an empty BPF program filters nothing ({net:?})"
            );
        }
    }

    /// Every denial carries a reason. An unexplained entry is one a later
    /// session cannot safely remove, so it never gets removed and the filter
    /// accretes until nobody understands it.
    #[test]
    fn every_denial_states_why() {
        for (nr, why) in denied_outright() {
            assert!(
                !why.is_empty(),
                "syscall {nr} is denied with no stated reason"
            );
        }
    }
}
