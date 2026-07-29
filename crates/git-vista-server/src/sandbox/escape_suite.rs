//! M1.13b (#66): declarative containment escape battery.
//!
//! Case declarations contain no acceptance logic. The single shared runner in
//! `escape_contract` owns parsing, exact errno comparisons, carrier checks,
//! report emission, production-seam spawning, and capability absence.

use super::escape_contract::{run_case, Class, Errno, EscapeCase, Exemption, MutantId};
use super::Tier;

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
};

#[test]
fn secret_read_denied() {
    run_case(&CASE_SECRET_READ_DENIED);
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

mod harness {
    use super::super::escape_contract::HarnessCtx;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PROBE_ID: AtomicUsize = AtomicUsize::new(0);
    static STRICT_LISTENER_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

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

    pub(super) fn strict_listener_probe(ctx: &HarnessCtx) -> String {
        let port = *STRICT_LISTENER_PORT.get_or_init(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:9418")
                .expect("bind git protocol listener");
            let port = listener.local_addr().expect("listener address").port();
            std::thread::spawn(move || {
                let _ = listener.accept();
            });
            port
        });
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

static int bind_errno(int type, unsigned short port) {{
    int fd = socket(AF_INET, type, 0);
    if (fd < 0) return errno;
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
    int denied = bind_errno(SOCK_STREAM, 9418);
    int granted = bind_errno(SOCK_DGRAM, 0);
    printf("GVPROBE {nonce} BEGIN\n");
    printf("TCP_BIND rc=%d errno=%d\n", denied ? -1 : 0, denied);
    printf("GRANTED rc=%d errno=%d\n", granted ? -1 : 0, granted);
    printf("GVPROBE {nonce} END\n");
    return 0;
}}
"#,
                nonce = ctx.nonce,
            ),
        )
    }
}
