//! Pure decisions for the settings surface (M13.03, #584): no DOM, no
//! leptos, no fetch — everything here is exercised directly by `cargo test`,
//! never only reachable through the wasm-only dialog around it.

use git_vista_protocol::TokenStatus;

/// The one sentence the dialog shows for the current status. Never renders
/// `masked`/`source` unless `configured` says there is something to name —
/// a malformed or future wire shape (`configured: true` with no `masked`)
/// degrades to a generic sentence rather than panicking on the missing
/// field or rendering `null`.
pub fn status_line(status: &TokenStatus) -> String {
    if !status.configured {
        return "Not configured — only needed for private repositories.".to_string();
    }
    match (&status.source, &status.masked) {
        (Some(source), Some(masked)) => format!("Configured via {source} ({masked})."),
        _ => "Configured.".to_string(),
    }
}

/// Whether the Save button should be enabled: a non-blank value (the same
/// rule `token_store::non_blank` applies server-side to what would
/// otherwise be written) and no save already in flight.
pub fn save_enabled(input: &str, saving: bool) -> bool {
    !saving && !input.trim().is_empty()
}

/// What the input field should hold once a save attempt settles.
///
/// Cleared on success: the value is now saved, and echoing it back into a
/// field the surface never round-trips serves no purpose — a stale draft
/// left sitting there would look like it still needs saving. Preserved
/// verbatim on failure, so a typo (or a keyring outage the user cannot fix
/// by retyping) does not cost the whole value — see [`status_line`]'s
/// sibling error text for what accompanies it.
pub fn input_after_save(result: &Result<TokenStatus, String>, previous_input: &str) -> String {
    match result {
        Ok(_) => String::new(),
        Err(_) => previous_input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(source: &str, masked: &str) -> TokenStatus {
        TokenStatus {
            configured: true,
            masked: Some(masked.to_string()),
            source: Some(source.to_string()),
        }
    }

    fn unconfigured() -> TokenStatus {
        TokenStatus {
            configured: false,
            masked: None,
            source: None,
        }
    }

    // -- status_line --

    #[test]
    fn unconfigured_names_when_it_matters() {
        assert_eq!(
            status_line(&unconfigured()),
            "Not configured — only needed for private repositories."
        );
    }

    #[test]
    fn configured_names_the_source_and_the_masked_value() {
        assert_eq!(
            status_line(&configured("OS keyring", "...wxyz")),
            "Configured via OS keyring (...wxyz)."
        );
    }

    #[test]
    fn a_configured_status_missing_the_optional_fields_does_not_panic() {
        // A malformed or future wire shape — configured with nothing to
        // name — degrades to a generic sentence rather than unwrapping a
        // `None` or rendering the literal word "null".
        let status = TokenStatus {
            configured: true,
            masked: None,
            source: None,
        };
        assert_eq!(status_line(&status), "Configured.");
    }

    #[test]
    fn unconfigured_ignores_stray_source_or_masked_fields() {
        // `configured: false` is the authority; a malformed status that
        // somehow carries a masked value alongside `false` must still read
        // as unconfigured, not leak the stray value into the sentence.
        let status = TokenStatus {
            configured: false,
            masked: Some("...leak".to_string()),
            source: Some("OS keyring".to_string()),
        };
        assert_eq!(
            status_line(&status),
            "Not configured — only needed for private repositories."
        );
    }

    // -- save_enabled --

    #[test]
    fn blank_input_never_enables_save() {
        assert!(!save_enabled("", false));
        assert!(!save_enabled("   ", false));
    }

    #[test]
    fn a_real_value_enables_save_when_not_already_saving() {
        assert!(save_enabled("a-real-token", false));
    }

    #[test]
    fn a_save_already_in_flight_disables_the_button_regardless_of_input() {
        assert!(!save_enabled("a-real-token", true));
    }

    // -- input_after_save --

    #[test]
    fn a_successful_save_clears_the_input() {
        assert_eq!(
            input_after_save(&Ok(configured("OS keyring", "...wxyz")), "typed-value"),
            ""
        );
    }

    #[test]
    fn a_failed_save_preserves_the_typed_value() {
        assert_eq!(
            input_after_save(&Err("Couldn't save.".to_string()), "typed-value"),
            "typed-value"
        );
    }
}

/// `dialogs/settings.rs` is `#[cfg(target_arch = "wasm32")]` — `cargo test`
/// never compiles it, so ADR 0115 asks either for a host test that
/// `include_str!`s it and pins its shape, or an argued `EXEMPT` entry. This
/// is the census: it proves the DOM shell actually CALLS the three
/// functions above rather than reimplementing any of their logic inline —
/// the composition site ADR 0115's own finding says a mutation proof of the
/// pure functions alone cannot see.
///
/// **What this catches:** a future edit that stops calling `status_line`,
/// `save_enabled`, or `input_after_save` from the dialog (e.g. someone
/// inlines "if configured { ... } else { ... }" directly in the view,
/// silently duplicating and un-testing the decision).
///
/// **What this does NOT catch:** the dialog calling the right function with
/// the WRONG arguments (e.g. `save_enabled(&token_input.get(), false)`
/// hardcoding `false` instead of reading `saving.get()`) — text-scanning for
/// a call site cannot see argument correctness, only presence. That class
/// of defect needs a browser spec (there is none for this dialog yet,
/// matching #670's own item-B/E precedent of naming a census's blind spot
/// explicitly rather than implying more coverage than exists).
#[cfg(test)]
mod settings_view_census {
    const SETTINGS_VIEW: &str = include_str!("../../dialogs/settings.rs");

    #[test]
    fn the_dialog_calls_status_line_rather_than_reimplementing_it() {
        assert!(
            SETTINGS_VIEW.contains("status_line(&status.get())"),
            "dialogs/settings.rs no longer calls core::status_line as an \
             explicit invocation — either it stopped calling it, or it now \
             calls it in a point-free style this census cannot see (which \
             is also what the reachability census flags: a real call site \
             looks like `status_line(x)`, not `.map(status_line)`)"
        );
    }

    #[test]
    fn the_dialog_calls_save_enabled_to_gate_the_button() {
        assert!(
            SETTINGS_VIEW.contains("save_enabled(&token_input.get(), saving.get())"),
            "dialogs/settings.rs no longer gates Save with core::save_enabled \
             using the live input and saving signals"
        );
    }

    #[test]
    fn the_dialog_applies_input_after_save_to_the_input_signal() {
        assert!(
            SETTINGS_VIEW.contains("token_input.set(input_after_save(&result, &token))"),
            "dialogs/settings.rs no longer applies core::input_after_save's \
             answer to the input field after a save settles"
        );
    }
}
