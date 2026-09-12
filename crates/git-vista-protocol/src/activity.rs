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

/// An activity row's optional historical selector. Flattening preserves the
/// existing event fields for clients that do not know about as-of viewing.
/// This is response-only: tokens are never journaled or part of the fold hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityObservation<E> {
    #[serde(flatten)]
    pub event: E,
    #[serde(default)]
    pub as_of: AsOfAvailability,
}

impl<E> std::ops::Deref for ActivityObservation<E> {
    type Target = E;
    fn deref(&self) -> &E {
        &self.event
    }
}

/// Availability is a sum type: a refusal can never also carry a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AsOfAvailability {
    Available { token: String },
    Unavailable { reason: AsOfUnavailable },
}

impl Default for AsOfAvailability {
    fn default() -> Self {
        Self::Unavailable {
            reason: AsOfUnavailable::CaptureNotRecorded,
        }
    }
}

/// Why one observation cannot be mounted. Missing fields are facts we do not
/// possess, distinct from an explicitly recorded empty map or unborn HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsOfUnavailable {
    CaptureNotRecorded,
    CaptureFailed,
    IncompleteCapture,
    TruncatedCapture,
    ShallowStateNotRecorded,
    ShallowRepository,
    UnreadableHead,
    UnresolvableHead,
    InvalidCapture,
    MissingCommitObjects,
}

impl std::fmt::Display for AsOfUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CaptureNotRecorded => "No complete journal observation was recorded for this activity.",
            Self::CaptureFailed => "The ref capture failed.",
            Self::IncompleteCapture => "This capture did not record HEAD, tags, and remote refs.",
            Self::TruncatedCapture => "This capture contains a truncated ref map.",
            Self::ShallowStateNotRecorded => "Shallow status was not recorded for this capture.",
            Self::ShallowRepository => "Historical shallow boundaries are not recorded; this shallow history cannot be replayed.",
            Self::UnreadableHead => "HEAD could not be read at capture time.",
            Self::UnresolvableHead => "HEAD did not resolve at capture time.",
            Self::InvalidCapture => "The captured refs or HEAD are inconsistent or invalid.",
            Self::MissingCommitObjects => "A commit object required by this capture is missing or unreadable.",
        })
    }
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
