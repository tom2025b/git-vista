//! The crate's one writer of process-wide environment state in tests.
//!
//! # Why this had to stop being file-scoped
//!
//! `sandbox::argv` owned a `SSH_AUTH_SOCK_LOCK` and said so in its own doc
//! comment: "nothing outside this file's tests touches `SSH_AUTH_SOCK`
//! (checked by grep when these tests were written), so a claim covering only
//! this file's own three callers is sufficient." That was true and is no
//! longer. #704's fix reads the **whole** environment
//! (`spawn::with_untrusted_checkout_env` calls `std::env::vars_os()`), and
//! `handlers::clone`'s spawn proof sets a canary and `SSH_AUTH_SOCK` in this
//! process so a real hook can testify about what it did and did not inherit.
//! Two files, one key, and — worse than a clobber — a full-environ iteration
//! racing a `set_var` in another thread.
//!
//! So the lock moves here and both callers take it. A lock whose scope is
//! narrower than its key's readership is not a lock; it is a comment.
//!
//! # What this does and does not guarantee
//!
//! It serialises every *deliberate* environment mutation and every
//! *deliberate* full-environ read in this crate's tests against each other.
//! It cannot serialise them against `fork`/`exec` in unrelated tests, which
//! read `environ` without asking anyone. That residual is unchanged from what
//! `argv.rs` already accepted, it is why callers hold the guard for
//! *composition* only (microseconds) rather than across a spawn, and it is
//! recorded here rather than argued away.

use std::ffi::{OsStr, OsString};

