//! `POST /api/clone` and `POST /api/delete-clone` (Phase 12, reshaped by ADR
//! 0008): clone a public repo from a pasted URL into the persistent clones
//! store and hand its descriptor back so the browser can offer the mode
//! picker; delete a clone again on request, guarded to the clones root.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum::Json;

use git_vista_core::identity::WorktreeId;
use git_vista_protocol::{
    validate_clone_url, CloneRequest, DeleteCloneRequest, RepositoryDescriptor,
};

use crate::state::{
    allow_repo_root, cleanup_clone, clones_root, delete_clone, descriptor_for, path_is_allowed,
    set_current, DeleteCloneOutcome,
};

/// A human-recognisable directory name for a clone of `url` — the URL's last
/// path segment, minus any `.git` suffix, restricted to safe filename
/// characters. `None` when nothing usable survives (the caller falls back to a
/// stamped name). The picker shows the directory base name (ADR 0008), so a
/// clone must not be called `clone-1721400000-0`.
fn clone_dir_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    // Strip the scheme+authority before taking the last segment — otherwise a
    // path-less URL like "https://host/" names the clone after the bare host.
    let path = path.split_once("://").map_or(path, |(_, rest)| rest);
    let path = path.trim_end_matches('/');
    let (_, tail) = path.split_once('/')?;
    let tail = tail.rsplit('/').next()?;
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let safe: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    // No hidden dirs, no "." / "..": dots can't lead or trail.
    let safe = safe.trim_matches('.').to_string();
    (!safe.is_empty()).then_some(safe)
}

/// First free directory under `root` for `name`: `name`, then `name-2`, `-3`, …
/// — clones persist (ADR 0008), so a second clone of the same repo needs its
/// own directory rather than evicting the first.
fn unique_dest(root: &Path, name: &str) -> PathBuf {
    let first = root.join(name);
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| root.join(format!("{name}-{n}")))
        .find(|p| !p.exists())
        .expect("some numeric suffix is free")
}

