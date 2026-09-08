//! The indicator decision runs on the host as well as in the browser.
use crate::features::status::detail::core::current_reading;
use git_vista_protocol::dto::BisectStatus;

/// A retained reply only describes the frame it was requested for.
pub fn indicator(
    loading: bool,
    reply: Option<(u64, Option<String>, Option<BisectStatus>)>,
    epoch: u64,
    repo: Option<&str>,
) -> Option<&'static str> {
    let status = current_reading(loading, reply, epoch, repo)?;
    if !status.in_progress {
        return None;
    }
    Some(if status.finished {
        "Bisect active — first bad commit found; reset to finish"
    } else {
        "Bisect active"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(in_progress: bool, finished: bool) -> BisectStatus {
        BisectStatus {
            in_progress,
            finished,
            current: None,
            started_from: None,
            bad: None,
            good: vec![],
            skipped: vec![],
            history: vec![],
        }
    }

    fn reply(
        in_progress: bool,
        finished: bool,
    ) -> Option<(u64, Option<String>, Option<BisectStatus>)> {
        Some((
            7,
            Some("repo-a".into()),
            Some(status(in_progress, finished)),
        ))
    }

    #[test]
    fn bisect_indicator_appears_from_a_read_without_an_action() {
        assert_eq!(
            indicator(false, reply(true, false), 7, Some("repo-a")),
            Some("Bisect active")
        );
        assert_eq!(
            indicator(false, reply(false, false), 7, Some("repo-a")),
            None
        );
    }

    #[test]
    fn bisect_indicator_keeps_a_finished_session_visible_until_reset() {
        assert_eq!(
            indicator(false, reply(true, true), 7, Some("repo-a")),
            Some("Bisect active — first bad commit found; reset to finish")
        );
        assert_eq!(
            indicator(false, reply(false, true), 7, Some("repo-a")),
            None
        );
    }

    #[test]
    fn bisect_indicator_refuses_old_missing_and_loading_readings() {
        assert_eq!(indicator(true, reply(true, false), 7, Some("repo-a")), None);
        assert_eq!(
            indicator(false, reply(true, false), 8, Some("repo-a")),
            None
        );
        assert_eq!(
            indicator(false, reply(true, false), 7, Some("repo-b")),
            None
        );
        assert_eq!(indicator(false, reply(true, false), 7, None), None);
        assert_eq!(indicator(false, None, 7, Some("repo-a")), None);
        assert_eq!(
            indicator(
                false,
                Some((7, Some("repo-a".into()), None)),
                7,
                Some("repo-a")
            ),
            None
        );
    }

    #[test]
    fn bisect_indicator_is_mounted_and_uses_the_host_decision() {
        let shell = include_str!("../../app/mod.rs");
        let signals = include_str!("signals.rs");
        let api = include_str!("../../api/bisect.rs");
        assert!(shell.contains("bisect::signals::indicator_view(graph, status_repo, online)"));
        assert!(signals.contains("create_local_resource("));
        assert!(signals.contains("fetch_bisect_status_for(id).await.ok()"));
        assert!(signals.contains("indicator("));
        assert!(signals.contains("resource.get()"));
        assert!(signals.contains(r#"role="status""#));
        assert!(api.contains("/api/bisect/status?t={}&repo={}"));
        assert!(api.contains("super::send_read(&url).await"));
    }
}
