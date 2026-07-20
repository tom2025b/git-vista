//! A minimal fixed-window rate limiter for LAN sign-in attempts (ADR 0005).
//! SECURITY_MODEL.md requires rate-limiting for any beyond-loopback exposure.
//! Wired only into the LAN listener's `create_session` handler — loopback
//! sign-in is unaffected, matching today's behavior.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many sign-in attempts one source IP gets per window before a `429`.
const MAX_ATTEMPTS: u32 = 5;
/// The fixed window's length. Resets fully once elapsed rather than sliding —
/// this only needs to blunt brute-forcing a stolen/guessed bootstrap token, not
/// meter traffic precisely.
const WINDOW: Duration = Duration::from_secs(60);

struct Bucket {
    count: u32,
    window_started: Instant,
}

/// Per-source-IP sign-in attempt counter, shared by every request the LAN
/// listener's `create_session` handler serves.
pub(crate) struct SignInLimiter {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl SignInLimiter {
    pub(crate) fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Record one attempt from `addr`; `true` = allowed, `false` = rate-limited.
    pub(crate) fn check(&self, addr: IpAddr) -> bool {
        let mut buckets = self.buckets.lock().expect("ratelimit lock");
        let now = Instant::now();
        let bucket = buckets.entry(addr).or_insert_with(|| Bucket {
            count: 0,
            window_started: now,
        });
        if now.duration_since(bucket.window_started) >= WINDOW {
            bucket.count = 0;
            bucket.window_started = now;
        }
        bucket.count += 1;
        bucket.count <= MAX_ATTEMPTS
    }

    /// Test-only: force an IP's window to have started at `when`, so a test can
    /// simulate window expiry without sleeping. Mirrors the pattern
    /// `session.rs`'s tests use on `Bootstrap::expires_at`.
    #[cfg(test)]
    fn force_window_start(&self, addr: IpAddr, when: Instant) {
        self.buckets.lock().unwrap().get_mut(&addr).unwrap().window_started = when;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "192.168.1.42".parse().unwrap()
    }

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let l = SignInLimiter::new();
        for _ in 0..MAX_ATTEMPTS {
            assert!(l.check(ip()));
        }
        assert!(!l.check(ip()), "the attempt past the limit is refused");
    }

    #[test]
    fn different_ips_get_independent_buckets() {
        let l = SignInLimiter::new();
        let other: IpAddr = "192.168.1.99".parse().unwrap();
        for _ in 0..MAX_ATTEMPTS {
            assert!(l.check(ip()));
        }
        assert!(!l.check(ip()));
        assert!(other != ip());
        assert!(l.check(other), "a different source IP is unaffected");
    }

    #[test]
    fn the_window_resets_after_it_elapses() {
        let l = SignInLimiter::new();
        assert!(l.check(ip()));
        l.force_window_start(ip(), Instant::now() - WINDOW - Duration::from_secs(1));
        for _ in 0..MAX_ATTEMPTS {
            assert!(l.check(ip()), "a fresh window allows a fresh batch of attempts");
        }
    }
}
