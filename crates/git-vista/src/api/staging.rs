//! Staging (index) endpoints — the whole-tree `POST /api/stage`/`POST
//! /api/unstage`, and the patch-based `GET /api/staging/diff`,
//! `POST /api/staging/preview`, `POST /api/staging/apply` (M2.17d, #215).
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_protocol::{PatchPlan, PatchPreview, StageDirection, StagingDiff};

use super::{
    network_error, refuse_if_offline, refuse_if_visualize, req_get, response_error,
    user_facing_error, write_empty, write_json,
};

/// Ask the backend to stage all working-tree changes (`POST /api/stage`) — a
/// plain `git add -A`, so modified/new/deleted files move into the index and can
/// then be committed. Bodyless, like the rebase request; a non-2xx body is git's
/// own error text, returned as `Err` for the caller to show.
pub async fn stage_request() -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_empty("/api/stage").await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/stage", resp).await)
    }
}

/// Ask the backend to unstage everything (`POST /api/unstage`) — a plain
/// `git reset HEAD`, the exact inverse of [`stage_request`]: the index goes
/// back to HEAD, the working tree keeps every edit. Same bodyless shape and
/// error posture as staging.
pub async fn unstage_request() -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_empty("/api/unstage").await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/unstage", resp).await)
    }
}

/// Fetch the staging base diff (`GET /api/staging/diff?direction=stage|unstage`,
/// M2.17d, #215) — the pinned diff a hunk/line selection is made against, and
/// the `diff-v1:` generation token the resulting [`PatchPlan`] must carry
/// back verbatim. A read, like [`fetch_status`]/[`fetch_rebase_status`], so no
/// offline/visualize guard here — those gate only the two writes below.
pub async fn staging_diff_request(direction: StageDirection) -> Result<StagingDiff, String> {
    let dir = match direction {
        StageDirection::Stage => "stage",
        StageDirection::Unstage => "unstage",
    };
    let url = format!(
        "/api/staging/diff?direction={dir}&t={}",
        js_sys::Date::now()
    );
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<StagingDiff>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Ask the backend what applying `plan` would do, without doing it
/// (`POST /api/staging/preview`, M2.17d, #215) — the exact bytes
/// `staging_apply_request` would execute while the plan's generation still
/// holds. A non-2xx body is the server's reason (stale generation, malformed
/// selection), returned as `Err` for the caller to show.
pub async fn staging_preview_request(plan: &PatchPlan) -> Result<PatchPreview, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_json("/api/staging/preview", plan).await?;
    if resp.ok() {
        resp.json::<PatchPreview>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Ask the backend to execute `plan` (`POST /api/staging/apply`, M2.17d,
/// #215) — stage or unstage exactly the selected hunks/lines, through the
/// same build-and-check pipeline [`staging_preview_request`] ran. A non-2xx
/// body is the server's reason, returned as `Err` for the caller to show.
pub async fn staging_apply_request(plan: &PatchPlan) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_json("/api/staging/apply", plan).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(response_error(resp).await)
    }
}
