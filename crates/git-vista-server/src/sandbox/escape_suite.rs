//! M1.13b (#66): declarative containment escape battery.
//!
//! Case declarations contain no acceptance logic. The single shared runner in
//! `escape_contract` owns parsing, exact errno comparisons, carrier checks,
//! report emission, production-seam spawning, and capability absence.

use super::escape_contract::{run_case, Class, Errno, EscapeCase, Exemption, GitPortUse, MutantId};
use super::Tier;

/// The one hostile-hook repository constructor, re-exported here because the
/// lifecycle (Task 12), non-coverage (Task 13) and compatibility (Task 14)
/// batteries all name it as `escape_suite::hostile_hook_repo`. It is defined in
/// `escape_contract` — composed from the same `fixture()` + `install_hook()`
/// pair `run_case`'s own two legs use — so a neighbouring battery's "same
/// fixture as the escape battery" is a fact about one function, not a
/// convention two files have to keep agreeing on.
pub(crate) use super::escape_contract::hostile_hook_repo;

const CASE_SECRET_READ_DENIED: EscapeCase = EscapeCase {
    id: "secret_read_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::secret_read_probe,
    probe_tag: "SECRET",
    expect_baseline: Errno(0),
    expect_inside: Errno(13),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M2, MutantId::M3],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

const CASE_IO_URING_DENIED: EscapeCase = EscapeCase {
    id: "io_uring_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::io_uring_probe,
    probe_tag: "IOURING",
    expect_baseline: Errno(0),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

const CASE_HIGH_BIT_PRCTL_DENIED: EscapeCase = EscapeCase {
    id: "high_bit_prctl_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::high_bit_prctl_probe,
    probe_tag: "HIGHBIT",
    expect_baseline: Errno(14),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1, MutantId::M7],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

const CASE_STRICT_LISTENER_DENIED: EscapeCase = EscapeCase {
    id: "strict_listener_denied",
    class: Class::Containment,
    tier: Tier::Strict,
    hooks_blocked: false,
    build_hook: harness::strict_listener_probe,
    probe_tag: "CONNECT",
    expect_baseline: Errno(0),
    expect_inside: Errno(13),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M2, MutantId::M5],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes Tier::Network",
    },
    // The probe connects to 9418, so the harness holds the listener; see
    // `test_ports` for why every holder of that one port must be serialized.
    git_port: GitPortUse::ExclusiveWithListener,
};

const CASE_STRICT_UDP_HOST_DENIED: EscapeCase = EscapeCase {
    id: "strict_udp_host_denied",
    class: Class::Containment,
    tier: Tier::Strict,
    hooks_blocked: false,
    build_hook: harness::strict_udp_host_probe,
    probe_tag: "UDP_HOST",
    expect_baseline: Errno(0),
    expect_inside: Errno(11),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M4],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes Tier::Network",
    },
    git_port: GitPortUse::Unused,
};

const CASE_STRICT_TCP_BIND_DENIED: EscapeCase = EscapeCase {
    id: "strict_tcp_bind_denied",
    class: Class::Containment,
    tier: Tier::Strict,
    hooks_blocked: false,
    build_hook: harness::strict_tcp_bind_probe,
    probe_tag: "TCP_BIND",
    expect_baseline: Errno(0),
    expect_inside: Errno(13),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M2],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes Tier::Network",
    },
    // Exclusive but listener-free: this probe's baseline leg *binds* 9418 to
    // establish the capability, so any listener there would turn the baseline
    // into EADDRINUSE and the whole case into a silent CapabilityAbsent.
    git_port: GitPortUse::Exclusive,
};

/// INV-4's `socket()` entry point. Two cases rather than one because the filter
/// carries two rules: `M8` removes both in one hunk, so a single case would go
/// red for either — but a later edit that dropped only the `socketpair` insert
/// would leave a green battery behind a half-removed claim. One case per
/// syscall is what makes that impossible.
///
/// `expect_granted` is an `AF_INET` socket **creation**, not a connect. Creation
/// is what the rule under test is scoped away from, and it succeeds in Strict
/// (bwrap's netns has no route, but the socket is still constructible);
/// `connect()` is Landlock's job and is denied here, which is
/// `strict_listener_denied`'s claim, not this one.
const CASE_AF_UNIX_SOCKET_DENIED: EscapeCase = EscapeCase {
    id: "af_unix_socket_denied",
    class: Class::Containment,
    tier: Tier::Strict,
    hooks_blocked: false,
    build_hook: harness::af_unix_probe,
    probe_tag: "UNIXSOCK",
    expect_baseline: Errno(0),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1, MutantId::M8],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes Tier::Network",
    },
    git_port: GitPortUse::Unused,
};

