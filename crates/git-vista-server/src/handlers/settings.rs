//! `GET`/`POST /api/settings/token` (M13.03, #584): the one settings surface
//! for the GitHub token #582/#583/#586 already know how to use.
//!
//! Deliberately thin: every decision that matters — precedence, masking, what
//! counts as blank, which tier a save targets — already lives in
//! [`crate::token_store`], most of it host-tested there since #583. This file
//! is only the HTTP plumbing: decode the body, call the store, map the
//! outcome to a status code.
//!
//! Both routes are registered `full_routes`-only in `main.rs`, never on the
//! LAN listener — ADR 0005's reasoning for every write/select/clone endpoint
//! applies here at least as strongly: a LAN viewer has no legitimate reason
//! to learn whether the operator has configured a GitHub token, masked or
//! not. See `route_authz.rs` for the explicit classification.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{SetTokenRequest, TokenStatus};

use crate::token_store::{store_token, token_status, StoreTokenError};

/// Whether a GitHub token is configured right now, and which tier answered —
/// resolved fresh on every call through the same [`crate::token_store::resolve_token`]
/// the credential helper itself uses, never a cached answer or a
/// keyring-specific existence check. A settings surface that only checked
/// "did my save happen" rather than "what is actually live" would tell the
/// user their token is configured on the strength of a keyring entry an
/// unrelated D-Bus outage has since made unreadable.
pub(crate) async fn get_token_status() -> Json<TokenStatus> {
    Json(token_status())
}

/// Save a token to the OS keyring (`POST /api/settings/token`). Returns the
/// freshly-resolved status on success — the same shape a `GET` would return —
/// so the client never has to guess whether its own optimistic update
/// matches what the server actually persisted.
pub(crate) async fn set_token(
    Json(req): Json<SetTokenRequest>,
) -> Result<Json<TokenStatus>, (StatusCode, String)> {
    store_token(&req.token).map_err(|e| store_error_response(&e))?;
    Ok(Json(token_status()))
}

/// Pure: maps a [`StoreTokenError`] to the client-facing status and message.
/// Split out from [`set_token`] so this mapping — the part a mutation could
/// silently invert, such as reporting a client error for a server-side
/// keyring failure — is host-tested directly rather than only reachable
/// through an HTTP round trip.
fn store_error_response(err: &StoreTokenError) -> (StatusCode, String) {
    match err {
        StoreTokenError::Blank => (
            StatusCode::BAD_REQUEST,
            "Token cannot be blank.".to_string(),
        ),
        StoreTokenError::Keyring(reason) => (
            StatusCode::BAD_GATEWAY,
            format!("Couldn't save to the OS keyring: {reason}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_token_is_a_client_error_not_a_server_one() {
        let (status, message) = store_error_response(&StoreTokenError::Blank);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(message, "Token cannot be blank.");
    }

    #[test]
    fn a_keyring_failure_is_a_server_error_naming_the_backend() {
        let (status, message) =
            store_error_response(&StoreTokenError::Keyring("no storage access".to_string()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(message.contains("keyring"));
        assert!(message.contains("no storage access"));
    }

    /// The two arms must not be swapped — a keyring failure reported as a
    /// client error would read as "you typed something wrong," and a blank
    /// value reported as a server failure would send the user looking for a
    /// D-Bus problem that does not exist.
    #[test]
    fn the_two_error_kinds_map_to_different_status_codes() {
        let (blank_status, _) = store_error_response(&StoreTokenError::Blank);
        let (keyring_status, _) = store_error_response(&StoreTokenError::Keyring("x".to_string()));
        assert_ne!(blank_status, keyring_status);
    }
}
