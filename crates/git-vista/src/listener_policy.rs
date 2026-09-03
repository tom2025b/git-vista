//! Pure client policy for a listener's declared capability profile.
//!
//! `picker.rs`, `api.rs`, and the preview panel are wasm-only.  The decisions
//! they need are not: whether a repository row may be actionable, what a 405
//! means for a write the client knows it sent with the correct method, and
//! whether failed preview copy may promise an operation is available.  Those
//! decisions live here so the native `git-vista-ui` test binary executes them;
//! the wasm modules only arrange their answers in the DOM.

use git_vista_protocol::ListenerProfile;

/// What the repository picker may offer for one declared listener profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositorySelection {
    /// A row may open the Visualize/Active choice and ultimately call
    /// `POST /api/select`.
    Offered,
    /// The row remains visible as catalog information, but it is not an action.
    Unavailable { notice: &'static str },
}

impl RepositorySelection {
    pub const fn is_offered(self) -> bool {
        matches!(self, Self::Offered)
    }

    pub const fn notice(self) -> Option<&'static str> {
        match self {
            Self::Offered => None,
            Self::Unavailable { notice } => Some(notice),
        }
    }
}

/// Derive repository-picker behaviour from the profile the server declared.
pub const fn repository_selection(profile: ListenerProfile) -> RepositorySelection {
    match profile {
        ListenerProfile::Full => RepositorySelection::Offered,
        ListenerProfile::ReadOnly => RepositorySelection::Unavailable {
            notice: "Read-only LAN view — open the loopback link to switch repositories.",
        },
    }
}

/// Turn an ordinary HTTP response into an explicit capability refusal.
///
/// A 405 is not a fetch exception.  For these call sites the client itself
/// chose the method and route, so 405 means the current listener does not offer
/// that operation.  Other statuses retain the server's structured message.
pub const fn is_capability_refusal(status: u16) -> bool {
    status == 405
}

pub fn capability_refusal(
    status: u16,
    route: &str,
    profile: Option<ListenerProfile>,
) -> Option<String> {
    if !is_capability_refusal(status) {
        return None;
    }
    Some(match profile {
        Some(ListenerProfile::ReadOnly) => format!(
            "The read-only LAN listener does not offer {route}, so this operation is unavailable \
             here. Open the loopback link and try again."
        ),
        Some(ListenerProfile::Full) => format!(
            "The full listener returned Method Not Allowed for {route}, so this operation is \
             unavailable. Reload git-vista before trying again."
        ),
        None => format!(
            "The current listener returned Method Not Allowed for {route}, so this operation is \
             unavailable. Reload git-vista before trying again."
        ),
    })
}

/// Copy shown while the two-request preview round trip is still unresolved.
/// It makes no availability claim because the first request may itself be
/// refused by the listener.
pub const fn preview_pending_message() -> &'static str {
    "Checking whether this listener can provide a preview…"
}

