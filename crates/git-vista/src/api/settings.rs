//! The settings surface — `GET`/`POST /api/settings/token` (M13.03, #584).

use git_vista_protocol::{SetTokenRequest, TokenStatus};

use super::{network_error, refuse_if_offline, req_get, response_error, write_json};

/// Whether a GitHub token is configured, and which tier answered — never the
/// value itself. The server-side `TokenStatus` DTO structurally cannot carry
/// it (see `git-vista-server::token_store::token_status_of`'s own doc), so
/// there is nothing this function could leak even if it wanted to.
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