/// INV-4's `socketpair()` entry point — the sub-claim the plan left as an open
/// follow-up. Same probe binary and same run shape as
/// `CASE_AF_UNIX_SOCKET_DENIED`; only the observed tag differs.
const CASE_AF_UNIX_SOCKETPAIR_DENIED: EscapeCase = EscapeCase {
    id: "af_unix_socketpair_denied",
    class: Class::Containment,
    tier: Tier::Strict,
    hooks_blocked: false,
    build_hook: harness::af_unix_probe,
    probe_tag: "UNIXPAIR",
    expect_baseline: Errno(0),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1, MutantId::M8],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes Tier::Network",
    },
    git_port: GitPortUse::Unused,
};

/// The width guard on the AF_UNIX rule — the sibling of `high_bit_prctl_denied`,
/// and the case M9 exists to kill.
///
/// Every other AF_UNIX case builds its family with libc's `socket()` wrapper,
/// whose `int` parameter truncates the high bits *in userspace*, before the
/// register seccomp compares ever carries them. Such a case cannot distinguish a
/// `Dword` comparison from a `Qword` one, so the entire battery could stay green
/// while the rule's width regressed — the exact defect this project already
/// shipped once on `prctl`. This case's probe issues a raw
/// `syscall(SYS_socket, AF_UNIX | 1<<32, …)` instead, so the hostile value
/// survives into the kernel.
///
/// The baseline errno is 0 and it is not an oversight: outside the sandbox the
/// kernel truncates the family itself and creates an ordinary AF_UNIX socket
/// (measured: `rc=3`). That is what makes the inside leg's `EPERM` attributable
/// to the filter and to nothing else.
const CASE_HIGH_BIT_AF_UNIX_DENIED: EscapeCase = EscapeCase {
    id: "high_bit_af_unix_denied",
    class: Class::Containment,
    tier: Tier::Strict,
    hooks_blocked: false,
    build_hook: harness::high_bit_af_unix_probe,
    probe_tag: "HIGHUNIX",
    expect_baseline: Errno(0),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M9],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes Tier::Network",
    },
    git_port: GitPortUse::Unused,
};

