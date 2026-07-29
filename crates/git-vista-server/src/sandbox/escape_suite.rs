//! M1.13b (#66) Tasks 10-12: the escape battery.
//!
//! These drive the **whole composed launcher** against a real repository and
//! assert that a hostile repository cannot reach past the boundary. Nothing
//! here builds a Landlock ruleset or a seccomp filter itself — that is the
//! composition rule (verdict §5).
//!
//! # Provenance, not liveness — the rule every test obeys
//!
//! An earlier version of this file paired each denial with a *liveness* control
//! (the probe ran, some granted op worked). An independent audit (C8) showed
//! that liveness is not enough: "any negative return counts as denied" credits
//! a kernel `EFAULT`, an outer container's `EPERM`, or an `ECONNREFUSED` from an
//! empty port as *this* sandbox working. Every test here therefore runs the
//! **same probe twice** — once **outside** the composed launcher (baseline) and
//! once **inside** it — and requires:
//!
//! 1. the baseline to *succeed* (the operation is genuinely possible on this
//!    host, so a denial inside means something), and
//! 2. the inside run to fail with the **exact errno the Git-Vista boundary
//!    produces** — `EPERM` from seccomp, `EACCES` from Landlock — not merely
//!    "a negative return."
//!
//! If the baseline cannot perform the operation, the test is `SKIPPED`, never
//! green: a test that passes because the host could not do the thing anyway is
//! the vacuity this structure exists to prevent.

use super::shim_cli::{fixture, launch, shim, strict_available, workable};
use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const EPERM: i32 = 1;
const EACCES: i32 = 13;

/// Combined stdout+stderr of a commit that fired the repo's `pre-commit` hook.
struct HookRun {
    /// The commit's exit code — a hook that ran and failed the commit is
    /// distinguishable from a commit that never reached hook discovery.
    commit_code: i32,
    out: String,
}

