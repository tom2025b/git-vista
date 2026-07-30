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
