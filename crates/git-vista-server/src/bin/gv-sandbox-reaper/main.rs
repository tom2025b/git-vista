//! #728: a process-lifetime supervisor for the sandbox launcher.
//!
//! `bwrap --die-with-parent` (INV-8) protects against exactly one thing: ITS
//! OWN parent dying while it is still directly parented to it. Two orphans
//! found alive on 2026-09-07 (both `git -C <tmp>/repo status --short`,
//! reparented to `systemd --user`, their fixtures already deleted) showed
//! that is not the whole guarantee a caller needs — see #728 and its ADR for
//! the full account of why, and why `--die-with-parent`/`PR_SET_PDEATHSIG`
//! cannot be trusted alone.
//!
//! This binary sits between the real caller (the server, or a test harness)
//! and both sandboxed launcher shapes: `Tier::Strict`'s `bwrap` and
//! `Tier::Network`'s bare `gv-sandbox` shim. `Tier::Unsandboxed`'s bare `git`
//! is never wrapped (see `sandbox::spawn::wrap_with_reaper`).
//! It is invoked as `gv-sandbox-reaper <expected-parent-pid> <program> <args…>`
//! and:
//!
//! 1. forks;
//! 2. the child becomes its own process group leader, registers
//!    `PR_SET_PDEATHSIG(SIGKILL)` against the reaper (an ordinary, single-hop
//!    use — the reaper is its real, immediate parent, no reparenting involved
//!    in this specific guarantee), and `execvp`s `<program> <args…>`
//!    unchanged — from that point on it *is* the launcher the caller asked
//!    for, byte for byte;
//! 3. the parent (this process) polls its own `getppid()`. If it ever differs
//!    from `<expected-parent-pid>` — the caller's own pid, passed as an
//!    argument rather than discovered later, see "Why the expected parent is
//!    passed in, not read" — the real caller is gone. It kills the whole
//!    child process group with `SIGKILL` and exits.
//!
//! # Why the expected parent is passed in, not read
//!
//! An earlier version of this file called `getppid()` once, right after
//! `main` starts, and used that as the baseline for every later comparison.
//! That is racy in a way review caught before it shipped: the OS-level fork
//! that creates *this* process happens entirely before a single line of this
//! file can run. If the real caller dies inside that window — between the
//! fork and this process's first executed instruction — this process is
//! reparented before it ever gets a chance to look, and a self-read baseline
//! would capture the *already-wrong* value, permanently. Every later
//! comparison against that baseline would then read "nothing has changed",
//! forever, and the launcher could survive unbounded — the exact #728 shape,
//! reintroduced one layer up.
//!
//! The caller passing its own pid as `argv[1]` has no equivalent window: the
//! caller reads `std::process::id()` (or platform equivalent) before it even
//! spawns this process, so there is nothing racing it. Whatever this process's
//! `getppid()` reads, at any point after it starts — however late that is, and
//! however much has already gone wrong before it got the chance to run — is
//! compared against a value that was correct before the race could begin.
//! `sandbox::lifecycle::a_reaper_process_reaps_a_launcher_orphaned_before_it_ever_ran`
//! proves this deterministically, forcing the worst-case ordering with
//! `SIGSTOP`/`SIGCONT` rather than hoping a timing window is hit.
//!
//! # Why polling, not `PR_SET_PDEATHSIG` on this process itself
//!
//! `SIGKILL` cannot be caught. A watcher that registers its own death signal
//! and relies on receiving it cannot run any cleanup code when that signal
//! actually arrives — it is simply gone, and its child is exactly as orphaned
//! as it would have been with no watcher at all. This process registers
//! nothing for *itself*; it only ever reads `getppid()`, a plain syscall whose
//! answer does not depend on any signal being delivered, to it or to anything
//! else, by whatever mechanism its real parent died. (Its *child*, one step
//! below, does register `PR_SET_PDEATHSIG` — but that is the ordinary,
//! non-racy, single-hop case: see step 2 above and `main`'s own comments.)
//!
//! # The named residual
//!
//! Polling has a window: an orphan can be alive for up to [`POLL_INTERVAL`]
//! after its real parent dies before this process notices and reaps it. That
//! is a bounded, measured cost, stated here rather than hidden — the
//! alternative this replaces (`--die-with-parent` alone) had an *unbounded*
//! one, as the two processes that motivated #728 demonstrate.
//!
//! # Exit status passthrough
//!
//! A caller that read the launcher's own exit status before #728 must keep
//! reading the same thing now that this process sits in front of it. On a
//! normal exit this process exits with the child's exact code. If the child
//! dies to a signal, this process re-raises that same signal against itself
//! after restoring its default disposition, so `ExitStatus::signal()` still
//! reports the number the caller would have seen without this wrapper. Only
//! the reparenting-detected path produces a status nothing else can: this
//! process raises `SIGKILL` against itself once the child is confirmed reaped,
//! which is indistinguishable from "the launcher was itself killed" — an
//! honest answer, since by that point every real cause is a real caller that
//! is already gone and in no position to read it.

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;

