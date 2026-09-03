//! Activity feed and undo endpoints — `GET /api/activity`,
//! `GET /api/undoables/{id}`, `POST /api/undo`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_core::activity::{ActivityEvent, UndoAction, Undoable};
use git_vista_protocol::{operation::IdempotencyKey, ActivityPage};

use super::{
    network_error, receipt, refuse_if_offline, refuse_if_visualize, req_get, send_write_with_key,
    WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// Fetch the whole available activity feed by walking its cursor pages.
///
/// Each request is still bounded by `limit`; the browser follows the opaque
/// cursor until the server explicitly returns `None`, so a 500-event response
/// ceiling can never become a silent 500-event history ceiling again. A stale
/// cursor is a server error and restarts on the panel's next refresh rather
/// than splicing two different live snapshots together.
pub async fn fetch_activity(limit: usize) -> Result<Vec<ActivityEvent>, String> {
    let mut events = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = fetch_activity_page(limit, cursor.as_deref()).await?;
        events.extend(page.events);
        match page.cursor {
            Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            Some(_) => return Err("activity paging returned the same cursor twice".to_string()),
            None => return Ok(events),
        }
    }
}

/// Fetch one activity page. Kept separate from the aggregate loop so the wire
/// shape is read and named at the boundary rather than deserialized ad hoc.
async fn fetch_activity_page(
    limit: usize,
    cursor: Option<&str>,
) -> Result<ActivityPage<ActivityEvent>, String> {
    let cursor = cursor
        .map(|value| format!("&cursor={value}"))
        .unwrap_or_default();
    let url = format!(
        "/api/activity?limit={limit}{cursor}&t={}",
        js_sys::Date::now()
    );
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<ActivityPage<ActivityEvent>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch the undo actions that apply to one commit (`GET /api/undoables/{id}`,
/// Activity/Undo step 5), computed live server-side — so the context menu's
/// undo section reflects the repo *now*, not the possibly-stale graph. Empty
/// on a read-only clone. Cache-busted like the other live reads.
pub async fn fetch_undoables(commit: &str) -> Result<Vec<Undoable>, String> {
    let url = format!("/api/undoables/{commit}?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<Undoable>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to execute one undo action (`POST /api/undo`, Activity/Undo
/// step 5). The body is the tagged [`UndoAction`] exactly as the server handed
/// it out. A non-2xx body is the server's reason — including the 409s for a
/// moved branch (compare-and-swap) or a dirty working tree — returned as `Err`
/// for the confirm flow to show.
pub async fn undo_request(
    action: &UndoAction,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(action).map_err(|e| e.to_string())?;
    let (resp, _key) =
        send_write_with_key("/api/undo", Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}
