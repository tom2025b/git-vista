//! The structured API error envelope, its machine-readable code, and the
//! per-request correlation id.
//!
//! Every `/api/*` endpoint returns an [`ApiError`] on failure — one consistent
//! shape across the whole surface (the server guarantees this with one response
//! layer, so a handler that still returns a plain status + string is wrapped into
//! this envelope on the way out). Success responses are *not* wrapped; only their
//! headers carry the protocol version and request id. A client switches on
//! [`ApiError::code`](ApiErrorBody::code) — never the human message — and quotes
//! [`ApiError::request_id`] when reporting a failure.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::version::PROTOCOL_VERSION;

/// An opaque per-request correlation id, minted by the server for every `/api/*`
/// request, echoed in the [`REQUEST_ID_HEADER`](crate::version::REQUEST_ID_HEADER)
/// response header and inside any [`ApiError`]. A user who reports "it failed with
/// id 000000000000002a" pins the exact server log line. Opaque: clients
/// round-trip it, they never parse it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    /// Wrap a request-id string (the server mints these; a client only ever
    /// deserialises one).
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stable, machine-readable classification for an API error, carried in the
/// [`ApiError`] envelope alongside the human message. Clients react to the
/// `code`, never the message text: the three `*_protocol*` codes drive the
/// "Update Required" screen; the rest map to inline error UI.
///
/// The wire form is a fixed `snake_case` string (`"protocol_incompatible"`, …)
/// and is **part of the versioned contract** — renaming a variant's serialised
/// form is a breaking protocol change, not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A non-`/api/protocol` request arrived without the protocol header.
    MissingProtocolHeader,
    /// The protocol header was present but not a base-10 version number.
    InvalidProtocolHeader,
    /// The client's protocol version is outside the server's accepted window.
    ProtocolIncompatible,
    /// The request was malformed (bad path param, or an unexpected/invalid body).
    BadRequest,
    /// The addressed thing (commit, file, …) does not exist.
    NotFound,
    /// The operation is not permitted in the current state.
    Forbidden,
    /// A write was attempted against a read-only clone.
    ReadOnly,
    /// An invoked `git` command failed; the message carries git's own stderr.
    GitFailed,
    /// An unexpected server-side failure (including a caught handler panic).
    Internal,
}

impl ErrorCode {
    /// The HTTP status this code is sent with. Returned as a bare `u16` so this
    /// pure crate needs no `http`/Axum dependency; the server maps it to its own
    /// `StatusCode`. All three protocol-mismatch codes use `426 Upgrade
    /// Required`, which is exactly the "your client and this server don't agree,
    /// reload to update" signal the frontend keys the Update-Required screen on.
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::MissingProtocolHeader
            | ErrorCode::InvalidProtocolHeader
            | ErrorCode::ProtocolIncompatible => 426,
            ErrorCode::BadRequest => 400,
            ErrorCode::NotFound => 404,
            ErrorCode::Forbidden | ErrorCode::ReadOnly => 403,
            ErrorCode::GitFailed | ErrorCode::Internal => 500,
        }
    }

    /// Best-effort classification of an HTTP status into a code, for the server's
    /// one response layer that wraps a handler's still-plain `(StatusCode,
    /// String)` return into the [`ApiError`] envelope. Anything unrecognised
    /// falls back to [`Internal`](ErrorCode::Internal).
    pub fn from_status(status: u16) -> Self {
        match status {
            403 => ErrorCode::Forbidden,
            404 => ErrorCode::NotFound,
            426 => ErrorCode::ProtocolIncompatible,
            // Every other 4xx — including the 422 an unprocessable body produces
            // (e.g. a rejected unknown field) — is a client error, not a server one.
            s if (400..500).contains(&s) => ErrorCode::BadRequest,
            _ => ErrorCode::Internal,
        }
    }

    /// True for the three codes that mean "client and server protocols don't
    /// agree" — the set the frontend treats as "show Update Required".
    pub fn is_protocol_mismatch(self) -> bool {
        matches!(
            self,
            ErrorCode::MissingProtocolHeader
                | ErrorCode::InvalidProtocolHeader
                | ErrorCode::ProtocolIncompatible
        )
    }
}

/// The `error` object inside an [`ApiError`]: the machine code plus a
/// human-readable message (git's own stderr, or a git-vista explanation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorBody {
    /// What kind of error this is — switch on this, not the message.
    pub code: ErrorCode,
    /// A human-readable explanation, safe to show the user.
    pub message: String,
}

