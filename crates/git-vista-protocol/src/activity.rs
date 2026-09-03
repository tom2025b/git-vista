//! Cursor-paginated activity-feed response.
//!
//! The event itself remains a `git-vista-core` domain type. This envelope is
//! generic for the same reason as the paged-history envelopes: protocol owns
//! the transport contract without depending on core, while server, browser,
//! and MCP instantiate `E` as `ActivityEvent`.

use serde::{Deserialize, Serialize};

/// One newest-first window of the folded activity feed.
///
/// `cursor: Some(_)` means more events exist after this page. Passing that
/// opaque value back resumes the same folded snapshot. `None` means this page
/// reached the end; callers never have to infer exhaustion from `events.len()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityPage<E> {
    pub events: Vec<E>,
    pub cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #562: exhaustion and truncation are different wire states even when
    /// both pages happen to contain the same number of events.
    ///
    /// Mutations: remove `cursor`, or serialize `None` as an omitted/defaulted
    /// field, and these two exact objects can no longer make that distinction.
    #[test]
    fn the_wire_distinguishes_more_events_from_the_end() {
        let more = ActivityPage {
            events: vec![1_u8, 2],
            cursor: Some("opaque".to_string()),
        };
        let end = ActivityPage {
            events: vec![1_u8, 2],
            cursor: None,
        };

        assert_eq!(
            serde_json::to_string(&more).unwrap(),
            r#"{"events":[1,2],"cursor":"opaque"}"#
        );
        assert_eq!(
            serde_json::to_string(&end).unwrap(),
            r#"{"events":[1,2],"cursor":null}"#
        );
        assert_ne!(more, end);
    }
}