/// The one mutex. Poisoning is ignored deliberately: a panicking test leaves
/// the environment restored by [`with_env`]'s own restore step running during
/// unwind, and refusing every later test because an earlier one failed turns
/// one red into a cascade that hides it.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How long [`lock_env`] waits before deciding the lock will never arrive.
/// Generous against a genuinely slow contended run, short against a wait that
/// is never going to return.
const LOCK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Take [`ENV_LOCK`], or **panic with a named reason** rather than waiting
/// forever.
///
/// # Why this is not just `.lock()`
///
/// Measured here, on the first run of this module (2026-09-07): `with_env`
/// called from inside a `with_env` body self-deadlocked on this very mutex.
/// `std::sync::Mutex` is not reentrant, so the second acquisition waits on a
/// guard the same thread is holding and cannot release. The test binary then
/// sat at zero CPU with one surviving thread for forty minutes, holding this
/// box's build lock, until it was killed from outside — two other lanes were
/// queued behind it.
///
/// A test that hangs forever is worse than a test that fails: the failure is
/// silent, it stalls whatever runner is executing it, and the diagnosis has to
/// come from `/proc` rather than from the output. So the wait is bounded and
/// the timeout says what is almost certainly wrong. Note the shape carefully —
/// **this is not a fix for re-entrancy**, it is a fix for re-entrancy being
/// *invisible*. Nesting is still a bug; it now announces itself in one line.
fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    let deadline = std::time::Instant::now() + LOCK_DEADLINE;
    loop {
        match ENV_LOCK.try_lock() {
            Ok(guard) => return guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "sandbox::test_env::with_env waited {}s for ENV_LOCK and gave up.\n\
                     The overwhelmingly likely cause is a nested with_env call: this is a \
                     plain, NON-REENTRANT Mutex, so a with_env inside another with_env body \
                     on the same thread waits on a guard it is itself holding. Use \
                     with_env_locked for the inner layer, or restructure so the calls are \
                     sequential.\n\
                     The other possibility is a genuinely stuck holder on another thread — \
                     check whether some caller is holding the guard across an .await, which \
                     this module's doc forbids for exactly this reason.",
                    LOCK_DEADLINE.as_secs()
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

/// Restores each captured variable to its prior state when dropped — including
/// while a panic unwinds, which a plain "set, run, restore" sequence does not.
struct Restore(Vec<(OsString, Option<OsString>)>);

impl Drop for Restore {
    fn drop(&mut self) {
        for (key, prior) in self.0.drain(..) {
            // SAFETY: `ENV_LOCK` is held for this whole value's lifetime by
            // the `with_env` frame that owns it, and this module is the
            // crate's only test-side writer of process environment (see the
            // module doc), so no other deliberate reader or writer can
            // observe a torn state here.
            match prior {
                Some(v) => unsafe { std::env::set_var(&key, v) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

/// Apply `vars` to the process environment, run `f`, restore. `None` clears the
/// name for the duration.
///
/// `f` is **synchronous on purpose**. The variables need to be live while a
/// command is *composed* — that is when `spawn::with_untrusted_checkout_env`
/// reads them, and a composed `Command` carries its own environment overrides
/// from then on — never while it runs. Compose inside, `await` outside: the
/// guard is held for microseconds and never across a suspension point.
///
/// # NEVER call this from inside another `with_env` body
///
/// [`ENV_LOCK`] is a plain `Mutex` and is not reentrant, so a nested call
/// self-deadlocks — see [`lock_env`] for the incident that earned this
/// paragraph. If you need a nested layer, call [`with_env_locked`], which
/// assumes the guard is already held.
pub(crate) fn with_env<T>(vars: &[(&str, Option<&OsStr>)], f: impl FnOnce() -> T) -> T {
    let _guard = lock_env();
    with_env_locked(vars, f)
}

/// [`with_env`]'s body, for a caller that **already holds** [`ENV_LOCK`].
///
/// Private to this module on purpose: it is sound only under that precondition,
/// which the type system cannot state here, and the one legitimate caller is
/// this module's own nesting test. A production or cross-module caller wanting
/// this is a caller that should be using [`with_env`].
fn with_env_locked<T>(vars: &[(&str, Option<&OsStr>)], f: impl FnOnce() -> T) -> T {
    let _restore = Restore(
        vars.iter()
            .map(|(key, _)| (OsString::from(key), std::env::var_os(key)))
            .collect(),
    );
    for (key, value) in vars {
        // SAFETY: as `Restore::drop` — `ENV_LOCK` is held for this frame,
        // either by `with_env` above or by the caller that is required to
        // hold it before calling this function.
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set and clear both work, and both are undone — including the "was
    /// absent, must go back to absent" direction, which a restore that only
    /// writes back captured values gets wrong.
    #[test]
    fn with_env_sets_clears_and_restores_both_directions() {
        const PRESENT: &str = "GV_TEST_ENV_PRESENT";
        const ABSENT: &str = "GV_TEST_ENV_ABSENT";

        with_env(&[(PRESENT, Some(OsStr::new("outer")))], || {
            assert_eq!(std::env::var(PRESENT).unwrap(), "outer");
            assert!(std::env::var_os(ABSENT).is_none(), "premise");

            // `with_env_locked`, NOT `with_env`: the outer frame already holds
            // ENV_LOCK and the mutex is not reentrant. Writing `with_env` here
            // is what deadlocked this binary for forty minutes on the day the
            // module was written.
            with_env_locked(
                &[
                    (PRESENT, Some(OsStr::new("inner"))),
                    (ABSENT, Some(OsStr::new("now-set"))),
                ],
                || {
                    assert_eq!(std::env::var(PRESENT).unwrap(), "inner");
                    assert_eq!(std::env::var(ABSENT).unwrap(), "now-set");
                },
            );

            assert_eq!(
                std::env::var(PRESENT).unwrap(),
                "outer",
                "a value that existed before must come back"
            );
            assert!(
                std::env::var_os(ABSENT).is_none(),
                "a name that did not exist before must go back to not existing, \
                 not be left behind holding its inner value"
            );
        });
    }

    /// The restore must run while a panic unwinds, which a bare
    /// set-run-restore sequence loses: the restore statement is simply skipped
    /// and the variable outlives the test that set it.
    #[test]
    fn a_panicking_body_still_restores_the_environment() {
        const KEY: &str = "GV_TEST_ENV_PANIC";
        assert!(std::env::var_os(KEY).is_none(), "premise");

        let panicked = std::panic::catch_unwind(|| {
            with_env(&[(KEY, Some(OsStr::new("during")))], || {
                assert_eq!(std::env::var(KEY).unwrap(), "during");
                panic!("deliberate");
            })
        })
        .is_err();

        assert!(panicked, "the body must really have panicked");
        assert!(
            std::env::var_os(KEY).is_none(),
            "the variable outlived the panicking body"
        );
    }
}
