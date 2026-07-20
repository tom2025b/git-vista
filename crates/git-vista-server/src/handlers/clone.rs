//! `POST /api/clone` (Phase 12): clone a public repo from a pasted URL into a
//! throwaway temp dir and switch the server to viewing it, read-only.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{validate_clone_url, CloneRequest};

use crate::state::{
    allow_repo_root, cleanup_clone, clones_root, current, path_is_allowed, set_current,
};

/// Clone a public repository from a pasted URL into a throwaway temp directory and
/// switch the server to viewing it, read-only (Phase 12).
///
/// Same B3 posture as the other git handlers: shell out to `git clone` and forward
/// git's own error text (bad host, repo not found, …) verbatim. The URL is
/// validated by [`validate_clone_url`] — only `http(s)://`/`git://`, so a pasted
/// SSH URL can't trigger a key prompt — and is passed as its own argv entry, never
/// a shell line. A full clone is made (history is bounded downstream by
/// `HISTORY_LIMIT`); the previous clone, if any, is deleted so at most one is kept.
pub(crate) async fn clone_repo(Json(req): Json<CloneRequest>) -> (StatusCode, String) {
    let url = match validate_clone_url(&req.url) {
        Ok(u) => u,
        Err(e) => return (StatusCode::BAD_REQUEST, e),
    };

    let root = clones_root();
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!(
            "git-vista: /api/clone couldn't create {}: {e}",
            root.display()
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't prepare temp dir: {e}"),
        );
    }
    // The clones root is an allowed root (M1.03): every clone registers under it,
    // and nothing outside it can be served. Adding it here (rather than only at
    // startup) also covers the case where a previous run's root was cleared.
    allow_repo_root(&root);
    // Unique per-clone dir: monotonic counter + a timestamp, so concurrent or
    // rapid clones never collide.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dest = root.join(format!("clone-{stamp}-{n}"));

    println!("[/api/clone] cloning {url} → {}", dest.display());
    let output = match tokio::process::Command::new("git")
        .arg("clone")
        // `--` so the URL is never read as an option, even past validation.
        .arg("--")
        .arg(&url)
        .arg(&dest)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/clone couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if !output.status.success() {
        // git printed why (host down, repo not found, auth needed…) on stderr.
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            "git clone failed.".to_string()
        } else {
            msg
        };
        cleanup_clone(&dest); // remove the empty/partial dir git may have left
        eprintln!("git-vista: /api/clone failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }

    // Defence in depth (M1.03): the destination is built under the clones root by
    // construction, but confirm its canonical path really is within an allowed
    // root before serving it — a clone must never escape the clones directory.
    let canonical = std::fs::canonicalize(&dest).unwrap_or_else(|_| dest.clone());
    if !path_is_allowed(&canonical) {
        cleanup_clone(&dest);
        eprintln!(
            "git-vista: /api/clone destination escaped the clones root: {}",
            dest.display()
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Clone destination was rejected.".to_string(),
        );
    }

    // Switch to the fresh clone (Visualize — look-only until the operator picks
    // a mode for it), then delete the previous one, if it was itself a clone —
    // so disk holds at most one clone at a time.
    let (old_path, old_read_only) = current();
    set_current(&dest, git_vista_protocol::RepoMode::Visualize);
    if old_read_only {
        cleanup_clone(&old_path);
    }
    println!("[/api/clone] now viewing {}", dest.display());
    (StatusCode::OK, format!("Cloned {url}"))
}
