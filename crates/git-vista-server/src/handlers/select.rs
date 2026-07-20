//! `POST /api/select` (ADR 0007) and `POST /api/rescan` (ADR 0009).
//!
//! Select moves the process-global current selection to a repository the catalog
//! already holds — addressed by opaque id, resolved fail-closed — and records the
//! Visualize/Active mode the operator chose. Rescan re-reads the configured repo
//! root without a restart, so a repo created after launch can be picked. Both sit
//! behind the full M1.04 auth gate (session + CSRF + Host/Origin) like every
//! other mutation.

use axum::http::StatusCode;
use axum::Json;

use git_vista_core::identity::WorktreeId;
use git_vista_protocol::SelectRequest;

use crate::state::{scan_clones_root, scan_repo_root, select_registered};

/// Make the repository addressed by `worktree` the current selection, in the
/// requested mode. Unknown/forged id → 404, the same fail-closed contract as
/// the `?repo=` reads; a string that isn't even id-shaped → 400.
pub(crate) async fn select_repo(Json(req): Json<SelectRequest>) -> (StatusCode, String) {
    let worktree: WorktreeId = match req.worktree.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()),
    };
    if select_registered(worktree, req.mode) {
        (StatusCode::OK, "Selected.".to_string())
    } else {
        (StatusCode::NOT_FOUND, "No such repository.".to_string())
    }
}

/// Re-scan the configured repo root and the clones root (ADR 0009/0008).
/// Bodyless POST, like `rebase`. Registered entries and the current selection
/// are untouched; this only adds/refreshes entries.
pub(crate) async fn rescan() -> (StatusCode, String) {
    // Repo-root scan first, clones-root scan second — same order as startup,
    // so the clones-root scan wins any path both roots would register
    // (keeping the `read_only` clone marker accurate) on a rescan too.
    let repo_result = scan_repo_root();
    let (clones_registered, _) = scan_clones_root();
    let summary = match repo_result {
        Some((registered, skipped)) => format!(
            "Rescanned: {registered} repos registered, {skipped} skipped; \
             {clones_registered} clone(s) re-registered."
        ),
        None => format!("No repo root configured; {clones_registered} clone(s) re-registered."),
    };
    (StatusCode::OK, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::RepoMode;

    #[tokio::test]
    async fn select_refuses_a_malformed_and_an_unknown_id() {
        let (status, _) = select_repo(axum::Json(SelectRequest {
            worktree: "not-an-id".into(),
            mode: RepoMode::Visualize,
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, msg) = select_repo(axum::Json(SelectRequest {
            // Valid id shape, never registered → fail-closed 404.
            worktree: "99999999-9999-5999-8999-999999999999".into(),
            mode: RepoMode::Active,
        }))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(msg, "No such repository.");
    }
}