/// Install a `pre-commit` hook body.
fn set_hook(repo: &Path, body: &str) {
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let hook = hooks.join("pre-commit");
    let mut f = std::fs::File::create(&hook).expect("hook file");
    writeln!(f, "#!/bin/sh\n{body}").expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn stripped_env(cmd: &mut Command) {
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", std::env::var("HOME").expect("HOME"));
}

/// Fire the hook **outside** the sandbox — a plain `git commit`. This is the
/// baseline: it proves the probe compiles, runs, and that the operation under
/// test is actually possible on this host. `git` is literal here (the argv
/// tripwire's carve-out for this file permits it; a shell is still forbidden).
fn commit_baseline(repo: &Path) -> HookRun {
    stage(repo, "baseline");
    let mut cmd = Command::new("git");
    stripped_env(cmd.arg("-C").arg(repo).args(["commit", "-q", "-m", "baseline"]));
    let out = cmd.output().expect("git runs");
    HookRun {
        commit_code: out.status.code().unwrap_or(-1),
        out: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// Fire the hook **inside** the composed launcher under `policy`.
fn commit_sandboxed(policy: &Policy, repo: &Path) -> HookRun {
    stage(repo, "sandboxed");
    let (add_code, _, add_err) = launch(policy, repo, &["add", "-A"]);
    assert_eq!(add_code, 0, "git add must succeed before the commit fires the hook: {add_err}");
    let (code, out, err) = launch(policy, repo, &["commit", "-q", "-m", "sandboxed"]);
    HookRun {
        commit_code: code,
        out: format!("{out}{err}"),
    }
}

fn stage(repo: &Path, tag: &str) {
    let f = repo.join(format!("payload_{tag}.txt"));
    std::fs::write(&f, tag).expect("write payload");
    // Baseline stages with plain git so the sandboxed run's `git add -A` is not
    // needed here; sandboxed staging happens in commit_sandboxed.
    if tag == "baseline" {
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "-A"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", std::env::var("HOME").expect("HOME"))
            .status();
    }
}

/// Extract the errno a probe printed for `tag` (`"TAG rc=.. errno=N"`).
fn errno_for(out: &str, tag: &str) -> Option<i32> {
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix(tag) {
            if let Some(e) = rest.split("errno=").nth(1) {
                return e.trim().split_whitespace().next()?.parse().ok();
            }
        }
    }
    None
}

/// Compile a C probe **into the repository's granted tree**, where a real
/// hostile hook's helper would live. A probe in `/tmp` is correctly denied
/// execution by the filesystem boundary, which would fail the seccomp tests for
/// the wrong reason.
fn probe_in_repo(repo: &Path, src: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let c = repo.join(format!("probe_{n}.c"));
    let bin = repo.join(format!("probe_{n}"));
    std::fs::write(&c, src).expect("write probe source");
    let ok = Command::new("cc")
        .args(["-O2", "-o"])
        .arg(&bin)
        .arg(&c)
        .status()
        .expect("cc runs")
        .success();
    assert!(ok, "probe failed to compile");
    bin
}

// =========================================================================
// Filesystem: a secret is unreadable, and the denial is Landlock's EACCES
// =========================================================================

/// A hook reading a secret must be denied by Landlock (`EACCES`), proven by an
/// A/B: an unsandboxed hook reads the *same controlled secret* and gets its
/// sentinel, while the sandboxed hook gets `Permission denied` and no sentinel.
///
/// The secret is a file this test creates *outside* every granted tree with a
/// unique sentinel, so "the read failed" cannot be a missing file, a
/// permissions quirk, or invalid syntax — the baseline proves it is readable.
#[test]
fn a_hook_reading_an_excluded_secret_is_denied_by_landlock() {
    // A controlled secret under an excluded tree (~/.ssh is in the default
    // exclude set). Written with a sentinel and mode 600.
    let home = std::env::var("HOME").expect("HOME");
    let ssh = Path::new(&home).join(".ssh");
    if !ssh.is_dir() {
        eprintln!("SKIPPED: ~/.ssh absent on this host");
        return;
    }
    let sentinel = "GVSECRET_9f13ab_do_not_leak";
    let secret = ssh.join("gv-escape-probe-secret");
    std::fs::write(&secret, sentinel).expect("write secret");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _c = Cleanup(secret.clone());

    let body = format!(
        "if cat {} 2>err; then echo \"READ_OK\"; else echo \"READ_FAIL rc=$?\"; fi; cat err 1>&2",
        secret.display()
    );

    // Baseline: the secret IS readable outside the sandbox and yields the sentinel.
    let base_repo = fixture();
    set_hook(base_repo.path(), &body);
    let base = commit_baseline(base_repo.path());
    assert!(
        base.out.contains(sentinel),
        "baseline could not read the controlled secret, so the test would be \
         vacuous — fix the fixture, do not trust the sandboxed result: {}",
        base.out
    );

    // Sandboxed: same hook, must be denied with Landlock's EACCES and no sentinel.
    let s = shim();
    let repo = fixture();
    set_hook(repo.path(), &body);
    let run = commit_sandboxed(&workable(Tier::Network, repo.path(), &s), repo.path());
    assert!(
        !run.out.contains(sentinel),
        "the sandboxed hook leaked the secret: {}",
        run.out
    );
    assert!(
        run.out.contains("Permission denied") || run.out.contains("READ_FAIL"),
        "the read must fail, and with a permission error specifically: {}",
        run.out
    );
}

// =========================================================================
// Seccomp: io_uring, with EPERM provenance and a faithful struct
// =========================================================================

/// io_uring must be denied by seccomp with `EPERM`. A/B: outside the sandbox
/// the *same* setup opens a ring (proving the syscall works on this host and
/// the struct is right); inside, it returns exactly `-1/EPERM`. A bare "negative
/// return" is not accepted — a permissive-host EPERM from an outer policy, or a
/// resource failure, would otherwise pass.
#[test]
fn a_hook_opening_io_uring_is_denied_by_seccomp_with_eperm() {
    // The real ABI struct is 120 bytes; a home-grown 96-byte struct lets a
    // *successful* setup copy past the object. Use the kernel header.
    let src = r#"
        #include <stdio.h>
        #include <string.h>
        #include <errno.h>
        #include <unistd.h>
        #include <linux/io_uring.h>
        #include <sys/syscall.h>
        int main(void){
            struct io_uring_params p; memset(&p,0,sizeof p);
            errno=0;
            long r = syscall(__NR_io_uring_setup, 8, &p);
            printf("IOURING rc=%ld errno=%d\n", r, r<0?errno:0);
            if (r>=0) close((int)r);
            return 0;
        }
        "#;
    let base_repo = fixture();
    let bp = probe_in_repo(base_repo.path(), src);
    set_hook(base_repo.path(), &format!("exec {}", bp.display()));
    let base = commit_baseline(base_repo.path());
    match errno_for(&base.out, "IOURING") {
        Some(0) => {} // opened — good, the syscall works here
        other => {
            eprintln!("SKIPPED: io_uring not available on this host (baseline errno={other:?})");
            return;
        }
    }

    let s = shim();
    let repo = fixture();
    let p = probe_in_repo(repo.path(), src);
    set_hook(repo.path(), &format!("exec {}", p.display()));
    let run = commit_sandboxed(&workable(Tier::Network, repo.path(), &s), repo.path());
    assert_eq!(
        errno_for(&run.out, "IOURING"),
        Some(EPERM),
        "io_uring must be denied by seccomp with EPERM specifically (the round-4 \
         bypass); got: {}",
        run.out
    );
}

/// C2's register-width trap, with provenance. The hostile value is
/// `PR_SET_SECCOMP | (1<<32)`. If the filter compared all 64 bits it would miss
/// the rule; the kernel then truncates to `PR_SET_SECCOMP` and evaluates mode 2
/// with a null filter pointer, which it rejects with `EFAULT`. So "r < 0" is
/// *not* proof the filter denied it. A/B: outside the sandbox the same call
/// returns a **non-EPERM** errno (the kernel's `EFAULT`); inside, it must be
/// exactly `EPERM` — only the filter produces that.
#[test]
fn the_seccomp_high_bit_denial_is_eperm_not_a_kernel_reject() {
    let src = r#"
        #include <stdio.h>
        #include <errno.h>
        #include <sys/prctl.h>
        #include <sys/syscall.h>
        int main(void){
            errno=0;
            long r = syscall(SYS_prctl, (long)PR_SET_SECCOMP | 0x100000000L, 2, 0, 0, 0);
            printf("HIGHBIT rc=%ld errno=%d\n", r, r<0?errno:0);
            return 0;
        }
        "#;
    let base_repo = fixture();
    let bp = probe_in_repo(base_repo.path(), src);
    set_hook(base_repo.path(), &format!("exec {}", bp.display()));
    let base = commit_baseline(base_repo.path());
    let base_errno = errno_for(&base.out, "HIGHBIT");
    assert!(
        base_errno.is_some() && base_errno != Some(EPERM),
        "baseline high-bit call should be rejected by the KERNEL with a non-EPERM \
         errno (EFAULT); if it is already EPERM the test cannot attribute the \
         denial to the filter: {}",
        base.out
    );

    let s = shim();
    let repo = fixture();
    let p = probe_in_repo(repo.path(), src);
    set_hook(repo.path(), &format!("exec {}", p.display()));
    let run = commit_sandboxed(&workable(Tier::Network, repo.path(), &s), repo.path());
    assert_eq!(
        errno_for(&run.out, "HIGHBIT"),
        Some(EPERM),
        "the filter must deny the high-bit prctl with EPERM (C2); a different \
         errno means the kernel truncated and rejected it, not the filter: {}",
        run.out
    );
}

// =========================================================================
// Hooks: blocked really blocks, and the commit still succeeds
// =========================================================================

/// `--hooks-blocked` must suppress the hook *while the commit still succeeds* —
/// otherwise a commit that failed before hook discovery (a bad argv, a shim
/// refusal) would show no marker and pass falsely. A/B: `--hooks-run` shows the
/// marker (the hook can run), `--hooks-blocked` hides it AND the commit lands.
#[test]
fn blocked_hooks_are_suppressed_while_the_commit_still_succeeds() {
    let s = shim();
    let marker = "HOOK_RAN_MARKER_5b2c";

    // --hooks-run: the marker appears (proves the hook path works at all).
    let run_repo = fixture();
    set_hook(run_repo.path(), &format!("echo {marker}"));
    let run = commit_sandboxed(&workable(Tier::Network, run_repo.path(), &s), run_repo.path());
    assert!(
        run.out.contains(marker),
        "a hook did not run under --hooks-run, so the blocked case is vacuous: {}",
        run.out
    );
    assert_eq!(run.commit_code, 0, "the --hooks-run commit must land: {}", run.out);

    // --hooks-blocked: same hook, marker absent AND the commit still succeeds.
    let repo = fixture();
    set_hook(repo.path(), &format!("echo {marker}"));
    let empty = tempfile::tempdir().expect("empty dir");
    let mut policy = workable(Tier::Network, repo.path(), &s);
    policy.hook_mode = HookMode::Blocked {
        empty_dir: empty.path().to_path_buf(),
    };
    let blocked = commit_sandboxed(&policy, repo.path());
    assert!(
        !blocked.out.contains(marker),
        "a hook ran despite --hooks-blocked: {}",
        blocked.out
    );
    assert_eq!(
        blocked.commit_code, 0,
        "the blocked commit must still SUCCEED — a failing commit hides the hook \
         for the wrong reason: {}",
        blocked.out
    );
}

// =========================================================================
// Strict tier: no network, proven by a live reachable listener
// =========================================================================

/// The strict tier's network denial is bwrap's `--unshare-net`, not a port
/// list. A/B with a **live loopback listener**: an unsandboxed hook connects to
/// it (rc=0), proving the listener is real and reachable; the strict-tier hook
/// cannot reach that same listener. `ECONNREFUSED` is meaningful *here* only
/// because the baseline proved a listener is live at that address — the fresh
/// network namespace is the sole difference between the two runs.
#[test]
fn the_strict_tier_hook_cannot_reach_a_live_listener() {
    if !strict_available() {
        eprintln!("SKIPPED: strict tier unavailable (no bwrap)");
        return;
    }
    // A listener the test owns, on an ephemeral port, in the host namespace.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().unwrap().port();
    // Accept in a background thread so a connect completes.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });

    let src = format!(
        r#"
        #include <stdio.h>
        #include <errno.h>
        #include <sys/socket.h>
        #include <netinet/in.h>
        #include <arpa/inet.h>
        int main(void){{
            int fd = socket(AF_INET, SOCK_STREAM, 0);
            if (fd < 0) {{ printf("SOCKET rc=-1 errno=%d\n", errno); return 0; }}
            struct sockaddr_in a = {{0}};
            a.sin_family = AF_INET; a.sin_port = htons({port});
            inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
            errno=0;
            int r = connect(fd, (struct sockaddr*)&a, sizeof a);
            printf("CONNECT rc=%d errno=%d\n", r, r<0?errno:0);
            return 0;
        }}
        "#
    );

    // Baseline: unsandboxed hook reaches the listener (rc=0).
    let base_repo = fixture();
    let bp = probe_in_repo(base_repo.path(), &src);
    set_hook(base_repo.path(), &format!("exec {}", bp.display()));
    let base = commit_baseline(base_repo.path());
    assert_eq!(
        errno_for(&base.out, "CONNECT"),
        Some(0),
        "baseline could not reach the live listener, so the strict result would \
         be vacuous: {}",
        base.out
    );

    // Strict: same listener, must NOT connect.
    let s = shim();
    let repo = fixture();
    let p = probe_in_repo(repo.path(), &src);
    set_hook(repo.path(), &format!("exec {}", p.display()));
    let run = commit_sandboxed(&workable(Tier::Strict, repo.path(), &s), repo.path());
    assert_ne!(
        errno_for(&run.out, "CONNECT"),
        Some(0),
        "the strict tier reached a listener the baseline showed is live — the \
         network namespace is not isolating: {}",
        run.out
    );
}