/// The x32 guard: an `__X32_SYSCALL_BIT`-numbered syscall must reach the
/// denylist, not fall through it.
///
/// seccompiler keys rules on bare syscall numbers and its arch prologue reads
/// `AUDIT_ARCH_X86_64` for an x32 call as well as an x86_64 one, so before the
/// aliased keys landed an `nr` carrying `0x4000_0000` matched nothing and fell
/// through to `mismatch_action` — Allow — taking the whole map with it, not one
/// entry. See `seccomp_filter`'s module header for the cBPF measurement.
///
/// **This case needs no x32 ABI and therefore no skip**, which is the whole
/// reason it can exist here: seccomp evaluates before the kernel's x64/x32
/// dispatch split, so a normal 64-bit binary can issue a high-bit `nr` and see
/// the filter's answer. The two legs differ for two independent reasons —
/// outside, this host's kernel has `CONFIG_X86_X32_ABI` unset and answers
/// `ENOSYS` (38); inside, the aliased key answers `EPERM` (1). Nothing but a
/// live filter matching a high-bit key can turn 38 into 1.
const CASE_HIGH_BIT_IO_URING_DENIED: EscapeCase = EscapeCase {
    id: "high_bit_io_uring_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::high_bit_io_uring_probe,
    probe_tag: "X32IOURING",
    expect_baseline: Errno(38),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

/// INV-1, the read-only-grant boundary: `$HOME` is granted, and granted **read
/// only**, so a write there is the one filesystem boundary in this policy whose
/// paired positive can be fully attributable.
///
/// R3 is carried here by the strongest pairing available anywhere in this
/// battery — the denied write and the granted read are siblings under the *same
/// tree*, in the same run, under the same policy. `enumerate()` implements
/// exclusion by omission and Landlock is deny-by-default, so `EACCES` from an
/// ungranted path and `EACCES` from a read-only grant are the same kernel
/// event; the `$HOME/.gitconfig` read is what says which one this is. Without
/// it, deleting the `ro.push(home)` line outright would leave this case green
/// while the secret set stopped being enumerated at all.
const CASE_WRITE_HOME_DENIED: EscapeCase = EscapeCase {
    id: "write_home_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::fs_boundary_probe,
    probe_tag: "WRITEHOME",
    expect_baseline: Errno(0),
    expect_inside: Errno(13),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M2],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

/// INV-1, the never-granted-tree boundary: the cgroup hierarchy is in no tier's
/// `rw_trees` or `ro_trees`, so a hostile hook cannot reach it at all.
///
/// The claim is stated as a **read** denial rather than the write the plan
/// sketches, and `harness::fs_boundary_probe`'s doc comment carries the
/// measurement that forced that: an unprivileged write into `/sys/fs/cgroup`
/// returns `EACCES` on the bare host too, so a write-shaped case would have
/// `expect_baseline == expect_inside` and could not tell Landlock from cgroupfs.
/// A tree that cannot be read cannot be written, and this form has a real
/// baseline (the control file is mode 444).
///
/// Its paired positive is the `$HOME/.gitconfig` read rather than a sibling
/// under `/sys` — by construction there is no granted sibling there, which is
/// the claim. What the positive rules out is the only alternative explanation
/// available: that the policy denied *everything* the hook tried.
const CASE_CGROUP_TREE_DENIED: EscapeCase = EscapeCase {
    id: "cgroup_tree_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::fs_boundary_probe,
    probe_tag: "CGROUPTREE",
    expect_baseline: Errno(0),
    expect_inside: Errno(13),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M2],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

/// INV-3a: `NO_NEW_PRIVS` is set for the process git actually runs in, and an
/// explicit attempt to clear it changes nothing.
///
/// **The observed integer is not an errno**, and it is deliberate. The clear
/// attempt itself returns `EINVAL` on both legs — the kernel refuses any
/// `PR_SET_NO_NEW_PRIVS` whose second argument is not `1`, host and sandbox
/// alike — so an errno-shaped case here would be the textbook vacuity: two legs
/// agreeing for a reason that has nothing to do with this sandbox. What is
/// attributable is the *state after the attempt*, read back through `prctl`:
/// `NoNewPrivs * 10 + Seccomp`, i.e. `0` outside (neither set) and `12` inside
/// (`NoNewPrivs: 1`, `Seccomp: 2`). That is the kernel's own report about the
/// post-`execve` process, which the contract's acceptance evidence (F) asks
/// every probe to print and which, until this case, nothing asserted.
///
/// **Why `M1` and not a mutant on the `NO_NEW_PRIVS` call itself.** There is no
/// single-hunk mutant that can clear this flag: `apply_landlock` sets it
/// explicitly *and* `seccompiler::apply_filter` sets it again on the way to
/// `SECCOMP_SET_MODE_FILTER` (seccompiler 0.5.0, `src/lib.rs:344`), so removing
/// either site leaves the flag set by the other. `M1` empties the filter, which
/// drops the `Seccomp` half of the composite from `2` to `0` and is what this
/// cell measures.
const CASE_NO_NEW_PRIVS_IRREVOCABLE: EscapeCase = EscapeCase {
    id: "no_new_privs_irrevocable",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::irrevocability_probe,
    probe_tag: "NNPSTATE",
    expect_baseline: Errno(0),
    expect_inside: Errno(12),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

/// INV-3b: a hostile hook may install a second, allow-**all** Landlock ruleset,
/// and it does not widen what the first one granted.
///
/// The shim's filter does not deny `landlock_create_ruleset`/`add_rule`/
/// `restrict_self`, so the hook genuinely performs this — the containment comes
/// from the kernel intersecting every layer of a Landlock domain, never unioning
/// them. The probe therefore installs a ruleset that grants every ABI-1 access
/// right on `/` and *then* writes into `$HOME`.
///
/// The baseline leg is what makes this attributable, and it is doing real work:
/// outside the sandbox that same allow-all ruleset is the process's *only*
/// domain, so the write succeeds (`0`). Crucially the probe reports the install
/// chain's own errno if any step of it fails, so a baseline `0` means "the
/// ruleset was installed **and** the write went through" — not "nothing
/// restricted me". (Measured against the negative control: the identical
/// sequence with the `/` rule omitted denies the same write with `EACCES`, so
/// `landlock_restrict_self` is demonstrably taking effect on the baseline leg.)
const CASE_SECOND_LANDLOCK_RULESET_DENIED: EscapeCase = EscapeCase {
    id: "second_landlock_ruleset_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::irrevocability_probe,
    probe_tag: "LANDLOCK2",
    expect_baseline: Errno(0),
    expect_inside: Errno(13),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M2],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

/// INV-3c: `unshare -Ur`'s syscall is not reachable, so there is no user
/// namespace in which the hook could hold uid 0 in the first place.
///
/// The baseline leg is the load-bearing half: unprivileged user namespaces are
/// *available* on this host (the CI preflight asserts `capabilities::probe()`
/// reports `userns`), so `unshare(CLONE_NEWUSER)` returns `0` outside. A host
/// that had them restricted would make this case report `CapabilityAbsent`
/// rather than pass — F-NEW-4 recorded that `unshare -Ur` did not escape
/// Landlock on kernel 7.0, and a case that could not tell "denied" from
/// "unavailable" would quietly convert that first-party evidence into nothing.
const CASE_UNSHARE_USERNS_DENIED: EscapeCase = EscapeCase {
    id: "unshare_userns_denied",
    class: Class::Containment,
    tier: Tier::Network,
    hooks_blocked: false,
    build_hook: harness::irrevocability_probe,
    probe_tag: "UNSHARE",
    expect_baseline: Errno(0),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1],
    exemption: Exemption::None,
    git_port: GitPortUse::Unused,
};

/// INV-4's io_uring entry point: an `AF_UNIX` socket obtained through
/// `IORING_OP_SOCKET`, which never issues `socket(2)` and therefore never meets
/// the seccomp rule keyed on that syscall's first argument.
///
/// **This is not a second spelling of `io_uring_denied`.** That case claims
/// `io_uring_setup` returns `EPERM`. This one claims the *consequence*: that
/// closing io_uring is what closes AF_UNIX at this entry point, because nothing
/// else in the stack does. Its baseline leg proves the bypass is real rather
/// than asserting it — outside the sandbox the probe builds a ring, submits one
/// `IORING_OP_SOCKET` SQE for `AF_UNIX`/`SOCK_STREAM`, and gets a live socket
/// fd back from the CQE (measured on this host: `res >= 0`). No `socket(2)` is
/// ever issued, so `seccomp_filter::af_unix_rule` cannot see it.
///
/// The tier is `Strict` for exactly that reason: `Strict` is the tier whose
/// threat model says "no AF_UNIX", and whose AF_UNIX denial is a `socket(2)`
/// argument rule. In `Network` the sub-claim would be untestable, since AF_UNIX
/// is deliberately permitted there (#188's deferred ssh-agent carve-out).
///
/// `M10` is what makes the claim mechanical: it removes only the io_uring
/// denials and leaves the AF_UNIX rules installed, so a red cell there says
/// "the AF_UNIX rule was in force and an AF_UNIX socket was created anyway."
/// `M1` alone could never say that — it removes both mechanisms at once.
const CASE_URING_SOCKET_BYPASS: EscapeCase = EscapeCase {
    id: "uring_socket_bypass_denied",
    class: Class::Containment,
    tier: Tier::Strict,
    hooks_blocked: false,
    build_hook: harness::uring_socket_probe,
    probe_tag: "URINGSOCKET",
    expect_baseline: Errno(0),
    expect_inside: Errno(1),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M1, MutantId::M10],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes Tier::Network",
    },
    git_port: GitPortUse::Unused,
};

