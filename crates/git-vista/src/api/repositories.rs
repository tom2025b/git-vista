//! Repository catalog and lifecycle endpoints — `GET /api/catalog`,
//! `POST /api/select`, `POST /api/rescan`, `POST /api/delete-clone`,
//! `POST /api/reset-test-repo`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_protocol::{DeleteCloneRequest, RepoMode, RepositoryDescriptor, SelectRequest};

use super::{
    network_error, refuse_if_lan_view, refuse_if_offline, refuse_if_visualize, req_get,
    response_error, user_facing_error, write_empty, write_json,
};

/// The servable repositories (`GET /api/catalog`) — M1.03 built the endpoint,
/// the repo picker finally consumes it. Cache-busted like every live read: the
/// catalog changes at runtime (clones, rescans).
pub async fn fetch_catalog() -> Result<Vec<RepositoryDescriptor>, String> {
    let url = format!("/api/catalog?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<RepositoryDescriptor>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Make `worktree` the current repo in `mode` (`POST /api/select`, ADR 0007).
/// A forged/unknown id comes back 404 from the fail-closed catalog; the picker
/// shows the server's reason.
pub async fn select_request(worktree: &str, mode: RepoMode) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_lan_view()?;
    let body = SelectRequest {
        worktree: worktree.to_string(),
        mode,
    };
    let (resp, _key) = write_json("/api/select", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(response_error(resp).await)
    }
}

/// Re-scan the configured repo root (`POST /api/rescan`, ADR 0009). `Ok` carries
/// the server's one-line summary for the picker to show.
pub async fn rescan_request() -> Result<String, String> {
    refuse_if_offline()?;
    refuse_if_lan_view()?;
    let (resp, _key) = write_empty("/api/rescan").await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(response_error(resp).await)
    }
}

/// Delete a persistent clone by id (`POST /api/delete-clone`, ADR 0008). `Ok`
/// carries the server's confirmation line for the picker; refusals (not a
/// clone, currently open, unknown id) come back as `Err` with the reason.
pub async fn delete_clone_request(worktree: &str) -> Result<String, String> {
    refuse_if_offline()?;
    refuse_if_lan_view()?;
    let body = DeleteCloneRequest {
        worktree: worktree.to_string(),
    };
    let (resp, _key) = write_json("/api/delete-clone", &body).await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(response_error(resp).await)
    }
}

/// Ask the backend to reset a seeded *test repo* to its recorded state
/// (`POST /api/reset-test-repo`). Only offered when the graph said
/// `resettable` (the repo was opted in with `gv --seed`). `Ok` carries the
/// server's summary line ("… 2 branches restored, 1 deleted, HEAD → ‘main’");
/// a non-2xx body is the server's reason (not a test repo, corrupt seed, or
/// the exact git step that refused), returned as `Err` for the dialog to show.
///
/// **M2.22a decision (#241):** this function was write-shaped but not in that
/// issue's enumerated list of 11 write functions — flagged there as an open
/// question rather than silently included or excluded. Decided **in**: it is
/// a real `POST` that mutates the repo (restores/deletes branches, moves
/// HEAD) over the exact same socket as every other write here, so it is
/// exposed to the exact same failure this guard exists to prevent — a write
/// going out and hanging/dying on a dropped SSH tunnel while the browser
/// already knew it had no network. "Test-repo-only" describes when the UI
/// *offers* this action (`resettable` graphs only, gated by `gv --seed`), not
/// whether the write itself is safe to attempt while offline; those are
/// independent facts, and only the second one is this guard's business.
pub async fn reset_test_repo_request() -> Result<String, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_empty("/api/reset-test-repo").await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(user_facing_error("/api/reset-test-repo", resp).await)
    }
}
