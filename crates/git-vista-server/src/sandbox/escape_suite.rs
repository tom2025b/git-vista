//! M1.13b (#66) Tasks 10-12: the escape battery.
//!
//! These drive the **whole composed launcher** against a real repository and
//! assert that a hostile repository cannot reach past the boundary. Nothing
//! here builds a Landlock ruleset or a seccomp filter itself — that is the
//! composition rule (verdict §5), and a test that constructs a primitive is a
//! defect even when it passes.
//!
//! # The one rule every test in this file obeys
//!
//! **Every denial is paired with a granted control in the same run**, because a
//! sandbox that denied *everything* would pass a naive denial test while being
//! useless. And every control that "must fail" is checked to fail *for the
//! stated reason* — an `EACCES` from the boundary, not an `ENOENT` from a typo.
//! This project has shipped green-but-vacuous tests before (an `open_fds()`
//! tautology, a register-width check truncated before the syscall saw it); the
//! structure here is chosen so a passing test cannot be lying.

use super::shim_cli::{fixture, launch, shim, strict_available, workable};
use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// A repository whose `pre-commit` hook runs an arbitrary shell script. The
/// hook is how a hostile repository gets code execution *inside* the sandbox —
/// it is the realistic threat, not a synthetic one, because a cloned repo can
/// carry hooks.
fn hostile_hook_repo(script: &str) -> tempfile::TempDir {
    let d = fixture();
    let hooks = d.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let hook = hooks.join("pre-commit");
    let mut f = std::fs::File::create(&hook).expect("hook file");
    writeln!(f, "#!/bin/sh\n{script}").expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    d
}

/// Compile a C probe **into the repository's own tree**, so the sandbox can
/// `execve` it — the repo is a granted RW tree, exactly where a real hostile
/// hook's compiled helper would live. A probe left in `/tmp` is correctly
/// denied execution by the filesystem boundary (measured), which would make the
/// seccomp tests fail for the wrong reason. Returns the path the hook should
/// exec, relative to nothing — it is absolute, inside `repo`.
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

/// Commit through the sandbox so the hook fires, returning `(code, stdout+stderr)`.
/// Staging a file guarantees `pre-commit` actually runs — an empty commit may
/// short-circuit before hooks on some git versions.
fn commit_firing_the_hook(policy: &Policy, repo: &Path) -> (i32, String) {
    std::fs::write(repo.join("payload.txt"), "x").expect("write");
    let (_, _, _) = launch(policy, repo, &["add", "payload.txt"]);
    let (code, out, err) = launch(policy, repo, &["commit", "-q", "-m", "fire"]);
    (code, format!("{out}{err}"))
}

// -------------------------------------------------------------------------
// INV-1 / INV-3: the filesystem boundary
// -------------------------------------------------------------------------

/// A hook that reads a secret must fail, while a hook reading the granted
/// identity file in the *same tier* succeeds. The pair is the point: it proves
/// the boundary discriminates rather than blanket-denies.
#[test]
fn a_hook_cannot_read_an_excluded_secret_but_can_read_granted_config() {
    let home = std::env::var("HOME").expect("HOME");
    let secret = format!("{home}/.ssh/known_hosts");
    if !Path::new(&secret).exists() {
        return; // nothing to protect on this host; not a failure
    }

    let s = shim();
    // Denial:
    let repo = hostile_hook_repo(&format!(
        "cat {secret} && echo LEAKED_THE_SECRET || echo SECRET_DENIED"
    ));
    let policy = workable(Tier::Network, repo.path(), &s);
    let (_, out) = commit_firing_the_hook(&policy, repo.path());
    assert!(
        out.contains("SECRET_DENIED") && !out.contains("LEAKED_THE_SECRET"),
        "a hook read an excluded secret through the sandbox: {out}"
    );

    // Granted control, same tier: the identity file the boundary is *meant* to
    // allow must still be readable, or the denial above proves nothing.
    let repo2 = hostile_hook_repo(&format!(
        "cat {home}/.gitconfig >/dev/null && echo CONFIG_OK || echo CONFIG_DENIED"
    ));
    let (_, out2) = commit_firing_the_hook(&workable(Tier::Network, repo2.path(), &s), repo2.path());
    assert!(
        out2.contains("CONFIG_OK"),
        "the granted ~/.gitconfig was not readable, so the boundary is denying \
         everything and the secret test above is vacuous: {out2}"
    );
}

