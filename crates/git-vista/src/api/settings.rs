//! The settings surface — `GET`/`POST /api/settings/token` (M13.03, #584).

use git_vista_protocol::{SetTokenRequest, TokenStatus};

use super::{network_error, refuse_if_offline, req_get, response_error, write_json};

/// Whether a GitHub token is configured, and which tier answered — never the
/// value itself. The guarantee lives in the server's only production
/// constructor for this DTO, `token_store::token_status_of`, which always
/// masks the value and is wire-tested against a real secret for every
/// resolution tier (see its own doc) — not in `TokenStatus`'s fields, which
/// are plain `Option<String>` with nothing stopping a hand-built value from
/// holding one.
pub async fn token_status_request() -> Result<TokenStatus, String> {
    let resp = req_get("/api/settings/token")
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        resp.json::<TokenStatus>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Save a new token (`POST /api/settings/token`). Returns the freshly
/// resolved status on success — the same shape a `GET` would return — so the
/// caller never has to guess whether its own optimistic update matches what
/// the server actually persisted.
pub async fn set_token_request(token: &str) -> Result<TokenStatus, String> {
    refuse_if_offline()?;
    let body = SetTokenRequest {
        token: token.to_string(),
    };
    let (resp, _key) = write_json("/api/settings/token", &body).await?;
    if resp.ok() {
        resp.json::<TokenStatus>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}
