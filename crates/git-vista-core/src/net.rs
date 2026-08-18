//! User-facing wording for network-level fetch failures.
//!
//! When a browser fetch is rejected at the *network* level — the request never
//! completed — the error the frontend gets is Safari's bare "TypeError: Load
//! failed" (Chrome's: "Failed to fetch"), which tells the user nothing they
//! can act on. In practice it means the git-vista server wasn't reachable:
//! its terminal/SSH session closed and took the process with it, the device
//! left the server's Wi-Fi, or iOS suspended the tab and Safari re-used a
//! pooled TCP socket that had silently died in the meantime. This module owns
//! the actionable message the UI shows instead; pure text-building, so the
//! wording is pinned by host tests.

/// The message for a fetch that never completed. `raw` is the browser's own
/// error text ("TypeError: Load failed"), kept in parentheses so the real
/// error stays diagnosable; an empty `raw` just omits it.
pub fn network_error_text(raw: &str) -> String {
    let raw = raw.trim();
    let detail = if raw.is_empty() {
        String::new()
    } else {
        format!(" ({raw})")
    };
    format!(
        "Couldn't reach the git-vista server{detail}.\n\
         Check that `gv` is still running and that this device is on the same \
         network, then try again."
    )
}

/// The message `git-vista`'s `with_deadline`-bounded requests show when the
/// client gives up waiting for an answer before one arrives (#216/#218, M2.19
/// #72).
///
/// `with_deadline` only drops the client's own `.await` — it has no
/// `AbortController`, so it cannot cancel the request in flight, and the
/// server keeps working regardless of whether anyone is still waiting. So
/// this message states only what the client actually knows (its own wait ran
/// out) and never claims the request failed, since on this deployment it very
/// often has not: a slow git hook or a large operation can legitimately run
/// past the wait. It deliberately does not guess at *why* the wait ran out —
/// earlier wording named "the SSH tunnel" as the likely cause, which was true
/// only for the since-paused iPad/SSH-tunnel deployment (see
/// `docs/SECURITY_MODEL.md` and the M1-05 ADR) and actively misleading for
/// the localhost-only setup this app actually runs under: there is no tunnel
/// to drop, so telling the user to "restart the port forward" sent them
/// looking for a cause that could not be there. The actionable step that
/// works regardless of deployment is checking the activity feed — the
/// server's own record of what actually happened — before retrying, because a
/// commit's write can have landed even though this client never saw the
/// answer, and retrying it blind creates a second commit. Kept as a pure
/// function, like [`offline_refusal_text`] above, so the wording is pinned by
/// a host test even though every caller (`git-vista/src/api.rs`) is
/// wasm-only.
pub fn timeout_error_text() -> String {
    "The request exceeded the time this client waits for an answer, so it \
     gave up on this attempt — that does not mean it failed. The server may \
     still be working (a slow git hook or a large operation can run past \
     that time budget). Check the activity feed for what actually happened \
     before retrying: retrying a commit that already landed creates a \
     second commit."
        .to_string()
}

/// The message `git-vista`'s `api.rs::refuse_if_offline()` guard shows when
/// `navigator.onLine` reports the device is offline (M2.22a, #241).
///
/// Deliberately attributes itself to the device's own report, not to the
/// server or the tunnel: `navigator.onLine` reflects only the network
/// *adapter*, and on this deployment the SSH tunnel is what actually drops —
/// the adapter can read "up" while the tunnel is dead underneath it. Wording
/// this as "the server is unreachable" (like [`network_error_text`] above, for
/// a request that actually went out and failed) would be a claim this signal
/// cannot back up, since it never touched the network at all. Kept as a pure
/// function, like `network_error_text`, so the exact wording is pinned by a
/// host test even though the guard that shows it (`api.rs`) is wasm-only and
/// cannot itself run under `cargo test --workspace`.
pub fn offline_refusal_text() -> String {
    "Your device reports it is offline. Reconnect to the network, then try again.".to_string()
}

