//! Backend HTTP calls — every `fetch`/`POST` the frontend makes.
//!
//! All URLs are relative, so they hit the same origin as the served SPA (no
//! CORS, no hardcoded host). The read endpoints cache-bust with a `t=<ms>`
//! query param: the backend already sends `Cache-Control: no-store`, but a
//! unique URL each call is belt-and-braces against iOS Safari's persistent
//! cache serving a stale response (so a branch created since the last launch
//! never shows). The write endpoints forward git's own error text verbatim on
//! failure, so the UI can show the real reason. Pure data plumbing — no UI —
//! so this stays testable on its own away from the view code.

use gloo_net::http::Request;

use git_vista_core::model::{
    BranchRequest, CloneRequest, CommitDetail, CreateBranchRequest, CreateCommitRequest, Graph,
};

/// Fetch the laid-out graph from the backend. Relative URL → same origin as the
/// served SPA, so no CORS and no hardcoded host.
///
/// The URL carries a per-load cache-busting `t=<ms>` param: the backend already
/// sends `Cache-Control: no-store`, but a unique URL each launch is belt-and-
/// braces against iOS Safari's persistent cache serving a stale graph (so a branch
/// created since the last launch never shows). The backend ignores the param.
pub async fn fetch_graph() -> Result<Graph, String> {
    let url = format!("/api/commits?t={}", js_sys::Date::now());
    Request::get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Graph>()
        .await
        .map_err(|e| e.to_string())
}

/// Fetch one commit's full detail for the side panel (Phase 10,
/// `GET /api/commit/<id>`). Same-origin relative URL, cache-busted like the graph
/// fetch. A non-2xx body is the server's reason (e.g. "No such commit."),
/// returned as `Err` for the panel to show.
pub async fn fetch_commit_detail(id: &str) -> Result<CommitDetail, String> {
    let url = format!("/api/commit/{id}?t={}", js_sys::Date::now());
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if resp.ok() {
        resp.json::<CommitDetail>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to clone a public URL and switch to viewing it read-only
/// (Phase 12, `POST /api/clone`). On a non-2xx response the body is the server's /
/// git's own error text (bad URL, repo not found, …), returned as `Err`.
pub async fn clone_request(url: &str) -> Result<(), String> {
    let body = CloneRequest { url: url.to_string() };
    let resp = Request::post("/api/clone")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to create `name` at `commit` (Issue #18, `POST /api/branch`).
/// On a non-2xx response the body is git's own error text, returned as `Err` so
/// the caller can show the real reason (branch exists, bad name, …).
pub async fn create_branch_request(name: &str, commit: &str) -> Result<(), String> {
    let body = CreateBranchRequest {
        name: name.to_string(),
        commit: commit.to_string(),
    };
    let resp = Request::post("/api/branch")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to create a commit on top of HEAD (Issue #33,
/// `POST /api/commit`). `allow_empty` picks `git commit --allow-empty` (empty
/// commit) vs a plain `git commit` (staged changes). As with the branch request,
/// a non-2xx body is git's own error text, returned as `Err`.
pub async fn create_commit_request(message: &str, allow_empty: bool) -> Result<(), String> {
    let body = CreateCommitRequest {
        message: message.to_string(),
        allow_empty,
    };
    let resp = Request::post("/api/commit")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch the live checked-out branch (Issue #33 follow-up), used to name the merge
/// target the moment the user clicks "Merge" — so it's correct even if the graph on
/// screen predates a branch switch. `Ok(None)` => detached HEAD. Cache-busted like
/// the graph fetch, since the answer changes as branches are switched.
pub async fn fetch_head_branch() -> Result<Option<String>, String> {
    let url = format!("/api/head-branch?t={}", js_sys::Date::now());
    Request::get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Option<String>>()
        .await
        .map_err(|e| e.to_string())
}

/// Ask the backend to rebase the checked-out branch onto main (`POST /api/rebase`).
/// Unlike the branch ops it carries no body — it always acts on the current HEAD,
/// and the server picks `origin/main` vs `main` as the base. A non-2xx body is
/// git's own error text (conflicts, detached HEAD, …), returned as `Err`.
pub async fn rebase_request() -> Result<(), String> {
    let resp = Request::post("/api/rebase")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to run a branch operation on `branch` (Issue #33 follow-up).
/// `path` is the endpoint — `/api/merge`, `/api/push`, `/api/delete-branch`, or
/// `/api/force-delete-branch` — all of which take the same `{ branch }` body. As with the other requests, a
/// non-2xx body is git's own error text, returned as `Err` for the caller to show.
pub async fn branch_op_request(path: &str, branch: &str) -> Result<(), String> {
    let body = BranchRequest {
        branch: branch.to_string(),
    };
    let resp = Request::post(path)
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}
