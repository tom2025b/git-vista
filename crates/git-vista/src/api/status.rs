//! Working-tree status endpoints — `GET /api/status`, `GET /api/status/v2`,
//! `POST /api/discard-tracked-paths`, `POST /api/delete-untracked-paths`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_core::status::RepoStatus;
use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{RemoveWorktreeRequest, WorktreePathsRequest, WorktreeStatus};

use super::{
    receipt, refuse_if_offline, refuse_if_visualize, response_error, send_read,
    send_write_with_key, WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// Fetch the live working-tree status (`GET /api/status`) — branch, ahead/
/// behind, and the dirty-file lists — for the topbar chip, the commit review
/// and the context menu's staging items. Resolved fresh server-side per
/// request and cache-busted like the other live reads, since it changes with
/// every edit.
///
/// Pin chip/commit/menu readings to the accepted frame's opaque repository id.
///
/// **`repo` is required, not optional, and that is deliberate.** An unscoped
/// request — one asking the server for "whatever repository you happen to be
/// resolving right now" — is unrepresentable here rather than merely uncalled,
/// so a later caller cannot forget to scope one. A reply to an unscoped
/// question belongs to no frame in particular, which is the staleness class
/// the epoch/repository pinning exists to refuse.
///
/// The reply is still only a *candidate* reading. Whether the frame it was
/// requested for is still the accepted one is
/// [`reading_is_current`](crate::features::status::detail::core::reading_is_current)'s
/// decision, at the call site, on values this function never sees.
pub async fn fetch_status_for(repo: &str) -> Result<RepoStatus, String> {
    let url = format!(
        "/api/status?t={}&repo={}",
        js_sys::Date::now(),
        js_sys::encode_uri_component(repo)
    );
    let resp = send_read(&url).await.map_err(|e| e.to_string())?;
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
/// Additive alongside [`fetch_status_for`], which serves the topbar chip's
/// coarser v1 shape.
///
/// **`repo` is required, not optional, and that is deliberate** — the same
/// reason [`fetch_status_for`] requires one. An unscoped request asks the
/// server for "whatever repository you happen to be resolving right now", and
/// a reply to that question belongs to no frame in particular. Making it
/// unrepresentable, rather than merely uncalled, is what stops a later caller
/// forgetting to scope one.
///
/// The stakes here are higher than the v1 read's. This reply builds the path
/// lists for "Discard Changes…" and "Delete Untracked Files…", so an answer
/// belonging to a repository the user has left would name *that* repository's
/// files inside a confirmation for *this* one. The server's own
/// `verify_path_states` re-check is a **conditional path-state recheck**, not
/// a repository check: it cannot tell a colliding path name in the live
/// repository from the one the user was actually shown. #721 gave the
/// destructive POSTs a repository selector that *can* answer that (ADR 0139),
/// but this client cannot fill it in yet — see
/// [`discard_tracked_paths_request`] — so for the shipped path this paragraph
/// still describes the whole of the server's contribution.
///
/// The reply is still only a *candidate* reading. Whether the frame it was
/// requested for is still the accepted one is
/// [`current_reading`](crate::features::status::detail::core::current_reading)'s
/// decision, at the call site, on values this function never sees.
///
/// Routed through [`send_read`] (#218) rather than a bare `req_get`, for the
/// reason that function documents: a read with no timeout over a dropped SSH
/// tunnel never settles, and this one gates a destructive confirmation.
pub async fn fetch_worktree_status_for(repo: &str) -> Result<WorktreeStatus, String> {
    let url = format!(
        "/api/status/v2?t={}&repo={}",
        js_sys::Date::now(),
        js_sys::encode_uri_component(repo)
    );
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
///
/// # `repo: None`, and it is not an oversight (#721)
///
/// That DTO now carries an optional repository selector: the worktree id the
/// path list was read out of. Sending it is what lets the server refuse a
/// batch aimed at a repository other than the selected one, with its own
/// `412` — see [`WorktreePathsRequest`]'s doc comment for the whole contract.
///
/// This function cannot supply it yet, and no value it could reach for would
/// be the right one. The scope that matters is the one captured **when the
/// list was built** — `features::status::signals`' pinned repository, the
/// same id `fetch_worktree_status_for` was called with. By the time the
/// request reaches here that reading has travelled through an
/// `OperationKind`, which carries `paths` and nothing else. Reading a *live*
/// repository id at this point would produce a selector that always matches
/// the selection and therefore proves nothing — a check that cannot fail is
/// worse than no check, because it reads like one.
///
/// Threading it properly means widening `OperationKind::DiscardTrackedPaths`
/// and the confirmation that constructs it. That is #721's second half and
/// lands with those callers, not here.
pub async fn discard_tracked_paths_request(
    paths: Vec<String>,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&WorktreePathsRequest { repo: None, paths })
        .map_err(|e| e.to_string())?;
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
///
/// `repo: None` for the reason [`discard_tracked_paths_request`] records —
/// and this is the endpoint where it costs the most, since a deletion has no
/// undo anywhere in this repository.
pub async fn delete_untracked_paths_request(
    paths: Vec<String>,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&WorktreePathsRequest { repo: None, paths })
        .map_err(|e| e.to_string())?;
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