/// The persistent offline banner's wording (M2.22b, #242), shown while the
/// browser's connectivity signal reports offline.
///
/// Same attribution rule as [`offline_refusal_text`] above, and for the same
/// reason: `navigator.onLine` speaks for the device's network adapter only, so
/// the banner must not claim anything about the server. It also names the
/// *consequence* ("write actions are hidden" — the controls M2.22b removes
/// from the menu, picker, topbar, and Activity panel) and the *recovery* as a
/// user action ("reconnect", the refusal text's own verb), because a bar that
/// only says "offline" leaves the user to guess why Commit just vanished.
/// Plain words over idiom on purpose: this is heard once through VoiceOver
/// with no scan-back. Pure text so the wording is pinned by a host test; the
/// banner view that shows it is wasm-only.
pub fn offline_banner_text() -> String {
    "This device reports it is offline. Write actions are hidden until you reconnect to the \
     network."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_names_what_happened_not_a_cause_it_cannot_see() {
        // Mutation tried: put "SSH tunnel" back in the message — this
        // assertion catches it, since a single-user localhost deployment has
        // no tunnel and the client cannot see one either way.
        let msg = timeout_error_text();
        assert!(
            !msg.to_lowercase().contains("tunnel"),
            "must not guess at a cause the client has no way to observe — {msg}"
        );
        assert!(
            msg.to_lowercase().contains("time"),
            "must say what actually happened: the wait ran out — {msg}"
        );
    }

    #[test]
    fn timeout_says_the_server_may_still_be_working() {
        let msg = timeout_error_text();
        assert!(
            msg.contains("may still be working"),
            "must not claim the request failed — with_deadline drops the \
             client-side wait, not the server-side work — {msg}"
        );
        assert!(
            msg.to_lowercase().contains("git hook") || msg.to_lowercase().contains("large"),
            "must name a real reason a request can outlast the wait — {msg}"
        );
    }

    #[test]
    fn timeout_tells_the_user_to_check_before_retrying() {
        let msg = timeout_error_text();
        assert!(
            msg.to_lowercase().contains("activity"),
            "must point at the activity feed as the way to find out what \
             really happened — {msg}"
        );
        assert!(
            msg.to_lowercase().contains("second commit")
                || msg.to_lowercase().contains("duplicate"),
            "must name the concrete cost of retrying blind — {msg}"
        );
    }

    #[test]
    fn offline_refusal_names_the_devices_own_report_not_the_server() {
        let msg = offline_refusal_text();
        assert!(
            msg.contains("device reports it is offline"),
            "must attribute itself to the device's own signal — {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("server"),
            "must not claim anything about server reachability, which this \
             signal cannot back up — {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("unreachable"),
            "must not overclaim reachability — {msg}"
        );
        assert!(
            msg.contains("try again"),
            "actionable: suggests a retry — {msg}"
        );
    }

    #[test]
    fn the_banner_names_the_device_the_consequence_and_the_recovery() {
        let msg = offline_banner_text();
        assert!(
            msg.contains("device reports it is offline"),
            "must attribute itself to the device's own signal, like the \
             refusal text — {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("server") && !msg.to_lowercase().contains("unreachable"),
            "must not claim server reachability, which navigator.onLine \
             cannot back up — {msg}"
        );
        assert!(
            msg.contains("Write actions are hidden"),
            "must say what changed in the UI, in plain words (no idiom — this \
             is heard once via VoiceOver), so the vanished controls are \
             explained — {msg}"
        );
        assert!(
            msg.contains("reconnect"),
            "must name the recovery as a user action, the refusal text's own \
             verb — {msg}"
        );
    }

    #[test]
    fn the_message_says_what_to_check_and_keeps_the_raw_error() {
        let msg = network_error_text("TypeError: Load failed");
        assert!(msg.contains("Couldn't reach the git-vista server"), "{msg}");
        assert!(msg.contains("(TypeError: Load failed)"), "{msg}");
        assert!(msg.contains("gv"), "actionable: names the launcher — {msg}");
        assert!(
            msg.contains("try again"),
            "actionable: suggests a retry — {msg}"
        );
    }

    #[test]
    fn an_empty_raw_error_leaves_no_dangling_parentheses() {
        let msg = network_error_text("  ");
        assert!(!msg.contains('('), "{msg}");
        assert!(
            msg.starts_with("Couldn't reach the git-vista server."),
            "{msg}"
        );
    }
}