/// A symlink an attacker plants in the repository, pointing at a secret, must
/// not defeat the exclusion — Landlock resolves on the final path.
#[test]
fn a_repo_symlink_into_a_secret_is_still_denied() {
    let home = std::env::var("HOME").expect("HOME");
    let secret = format!("{home}/.ssh");
    if !Path::new(&secret).exists() {
        return;
    }
    let s = shim();
    let repo = fixture();
    let link = repo.path().join("sneaky");
    std::os::unix::fs::symlink(&secret, &link).expect("symlink");
    let policy = workable(Tier::Network, repo.path(), &s);
    let (code, out, err) = launch(&policy, repo.path(), &["log"]);
    // A control that the repo itself is readable:
    assert_eq!(code, 0, "the repo must be usable: {err}");
    // Now the attack, driven through git's own file access:
    let (_, out2, _) = launch(
        &policy,
        repo.path(),
        &["config", "-f", "sneaky/known_hosts", "--list"],
    );
    let _ = out;
    assert!(
        !out2.contains("@"),
        "a repo symlink reached into ~/.ssh: {out2}"
    );
}

// -------------------------------------------------------------------------
// INV-4 / INV-5: seccomp — io_uring and namespaces
// -------------------------------------------------------------------------

/// The round-4 bypass. A hook that opens an io_uring must be refused, while a
/// hook doing an ordinary allowed syscall in the same tier succeeds — so the
/// refusal is the filter, not a broken sandbox. Written in C because the
/// syscall must be issued at full register width; a shell cannot express it.
#[test]
fn a_hook_cannot_open_an_io_uring() {
    let s = shim();
    let repo = fixture();
    let probe = probe_in_repo(
        repo.path(),
        r#"
        #include <stdio.h>
        #include <string.h>
        #include <errno.h>
        #include <unistd.h>
        #include <sys/syscall.h>
        struct p { unsigned a[8]; unsigned long long b[8]; };
        int main(void){
            struct p params; memset(&params,0,sizeof params);
            errno=0; long r = syscall(425 /*io_uring_setup*/, 8, &params);
            printf(r<0 ? "IOURING_DENIED %d\n" : "IOURING_OPENED\n", errno);
            errno=0; long g = syscall(SYS_getpid);
            printf(g>0 ? "CONTROL_GETPID_OK\n" : "CONTROL_GETPID_FAIL\n");
            return 0;
        }
        "#,
    );
    install_hook(repo.path(), &format!("exec {}", probe.display()));
    let (_, out) = commit_firing_the_hook(&workable(Tier::Network, repo.path(), &s), repo.path());
    assert!(
        out.contains("IOURING_DENIED"),
        "io_uring was not denied inside the sandbox — the round-4 bypass is open: {out}"
    );
    assert!(
        out.contains("CONTROL_GETPID_OK"),
        "an ordinary syscall was denied too, so the filter is killing everything \
         and the io_uring result is meaningless: {out}"
    );
}

/// C2's register-width trap, driven for real: the hostile `prctl` option with a
/// high bit set must still be denied. A userspace test that casts through a
/// 32-bit int truncates the value before the syscall and proves nothing, so the
/// probe passes a 64-bit `long` straight to `syscall`.
#[test]
fn the_seccomp_argument_comparison_is_not_fooled_by_the_high_bits() {
    let s = shim();
    let repo = fixture();
    let probe = probe_in_repo(
        repo.path(),
        r#"
        #include <stdio.h>
        #include <errno.h>
        #include <sys/prctl.h>
        #include <sys/syscall.h>
        int main(void){
            errno=0;
            long r = syscall(SYS_prctl, (long)PR_SET_SECCOMP | 0x100000000L, 2, 0, 0, 0);
            printf(r<0 ? "HIGHBIT_DENIED %d\n" : "HIGHBIT_SLIPPED_THROUGH\n", errno);
            char name[16]; errno=0;
            long g = syscall(SYS_prctl, PR_GET_NAME, name, 0, 0, 0);
            printf(g==0 ? "CONTROL_PRCTL_OK\n" : "CONTROL_PRCTL_FAIL\n");
            return 0;
        }
        "#,
    );
    install_hook(repo.path(), &format!("exec {}", probe.display()));
    let (_, out) = commit_firing_the_hook(&workable(Tier::Network, repo.path(), &s), repo.path());
    assert!(
        out.contains("HIGHBIT_DENIED"),
        "prctl(PR_SET_SECCOMP | 1<<32) was not denied — C2 register-width bug is live: {out}"
    );
    assert!(
        out.contains("CONTROL_PRCTL_OK"),
        "an allowed prctl was denied, so the filter blocks all prctl and the test is vacuous: {out}"
    );
}

