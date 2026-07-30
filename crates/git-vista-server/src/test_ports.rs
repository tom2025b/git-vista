//! M1.13b (#66): the one owner of TCP port 9418 inside this test binary.
//!
//! Two unrelated tests in this binary need port 9418 and neither can move off
//! it: it is the only unprivileged entry in `sandbox::DEFAULT_GIT_PORTS`, so it
//! is the only port a Network-tier Landlock connect grant covers.
//!
//! * `sandbox::escape_suite`'s `strict_listener_denied` needs a loopback
//!   listener on 9418 for its baseline leg to connect to.
//! * `planner::contract_suite`'s `push_branch_executes_through_the_pipeline`
//!   needs a `git daemon` bound to 9418 to receive a real `git://` push.
//! * `sandbox::escape_suite`'s `strict_tcp_bind_denied` needs 9418 *unbound*,
//!   because its baseline leg proves the operation is possible on this host by
//!   binding it.
//!
//! `cargo test` runs all three on separate threads of one process, so without a
//! rendezvous whichever lands second fails (or, worse for the third, degrades
//! into a silently-vacuous `Outcome::CapabilityAbsent` when its baseline bind
//! returns `EADDRINUSE`). This module is that rendezvous: a claim, not a port
//! allocator — the port number is fixed by the Landlock grant, so the only
//! thing left to arbitrate is *who has it right now*.

use std::net::TcpStream;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The one owner of TCP port 9418 in this test binary.
static GIT_PROTOCOL_PORT: Mutex<()> = Mutex::new(());

/// An exclusive claim on port 9418, released on drop.
///
/// Hold it for as long as anything is bound to the port — a claim released
/// while a listener or a `git daemon` is still up hands the next claimant a
/// port that is still occupied, which is the whole failure this type exists to
/// prevent.
pub(crate) struct PortClaim {
    _guard: MutexGuard<'static, ()>,
}

impl PortClaim {
    /// The git protocol port. Fixed, not negotiable: see the module docs.
    pub(crate) const PORT: u16 = 9418;

    /// How long `acquire` waits for the port to actually come free after the
    /// mutex is ours.
    const FREE_DEADLINE: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(50);

    /// Block until port 9418 is ours.
    pub(crate) fn acquire() -> Self {
        // Poison recovery is deliberate, and load-bearing. If a test panics
        // while holding this claim, `lock()` returns `Err(PoisonError)` for the
        // rest of the process — so a plain `unwrap()` here would turn *one*
        // failing test into a cascade in which every later claimant fails with
        // a confusing "poisoned lock" instead of its own real result. The
        // poison flag carries no state we depend on: the guarded value is `()`
        // and has no invariant a panic could have left half-broken, so taking
        // the guard out of the error is exactly as safe as taking it out of
        // `Ok`. What *is* recoverable state — a listener or a daemon still
        // bound to the port — is not tracked by the flag at all; that is
        // `wait_until_free`'s job, below, and it runs either way.
        let guard = GIT_PROTOCOL_PORT.lock().unwrap_or_else(|e| e.into_inner());
        // Only after the mutex is ours: a previous holder's socket can linger
        // briefly while it closes, and a *leaked* `git daemon` from an earlier
        // SIGKILLed run is outside this mutex's knowledge entirely.
        wait_until_free();
        Self { _guard: guard }
    }
}

/// Poll until nothing answers on 9418, or panic naming both possible holders.
///
/// A successful connect means something is still bound. Two causes, and the
/// message names both because the fixes differ: an out-of-process leak has to
/// be killed by hand, while an in-binary holder means some code path bound the
/// port without taking a `PortClaim` (or released its claim before dropping its
/// listener).
fn wait_until_free() {
    let deadline = Instant::now() + PortClaim::FREE_DEADLINE;
    loop {
        if TcpStream::connect(("127.0.0.1", PortClaim::PORT)).is_err() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "port {} is still in use after waiting {:?} with the claim mutex held. \
             Either a leaked `git daemon` from an earlier run is squatting it \
             (`pgrep -af git-daemon`, kill it, rerun), or something in this test \
             binary bound the port without taking a `test_ports::PortClaim` — or \
             dropped its claim while its listener was still alive.",
            PortClaim::PORT,
            PortClaim::FREE_DEADLINE,
        );
        std::thread::sleep(PortClaim::POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The claim actually excludes: two threads that both hold one can never be
    /// inside their critical sections at the same time.
    #[test]
    fn a_claim_serializes_concurrent_holders() {
        let inside = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for _ in 0..4 {
            let inside = Arc::clone(&inside);
            let overlapped = Arc::clone(&overlapped);
            threads.push(std::thread::spawn(move || {
                let _claim = PortClaim::acquire();
                if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                    overlapped.store(true, Ordering::SeqCst);
                }
                // Wide enough that an unserialized run would overlap: without
                // the claim, four threads sleeping 50ms each inside the section
                // are essentially guaranteed to be caught together.
                std::thread::sleep(Duration::from_millis(50));
                if inside.fetch_sub(1, Ordering::SeqCst) != 1 {
                    overlapped.store(true, Ordering::SeqCst);
                }
            }));
        }
        for t in threads {
            t.join().expect("claimant thread must not panic");
        }
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "two PortClaim holders were inside the critical section at once"
        );
        assert_eq!(
            inside.load(Ordering::SeqCst),
            0,
            "critical-section counter must unwind to zero"
        );
    }

    /// A panic while holding the claim poisons the mutex forever. `acquire`
    /// must recover from that, or one failing test would cascade into every
    /// later claimant — the reason `acquire` uses `unwrap_or_else(into_inner)`
    /// rather than `unwrap()`.
    #[test]
    fn a_poisoned_claim_does_not_cascade() {
        let panicked = std::thread::spawn(|| {
            let _claim = PortClaim::acquire();
            // Expected in the test output: this panic is the fixture.
            panic!("test_ports fixture: deliberate panic while holding the claim");
        })
        .join();
        assert!(
            panicked.is_err(),
            "the fixture thread must actually panic, or this test proves nothing"
        );
        assert!(
            GIT_PROTOCOL_PORT.lock().is_err(),
            "the mutex must be genuinely poisoned, or the recovery path below is \
             not the path under test"
        );

        // The claim this test is really about: it must succeed anyway.
        let _claim = PortClaim::acquire();
    }
}