#[test]
fn write_home_denied() {
    run_case(&CASE_WRITE_HOME_DENIED);
}

#[test]
fn cgroup_tree_denied() {
    run_case(&CASE_CGROUP_TREE_DENIED);
}

#[test]
fn no_new_privs_irrevocable() {
    run_case(&CASE_NO_NEW_PRIVS_IRREVOCABLE);
}

#[test]
fn second_landlock_ruleset_denied() {
    run_case(&CASE_SECOND_LANDLOCK_RULESET_DENIED);
}

#[test]
fn unshare_userns_denied() {
    run_case(&CASE_UNSHARE_USERNS_DENIED);
}

#[test]
fn uring_socket_bypass_denied() {
    run_case(&CASE_URING_SOCKET_BYPASS);
}

#[test]
fn secret_read_denied() {
    run_case(&CASE_SECRET_READ_DENIED);
}

#[test]
fn high_bit_af_unix_denied() {
    run_case(&CASE_HIGH_BIT_AF_UNIX_DENIED);
}

#[test]
fn high_bit_io_uring_denied() {
    run_case(&CASE_HIGH_BIT_IO_URING_DENIED);
}

#[test]
fn io_uring_denied() {
    run_case(&CASE_IO_URING_DENIED);
}

#[test]
fn high_bit_prctl_denied() {
    run_case(&CASE_HIGH_BIT_PRCTL_DENIED);
}

