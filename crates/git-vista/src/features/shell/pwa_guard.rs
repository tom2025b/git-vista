//! ADR 0032's tripwire: this app ships **no service worker**, permanently.
//!
//! The full reasoning lives in `docs/adr/0032-no-service-worker.md` and, at
//! the registration's natural insertion point, in `index.html`'s comment. The
//! short form: every static byte is deliberately `no-cache`, `/api` must never
//! be cached, and on this transport (loopback + SSH forward, or the opt-in LAN
//! view) "offline" means the tunnel died — a worker would convert that
//! diagnosable failure into an apparent app bug.
//!
//! Scope honesty: this scans `index.html` only. A Rust-side registration
//! would go through `web_sys::ServiceWorkerContainer` — that name is the
//! thing to grep for in review if suspicion ever arises.

#[cfg(test)]
mod tests {
    const INDEX_HTML: &str = include_str!("../../../index.html");

    /// Both directions guarded: a registration appearing is the obvious
    /// violation; the deliberate-NO comment disappearing is the quiet one —
    /// it is the only thing standing at the insertion point when the next
    /// "finish the PWA" pass arrives.
    #[test]
    fn no_service_worker_is_registered_and_the_refusal_stays_written_down() {
        assert!(
            !INDEX_HTML.contains("serviceWorker"),
            "index.html mentions `serviceWorker` — ADR 0032 refuses a service \
             worker permanently (no-cache statics, uncacheable /api, and a \
             worker masks tunnel death as an app bug). If the transport model \
             has genuinely changed, supersede ADR 0032 with a new ADR; do not \
             register quietly."
        );
        assert!(
            INDEX_HTML.contains("deliberately NO service worker"),
            "index.html's deliberate-NO comment is gone. That comment is ADR \
             0032's guard at the exact spot a worker would be registered — \
             restore it, or supersede the ADR before removing it."
        );
    }
}