/// Clone a public repository from a pasted URL into the persistent clones
/// store (ADR 0008) and open it look-only pending the operator's mode choice.
///
/// Same B3 posture as the other git handlers: shell out to `git clone` and forward
/// git's own error text (bad host, repo not found, …) verbatim. The URL is
/// validated by [`validate_clone_url`] — only `http(s)://`/`git://`, so a pasted
/// SSH URL can't trigger a key prompt — and is passed as its own argv entry, never
/// a shell line. A full clone is made (history is bounded downstream by
/// `HISTORY_LIMIT`); clones persist under the clones root (ADR 0008) until
/// deleted via `/api/delete-clone`.
pub(crate) async fn clone_repo(
    Json(req): Json<CloneRequest>,
) -> Result<Json<RepositoryDescriptor>, (StatusCode, String)> {
    let url = match validate_clone_url(&req.url) {
        Ok(u) => u,
        Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
    };

    let root = clones_root();
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!(
            "git-vista: /api/clone couldn't create {}: {e}",
            root.display()
        );
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't prepare temp dir: {e}"),
        ));
    }
    // The clones root is an allowed root (M1.03): every clone registers under it,
    // and nothing outside it can be served. Adding it here (rather than only at
    // startup) also covers the case where a previous run's root was cleared.
    allow_repo_root(&root);
    // Recognisable, unique per-clone dir (ADR 0008): the repo's own name where
    // the URL yields one, a stamped name otherwise. Never collides — suffixed
    // (`-2`, `-3`, …) or stamped-and-countered.
    let dest = match clone_dir_name(&url) {
        Some(name) => unique_dest(&root, &name),
        None => {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            root.join(format!("clone-{stamp}-{n}"))
        }
    };

    println!("[/api/clone] cloning {url} → {}", dest.display());
    // D4 (#66, Task 7/D2): clone's own dedicated policy constructor, not the
    // general-purpose `sandbox::policy_for` other git spawns go through.
    //
    // The policy is built from the **clones root**, not from a repository: the
    // destination does not exist yet at policy time — `sandbox::policy_for`
    // would refuse it outright (`repo_paths::resolve` requires an existing
    // `.git`), which is exactly why this is a separate constructor rather than
    // a call to that one. `policy_for_clone` grants RW on `root` (what `git
    // clone` needs to be able to write) and pins `trusted = false`
    // structurally — see that function's doc comment for why clone must never
    // be reachable at the `Unsandboxed` tier even once per-repo operator trust
    // exists, unlike every other repository operation.
    //
    // A prior comment here described a `policy_for_clone` "still awaiting
    // approval" as the reason this went through the general policy path in
    // the interim; D4 is now approved and implemented, so that interim is
    // gone — this is the direct call the earlier comment anticipated.
    //
    // Also note this needs the resolver grant — see `NETWORK_ONLY_RO_TREES`:
    // sandboxed with only `/usr /bin /lib /lib64 /etc` readable, every clone of
    // a named remote would fail `Could not resolve host`. `policy_for_clone`
    // gets it the same way `policy_for` does, via `default_system_trees`.
    //
    // `git clone` takes no `-C`, but the launcher's fixed `-C <root>` is
    // harmless (the clones root is a real directory, created just above) and
    // keeps one argv shape for every spawn site. The URL still travels as its
    // own argv entry, after `validate_clone_url`, behind `--`.
    let dest_str = dest.to_string_lossy();
    // `--` so the URL is never read as an option, even past validation.
    let args: [&str; 4] = ["clone", "--", url.as_str(), &dest_str];
    let output = match crate::sandbox::policy_for_clone(&root) {
        Ok(policy) => {
            // #216: bound the child's lifetime. `git clone` against a remote that
            // stops answering mid-transfer does not fail — it *waits*, and this
            // handler waits with it, holding the request open forever. The client
            // now times out at 60s (`api.rs::REQUEST_TIMEOUT_MS`), but a client
            // timeout does not reap the child: without this the server keeps a
            // wedged git and a half-written destination directory indefinitely,
            // and the next attempt collides with the leftover.
            //
            // Ten minutes, not the client's sixty seconds, and deliberately so:
            // a large repository over a slow link is a *legitimately* long clone,
            // and killing a working transfer because a phone tether is slow would
            // trade one bug for a worse one. This bound exists to stop a wedged
            // clone living forever, not to enforce a latency budget.
            const CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
            let spawned = crate::sandbox::spawn::command_async(&policy, &root, &args).output();
            match tokio::time::timeout(CLONE_TIMEOUT, spawned).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    eprintln!("git-vista: /api/clone couldn't run git: {e}");
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Couldn't run git: {e}"),
                    ));
                }
                Err(_elapsed) => {
                    // Leave nothing half-cloned behind: the destination is a
                    // fresh, uniquely-named directory this call created, so
                    // removing it cannot touch anything the operator owns.
                    let _ = std::fs::remove_dir_all(&dest);
                    eprintln!(
                        "git-vista: /api/clone timed out after {}s cloning {url}",
                        CLONE_TIMEOUT.as_secs()
                    );
                    return Err((
                        StatusCode::GATEWAY_TIMEOUT,
                        format!(
                            "The clone did not finish within {} minutes and was stopped. \
                             The remote may be unreachable or the repository very large.",
                            CLONE_TIMEOUT.as_secs() / 60
                        ),
                    ));
                }
            }
        }
        Err(e) => {
            eprintln!("git-vista: /api/clone couldn't build a sandbox policy: {e}");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            ));
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
        return Err((StatusCode::BAD_REQUEST, msg));
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
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Clone destination was rejected.".to_string(),
        ));
    }

    // ADR 0008: clones persist — no eviction of any previous clone. Open the
    // fresh clone look-only (safe default); the browser follows up with the
    // Visualize/Active mode screen for it, using the descriptor we return.
    // Built from the handle `set_current` just gave back, not by re-reading
    // CURRENT — a concurrent /api/select landing in between must never hand
    // this response someone else's repository.
    let descriptor = set_current(&dest, git_vista_protocol::RepoMode::Visualize)
        .and_then(|h| descriptor_for(h.worktree));
    match descriptor {
        Some(d) => {
            println!("[/api/clone] now viewing {}", dest.display());
            Ok(Json(d))
        }
        // set_current fell to degraded mode (the clone didn't classify as a
        // repo) — surface it rather than handing back a phantom descriptor.
        None => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Clone finished but the repository could not be registered.".to_string(),
        )),
    }
}

