//! `GET`/`POST /api/settings/token` (M13.03, #584): the one settings surface
//! for the GitHub token #582/#583/#586 already know how to use.
//!
//! Deliberately thin: every decision that matters — precedence, masking, what
//! counts as blank, which tier a save targets, and the bounded request policy
//! for a locked keyring — lives in [`crate::token_store`]. This file is the
//! HTTP plumbing and the async/synchronous boundary: keyring writes run on
//! Tokio's blocking pool, never on a runtime worker (#692).
//!
//! Both routes are registered `full_routes`-only in `main.rs`, never on the
//! LAN listener — ADR 0005's reasoning for every write/select/clone endpoint
//! applies here at least as strongly: a LAN viewer has no legitimate reason
//! to learn whether the operator has configured a GitHub token, masked or
//! not. See `route_authz.rs` for the explicit classification.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{SetTokenRequest, TokenStatus};

use crate::token_store::{RequestTokenResolver, StoreTokenError};

/// Report token status under #692's bounded request policy. The keyring part
/// is the masked startup/post-write snapshot; this call has no path to the
/// synchronous keyring API. When that snapshot is absent, the environment
/// and fallback file are still resolved lazily and live.
pub(crate) async fn get_token_status(
    Extension(resolver): Extension<Arc<RequestTokenResolver>>,
) -> Json<TokenStatus> {
    Json(resolver.status())
}

/// Save a token to the OS keyring (`POST /api/settings/token`). The blocking
/// platform call runs on Tokio's blocking pool. On success the response is
/// constructed from the value just persisted and the masked request snapshot
/// is updated; there is deliberately no immediate D-Bus read-back.
pub(crate) async fn set_token(
    Extension(resolver): Extension<Arc<RequestTokenResolver>>,
    Json(req): Json<SetTokenRequest>,
) -> Result<Json<TokenStatus>, (StatusCode, String)> {
    let status = store_on_blocking_pool(req.token, resolver, crate::token_store::store_token)
        .await
        .map_err(|error| match error {
            BlockingStoreError::Store(error) => store_error_response(&error),
            BlockingStoreError::Worker(reason) => {
                eprintln!("git-vista: settings keyring worker failed: {reason}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Couldn't save to the OS keyring: the blocking worker failed.".to_string(),
                )
            }
        })?;
    Ok(Json(status))
}

enum BlockingStoreError {
    Store(StoreTokenError),
    Worker(String),
}

/// Isolated so a host test can prove the keyring write executes on a blocking
/// thread rather than merely inspecting `set_token` for a `spawn_blocking`
/// call.
async fn store_on_blocking_pool<F>(
    token: String,
    resolver: Arc<RequestTokenResolver>,
    store: F,
) -> Result<TokenStatus, BlockingStoreError>
where
    F: FnOnce(&str) -> Result<(), StoreTokenError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        store(&token).map_err(BlockingStoreError::Store)?;
        Ok(resolver.record_successful_store(&token))
    })
    .await
    .map_err(|error| BlockingStoreError::Worker(error.to_string()))?
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

    #[tokio::test(flavor = "current_thread")]
    async fn keyring_store_work_runs_off_the_async_runtime_thread() {
        let runtime_thread = std::thread::current().id();
        let observed = Arc::new(std::sync::Mutex::new(None));
        let observed_by_store = observed.clone();
        let resolver = Arc::new(RequestTokenResolver::without_keyring());

        let result = store_on_blocking_pool("saved-token".to_string(), resolver, move |_| {
            *observed_by_store.lock().unwrap() = Some(std::thread::current().id());
            Ok(())
        })
        .await;

        assert!(matches!(
            result,
            Ok(TokenStatus {
                configured: true,
                ..
            })
        ));
        assert_ne!(
            *observed.lock().unwrap(),
            Some(runtime_thread),
            "the synchronous keyring writer ran on Tokio's runtime thread"
        );
    }
}