/// How often this process checks whether it has been reparented. The
/// residual named in the module doc: at most this long an orphan can survive
/// after its real parent dies before this process notices.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// This process's own exit code when its argv is unusable. Distinct from any
/// code the wrapped program could itself produce, so a misuse of this binary
/// never reads like the launcher failed.
const EXIT_USAGE: i32 = 120;
/// `execvp` itself failed (bad path, not executable, …). Mirrors the shim's
/// own `EXIT_EXEC` posture: distinct from the wrapped program's own exit
/// codes, so this failure is never misread as that program's.
const EXIT_EXEC_FAILED: i32 = 121;

fn main() {
    let argv: Vec<OsString> = std::env::args_os().collect();
    if argv.len() < 3 {
        eprintln!(
            "gv-sandbox-reaper: usage: gv-sandbox-reaper <expected-parent-pid> <program> <args…>"
        );
        std::process::exit(EXIT_USAGE);
    }
    // The caller's own pid, established BEFORE it spawned this process — see
    // the module doc's "Why the expected parent is passed in, not read" for
    // why this must never be a value this process discovers for itself later.
    let expected_parent_pid: libc::pid_t = match argv[1].to_str().and_then(|s| s.parse().ok()) {
        Some(pid) => pid,
        None => {
            eprintln!(
                "gv-sandbox-reaper: <expected-parent-pid> must be a plain integer, got {:?}",
                argv[1]
            );
            std::process::exit(EXIT_USAGE);
        }
    };
    let child_argv = to_cstrings(&argv[2..]);

    // This process's own pid: the child's PPID once forked, and therefore
    // what the CHILD (not this process) must compare its own `getppid()`
    // against below — a different value from `expected_parent_pid` above,
    // which is THIS process's parent, not this process itself.
    let reaper_pid = unsafe { libc::getpid() };

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!(
            "gv-sandbox-reaper: fork failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(EXIT_EXEC_FAILED);
    }
    if pid == 0 {
        // Child: its own process group, so the parent below can `killpg` it
        // (and anything it has itself spawned, e.g. bwrap's own pid-namespace
        // descendants) without touching an unrelated group. Errors here are
        // not fatal — a race where the parent already set this group is a
        // documented, harmless POSIX double-set, not a failure.
        unsafe {
            libc::setpgid(0, 0);
        }
        // This process (`Tier::Strict`'s bwrap or `Tier::Network`'s bare
        // gv-sandbox shim — `Tier::Unsandboxed` is never wrapped; see
        // `spawn::wrap_with_reaper`) must die if THIS reaper does, by
        // any means — not only when the reaper
        // notices it has been reparented and reacts, but also when something
        // simply kills the reaper outright. A caller cancelling a running
        // operation does exactly that: it holds this process's OS parent as
        // `tokio::process::Child` and calls `.kill()`/`kill_on_drop`, which
        // now lands on the reaper, not on this process. Without its own
        // registered death signal, that SIGKILL would leave this process an
        // immediate, un-orphaned-by-reparenting-but-just-as-real orphan —
        // the reaper polling loop below cannot help, because SIGKILL killed
        // it too, and cannot run any code on the way out.
        //
        // Registering this is the ordinary, single-hop use of
        // `PR_SET_PDEATHSIG` — the reaper is this process's *real, immediate*
        // parent, so no reparenting is involved in this specific guarantee.
        // It is the same mechanism `bwrap --die-with-parent` already uses for
        // itself; registering it here too is what keeps the reaper's own
        // cancel-compatibility fix (see ADR 0141) independent of bwrap's own
        // flag.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
        }
        // Close the same race bwrap's own implementation closes: if the
        // reaper had already died between the fork above and the prctl call
        // just now, this process has already been reparented away from the
        // reaper and the just-registered signal is watching the wrong (new)
        // parent, which may never die. Checking `getppid()` immediately after
        // — against the reaper's own pid, not `expected_parent_pid` above,
        // which is the reaper's parent, not the reaper itself — and
        // self-killing on a mismatch is what makes that window not matter.
        if unsafe { libc::getppid() } != reaper_pid {
            unsafe {
                libc::_exit(125);
            }
        }
        exec_or_die(&child_argv);
    }

    // Parent: same belt-and-suspenders `setpgid`, tolerating the child having
    // already won the race to set it.
    unsafe {
        libc::setpgid(pid, pid);
    }

    loop {
        let mut status: i32 = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            exit_matching_child(status);
        }
        if waited < 0 {
            // The child is already gone and already reaped by something else.
            // Nothing left to supervise; behave as if it exited cleanly rather
            // than guessing at a status that no longer exists.
            std::process::exit(0);
        }

        let current_ppid = unsafe { libc::getppid() };
        if current_ppid != expected_parent_pid {
            reap_orphan(pid);
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Kill the child's entire process group and exit in a way that reads,
/// honestly, as "the launcher was killed" — see the module doc's exit-status
/// section for why `SIGKILL` against self is the right answer here rather
/// than a distinguishable made-up code.
fn reap_orphan(child_pid: libc::pid_t) -> ! {
    unsafe {
        libc::killpg(child_pid, libc::SIGKILL);
    }
    let mut status: i32 = 0;
    unsafe {
        // Blocking: the child is already sentenced, so there is nothing left
        // to poll for. This reaps it rather than leaving a zombie behind.
        libc::waitpid(child_pid, &mut status, 0);
        libc::signal(libc::SIGKILL, libc::SIG_DFL);
        libc::raise(libc::SIGKILL);
    }
    // `raise(SIGKILL)` does not return. This line exists only so the function
    // still type-checks as `-> !` if it somehow did.
    std::process::exit(137);
}

/// Exit (or re-raise a signal against self) so this process's own final
/// status is indistinguishable from the child's, for a caller that reads it.
fn exit_matching_child(status: i32) -> ! {
    if libc_wifexited(status) {
        std::process::exit(libc_wexitstatus(status));
    }
    if libc_wifsignaled(status) {
        let sig = libc_wtermsig(status);
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
        // Only reached if `raise` somehow did not terminate the process.
        std::process::exit(128 + sig);
    }
    // Neither exited nor signalled is not a real waitpid outcome for a
    // terminated child; fall back to a distinct, honest failure code rather
    // than guessing.
    std::process::exit(EXIT_EXEC_FAILED);
}

/// Thin wrappers around the `WIF*`/`W*` macros: `libc` exposes them as
/// `const fn`s taking the raw status, not methods, so naming them once here
/// keeps `main` reading as the state machine it is.
fn libc_wifexited(status: i32) -> bool {
    libc::WIFEXITED(status)
}
fn libc_wexitstatus(status: i32) -> i32 {
    libc::WEXITSTATUS(status)
}
fn libc_wifsignaled(status: i32) -> bool {
    libc::WIFSIGNALED(status)
}
fn libc_wtermsig(status: i32) -> i32 {
    libc::WTERMSIG(status)
}

/// `execvp` the given program-and-args in the current (child) process, or die
/// trying. Never returns on success, by definition of `execvp`.
fn exec_or_die(argv: &[CString]) -> ! {
    let mut raw: Vec<*const libc::c_char> = argv.iter().map(|s| s.as_ptr()).collect();
    raw.push(std::ptr::null());
    unsafe {
        libc::execvp(argv[0].as_ptr(), raw.as_ptr());
    }
    // execvp only returns on failure.
    eprintln!(
        "gv-sandbox-reaper: exec {:?} failed: {}",
        argv[0],
        std::io::Error::last_os_error()
    );
    std::process::exit(EXIT_EXEC_FAILED);
}

/// `OsString` -> `CString`, for the raw `execvp` call. Panics on an embedded
/// NUL, which cannot occur in a real argv (the kernel itself refuses one),
/// making this a fine place to fail loudly rather than degrade.
fn to_cstrings(args: &[OsString]) -> Vec<CString> {
    args.iter()
        .map(|a| CString::new(a.as_os_str().as_bytes()).expect("argv entry must not contain NUL"))
        .collect()
}
