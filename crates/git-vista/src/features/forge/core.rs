//! Host-tested display and stale-response decisions (ADR 0115).
use git_vista_protocol::forge::{ForgeAvailability, ForgeCapabilities, ForgePage};

pub fn availability_line(page: &ForgePage) -> String {
    match page.availability {
        ForgeAvailability::Ready if page.pulls.is_empty() => "No open pull requests on this page.".into(),
        ForgeAvailability::Ready => "Open pull requests".into(),
        ForgeAvailability::Unsupported => "Pull request summaries are available for github.com origin remotes.".into(),
        ForgeAvailability::Unavailable => "The provider could not be reached or its response could not be read. Local Git is still available.".into(),
        ForgeAvailability::AccessUncertain => "The provider could not grant access. This may be a permission, token, repository, or rate-limit issue; the response does not establish which.".into(),
        ForgeAvailability::RateLimited => format!("Provider requests are paused. Try again in {} seconds.", page.retry_after_seconds.unwrap_or(60)),
    }
}
pub fn capability_line(c: &ForgeCapabilities) -> String {
    let state = |available| {
        if available {
            "available"
        } else {
            "unavailable"
        }
    };
    format!(
        "Summaries: {} · Checks: {} · Reviews: {}",
        state(c.summaries),
        state(c.checks),
        state(c.reviews)
    )
}
/// Page selection belongs to one repository. Decide before issuing a request,
/// so changing repositories cannot briefly request the old page in the new one.
pub fn page_for(previous_repo: Option<&str>, current_repo: Option<&str>, page: u32) -> u32 {
    if previous_repo == current_repo {
        page
    } else {
        1
    }
}

/// A closed panel, new request, or changed repository discards an old answer.
pub fn accepts_response(
    open: bool,
    requested_epoch: u64,
    current_epoch: u64,
    requested_repo: &str,
    current_repo: Option<&str>,
) -> bool {
    open && requested_epoch == current_epoch && current_repo == Some(requested_repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn close_selection_change_and_new_request_each_refuse_stale_private_data() {
        assert!(accepts_response(true, 2, 2, "repo-a", Some("repo-a")));
        assert!(!accepts_response(false, 2, 2, "repo-a", Some("repo-a")));
        assert!(!accepts_response(true, 1, 2, "repo-a", Some("repo-a")));
        assert!(!accepts_response(true, 2, 2, "repo-a", Some("repo-b")));
        assert!(!accepts_response(true, 2, 2, "repo-a", None));
    }
    #[test]
    fn repository_change_resets_before_requesting_even_from_a_pending_second_page() {
        assert_eq!(page_for(Some("a"), Some("b"), 2), 1);
        assert_eq!(page_for(Some("b"), Some("b"), 2), 2);
        assert_eq!(page_for(Some("a"), None, 2), 1);
        assert!(!accepts_response(true, 1, 2, "a", Some("b")));
        assert!(accepts_response(true, 2, 2, "b", Some("b")));
    }
    #[test]
    fn unsupported_details_are_explicit_not_success_states() {
        let line = capability_line(&ForgeCapabilities {
            summaries: true,
            checks: false,
            reviews: false,
        });
        assert_eq!(
            line,
            "Summaries: available · Checks: unavailable · Reviews: unavailable"
        );
    }
}
