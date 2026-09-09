//! Host-tested display and stale-response decisions (ADR 0115).
use git_vista_protocol::forge::{ForgeAvailability, ForgeCapabilities, ForgePage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRollup {
    Unknown,
    NoChecks,
    Pending,
    Passing,
    Failing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRollup {
    Unknown,
    NoReviews,
    Reviewed,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Queued,
    InProgress,
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CheckSummary {
    pub name: String,
    pub state: CheckState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ReviewSummary {
    pub reviewer: String,
    pub state: ReviewState,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct PullDetails {
    pub number: u64,
    pub availability: ForgeAvailability,
    pub check_rollup: CheckRollup,
    pub review_rollup: ReviewRollup,
    pub checks: Vec<CheckSummary>,
    pub reviews: Vec<ReviewSummary>,
    pub retry_after_seconds: Option<u64>,
}

pub fn availability_line(page: &ForgePage) -> String {
    match page.availability {
        ForgeAvailability::Ready if page.pulls.is_empty() => "No open pull requests on this page.".into(),
        ForgeAvailability::Ready => "Open pull requests".into(),
        ForgeAvailability::Unsupported => "Pull request summaries are available for github.com origin remotes.".into(),
        ForgeAvailability::Unavailable => "The provider could not be reached or its response could not be read. Local Git is still available.".into(),
        ForgeAvailability::AccessUncertain => "Pull request view unavailable. No GitHub token is configured, or the provider could not grant access; local Git is still available.".into(),
        ForgeAvailability::RateLimited => format!("Provider requests are paused. Try again in {} seconds.", page.retry_after_seconds.unwrap_or(60)),
    }
}
pub fn detail_line(details: &PullDetails) -> String {
    match details.availability {
        ForgeAvailability::Ready => format!(
            "Checks: {} · Reviews: {}",
            check_rollup_label(details.check_rollup),
            review_rollup_label(details.review_rollup)
        ),
        ForgeAvailability::Unsupported => {
            "Check and review details are available for github.com origin remotes.".into()
        }
        ForgeAvailability::Unavailable => {
            "Check and review details could not be loaded. Local Git is still available.".into()
        }
        ForgeAvailability::AccessUncertain => {
            "Check and review details are unavailable. Configure a GitHub token with read access, then retry.".into()
        }
        ForgeAvailability::RateLimited => format!(
            "Provider requests are paused. Try details again in {} seconds.",
            details.retry_after_seconds.unwrap_or(60)
        ),
    }
}

fn check_rollup_label(rollup: CheckRollup) -> &'static str {
    match rollup {
        CheckRollup::Unknown => "unknown",
        CheckRollup::NoChecks => "none",
        CheckRollup::Pending => "pending",
        CheckRollup::Passing => "passing",
        CheckRollup::Failing => "failing",
    }
}

fn review_rollup_label(rollup: ReviewRollup) -> &'static str {
    match rollup {
        ReviewRollup::Unknown => "unknown",
        ReviewRollup::NoReviews => "none",
        ReviewRollup::Reviewed => "comments only",
        ReviewRollup::Approved => "approved",
        ReviewRollup::ChangesRequested => "changes requested",
    }
}

pub fn check_state_label(state: CheckState) -> &'static str {
    match state {
        CheckState::Queued => "queued",
        CheckState::InProgress => "in progress",
        CheckState::Success => "passed",
        CheckState::Failure => "failed",
        CheckState::Neutral => "neutral",
        CheckState::Cancelled => "cancelled",
        CheckState::Skipped => "skipped",
        CheckState::TimedOut => "timed out",
        CheckState::ActionRequired => "action required",
        CheckState::Stale => "stale",
        CheckState::Unknown => "unknown",
    }
}

pub fn review_state_label(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Commented => "commented",
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
    const VIEW: &str = include_str!("view.rs");
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
            checks: true,
            reviews: true,
        });
        assert_eq!(
            line,
            "Summaries: available · Checks: available · Reviews: available"
        );
    }

    #[test]
    fn detail_rollups_have_honest_user_facing_copy() {
        let details = PullDetails {
            number: 7,
            availability: ForgeAvailability::Ready,
            check_rollup: CheckRollup::Failing,
            review_rollup: ReviewRollup::ChangesRequested,
            checks: vec![],
            reviews: vec![],
            retry_after_seconds: None,
        };
        assert_eq!(
            detail_line(&details),
            "Checks: failing · Reviews: changes requested"
        );
    }

    #[test]
    fn detail_wire_shape_decodes_the_server_contract() {
        let details: PullDetails = serde_json::from_str(
            r#"{
                "number": 7,
                "availability": "ready",
                "check_rollup": "passing",
                "review_rollup": "approved",
                "checks": [{"name": "build", "state": "success"}],
                "reviews": [{"reviewer": "octocat", "state": "approved"}],
                "retry_after_seconds": null
            }"#,
        )
        .unwrap();
        assert_eq!(details.check_rollup, CheckRollup::Passing);
        assert_eq!(details.checks[0].state, CheckState::Success);
        assert_eq!(details.review_rollup, ReviewRollup::Approved);
        assert_eq!(details.reviews[0].state, ReviewState::Approved);
    }

    #[test]
    fn wasm_view_reaches_the_detail_fetch_and_host_tested_labels() {
        for seam in [
            "fetch_pull_details",
            "detail_line(&details)",
            "check_state_label(check.state)",
            "review_state_label(review.state)",
        ] {
            assert!(
                VIEW.contains(seam),
                "forge detail seam is not wired: {seam}"
            );
        }
    }
}
