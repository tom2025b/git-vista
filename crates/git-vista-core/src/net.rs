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

#[cfg(test)]
mod tests {
    use super::*;

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