/// The structured error envelope every `/api/*` endpoint returns on failure.
///
/// It carries the machine [`code`](ApiErrorBody::code) and message, the
/// [`request_id`](ApiError::request_id) for correlating with server logs, and the
/// server's [`protocol`](ApiError::protocol) version — so even a failed response
/// still tells a client which protocol it was talking to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// The error itself (code + message).
    pub error: ApiErrorBody,
    /// The id of the request that failed.
    pub request_id: RequestId,
    /// The server's protocol version at the time of the error.
    pub protocol: u32,
}

impl ApiError {
    /// Build an envelope for `code`/`message` against a given request id. The
    /// protocol version is filled from the build's [`PROTOCOL_VERSION`].
    pub fn new(code: ErrorCode, message: impl Into<String>, request_id: RequestId) -> Self {
        Self {
            error: ApiErrorBody {
                code,
                message: message.into(),
            },
            request_id,
            protocol: PROTOCOL_VERSION,
        }
    }

    /// The HTTP status this error should be sent with (from its code).
    pub fn http_status(&self) -> u16 {
        self.error.code.http_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_wire_forms_are_snake_case_and_stable() {
        // These strings are the contract — a client matches on them.
        assert_eq!(
            serde_json::to_string(&ErrorCode::ProtocolIncompatible).unwrap(),
            "\"protocol_incompatible\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::MissingProtocolHeader).unwrap(),
            "\"missing_protocol_header\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::GitFailed).unwrap(),
            "\"git_failed\""
        );
    }

    #[test]
    fn status_mapping_is_what_the_server_sends() {
        assert_eq!(ErrorCode::ProtocolIncompatible.http_status(), 426);
        assert_eq!(ErrorCode::MissingProtocolHeader.http_status(), 426);
        assert_eq!(ErrorCode::BadRequest.http_status(), 400);
        assert_eq!(ErrorCode::NotFound.http_status(), 404);
        assert_eq!(ErrorCode::Forbidden.http_status(), 403);
        assert_eq!(ErrorCode::ReadOnly.http_status(), 403);
        assert_eq!(ErrorCode::GitFailed.http_status(), 500);
        assert_eq!(ErrorCode::Internal.http_status(), 500);
    }

    #[test]
    fn from_status_classifies_the_handler_statuses() {
        assert_eq!(ErrorCode::from_status(400), ErrorCode::BadRequest);
        assert_eq!(ErrorCode::from_status(403), ErrorCode::Forbidden);
        assert_eq!(ErrorCode::from_status(404), ErrorCode::NotFound);
        assert_eq!(ErrorCode::from_status(426), ErrorCode::ProtocolIncompatible);
        // A 422 (unprocessable body) and any other 4xx are client errors.
        assert_eq!(ErrorCode::from_status(422), ErrorCode::BadRequest);
        assert_eq!(ErrorCode::from_status(418), ErrorCode::BadRequest);
        assert_eq!(ErrorCode::from_status(500), ErrorCode::Internal);
        assert_eq!(ErrorCode::from_status(503), ErrorCode::Internal);
    }

    #[test]
    fn only_the_protocol_codes_are_flagged_as_mismatch() {
        assert!(ErrorCode::MissingProtocolHeader.is_protocol_mismatch());
        assert!(ErrorCode::InvalidProtocolHeader.is_protocol_mismatch());
        assert!(ErrorCode::ProtocolIncompatible.is_protocol_mismatch());
        assert!(!ErrorCode::BadRequest.is_protocol_mismatch());
        assert!(!ErrorCode::GitFailed.is_protocol_mismatch());
    }

    #[test]
    fn api_error_roundtrips_through_json() {
        let err = ApiError::new(
            ErrorCode::GitFailed,
            "fatal: not a git repository",
            RequestId::new("00000000000000ff"),
        );
        let json = serde_json::to_string(&err).unwrap();
        let back: ApiError = serde_json::from_str(&json).unwrap();
        assert_eq!(err, back);
        assert_eq!(back.error.code, ErrorCode::GitFailed);
        assert_eq!(back.request_id.as_str(), "00000000000000ff");
        assert_eq!(back.protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn request_id_is_a_transparent_string() {
        let id = RequestId::new("abc");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"abc\"");
        assert_eq!(id.to_string(), "abc");
    }
}
