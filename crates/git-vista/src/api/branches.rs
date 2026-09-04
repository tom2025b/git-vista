//! Branch endpoints — `POST /api/branch`, `GET /api/head-branch`,
//! `GET /api/rebase-status`, `POST /api/rebase`, and the shared
//! `{ branch }`-body ops (`/api/merge`, `/api/delete-branch`,
//! `/api/force-delete-branch`).
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{BranchRequest, CreateBranchRequest, RebaseStatus, WorktreeCensus};

use super::{
    network_error, receipt, refuse_if_offline, refuse_if_visualize, req_get, send_write_with_key,
    user_facing_error, write_json, WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// Ask the backend to create `name` at `commit` (Issue #18, `POST /api/branch`).
/// On a non-2xx response the envelope's `error.message` is returned as `Err`
/// (#316) so the caller can show the real reason (branch exists, bad name, …)
/// without the wire JSON around it.
///
/// The network-failure retry that used to live here is now [`send_write`]'s,
/// for every write rather than only this one: since M1.08 both attempts carry
/// the same idempotency key, so a duplicate is replayed rather than re-run.
pub async fn create_branch_request(name: &str, commit: &str) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = CreateBranchRequest {
        name: name.to_string(),
        commit: commit.to_string(),
    };
    let (resp, _key) = write_json("/api/branch", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/branch", resp).await)
    }
}

/// Fetch the live checked-out branch (Issue #33 follow-up), used to name the merge
/// target the moment the user clicks "Merge" — so it's correct even if the graph on
/// screen predates a branch switch. `Ok(None)` => detached HEAD. Cache-busted like
/// the graph fetch, since the answer changes as branches are switched.
pub async fn fetch_head_branch() -> Result<Option<String>, String> {
    let url = format!("/api/head-branch?t={}", js_sys::Date::now());
    req_get(&url)
        .send()
        .await
        .map_err(network_error)?
        .json::<Option<String>>()
        .await
        .map_err(|e| e.to_string())
}

/// Fetch whether "Rebase onto main" would do anything right now
/// (`GET /api/rebase-status`): the checked-out branch, the base the server
/// would use (`origin/main` vs `main`), and whether HEAD is already based on
/// it. Fetched live when the menu opens — like `fetch_undoables` — so the
/// item's enabled state reflects the repo *now*, not the possibly-stale graph.
pub async fn fetch_rebase_status() -> Result<RebaseStatus, String> {
    let url = format!("/api/rebase-status?t={}", js_sys::Date::now());
    req_get(&url)
        .send()
        .await
        .map_err(network_error)?
        .json::<RebaseStatus>()
        .await
        .map_err(|e| e.to_string())
}

/// Fetch the worktree census (`GET /api/worktrees`, M11.02 #547) — every
/// linked worktree of this repository and the branch each one holds.
///
/// Read live, on the click that offers a checkout, for the same reason
/// [`fetch_head_branch`] is: another worktree can open or close at any moment,
/// and the graph on screen knows nothing about either.
///
/// A **transport or JSON failure returns `Err`**, and the caller must not
/// turn that into an empty census. `WorktreeCensus::CensusFailed` is what the
/// *server* says when it could not read the list; an `Err` here is what this
/// client says when it could not reach the server. Both mean "nothing is
/// known about any branch", and `CheckoutElsewhere::classify` is where the
/// two become one answer — never `Ok(WorktreeCensus::Observed { siblings:
/// vec![] })`, which would claim an observation nobody made.
pub async fn fetch_worktree_census() -> Result<WorktreeCensus, String> {
    let url = format!("/api/worktrees?t={}", js_sys::Date::now());
    req_get(&url)
        .send()
        .await
        .map_err(network_error)?
        .json::<WorktreeCensus>()
        .await
        .map_err(|e| e.to_string())
}

/// Ask the backend to rebase the checked-out branch onto main (`POST /api/rebase`).
/// Unlike the branch ops it carries no body — it always acts on the current HEAD,
/// and the server picks `origin/main` vs `main` as the base. `Ok` carries the
/// server's success line so the caller can tell a real rebase from the
/// "Already up to date" no-op (a raced click from a stale menu). A non-2xx body
/// is git's own error text (conflicts, detached HEAD, …), returned as `Err`.
pub async fn rebase_request(key: IdempotencyKey) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = send_write_with_key("/api/rebase", None, key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}

/// Ask the backend to run a branch operation on `branch` (Issue #33 follow-up).
/// `path` is the endpoint — `/api/merge`, `/api/delete-branch`, or
/// `/api/force-delete-branch` — all of which take the same `{ branch }` body. As with the other requests, a
/// non-2xx body is git's own error text, returned as `Err` for the caller to show.
/// `Ok` carries the server's success line — most callers ignore it, but the merge
/// flow reads it to tell a real merge from git's "Already up to date" no-op.
///
/// `/api/push` moved off this shared function in #233, once `PushRequest` grew
/// `set_upstream`/`force` beyond the bare `{ branch }` shape every other caller
/// here still sends — see [`push_request`].
pub async fn branch_op_request(
    path: &str,
    branch: &str,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = BranchRequest {
        branch: branch.to_string(),
    };
    let json = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key(path, Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}
