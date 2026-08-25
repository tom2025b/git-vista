//! Reading the one-time bootstrap token out of a URL fragment (M1.04 #57,
//! #392).
//!
//! # Why this is its own host-compiled module
//!
//! The parse used to be inlined in `session::take_bootstrap_token`, and `mod
//! session` is `#[cfg(target_arch = "wasm32")]` — so the one decision the whole
//! sign-in path turns on ("is there a token in this fragment?") had no host
//! test and structurally could not have one. Same posture as [`crate::
//! head_notice`] and [`crate::hook_policy_disclosure`]: the decision is pure and
//! tested here, the wasm code only acts on what it returns.
//!
//! #392 gave that a second reason. Two places now ask the question — startup,
//! which redeems the token, and the `hashchange` listener, which decides
//! whether a fragment that just arrived is worth re-running sign-in for. Had
//! they each carried their own parse they could disagree, and the disagreement
//! is silent in both directions: a listener stricter than startup ignores a
//! usable token, and one looser reloads the page over a fragment startup will
//! discard.

/// The bootstrap token carried by `fragment`, or `None` when it carries none.
///
/// Accepts the fragment with or without its leading `#`, so a raw
/// `location.hash` (which includes it) and a bare query-shaped string both
/// work.
///
/// An `s=` with an **empty** value is `None`, not `Some("")`. The server would
/// refuse the empty string anyway, but the caller that matters is #392's
/// listener: `Some("")` there means reloading a signed-in tab — destroying its
/// state — over a fragment that could never have signed anyone in.
pub fn token_in_fragment(fragment: &str) -> Option<&str> {
    let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
    fragment.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "s" && !value.is_empty()).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole sign-in path turns on this parse, and until #392 it lived
    /// inside a wasm-only module where no host test could reach it.
    ///
    /// The negative half is the load-bearing one. `s=` with no value and a key
    /// that merely *starts* with `s` both look like tokens to a sloppy parse,
    /// and both would make #392's listener reload a live tab for nothing.
    ///
    /// MUTATION 1: drop the `!value.is_empty()` guard — red, `"s="` yields
    ///   `Some("")` and an empty token is treated as a real one.
    /// MUTATION 2: match `key.starts_with('s')` instead of `key == "s"` — red,
    ///   `"sort=date"` is read as a token.
    #[test]
    fn only_a_non_empty_s_parameter_is_a_token() {
        assert_eq!(token_in_fragment("s=abc"), Some("abc"));
        assert_eq!(
            token_in_fragment("#s=abc"),
            Some("abc"),
            "a raw location.hash"
        );
        assert_eq!(
            token_in_fragment("tab=diff&s=abc"),
            Some("abc"),
            "not first"
        );
        assert_eq!(token_in_fragment("s=abc&tab=diff"), Some("abc"), "not last");

        for carries_none in ["", "#", "s=", "#s=", "tab=diff", "sort=date", "s"] {
            assert_eq!(
                token_in_fragment(carries_none),
                None,
                "{carries_none:?} carries no token and must not be read as one"
            );
        }
    }
}
