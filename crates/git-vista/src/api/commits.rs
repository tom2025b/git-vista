//! Commit-object endpoints — `GET /api/commit/<id>`, `POST /api/commit`,
//! `POST /api/amend-commit`, `POST /api/cherry-pick`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_core::model::CommitDetail;
use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{CherryPickRequest, CreateCommitRequest};

use crate::features::dialogs::commit::{
    amend_body, classify_amend_response, classify_create_commit_response, AmendOutcome,
    CreateCommitOutcome,
};

use super::{
    network_error, receipt, refuse_if_offline, refuse_if_visualize, req_get, send_write_with_key,
    write_json, WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// Fetch one commit's full detail for the side panel (Phase 10,
/// `GET /api/commit/<id>`). Same-origin relative URL, cache-busted like the graph
/// fetch. A non-2xx body is the server's reason (e.g. "No such commit."),
/// returned as `Err` for the panel to show.
pub async fn fetch_commit_detail(id: &str) -> Result<CommitDetail, String> {
    let url = format!("/api/commit/{id}?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<CommitDetail>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to create a commit (Issue #33, `POST /api/commit`).
/// `allow_empty` picks `git commit --allow-empty` (empty commit) vs a plain
/// `git commit` (staged changes). `branch` targets a branch other than the
/// checked-out one — the branch-stub path, empty commits only; `None` commits
/// on HEAD as before.
///
/// Reads the response through [`classify_create_commit_response`] (#72,
/// M2.19) rather than the generic envelope-unwrap [`user_facing_error`] every
/// other write here uses: `POST /api/commit`'s own execution failures
/// (signing, a rejected hook, nothing staged) carry a typed
/// [`git_vista_protocol::CommitFailureKind`] the caller can show actionable
/// guidance for, the same posture [`amend_commit_request`] already takes for
/// its own typed contract.
pub async fn create_commit_request(
    message: &str,
    allow_empty: bool,
    branch: Option<&str>,
) -> CreateCommitOutcome {
    if let Err(refusal) = refuse_if_offline().and_then(|()| refuse_if_visualize()) {
        return CreateCommitOutcome::Unavailable(refusal);
    }
    let body = CreateCommitRequest {
        message: message.to_string(),
        allow_empty,
        branch: branch.map(str::to_string),
    };
    let resp = match write_json("/api/commit", &body).await {
        Ok((resp, _key)) => resp,
        // A transport failure: the request may or may not have reached the
        // server, which is precisely what `Unavailable`'s copy says.
        Err(e) => return CreateCommitOutcome::Unavailable(e),
    };
    let status = resp.status();
    let text = resp
        .text()
        .await
        .unwrap_or_else(|_| format!("HTTP {status}"));
    classify_create_commit_response(status, &text)
}

/// Rewrite the checked-out branch's tip commit (`POST /api/amend-commit`,
/// M2.19c #224 over M2.19a #222 / M2.19b #223).
///
/// Three things make this a different function from [`create_commit_request`]
/// rather than a flag on it, and all three are the endpoint's own design (ADR
/// 0040):
///
/// - **A separate route.** An amend sent to a server that predates #223 must
///   404, never be quietly accepted as a plain commit — "created a second
///   commit instead of rewriting the first" is a silent wrong outcome.
/// - **A compare-and-swap.** `expected_tip` is the full commit id the *user*
///   reviewed. The server refuses if HEAD has moved since, which is the whole
///   protection: it is not a staleness optimisation, it is what stops an amend
///   rewriting a commit nobody looked at.
/// - **A typed answer.** Every 400 from this route is an `AmendCommitError`,
///   and 200 is an `AmendCommitSuccess`. Reading them is
///   `features::dialogs::commit::classify_amend_response` — pure, host-tested
///   against bodies serialized from the server's own DTOs — so this function
///   carries no parsing or classification of its own.
///
/// Never returns `Result`: the caller must handle a stale tip differently from
/// an error (see [`AmendOutcome`]), and a `Result<_, String>` is exactly the
/// shape that would let it treat them the same.
pub async fn amend_commit_request(message: &str, expected_tip: &str) -> AmendOutcome {
    if let Err(refusal) = refuse_if_offline().and_then(|()| refuse_if_visualize()) {
        return AmendOutcome::Unavailable(refusal);
    }
    let body = amend_body(message, expected_tip);
    let resp = match write_json("/api/amend-commit", &body).await {
        Ok((resp, _key)) => resp,
        // A transport failure: the request may or may not have reached the
        // server, which is precisely what `Unavailable`'s copy says.
        Err(e) => return AmendOutcome::Unavailable(e),
    };
    let status = resp.status();
    let text = resp
        .text()
        .await
        .unwrap_or_else(|_| format!("HTTP {status}"));
    classify_amend_response(status, &text)
}

/// Cherry-pick `commit` onto the checked-out branch (`POST /api/cherry-pick`,
/// M10.09/#596).
///
/// Shaped like [`branch_op_request`](super::branches::branch_op_request) rather
/// than like [`amend_commit_request`] above: a cherry-pick is a *tracked*
/// operation — it goes through the operations registry, carries `key` for
/// idempotent retry, and reports through the progress strip — so it returns a
/// [`WriteReceipt`] and lets `dialogs/confirm.rs` classify the outcome, instead
/// of parsing a bespoke response shape here. That is also why it is reached
/// through this route at all rather than through `/api/plan` +
/// `/api/execute-plan`, which would have skipped every one of those.
///
/// `commit` is the full hex id the confirm dialog reviewed. The server refuses
/// anything else with a 400 rather than resolving it — see `CherryPickRequest`.
pub async fn cherry_pick_request(
    commit: &str,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = CherryPickRequest {
        commit: commit.to_string(),
    };
    let json = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let (resp, _key) =
        send_write_with_key("/api/cherry-pick", Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}
