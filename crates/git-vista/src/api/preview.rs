//! The graph-preview round trip — `POST /api/plan` then `POST /api/preview`
//! (M10.08 A6, #594).
//!
//! Two requests, because the server deliberately has two endpoints: `/api/plan`
//! answers *what an operation will do*, in words, and `/api/preview` answers
//! *what the graph will look like*, as data, from the plan the first call
//! handed back. Neither executes anything. ADR 0099 records why the second
//! takes a bare [`Plan`] rather than a wrapper DTO.
//!
//! # Why there is no idempotency key here
//!
//! The same reason [`super::preview_push`] sends none: neither route reaches
//! `operations::admit`, so there is no operation record for a replayed key to
//! settle against. `middleware::idempotency` treats an absent header as a
//! plain pass-through, not a refusal.
//!
//! # Why both guards still apply
//!
//! Both functions are reads in everything but the HTTP verb the CSRF gate
//! demands, and both still call `refuse_if_offline` and `refuse_if_visualize`
//! first — the house pattern `preview_push` set, for the reason ADR 0046
//! gives: building a plan is the first half of a write even when the second
//! half never runs.
//!
//! **One honest consequence, stated rather than left to be discovered.**
//! `PreviewUnavailable::RepositoryReadOnly` is the arm the *server* returns
//! for a repository open in Visualize mode, and `refuse_if_visualize` means
//! this client never sees it: the round trip is refused here first. That is
//! not a contradiction. In Visualize mode the app offers no merge and no
//! revert, so no confirm dialog exists to hang a preview off, and the arm is
//! reachable by any other caller of the endpoint (`git-vista-mcp`, a test, a
//! future read-only plan review). The dialog renders it correctly if it ever
//! arrives — `features::preview::core::unavailable_view` has never depended on
//! which caller produced it.

use git_vista_protocol::{GitOperation, Plan};

use crate::features::preview::core::PreviewResponse;

use super::{
    network_error, refuse_if_offline, refuse_if_visualize, req_post, timeout_error,
    user_facing_error, with_deadline, REQUEST_TIMEOUT_MS,
};

/// Build a [`Plan`] for `op` without executing it (`POST /api/plan`).
///
/// The generic form of what [`super::preview_push`] does for one operation.
/// It is a separate function rather than a widening of that one because
/// `preview_push` builds its own `GitOperation` from push's four arguments and
/// is called twice by the force-with-lease ceremony; this takes an operation
/// already built, which is what a confirm dialog holds.
///
/// Deadline plus a single retry, matching every other read in this file. A
/// retry is safe precisely because nothing is admitted: a second `/api/plan`
/// builds a second plan and no operation is started twice.
pub async fn plan_request(op: &GitOperation) -> Result<Plan, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let attempt = || async {
        let sent = async {
            req_post("/api/plan")
                .json(op)
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

/// Draw the repository as `plan` would leave it (`POST /api/preview`).
///
/// The `Ok` value is the server's four-arm answer, **including its refusals**:
/// a conflict, an unsupported operation and an unavailable host all arrive
/// here as `Ok`, because each is an answer the engine computed rather than a
/// failure of this request. Only a transport failure, a guard refusal or a
/// non-2xx status becomes `Err`.
///
/// That split is the whole reason #576 built a four-arm enum instead of
/// returning `Result<Graph, String>`, and collapsing it here — treating a
/// `Conflict` as an error because it has no picture in it — would undo the
/// feature's argument at the last layer before a person sees it.
pub async fn preview_request(plan: &Plan) -> Result<PreviewResponse, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let attempt = || async {
        let sent = async {
            req_post("/api/preview")
                .json(plan)
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
        resp.json::<PreviewResponse>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(user_facing_error("/api/preview", resp).await)
    }
}