/// `POST /api/delete-clone` (ADR 0008): remove a clone — catalog entry and
/// directory — addressed by opaque id. Malformed id → 400; unknown id → 404
/// (fail closed, like the reads); a repo that isn't a clone → 400; the
/// currently open repo → 409. The guard is [`crate::state::delete_clone`]:
/// nothing outside the canonical clones root is ever removed.
pub(crate) async fn delete_clone_repo(Json(req): Json<DeleteCloneRequest>) -> (StatusCode, String) {
    let worktree: WorktreeId = match req.worktree.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()),
    };
    match delete_clone(worktree, &clones_root()) {
        DeleteCloneOutcome::NotFound => (StatusCode::NOT_FOUND, "No such repository.".to_string()),
        DeleteCloneOutcome::NotAClone => (
            StatusCode::BAD_REQUEST,
            "Not a clone — only cloned repositories can be deleted.".to_string(),
        ),
        DeleteCloneOutcome::CurrentlyOpen => (
            StatusCode::CONFLICT,
            "This repository is open right now. Open another repository first.".to_string(),
        ),
        DeleteCloneOutcome::DeleteFailed(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't delete the clone: {e}"),
        ),
        DeleteCloneOutcome::Deleted => (StatusCode::OK, "Clone deleted.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_dir_name_takes_the_last_segment_and_strips_dot_git() {
        assert_eq!(
            clone_dir_name("https://github.com/octocat/Hello-World.git"),
            Some("Hello-World".to_string())
        );
        assert_eq!(
            clone_dir_name("https://github.com/octocat/Hello-World"),
            Some("Hello-World".to_string())
        );
        assert_eq!(
            clone_dir_name("https://gitlab.com/group/sub/repo.git/"),
            Some("repo".to_string())
        );
    }

    #[test]
    fn clone_dir_name_drops_query_fragment_and_unsafe_characters() {
        assert_eq!(
            clone_dir_name("https://host/repo.git?ref=main#frag"),
            Some("repo".to_string())
        );
        assert_eq!(
            clone_dir_name("https://host/we ird$name"),
            Some("weirdname".to_string())
        );
    }

    #[test]
    fn clone_dir_name_refuses_names_that_reduce_to_nothing_or_dots() {
        assert_eq!(clone_dir_name("https://host/"), None);
        assert_eq!(clone_dir_name("https://host/..."), None);
        assert_eq!(clone_dir_name("https://host/$$$"), None);
    }

    #[test]
    fn unique_dest_suffixes_until_free() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo"));
        std::fs::create_dir_all(root.path().join("repo")).unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo-2"));
        std::fs::create_dir_all(root.path().join("repo-2")).unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo-3"));
    }

    #[tokio::test]
    async fn delete_clone_refuses_a_malformed_and_an_unknown_id() {
        let (status, _) = delete_clone_repo(axum::Json(DeleteCloneRequest {
            worktree: "not-an-id".into(),
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, msg) = delete_clone_repo(axum::Json(DeleteCloneRequest {
            // Valid id shape, never registered → fail-closed 404.
            worktree: "99999999-9999-5999-8999-999999999999".into(),
        }))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(msg, "No such repository.");
    }
}
