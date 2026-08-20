//! The paged commit graph reads — `GET /api/frame`, `GET /api/commits`.
//!
//! Split out of the former monolithic `api.rs`. Both endpoints answer through
//! [`super::HistoryFetchError`]/[`super::history_json`] rather than a bare
//! `String`, since the *status* of a history read is itself an instruction to
//! the caller (see that type's doc comment in the parent module) — which is
//! why it stays shared plumbing there rather than moving in here with its
//! only two callers.

use crate::features::graph::core::{Frame, Page};

use super::{encode_component, history_json, send_read, HistoryFetchError};

/// The largest page the server will mint, mirrored here so a caller's request is
/// clamped before it goes out rather than silently rewritten server-side. Kept
/// in step with `MAX_PAGE_LIMIT` in `git-vista-server`'s read handlers.
const MAX_PAGE_LIMIT: usize = 1_000;

/// Fetch the once-per-view [`Frame`] (`GET /api/frame`, M1.10): refs, branch
/// colours and the repo's own metadata — no commits at all. Relative URL → same
/// origin as the served SPA, cache-busted with `t=<ms>` like every other read
/// (the backend ignores the param; iOS Safari's persistent cache does not).
///
/// No `?repo=` selector: the Frame *is* what resolves the view's target, and
/// every page fetched after it pins that answer via
/// [`Frame::worktree_id`](git_vista_protocol::HistoryFrame::worktree_id).
pub async fn fetch_frame() -> Result<Frame, HistoryFetchError> {
    let url = format!("/api/frame?t={}", js_sys::Date::now());
    let resp = send_read(&url).await?;
    history_json(resp).await
}

/// Fetch one page of history (`GET /api/commits`, M1.10): rows, edges and stubs
/// plus the cursor for the next page, or `None` once history is exhausted.
///
/// `repo` is the accepted Frame's `worktree_id` — passed on page 1 *and* on
/// every append, so a server whose default selection changes mid-scroll can't
/// splice another repository's rows onto this graph. A degraded Frame has no id
/// and passes `None`, which keeps the server's default-selection behaviour.
/// `cursor` is `None` for page 1. Both are percent-encoded; `limit` is clamped
/// into the server's own accepted range.
pub async fn fetch_page(
    repo: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page, HistoryFetchError> {
    // Built by appending, not by `format!`ing a fixed shape: an absent selector
    // must be *omitted*, not sent empty — an empty `?repo=` is a different
    // request from no `?repo=` at all.
    let mut url = String::from("/api/commits?");
    if let Some(repo) = repo {
        url.push_str(&format!("repo={}&", encode_component(repo)));
    }
    if let Some(cursor) = cursor {
        url.push_str(&format!("cursor={}&", encode_component(cursor)));
    }
    url.push_str(&format!(
        "limit={}&t={}",
        limit.clamp(1, MAX_PAGE_LIMIT),
        js_sys::Date::now()
    ));
    let resp = send_read(&url).await?;
    history_json(resp).await
}
