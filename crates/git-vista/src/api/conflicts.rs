//! Inspecting a conflict (M4.31a, #428) — `GET /api/conflicts`,
//! `GET /api/blob/{oid}`, `GET /api/worktree-file/{*path}`.
//!
//! Three reads and one assembler. The assembler
//! ([`fetch_conflict_panes`]) is the only thing outside
//! [`crate::features::conflicts::core`] that knows all four panes belong
//! together, and it delegates every *decision* about what a pane shows back to
//! that host-tested core — it fetches, and the core folds. Nothing here
//! decides whether an absent stage renders as empty, because that is exactly
//! the judgement #428 keeps out of wasm-only code.

use git_vista_core::diff::{BlobContent, WorktreeFileContent};
use git_vista_protocol::conflict::{ConflictedFile, Resolution};
use git_vista_protocol::ResolveConflictRequest;

use crate::features::conflicts::core::{result_pane_state, ConflictPanes, PaneState, ResultRead};

use super::{
    network_error, refuse_if_offline, refuse_if_visualize, req_get, user_facing_error, write_json,
};

/// Every conflicted path with its three stages described — metadata only, no
/// content (`GET /api/conflicts`).
pub async fn fetch_conflicts() -> Result<Vec<ConflictedFile>, String> {
    let url = format!("/api/conflicts?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<ConflictedFile>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// One conflict stage's blob by object id (`GET /api/blob/{oid}`).
///
/// The oid goes into the URL path unencoded on purpose: the server admits only
/// 40 or 64 lowercase hex characters and refuses anything else with a 400
/// before it spawns git, so there is no byte here that percent-encoding would
/// protect — unlike `fetch_file`'s path, which is arbitrary user text.
pub async fn fetch_blob(oid: &str) -> Result<BlobContent, String> {
    let url = format!("/api/blob/{oid}?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<BlobContent>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// The working tree's own copy of one path — the result pane
/// (`GET /api/worktree-file/{*path}`).
///
/// `Ok(None)` means the server answered 404: there is no file at that path.
/// That is **information**, not a failure — a delete/modify conflict resolved
/// toward deletion legitimately leaves nothing on disk — so it is a distinct
/// value rather than an `Err`, and [`ResultRead::NoFile`] is where it lands.
///
/// Path segments are encoded exactly as `fetch_file` encodes them, and for the
/// same reason: a `#` or `?` in a filename would otherwise cut the request
/// short. Slashes stay literal for the server's wildcard route.
pub async fn fetch_worktree_file(path: &str) -> Result<Option<WorktreeFileContent>, String> {
    let encoded: Vec<String> = path
        .split('/')
        .map(|seg| {
            js_sys::encode_uri_component(seg)
                .as_string()
                .unwrap_or_default()
        })
        .collect();
    let url = format!(
        "/api/worktree-file/{}?t={}",
        encoded.join("/"),
        js_sys::Date::now()
    );
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<WorktreeFileContent>()
            .await
            .map(Some)
            .map_err(|e| e.to_string())
    } else if resp.status() == 404 {
        Ok(None)
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Assemble all four panes for one conflicted path.
///
/// Order matters and is the reason this is one function rather than four calls
/// at the call site: the stage metadata decides *which* blobs are worth
/// fetching at all. A binary or absent stage is never fetched — the core
/// resolves it to a terminal pane state from metadata alone — so a conflict
/// with a 200 MB binary side costs one `/api/conflicts` read, not a download.
///
/// A path that `/api/conflicts` no longer reports is an `Err`, not an empty
/// set of panes: it means the conflict was resolved (or the repository moved)
/// while the viewer was open, and showing four empty panes would present that
/// as a file with nothing on any side.
pub async fn fetch_conflict_panes(path: &str) -> Result<ConflictPanes, String> {
    let files = fetch_conflicts().await?;
    let file = files
        .into_iter()
        .find(|f| f.path == path)
        .ok_or_else(|| format!("‘{path}’ is no longer conflicted."))?;

    let mut panes = ConflictPanes::open(&file);

    // Only an `AwaitingContent` pane has content to fetch. `with_content`
    // ignores a response for any other state, so this loop cannot turn an
    // Absent or Unreadable pane into text even if a fetch somehow answered.
    for pane in [&mut panes.base, &mut panes.ours, &mut panes.theirs] {
        if let PaneState::AwaitingContent { oid } = pane.clone() {
            let fetched = fetch_blob(&oid).await;
            *pane = pane.clone().with_content(fetched);
        }
    }

    panes.result = result_pane_state(match fetch_worktree_file(path).await {
        Ok(Some(file)) => ResultRead::Wrote(file),
        Ok(None) => ResultRead::NoFile,
        Err(e) => ResultRead::Failed(e),
    });

    Ok(panes)
}

/// Resolve one conflicted path (`POST /api/resolve-conflict`, M4.31b, #429).
///
/// The `Err` string is the server's own words, not a generic failure. That is
/// the whole point of the endpoint's refusal handling: taking a side that is
/// absent, or one that could not be read, each produce a *different* sentence
/// naming which side and why, and a caller that collapses them into "it
/// failed" throws away the only thing that tells the user what to do next.
pub async fn resolve_conflict_request(path: &str, resolution: Resolution) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = ResolveConflictRequest {
        path: path.to_string(),
        resolution,
    };
    let (resp, _key) = write_json("/api/resolve-conflict", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/resolve-conflict", resp).await)
    }
}
