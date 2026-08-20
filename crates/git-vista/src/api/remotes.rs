//! Remote-transfer endpoints — `POST /api/fetch`, `POST /api/pull`,
//! `POST /api/push`, `POST /api/plan`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{
    BranchName, FetchRequest, ForcePublish, GitOperation, MergeStrategy, Plan, PullRequest,
    PushRequest, RemoteName,
};

use super::{
    network_error, receipt, refuse_if_offline, refuse_if_visualize, req_post, send_write_with_key,
    timeout_error, user_facing_error, with_deadline, WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// The deadline for `POST /api/fetch` alone (M2.20f, #232).
///
/// Fetch is operation-tracked — the write response only comes back once the
/// server observes a terminal state (`planner::plan_and_execute_tracked`
/// awaits `record.wait_terminal()` before the handler answers) — but that
/// terminal state is not reached until the transfer itself is over.
/// Architecturally this is the **clone shape** (a long-poll bounded by real
/// transfer time), not the "fast op-tracked write" shape the branch
/// operations are, so [`REQUEST_TIMEOUT_MS`] would abandon a fetch that is
/// still genuinely receiving objects. Unlike clone, fetch *is*
/// operation-tracked, so [`send_write_with_key`]'s single retry is safe even
/// if it fires — a second attempt lands on the same admitted record rather
/// than starting a second `git fetch` — but a needless retry racing a real
/// transfer is still worth avoiding, which is what this deadline is for.
///
/// Set to mirror [`CLONE_TIMEOUT_MS`] exactly, for the same reasoning: late
/// enough never to interrupt a transfer the server is still making progress
/// on, early enough that the client still gives up before the server does.
/// A separate named constant rather than reusing `CLONE_TIMEOUT_MS` — the
/// issue's own wording — so pull's integration half growing slower later
/// doesn't force renaming a constant fetch alone owns.
const FETCH_TIMEOUT_MS: u64 = 570_000;

/// The deadline for `POST /api/pull` alone (M2.20f, #232).
///
/// A pull's fetch half is exactly the same unbounded transfer
/// [`FETCH_TIMEOUT_MS`] exists for — see that constant's doc comment for the
/// full reasoning, which applies here unchanged. A distinct name, not a
/// shared one, so pull's integration half growing slower some day doesn't
/// force renaming a constant fetch alone owns.
const PULL_TIMEOUT_MS: u64 = 570_000;

/// Ask the backend to fetch from `remote` (`POST /api/fetch`, M2.20f, #232).
///
/// Operation-tracked, the same shape as [`branch_op_request`] just above:
/// the response carries an operation id ([`WriteReceipt::operation`]) the
/// caller subscribes to for live progress, and a non-2xx `message` is the
/// settled record's raw JSON body (a `FetchSuccess`/`FetchError`) rather
/// than plain text — parse it instead of showing it verbatim.
///
/// Bounded by [`FETCH_TIMEOUT_MS`], not [`REQUEST_TIMEOUT_MS`] — see that
/// constant's doc comment for why a fetch needs the longer deadline.
pub async fn fetch_request(remote: &str, key: IdempotencyKey) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&FetchRequest {
        remote: remote.to_string(),
    })
    .map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key("/api/fetch", Some(json), key, FETCH_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}

/// Ask the backend to pull `branch` from `remote` into the checked-out
/// branch, integrating with `strategy` (`POST /api/pull`, M2.20f, #232).
///
/// `strategy` is a plain, always-present [`MergeStrategy`] here, never an
/// `Option` — there is no sentinel this function could pass through for
/// "not yet chosen". A caller can only reach this function once the user has
/// actually picked Merge or Rebase; see `OperationKind::Pull`'s doc comment
/// for where that discipline starts, and ADR 0044 for why the wire request
/// enforces the identical rule one layer further out (an omitted `strategy`
/// field is a `400`, never a fallback).
///
/// Bounded by [`PULL_TIMEOUT_MS`], for the same reason as [`fetch_request`]:
/// a pull's fetch half is the same unbounded transfer.
pub async fn pull_request(
    remote: &str,
    branch: &str,
    strategy: MergeStrategy,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&PullRequest {
        remote: remote.to_string(),
        branch: branch.to_string(),
        strategy,
    })
    .map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key("/api/pull", Some(json), key, PULL_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}

/// Ask the backend to push `branch` to origin, optionally recording it as
/// the upstream and/or forcing it with a reviewed lease (`POST /api/push`,
/// #233 — `Push` outgrew [`branch_op_request`]'s shared `{ branch }` shape
/// the same way `Pull` already had its own [`pull_request`] rather than
/// reusing it).
///
/// `force` is `None` for the ordinary fast-forward path every push before
/// #233 ran; `Some(ForcePublish::WithLease { .. })` only after the caller
/// has already walked the danger-styled confirmation `dialogs/confirm.rs`
/// gates a lease behind (`OperationKind::Push`'s own doc comment) — this
/// function sends exactly what it is given and gates nothing itself.
pub async fn push_request(
    branch: &str,
    set_upstream: bool,
    force: Option<ForcePublish>,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&PushRequest {
        branch: branch.to_string(),
        set_upstream,
        force,
    })
    .map_err(|e| e.to_string())?;
    let (resp, _key) =
        send_write_with_key("/api/push", Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}

/// Preview a `GitOperation::PushBranch` without executing it
/// (`POST /api/plan`, #233) — the server's build-only endpoint
/// (`handlers::plan::plan_operation`; `git-vista-mcp` was its only caller
/// before this). The menu's force-push entry point calls this twice: once
/// with `ForcePublish::None` to read the remote-tracking ref's live tip
/// (`Plan::expected_ref_changes`, via
/// `features::graph::core::remote_tip_from_plan`), and again with
/// `ForcePublish::WithLease` built from that tip, to read the server's own
/// `RiskLevel` for exactly the plan about to be confirmed — never assumed
/// client-side from the `ForcePublish` variant alone
/// (`push_confirm_copy`'s doc comment says why).
///
/// Sends no idempotency key, unlike every other function in this file:
/// `/api/plan` never reaches `operations::admit` (checked against
/// `handlers/plan.rs` and `middleware::idempotency`, which treats an absent
/// header as a plain pass-through, not a refusal), so there is no operation
/// to track and nothing a retried key would replay differently. It is a
/// read in every sense but the HTTP verb the CSRF gate demands
/// (`route_authz.rs`: `SessionAndCsrf`, same as every other `POST`), which
/// is why the offline/visualize guards below still apply — a plan build is
/// the first half of a write by ADR 0046's own reasoning, even though this
/// one is never executed.
pub async fn preview_push(
    remote: &str,
    branch: &str,
    set_upstream: bool,
    force: ForcePublish,
) -> Result<Plan, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let branch = BranchName::new(branch).map_err(|e| e.to_string())?;
    let remote = RemoteName::new(remote).map_err(|e| e.to_string())?;
    let op = GitOperation::PushBranch {
        branch,
        remote,
        set_upstream,
        force,
    };
    let attempt = || async {
        let sent = async {
            req_post("/api/plan")
                .json(&op)
                .map_err(|e| e.to_string())?
                .send()
                .await
                .map_err(network_error)
        };
        with_deadline(sent, REQUEST_TIMEOUT_MS)
            .await
            .unwrap_or_else(|| Err(timeout_error()))
    };
    let resp = match attempt().await {
        Ok(resp) => resp,
        Err(_) => attempt().await?,
    };
    if resp.ok() {
        resp.json::<Plan>().await.map_err(|e| e.to_string())
    } else {
        Err(user_facing_error("/api/plan", resp).await)
    }
}
