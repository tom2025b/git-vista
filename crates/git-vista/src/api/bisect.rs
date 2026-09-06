//! Bisect endpoints — `POST /api/bisect/start`, `/mark`, `/reset` (M5.34,
//! #87, ADR 0130).
//!
//! No status read here yet: the server side of that (`bisect_exec::discover`)
//! exists and is host-tested, but there is no `GET /api/bisect/status` route
//! — see `handlers/bisect.rs`'s module doc for why that is its own,
//! separable slice. These three writes are usable without it: git itself
//! refuses `mark`/`reset` with a 409 when no bisect is in progress, and the
//! app's existing error-notice path (`ErrorNotice`) already shows that.

use git_vista_protocol::plan::BisectVerdict;
use git_vista_protocol::CommitOid;

use super::{refuse_if_offline, refuse_if_visualize, user_facing_error, write_empty, write_json};

/// Body of `POST /api/bisect/start` — kept private to this module. The DTO
/// lives in `git-vista-protocol` (shared with the server, ADR 0079's rule);
/// this is that same shape, constructed here rather than re-exported, since
/// nothing outside this file needs to name it.
#[derive(serde::Serialize)]
struct BisectStartBody {
    bad: CommitOid,
    good: Vec<CommitOid>,
}

#[derive(serde::Serialize)]
struct BisectMarkBody {
    verdict: BisectVerdict,
}

/// Start a bisect (`POST /api/bisect/start`): `bad` and every commit in
/// `good` must already be valid oids — the caller (a menu item acting on
/// commits the graph itself drew) is expected to have them from
/// `MenuData::commit`, never typed by a user.
pub async fn bisect_start_request(bad: CommitOid, good: Vec<CommitOid>) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = BisectStartBody { bad, good };
    let (resp, _key) = write_json("/api/bisect/start", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/bisect/start", resp).await)
    }
}

/// Mark the current bisect candidate (`POST /api/bisect/mark`) — no commit
/// argument, on purpose: see [`git_vista_protocol::GitOperation::BisectMark`]'s
/// own doc comment. The executor reads `HEAD` itself and refuses with 409 if
/// no bisect is in progress.
pub async fn bisect_mark_request(verdict: BisectVerdict) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = BisectMarkBody { verdict };
    let (resp, _key) = write_json("/api/bisect/mark", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/bisect/mark", resp).await)
    }
}

/// End the bisect and return to the pre-bisect position (`POST
/// /api/bisect/reset`). Bodyless, like `/api/stage`. Always safe to send —
/// the executor answers 200 with "nothing to reset" when none is in
/// progress rather than refusing.
pub async fn bisect_reset_request() -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_empty("/api/bisect/reset").await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/bisect/reset", resp).await)
    }
}