#[test]
fn strict_listener_denied() {
    run_case(&CASE_STRICT_LISTENER_DENIED);
}

#[test]
fn strict_udp_host_denied() {
    run_case(&CASE_STRICT_UDP_HOST_DENIED);
}

#[test]
fn strict_tcp_bind_denied() {
    run_case(&CASE_STRICT_TCP_BIND_DENIED);
}

#[test]
fn af_unix_socket_denied() {
    run_case(&CASE_AF_UNIX_SOCKET_DENIED);
}

#[test]
fn af_unix_socketpair_denied() {
    run_case(&CASE_AF_UNIX_SOCKETPAIR_DENIED);
}

mod harness {
    use super::super::escape_contract::HarnessCtx;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PROBE_ID: AtomicUsize = AtomicUsize::new(0);

    fn c_string(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    fn compile_probe(ctx: &HarnessCtx, source: &str) -> PathBuf {
        let id = PROBE_ID.fetch_add(1, Ordering::Relaxed);
        let c = ctx.repo.join(format!("gv_escape_probe_{id}.c"));
        let bin = ctx.repo.join(format!("gv_escape_probe_{id}"));
        std::fs::write(&c, source).expect("write probe source");
        let ok = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-o"])
            .arg(&bin)
            .arg(&c)
            .status()
            .expect("cc runs")
            .success();
        assert!(ok, "escape probe failed to compile");
        bin
    }

    fn granted_path(ctx: &HarnessCtx) -> PathBuf {
        let path = ctx.repo.join("gv_escape_granted.txt");
        std::fs::write(&path, "granted\n").expect("write paired-positive fixture");
        path
    }

    fn hook_for(ctx: &HarnessCtx, source: String) -> String {
        let probe = compile_probe(ctx, &source);
        format!("exec {}", probe.display())
    }

    pub(super) fn secret_read_probe(ctx: &HarnessCtx) -> String {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
        let secret = c_string(&home.join(".ssh/known_hosts"));
        let granted = c_string(&home.join(".gitconfig"));
        hook_for(
            ctx,
            format!(
                r#"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <unistd.h>

static int read_errno(const char *path) {{
    errno = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return errno;
    char byte;
    ssize_t n = read(fd, &byte, 1);
    int saved = n < 0 ? errno : 0;
    close(fd);
    return saved;
}}

int main(void) {{
    printf("GVPROBE {nonce} BEGIN\n");
    int denied = read_errno("{secret}");
    printf("SECRET rc=%d errno=%d Seccomp: %d NoNewPrivs: %d\n",
           denied ? -1 : 0, denied, prctl(PR_GET_SECCOMP), prctl(PR_GET_NO_NEW_PRIVS));
    int allowed = read_errno("{granted}");
    printf("GRANTED rc=%d errno=%d\n", allowed ? -1 : 0, allowed);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    pub(super) fn io_uring_probe(ctx: &HarnessCtx) -> String {
        let granted = c_string(&granted_path(ctx));
        hook_for(
            ctx,
            format!(
                r#"
#include <errno.h>
#include <fcntl.h>
#include <linux/io_uring.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

static int read_errno(const char *path) {{
    errno = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return errno;
    close(fd);
    return 0;
}}

int main(void) {{
    struct io_uring_params params;
    memset(&params, 0, sizeof params);
    errno = 0;
    long ring = syscall(__NR_io_uring_setup, 8, &params);
    int saved = ring < 0 ? errno : 0;
    printf("GVPROBE {nonce} BEGIN\n");
    printf("IOURING rc=%ld errno=%d Seccomp: %d NoNewPrivs: %d\n",
           ring, saved, prctl(PR_GET_SECCOMP), prctl(PR_GET_NO_NEW_PRIVS));
    if (ring >= 0) close((int)ring);
    int allowed = read_errno("{granted}");
    printf("GRANTED rc=%d errno=%d\n", allowed ? -1 : 0, allowed);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    pub(super) fn high_bit_prctl_probe(ctx: &HarnessCtx) -> String {
        let granted = c_string(&granted_path(ctx));
        hook_for(
            ctx,
            format!(
                r#"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

static int read_errno(const char *path) {{
    errno = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return errno;
    close(fd);
    return 0;
}}

int main(void) {{
    errno = 0;
    long rc = syscall(SYS_prctl, (long)PR_SET_SECCOMP | 0x100000000L, 2, 0, 0, 0);
    int saved = rc < 0 ? errno : 0;
    printf("GVPROBE {nonce} BEGIN\n");
    printf("HIGHBIT rc=%ld errno=%d Seccomp: %d NoNewPrivs: %d\n",
           rc, saved, prctl(PR_GET_SECCOMP), prctl(PR_GET_NO_NEW_PRIVS));
    int allowed = read_errno("{granted}");
    printf("GRANTED rc=%d errno=%d\n", allowed ? -1 : 0, allowed);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    /// `socket(AF_UNIX | 1<<32)` through the **raw** `syscall()`, which is the
    /// entire point of a separate probe.
    ///
    /// libc's `socket()` declares `int domain`, so passing the hostile value
    /// through it truncates the high bits in userspace — before the register
    /// seccomp compares ever holds them — and the resulting case would pass
    /// identically against a `Dword` and a `Qword` comparison. That is a vacuous
    /// case wearing the costume of a width guard, so this probe never touches the
    /// wrapper for the denial leg.
    ///
    /// The paired positive is an ordinary `AF_INET` socket creation in the same
    /// process under the same filter (the same positive the other AF_UNIX cases
    /// use): without it, "the high-bit family was denied" would be
    /// indistinguishable from "this filter denies `socket(2)` outright".
    pub(super) fn high_bit_af_unix_probe(ctx: &HarnessCtx) -> String {
        hook_for(
            ctx,
            format!(
                r#"
#include <errno.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {{
    errno = 0;
    long high = syscall(SYS_socket, (long)AF_UNIX | 0x100000000L, SOCK_STREAM, 0);
    int denied = high < 0 ? errno : 0;
    if (high >= 0) close((int)high);
    errno = 0;
    long inet = syscall(SYS_socket, (long)AF_INET, SOCK_STREAM, 0);
    int granted = inet < 0 ? errno : 0;
    if (inet >= 0) close((int)inet);
    printf("GVPROBE {nonce} BEGIN\n");
    printf("HIGHUNIX rc=%ld errno=%d Seccomp: %d NoNewPrivs: %d\n",
           high, denied, prctl(PR_GET_SECCOMP), prctl(PR_GET_NO_NEW_PRIVS));
    printf("GRANTED rc=%ld errno=%d\n", inet, granted);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    /// `io_uring_setup` under `__X32_SYSCALL_BIT`, from an ordinary 64-bit
    /// binary.
    ///
    /// No x32 process is involved, and none is needed: seccomp runs in
    /// `syscall_enter_from_user_mode()`, before the kernel's x64/x32 dispatch
    /// split, so the filter sees `nr = 0x400001A9` and answers before anything
    /// decides the call is not dispatchable. Outside the sandbox this host
    /// answers `ENOSYS` (`CONFIG_X86_X32_ABI` is unset); inside, the aliased key
    /// answers `EPERM`. The paired positive is a read of a granted file in the
    /// repository, exactly as `io_uring_probe` does.
    pub(super) fn high_bit_io_uring_probe(ctx: &HarnessCtx) -> String {
        let granted = c_string(&granted_path(ctx));
        hook_for(
            ctx,
            format!(
                r#"
#include <errno.h>
#include <fcntl.h>
#include <linux/io_uring.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

static int read_errno(const char *path) {{
    errno = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return errno;
    close(fd);
    return 0;
}}

int main(void) {{
    struct io_uring_params params;
    memset(&params, 0, sizeof params);
    errno = 0;
    long ring = syscall(0x40000000L | __NR_io_uring_setup, 8, &params);
    int saved = ring < 0 ? errno : 0;
    if (ring >= 0) close((int)ring);
    printf("GVPROBE {nonce} BEGIN\n");
    printf("X32IOURING rc=%ld errno=%d Seccomp: %d NoNewPrivs: %d\n",
           ring, saved, prctl(PR_GET_SECCOMP), prctl(PR_GET_NO_NEW_PRIVS));
    int allowed = read_errno("{granted}");
    printf("GRANTED rc=%d errno=%d\n", allowed ? -1 : 0, allowed);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    /// One probe binary serving both AF_UNIX cases. Each case reads its own tag
    /// (`UNIXSOCK`, `UNIXPAIR`) out of the same output, so the two claims are
    /// observed under identical conditions instead of through two probes that
    /// could drift apart. The tags deliberately share no prefix: `parse_observation`
    /// matches a line by `strip_prefix(tag)`, so `UNIX_SOCKET`/`UNIX_SOCKETPAIR`
    /// would be a trap — the shorter tag would match the longer line's head.
    ///
    /// The paired positive is an `AF_INET` socket creation in the same process,
    /// under the same filter: without it, "AF_UNIX is denied" would be
    /// indistinguishable from "this filter denies `socket(2)` outright", which is
    /// exactly the blanket denial the rule is scoped to avoid and would break the
    /// Network tier's TCP.
    pub(super) fn af_unix_probe(ctx: &HarnessCtx) -> String {
        hook_for(
            ctx,
            format!(
                r#"
#include <errno.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static int socket_errno(int family) {{
    errno = 0;
    int fd = socket(family, SOCK_STREAM, 0);
    if (fd < 0) return errno;
    close(fd);
    return 0;
}}

static int socketpair_errno(void) {{
    int fds[2] = {{ -1, -1 }};
    errno = 0;
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, fds) != 0) return errno;
    close(fds[0]);
    close(fds[1]);
    return 0;
}}

int main(void) {{
    int unix_sock = socket_errno(AF_UNIX);
    int unix_pair = socketpair_errno();
    int inet_sock = socket_errno(AF_INET);
    printf("GVPROBE {nonce} BEGIN\n");
    printf("UNIXSOCK rc=%d errno=%d Seccomp: %d NoNewPrivs: %d\n",
           unix_sock ? -1 : 0, unix_sock, prctl(PR_GET_SECCOMP), prctl(PR_GET_NO_NEW_PRIVS));
    printf("UNIXPAIR rc=%d errno=%d\n", unix_pair ? -1 : 0, unix_pair);
    printf("GRANTED rc=%d errno=%d\n", inet_sock ? -1 : 0, inet_sock);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    /// The listener this probe connects to is owned by the harness (see
    /// `escape_contract::GitProtocolPort`), not by this function: it is bound
    /// under a `test_ports::PortClaim` and torn down when the case ends. The
    /// pre-contract version bound it here through a process-lifetime `OnceLock`
    /// and parked a thread in a blocking `accept()`, which held port 9418 for
    /// the rest of the binary's life and collided with the two other tests that
    /// need it.
    pub(super) fn strict_listener_probe(ctx: &HarnessCtx) -> String {
        let port = ctx
            .listener_port
            .expect("the harness must bind a listener for an ExclusiveWithListener case");
        let granted = c_string(&granted_path(ctx));
        hook_for(
            ctx,
            format!(
                r#"
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <unistd.h>

static int read_errno(const char *path) {{
    errno = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return errno;
    close(fd);
    return 0;
}}

int main(void) {{
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    int rc = -1;
    int saved = fd < 0 ? errno : 0;
    if (fd >= 0) {{
        struct sockaddr_in address;
        memset(&address, 0, sizeof address);
        address.sin_family = AF_INET;
        address.sin_port = htons({port});
        inet_pton(AF_INET, "127.0.0.1", &address.sin_addr);
        errno = 0;
        rc = connect(fd, (struct sockaddr *)&address, sizeof address);
        saved = rc < 0 ? errno : 0;
        close(fd);
    }}
    printf("GVPROBE {nonce} BEGIN\n");
    printf("CONNECT rc=%d errno=%d Seccomp: %d NoNewPrivs: %d\n",
           rc, saved, prctl(PR_GET_SECCOMP), prctl(PR_GET_NO_NEW_PRIVS));
    int allowed = read_errno("{granted}");
    printf("GRANTED rc=%d errno=%d\n", allowed ? -1 : 0, allowed);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    pub(super) fn strict_udp_host_probe(ctx: &HarnessCtx) -> String {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind UDP echo socket");
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .expect("set UDP echo timeout");
        let port = socket.local_addr().expect("UDP echo address").port();
        std::thread::spawn(move || {
            let mut byte = [0_u8; 1];
            if let Ok((len, peer)) = socket.recv_from(&mut byte) {
                let _ = socket.send_to(&byte[..len], peer);
            }
        });
        hook_for(
            ctx,
            format!(
                r#"
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static int host_round_trip_errno(void) {{
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) return errno;
    // 5s, deliberately longer than the host echo thread's own 3s read window.
    // A shorter child timeout than the host's would let a slow loopback round
    // trip return EAGAIN and read as CONTAINED when the datagram actually
    // escaped the namespace — a false negative in the dangerous direction,
    // and one that would silently un-kill M4 on a loaded host (the mutation
    // matrix rebuilds two crates seven times while this runs).
    struct timeval timeout = {{ .tv_sec = 5, .tv_usec = 0 }};
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout) != 0) {{
        int saved = errno;
        close(fd);
        return saved;
    }}
    struct sockaddr_in address;
    memset(&address, 0, sizeof address);
    address.sin_family = AF_INET;
    address.sin_port = htons({port});
    inet_pton(AF_INET, "127.0.0.1", &address.sin_addr);
    char byte = 'x';
    errno = 0;
    if (sendto(fd, &byte, 1, 0, (struct sockaddr *)&address, sizeof address) != 1) {{
        int saved = errno;
        close(fd);
        return saved;
    }}
    errno = 0;
    int saved = recv(fd, &byte, 1, 0) == 1 ? 0 : errno;
    close(fd);
    return saved;
}}

static int udp_bind_errno(void) {{
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) return errno;
    struct sockaddr_in address;
    memset(&address, 0, sizeof address);
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_ANY);
    address.sin_port = htons(0);
    errno = 0;
    int saved = bind(fd, (struct sockaddr *)&address, sizeof address) == 0 ? 0 : errno;
    close(fd);
    return saved;
}}

int main(void) {{
    int denied = host_round_trip_errno();
    int granted = udp_bind_errno();
    printf("GVPROBE {nonce} BEGIN\n");
    printf("UDP_HOST rc=%d errno=%d\n", denied ? -1 : 0, denied);
    printf("GRANTED rc=%d errno=%d\n", granted ? -1 : 0, granted);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }

    /// The bound port is `PortClaim::PORT`, not a bare literal: this probe's
    /// baseline leg genuinely binds it on the host, so the case holds an
    /// exclusive (listener-free) claim on exactly that port and the two must not
    /// be able to drift apart.
    pub(super) fn strict_tcp_bind_probe(ctx: &HarnessCtx) -> String {
        hook_for(
            ctx,
            format!(
                r#"
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

// `reuse` sets SO_REUSEADDR, which the fixed-port leg needs and the ephemeral
// one does not. Without it a TIME_WAIT socket left on 127.0.0.1:{port} by any
// *earlier* user of the git protocol port — the escape battery's own connect
// case, the planner's `git daemon` push fixture, a run 30 seconds ago — makes
// this bind fail EADDRINUSE, which `run_case` then reports as
// `CapabilityAbsent`: a silently-vacuous pass, the exact failure mode the
// anti-vacuity contract exists to prevent. TIME_WAIT residue is not "this host
// cannot bind"; it is an artifact with a 60-second half-life, and SO_REUSEADDR
// is what every real server sets to ignore it (`git daemon --reuseaddr`, and
// Rust's own `TcpListener::bind`, both do). It is orthogonal to the claim under
// test: Landlock denies the bind with EACCES either way, and a live listener on
// the port would still be EADDRINUSE, which is why the case also holds
// `GitPortUse::Exclusive`.
static int bind_errno(int type, unsigned short port, int reuse) {{
    int fd = socket(AF_INET, type, 0);
    if (fd < 0) return errno;
    if (reuse) {{
        int on = 1;
        errno = 0;
        if (setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof on) != 0) {{
            int saved = errno;
            close(fd);
            return saved;
        }}
    }}
    struct sockaddr_in address;
    memset(&address, 0, sizeof address);
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_ANY);
    address.sin_port = htons(port);
    errno = 0;
    int saved = bind(fd, (struct sockaddr *)&address, sizeof address) == 0 ? 0 : errno;
    close(fd);
    return saved;
}}

int main(void) {{
    int denied = bind_errno(SOCK_STREAM, {port}, 1);
    int granted = bind_errno(SOCK_DGRAM, 0, 0);
    printf("GVPROBE {nonce} BEGIN\n");
    printf("TCP_BIND rc=%d errno=%d\n", denied ? -1 : 0, denied);
    printf("GRANTED rc=%d errno=%d\n", granted ? -1 : 0, granted);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
                port = crate::test_ports::PortClaim::PORT,
            ),
        )
    }
}
