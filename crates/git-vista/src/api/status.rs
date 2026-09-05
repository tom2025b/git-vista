//! Working-tree status endpoints — `GET /api/status`, `GET /api/status/v2`,
//! `POST /api/discard-tracked-paths`, `POST /api/delete-untracked-paths`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_core::status::RepoStatus;
use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{RemoveWorktreeRequest, WorktreePathsRequest, WorktreeStatus};

use super::{
    network_error, receipt, refuse_if_offline, refuse_if_visualize, req_get, response_error,
    send_read, send_write_with_key, WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// Fetch the live working-tree status (`GET /api/status`) — branch, ahead/
/// behind, and the dirty-file lists — for the topbar chip and the Activity
/// panel's status section. Resolved fresh server-side per request and cache-
/// busted like the other live reads, since it changes with every edit.
pub async fn fetch_status() -> Result<RepoStatus, String> {
    let url = format!("/api/status?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<RepoStatus>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch the generation-tagged working-tree status (`GET /api/status/v2`,
/// #68c) — the per-path [`WorktreeStatus`] the discard/delete menu items need
/// to name exactly which files each operation would touch (M2.18b, #220).
///
/// Additive alongside [`fetch_status`], which serves the topbar chip's
/// coarser v1 shape and is untouched — migrating that consumer is 68d's job,
/// not this one's.
///
/// Routed through [`send_read`] (#218) rather than a bare `req_get`, for the
/// reason that function documents: a read with no timeout over a dropped SSH
/// tunnel never settles, and this one gates a destructive confirmation.
pub async fn fetch_worktree_status() -> Result<WorktreeStatus, String> {
    let url = format!("/api/status/v2?t={}", js_sys::Date::now());
    let resp = send_read(&url).await.map_err(|e| e.to_string())?;
    if resp.ok() {
        resp.json::<WorktreeStatus>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Ask the backend to discard uncommitted changes to `paths`
/// (`POST /api/discard-tracked-paths`, M2.18a/#219, wired by M2.18b/#220).
///
/// Every path must be tracked-and-dirty *at execution time*: the server
/// re-derives that from a fresh `git status` immediately before running git
/// and refuses the whole batch — never partially applies — if any path has
/// since drifted. That 409 is a normal answer here, not a bug, and its text
/// names the path.
///
/// The body is [`WorktreePathsRequest`], the server's own DTO, so the
/// `#[serde(deny_unknown_fields)]` on it cannot be violated by a stray field
/// invented on this side.
pub async fn discard_tracked_paths_request(
    paths: Vec<String>,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&WorktreePathsRequest { paths }).map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key(
        "/api/discard-tracked-paths",
        Some(json),
        key,
        REQUEST_TIMEOUT_MS,
    )
    .await?;
    Ok(receipt(resp).await)
}

/// Ask the backend to delete untracked `paths` outright
/// (`POST /api/delete-untracked-paths`).
///
/// A **separate function** from [`discard_tracked_paths_request`], mirroring
/// the two separate `GitOperation` variants behind them — never one call
/// parameterised by a bool (#71). The two requests are the same shape and
/// different operations, and the one with no way back does not share a code
/// path with the one that has a qualified recovery story.
///
/// Retries are safe for the same reason every other write's are: the
/// idempotency key is minted by the caller and replayed rather than re-run.
pub async fn delete_untracked_paths_request(
    paths: Vec<String>,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&WorktreePathsRequest { paths }).map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key(
        "/api/delete-untracked-paths",
        Some(json),
        key,
        REQUEST_TIMEOUT_MS,
    )
    .await?;
    Ok(receipt(resp).await)
}

/// Close a linked sibling worktree, addressed by its opaque census id
/// (`POST /api/remove-worktree`, M11.05, #550).
///
/// Carries only `id` — never a path, and never the display name the drawer
/// showed: the server resolves `id` to a real path itself, via a fresh
/// census, immediately before acting (see
/// [`GitOperation::RemoveWorktree`](git_vista_protocol::GitOperation::RemoveWorktree)'s
/// doc comment for the compare-and-swap this reaches into). `id` comes
/// straight from a census this client already read, so a validation failure
/// here would be this client's own bug, not a user mistake — mapped to a
/// string like every other client-side error in this module rather than
/// unwrapped, so a malformed id refuses the request instead of panicking the
/// tab.
pub async fn remove_worktree_request(
    id: &str,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = RemoveWorktreeRequest {
        id: git_vista_protocol::WorktreeSiblingId::new(id).map_err(|e| e.to_string())?,
    };
    let json = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let (resp, _key) =
        send_write_with_key("/api/remove-worktree", Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}
