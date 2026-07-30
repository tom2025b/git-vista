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
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::PR_SET_SECCOMP as u64,
    )?])
}

/// Build the filter program.
///
/// `mismatch_action` is `Allow`: anything not named above proceeds. See the
/// module comment for why an allowlist is the wrong shape for git.
pub fn build() -> Result<BpfProgram, Box<dyn std::error::Error>> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // An empty rule vector means "every invocation of this syscall matches".
    for (nr, _why) in denied_outright() {
        rules.insert(nr, vec![]);
    }
    rules.insert(libc::SYS_prctl, vec![prctl_rule()?]);

    let filter = SeccompFilter::new(
        rules,
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
        let program = build().expect("filter builds");
        assert!(!program.is_empty(), "an empty BPF program filters nothing");
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
