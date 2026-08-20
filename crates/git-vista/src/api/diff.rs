//! Diff and file-content endpoints — `GET /api/diff/{id}`,
//! `POST /api/diff/spec`, `GET /api/diff/{id}?full=1`, `GET /api/file/{id}/{path}`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_core::diff::{CommitDiff, FileContent};
use git_vista_protocol::diff::{DiffSpec, SpecDiff};

use super::{
    network_error, refuse_if_offline, refuse_if_visualize, req_get, req_post, user_facing_error,
};

/// Fetch one commit's diff — file list + unified patch — for the detail
/// panel's Changes section (`GET /api/diff/{id}`). Fetched lazily alongside
/// the commit detail, cache-busted the same way. A non-2xx body is the
/// server's reason, returned as `Err` for the panel to show.
pub async fn fetch_diff(id: &str) -> Result<CommitDiff, String> {
    let url = format!("/api/diff/{id}?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<CommitDiff>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch the diff of an explicit source/target pair (`POST /api/diff/spec`,
/// M2.16 #69) — the four modes of [`DiffSpec`].
///
/// POST for a read: `DiffSpec` is an internally-tagged enum whose variants
/// carry different fields, and a query string could only carry that by
/// flattening it into loose optional parameters — the un-explicit shape the
/// type exists to remove. `preview_push` states the same trade-off in its own
/// words: a read in every sense but the HTTP verb the CSRF gate demands. That
/// verb is why [`refuse_if_visualize`] applies; the endpoint is also
/// loopback-only server-side, so this is the client half of a boundary the
/// server enforces independently (ADR 0005).
///
/// **The response echoes the request.** Callers must compare [`SpecDiff::spec`]
/// against what they currently want before painting, exactly as the viewer
/// compares `CommitDiff.id`. ADR 0053 concluded #69's "cancellable" criterion
/// is met partly *because* diff responses echo their request that way; dropping
/// the check at a call site quietly reopens that argument.
pub async fn fetch_spec_diff(spec: &DiffSpec) -> Result<SpecDiff, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let resp = req_post("/api/diff/spec")
        .json(spec)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        resp.json::<SpecDiff>().await.map_err(|e| e.to_string())
    } else {
        Err(user_facing_error("/api/diff/spec", resp).await)
    }
}

/// Fetch one commit's diff with the patch cap lifted (`GET /api/diff/{id}?full=1`)
/// for the full-screen viewer — the panel's capped fetch is [`fetch_diff`].
pub async fn fetch_diff_full(id: &str) -> Result<CommitDiff, String> {
    let url = format!("/api/diff/{id}?full=1&t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<CommitDiff>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch one file's full content at one commit (`GET /api/file/{id}/{path}`)
/// for the full file viewer. The path rides in the URL path (encoded per
/// segment so `#`/`?` in a filename can't cut the request short); slashes stay
/// literal — the server's wildcard route consumes them.
pub async fn fetch_file(id: &str, path: &str) -> Result<FileContent, String> {
    let encoded: Vec<String> = path
        .split('/')
        .map(|seg| {
            js_sys::encode_uri_component(seg)
                .as_string()
                .unwrap_or_default()
        })
        .collect();
    let url = format!(
        "/api/file/{id}/{}?t={}",
        encoded.join("/"),
        js_sys::Date::now()
    );
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<FileContent>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}