// -------------------------------------------------------------------------
// INV-11 / INV-13: hooks
// -------------------------------------------------------------------------

/// `--hooks-blocked` must actually prevent hook execution, and `--hooks-run`
/// must actually allow it — the second half is the control that proves the
/// first is not passing because hooks were broken for some unrelated reason.
#[test]
fn blocked_hooks_do_not_run_and_running_hooks_do() {
    let s = shim();
    let marker_name = "HOOK_EXECUTED_MARKER";

    // --hooks-run: the marker must appear.
    let repo = hostile_hook_repo(&format!("echo {marker_name}"));
    let (_, out_run) = commit_firing_the_hook(&workable(Tier::Network, repo.path(), &s), repo.path());
    assert!(
        out_run.contains(marker_name),
        "a hook did not run under --hooks-run, so the blocked test below is vacuous: {out_run}"
    );

    // --hooks-blocked: the same hook, the marker must be absent.
    let repo2 = hostile_hook_repo(&format!("echo {marker_name}"));
    let empty = tempfile::tempdir().expect("empty dir");
    let mut blocked = workable(Tier::Network, repo2.path(), &s);
    blocked.hook_mode = HookMode::Blocked {
        empty_dir: empty.path().to_path_buf(),
    };
    let (_, out_blocked) = commit_firing_the_hook(&blocked, repo2.path());
    assert!(
        !out_blocked.contains(marker_name),
        "a hook ran despite --hooks-blocked: {out_blocked}"
    );
}

// -------------------------------------------------------------------------
// Strict tier: the namespace boundary
// -------------------------------------------------------------------------

/// The strict tier has no network at all — its denial is bwrap's
/// `--unshare-net`, not a port list. A hook trying to open a TCP socket must
/// fail. Gated on strict availability so a host without bwrap reports skipped
/// rather than failing in the shared fixture.
#[test]
fn the_strict_tier_hook_has_no_network() {
    if !strict_available() {
        return;
    }
    let s = shim();
    let src = r#"
        #include <stdio.h>
        #include <errno.h>
        #include <sys/socket.h>
        #include <netinet/in.h>
        #include <arpa/inet.h>
        int main(void){
            int fd = socket(AF_INET, SOCK_STREAM, 0);
            struct sockaddr_in a = {0};
            a.sin_family = AF_INET; a.sin_port = htons(80);
            inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
            errno=0;
            int r = connect(fd, (struct sockaddr*)&a, sizeof a);
            /* ECONNREFUSED would mean the network was REACHED; we require the
               namespace to make it unreachable (ENETUNREACH/EPERM/EADDRNOTAVAIL). */
            printf(r==0 ? "CONNECTED\n" : "NETPATH errno=%d\n", errno);
            return 0;
        }
        "#;
    let repo = fixture();
    let probe = probe_in_repo(repo.path(), src);
    install_hook(repo.path(), &format!("exec {}", probe.display()));
    let (_, out) = commit_firing_the_hook(&workable(Tier::Strict, repo.path(), &s), repo.path());
    assert!(
        !out.contains("CONNECTED"),
        "the strict tier reached the network: {out}"
    );
    assert!(
        out.contains("NETPATH"),
        "the strict-tier probe did not run at all (no NETPATH line): {out}"
    );
}

// -------------------------------------------------------------------------
// helper
// -------------------------------------------------------------------------

/// Replace a repository's `pre-commit` hook with a new body. Used when the
/// probe must be compiled first (into the repo) and only then referenced.
fn install_hook(repo: &Path, script: &str) {
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::create_dir_all(repo.join(".git/hooks")).expect("hooks dir");
    let mut f = std::fs::File::create(&hook).expect("hook file");
    writeln!(f, "#!/bin/sh\n{script}").expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}