/// Copy shown after the preview round trip fails.  The reason may be a
/// capability refusal, so this function reports only the failed request and
/// never reassures past what the response established.
pub fn preview_failure_message(reason: &str) -> String {
    format!("The preview could not be fetched: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The browser modules are not compiled by the host test target. These
    // source-level seam checks are deliberately narrow: the behavioural
    // decisions above are ordinary Rust tests, while these only prove the
    // wasm glue still asks those decisions and uses their answers.
    const API_SOURCE: &str = include_str!("api.rs");
    const PICKER_SOURCE: &str = include_str!("picker.rs");
    const PREVIEW_PANEL_SOURCE: &str = include_str!("dialogs/preview_panel.rs");

    #[test]
    fn a_read_only_profile_never_offers_repository_selection_and_says_why() {
        assert_eq!(
            repository_selection(ListenerProfile::Full),
            RepositorySelection::Offered
        );

        let read_only = repository_selection(ListenerProfile::ReadOnly);
        assert!(!read_only.is_offered());
        let notice = read_only
            .notice()
            .expect("read-only must carry visible copy");
        assert!(notice.contains("Read-only LAN view"), "{notice}");
        assert!(notice.contains("loopback link"), "{notice}");
        assert!(notice.contains("switch repositories"), "{notice}");
    }

    #[test]
    fn the_picker_wires_the_policy_to_both_action_and_explanation() {
        assert!(
            PICKER_SOURCE.contains("repository_selection(data.listener_profile)"),
            "picker.rs no longer asks the host-tested listener policy"
        );
        assert!(
            PICKER_SOURCE.contains("disabled=!can_select"),
            "the repository row no longer applies the policy's disabled answer"
        );
        assert!(
            PICKER_SOURCE.contains("if can_select {"),
            "the repository row's click handler no longer defends the disabled answer"
        );
        assert!(
            PICKER_SOURCE.contains("{unavailable_notice.map"),
            "the repository row no longer renders the policy's visible explanation"
        );
    }

    #[test]
    fn only_method_not_allowed_becomes_a_listener_capability_refusal() {
        for status in [400, 401, 403, 404, 409, 422, 500] {
            assert!(!is_capability_refusal(status));
            assert_eq!(
                capability_refusal(status, "/api/select", Some(ListenerProfile::ReadOnly)),
                None
            );
        }
        assert!(is_capability_refusal(405));
        let refusal =
            capability_refusal(405, "/api/select", Some(ListenerProfile::ReadOnly)).unwrap();
        assert!(refusal.contains("read-only LAN listener"), "{refusal}");
        assert!(refusal.contains("/api/select"), "{refusal}");
        assert!(refusal.contains("unavailable here"), "{refusal}");
        assert!(refusal.contains("loopback link"), "{refusal}");

        let full = capability_refusal(405, "/api/select", Some(ListenerProfile::Full)).unwrap();
        assert!(full.contains("full listener"), "{full}");
        assert!(!full.contains("loopback link"), "{full}");
        let undeclared = capability_refusal(405, "/api/select", None).unwrap();
        assert!(undeclared.contains("current listener"), "{undeclared}");
        assert!(!undeclared.contains("loopback link"), "{undeclared}");
    }

    #[test]
    fn both_wasm_post_funnels_consult_the_host_tested_405_classifier() {
        let retrying_funnel = API_SOURCE
            .split_once("async fn send_write_with_key(")
            .expect("api.rs no longer has the shared retrying write funnel")
            .1
            .split_once("async fn send_read(")
            .expect("shared write funnel is no longer bounded by send_read")
            .0;
        assert!(
            retrying_funnel.contains("if is_capability_refusal(resp.status())"),
            "ordinary write responses no longer classify an answered 405"
        );

        let direct_post_funnel = API_SOURCE
            .split_once("async fn user_facing_error(")
            .expect("api.rs no longer has the direct-POST error funnel")
            .1;
        assert!(
            direct_post_funnel.contains("capability_refusal(status, route, listener_profile)"),
            "direct POST errors such as /api/plan no longer classify an answered 405"
        );
        assert!(
            direct_post_funnel.contains("LISTENER_PROFILE_HEADER"),
            "the 405 refusal no longer reads the profile from the answering listener"
        );
    }

    #[test]
    fn a_plan_405_can_never_be_followed_by_an_availability_promise() {
        let refusal =
            capability_refusal(405, "/api/plan", Some(ListenerProfile::ReadOnly)).unwrap();
        let rendered = preview_failure_message(&refusal);

        assert!(rendered.contains("/api/plan"), "{rendered}");
        assert!(rendered.contains("unavailable"), "{rendered}");
        assert!(!rendered.contains("still available"), "{rendered}");
        assert!(!rendered.contains("ready either way"), "{rendered}");
        assert!(!preview_pending_message().contains("ready"));
        assert!(!preview_pending_message().contains("available"));
    }

    #[test]
    fn the_failed_preview_arm_cannot_append_reassurance_behind_the_policy() {
        let failed_arm = PREVIEW_PANEL_SOURCE
            .split_once("PreviewSlot::Failed(why)")
            .expect("preview panel no longer has a failed-request arm")
            .1
            .split_once("PreviewSlot::Ready")
            .expect("failed-request arm is no longer bounded by the ready arm")
            .0;
        assert!(
            failed_arm.contains("preview_failure_message(&why)"),
            "failed preview no longer renders the host-tested failure copy"
        );
        for forbidden in ["reassurance(", "still available", "ready either way"] {
            assert!(
                !failed_arm.contains(forbidden),
                "failed preview appended forbidden reassurance {forbidden:?}: {failed_arm}"
            );
        }
    }
}
