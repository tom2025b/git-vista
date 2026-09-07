//! The bisect HTTP surface (M5.34, #87, ADR 0131).
//!
//! Three write endpoints, one operation each — the mechanism itself
//! (`discover`, the executors) lives in `planner::bisect_exec`; this file is
//! only the wire boundary.
//!
//! | endpoint | body | operation |
//! |---|---|---|
//! | `POST /api/bisect/start` | [`BisectStartRequest`] | [`GitOperation::BisectStart`] |
//! | `POST /api/bisect/mark`  | [`BisectMarkRequest`]  | [`GitOperation::BisectMark`]  |
//! | `POST /api/bisect/reset` | none                   | [`GitOperation::BisectReset`] |
//!
//! No `GET /api/bisect/status` yet — deliberately out of this slice. The
//! executors already answer "is one in progress, what's the history, is it
//! finished" through `bisect_exec::discover`, called fresh every sweep the
//! same way every other live fact in this app is; a read route for it is
//! real, separable work for whoever builds the status panel, not a
//! precondition for these three writes to be safe to expose.

use axum::extract::Json;
use axum::http::StatusCode;

use git_vista_protocol::dto::{BisectMarkRequest, BisectStartRequest};
use git_vista_protocol::GitOperation;

use crate::planner;
use crate::state::reject_if_read_only;

/// `POST /api/bisect/start`: [`GitOperation::BisectStart`].
pub(crate) async fn bisect_start(Json(req): Json<BisectStartRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::BisectStart {
        bad: req.bad,
        good: req.good,
    })
    .await
}

/// `POST /api/bisect/mark`: [`GitOperation::BisectMark`].
pub(crate) async fn bisect_mark(Json(req): Json<BisectMarkRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::BisectMark {
        verdict: req.verdict,
    })
    .await
}

/// `POST /api/bisect/reset`: [`GitOperation::BisectReset`]. No body — same
/// shape as `/api/stage`/`/api/unstage`.
pub(crate) async fn bisect_reset() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::BisectReset).await
}
