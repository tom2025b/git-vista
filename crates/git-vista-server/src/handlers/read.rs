//! The read endpoints (all `no-store` GETs): the laid-out history graph, one
//! commit's detail and diff, and the two live "state" reads (checked-out branch,
//! working-tree status). Reads, so they work on read-only clones too.

use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use git_vista_core::identity::{RepositoryHandle, WorktreeId};
use git_vista_core::layout;
use git_vista_core::model::{CommitSummary, GitRef, RefKind};
use git_vista_core::status::parse_porcelain_v2;
use git_vista_git::{read_commit, read_refs, walk_history, RepoError};

use crate::git_cmd::git_stdout;
use crate::handlers::reset::has_seed;
use crate::state::{current, current_handle, repo_label, resolve_worktree, HISTORY_LIMIT};

/// The optional opaque repository selector shared by the read endpoints (M1.03):
/// `?repo=<worktree-id>` addresses one servable worktree by its opaque id. When
/// absent, the endpoint acts on the server's current default selection — the
/// backward-compatible behaviour the existing single-repo frontend relies on
/// until it adopts ids (M1.11).
#[derive(Deserialize)]
pub(crate) struct RepoQuery {
    #[serde(default)]
    repo: Option<String>,
}

/// Resolve the `?repo=` selector to a concrete repository, failing closed. A
/// malformed id is a `400`; an id the catalog does not hold is a `404` — the
/// server only ever resolves an id it itself registered, never a path from the
/// request. An absent selector falls back to the current default selection.
fn resolve_repo(
    repo: Option<&str>,
) -> Result<(PathBuf, bool, Option<RepositoryHandle>), (StatusCode, String)> {
    match repo {
        None => {
            let (path, read_only) = current();
            Ok((path, read_only, current_handle()))
        }
        Some(id) => {
            let worktree: WorktreeId = id
                .parse()
                .map_err(|_| (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()))?;
            let (path, read_only, handle) = resolve_worktree(worktree)
                .ok_or((StatusCode::NOT_FOUND, "No such repository.".to_string()))?;
            Ok((path, read_only, Some(handle)))
        }
    }
}

/// Walk the configured repository (see [`repo_path`]) and return its laid-out
/// graph as JSON, with branch/tag/HEAD refs attached for badging and per-branch
/// colouring.
///
/// Sent `Cache-Control: no-store` so the browser never caches the graph: the repo
/// changes underneath us (new commits, new/switched branches) between launches,
/// and iOS Safari's on-disk cache otherwise persists a stale graph across app —
/// and even device — restarts, making freshly created branches never appear.
pub(crate) async fn commits(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (repo, read_only, handle) = resolve_repo(q.repo.as_deref())?;
    let repo = repo.as_path();
    let history = walk_history(repo, HISTORY_LIMIT).map_err(|e| {
        eprintln!("git-vista: /api/commits failed reading history: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let refs = read_refs(repo).map_err(|e| {
        eprintln!("git-vista: /api/commits failed reading refs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // Log, to the local terminal, exactly which branches this read found. This is
    // the diagnostic for issue #16's "a new branch doesn't show up": if the branch
    // you just made is listed here but not on the iPad, the graph is being read
    // fine and the browser is showing a cached copy; if it's missing here, the
    // problem is the repo being served (wrong path) or a ref the walk couldn't read.
    log_commits_summary(repo, &history, &refs);
    // The checked-out branch owns its line, so a branch just created from its tip
    // is the one drawn as a new stub line (not the trunk). See `layout_with_refs`.
    let head_branch = git_vista_git::read_head_branch(repo);
    let mut graph = layout::layout_with_refs(history, refs, head_branch.as_deref());
    // Tell the UI which repo this graph came from, as a short non-path label so
    // the header can show *which* repo without leaking the server's filesystem
    // (M1.03; the full path only when the operator opts into `GIT_VISTA_EXPOSE_PATHS`).
    graph.repo_label = Some(repo_label(repo));
    // Stamp the opaque ids the client addresses this repo by (M1.03), so a later
    // request can select this exact worktree with `?repo=`. Absent in degraded mode.
    if let Some(handle) = handle {
        graph.repo_id = Some(handle.repository.to_string());
        graph.worktree_id = Some(handle.worktree.to_string());
    }
    // A cloned URL is view-only: tell the UI to hide every write action.
    graph.read_only = read_only;
    // Offer "Reset Test Repo" only for a repo explicitly opted in with
    // `gv --seed` (the seed files exist) — and never on a read-only clone.
    graph.resettable = !read_only && has_seed(repo);
    // Attach the GitHub web base (if this repo has a github.com origin) so the UI
    // can link commits and refs. None => the frontend renders plain-text labels.
    graph.repo_url = git_vista_git::github_web_base(repo);
    // Mark which commits are on the remote, so the UI only links pushed objects —
    // an unpushed commit/ref would 404 on GitHub. Only worth computing when we
    // have a web base to link to; on failure we leave it empty (nothing linked).
    if graph.repo_url.is_some() {
        if let Ok(remote) = git_vista_git::read_remote_commits(repo, HISTORY_LIMIT) {
            graph.remote_commits = remote.into_iter().collect();
        }
    }
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(graph)))
}

/// Full detail for one commit (Phase 10 — the detail panel): the whole message
/// body plus the author and committer signatures, looked up by hex id in the
/// current repo. A read, so it works on read-only clones too. A bad or unknown id
/// is a `404`; any other read failure a `500`. Sent `no-store` like the graph.
pub(crate) async fn commit_detail(
    AxumPath(id): AxumPath<String>,
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let detail = read_commit(&repo, &id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/commit/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(detail)))
}

/// Upper bound on the patch text returned by `/api/diff/{id}`. A huge commit
/// (vendored deps, generated files) can carry a multi-megabyte patch that would
/// choke the iPad both in transfer and in rendering; past this, the patch is
/// cut at a line boundary and flagged `truncated` so the panel says so.
const DIFF_PATCH_CAP: usize = 200_000;

/// The cap when `?full=1` is passed — the full-screen diff viewer asks for the
/// whole patch (it exists to escape the panel's cap), but a hard ceiling still
/// protects the iPad's tab from a truly pathological multi-hundred-MB diff.
const DIFF_PATCH_CAP_FULL: usize = 5_000_000;

/// Query of `GET /api/diff/{id}`: `full=1` lifts the patch cap to
/// [`DIFF_PATCH_CAP_FULL`] for the full-screen viewer.
#[derive(Deserialize)]
pub(crate) struct DiffQuery {
    #[serde(default)]
    full: Option<u8>,
    /// Opaque repository selector (M1.03), same meaning as [`RepoQuery::repo`].
    #[serde(default)]
    repo: Option<String>,
}

/// One commit's diff (Activity/Undo feature, step 2): the per-file change list
/// and the unified patch, for the detail panel's Changes section.
///
/// The commit is first resolved via gix ([`read_commit`]) — validating the id
/// and yielding the parent count — then git itself produces the diff (same B3
/// posture as everywhere: git's diff engine handles renames, binaries and
/// merges; we only parse its machine-readable listings, in core, where that's
/// unit-tested). Three reads of the same diff:
///
///   * `--name-status -z` — the file list (order + change kinds),
///   * `--numstat -z`     — per-file added/deleted counts folded into it,
///   * `--patch`          — the unified text, capped at [`DIFF_PATCH_CAP`].
///
/// An ordinary commit uses `git show` (its diff vs the parent; a root commit
/// diffs against the empty tree). A merge commit is diffed against its *first
/// parent* instead — `git show` on a merge prints the usually-empty combined
/// diff, while "what did this merge bring in?" is exactly the first-parent
/// diff — and the response says so (`against_first_parent`).
pub(crate) async fn commit_diff(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<DiffQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(query.repo.as_deref())?.0;
    // Belt-and-braces before the id goes anywhere near argv: real ids are hex.
    if id.len() < 4 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }
    let detail = read_commit(&repo, &id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/diff/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let against_first_parent = detail.parents.len() >= 2;

    // The base args for the three reads: `show <id>` for ordinary commits,
    // `diff <id>^1 <id>` for merges. `--format=` silences show's commit header
    // so only the diff comes back; harmless on `diff`... which doesn't take it,
    // so the merge arm simply omits it.
    let first_parent = format!("{id}^1");
    let base: Vec<&str> = if against_first_parent {
        vec!["diff", first_parent.as_str(), id.as_str()]
    } else {
        vec!["show", "--format=", id.as_str()]
    };
    let with = |extra: &[&str]| -> Vec<String> {
        // Diff options go *before* the revisions so git never reads a
        // revision as an option's value.
        let mut args = vec![base[0].to_string()];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.extend(base[1..].iter().map(|s| s.to_string()));
        args
    };

    let name_status = git_stdout(&repo, &with(&["--name-status", "-z"]), "/api/diff").await?;
    let numstat = git_stdout(&repo, &with(&["--numstat", "-z"]), "/api/diff").await?;
    let patch_bytes = git_stdout(&repo, &with(&["--patch", "--no-color"]), "/api/diff").await?;

    let mut files = git_vista_core::diff::parse_name_status_z(&name_status);
    git_vista_core::diff::fold_numstat_z(&numstat, &mut files);

    // Cap the patch at a line boundary so the panel never gets half a line.
    // The full-screen viewer (`?full=1`) gets the much higher ceiling.
    let cap = if query.full == Some(1) {
        DIFF_PATCH_CAP_FULL
    } else {
        DIFF_PATCH_CAP
    };
    let mut patch = String::from_utf8_lossy(&patch_bytes).into_owned();
    let truncated = patch.len() > cap;
    if truncated {
        truncate_at_line(&mut patch, cap);
    }

    let diff = git_vista_core::diff::CommitDiff {
        id: detail.id.0,
        files,
        patch,
        truncated,
        against_first_parent,
    };
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(diff)))
}

/// Cut `text` down to at most `cap` bytes, at the last full line before the
/// cap. The cap is first walked back to a char boundary so a multi-byte
/// character straddling it can't panic the slice.
fn truncate_at_line(text: &mut String, cap: usize) {
    let mut end = cap.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].rfind('\n').unwrap_or(end);
    text.truncate(cut);
}

/// Upper bound on the text returned by `/api/file/{id}/{path}` — same iPad
/// protection as the diff caps; past this the content is cut at a line
/// boundary and flagged `truncated`.
const FILE_CONTENT_CAP: usize = 2_000_000;

/// One file's full content at one commit (`GET /api/file/{id}/{*path}`), for
/// the full file viewer opened from the diff's file list.
///
/// `git show <id>:<path>` does the reading — the same B3 posture as the diff:
/// git resolves the path inside the tree and reports a clear error when it
/// isn't there. A path deleted *by* this commit doesn't exist in its tree, so
/// on failure the first parent (`<id>^:<path>`) is tried before giving up —
/// tapping a deleted file then shows what was deleted rather than an error.
/// Binary blobs (NUL bytes near the start) come back flagged, not as garbage.
pub(crate) async fn file_at_commit(
    AxumPath((id, path)): AxumPath<(String, String)>,
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    // Same belt-and-braces as the diff: real ids are hex, and the id leads the
    // `<id>:<path>` argument, so neither half can ever read as an option.
    if id.len() < 4 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }
    let show = |spec: String| {
        let repo = repo.clone();
        async move { git_stdout(&repo, &["show".to_string(), spec], "/api/file").await }
    };
    let bytes = match show(format!("{id}:{path}")).await {
        Ok(bytes) => bytes,
        // Not in this commit's tree — a file this commit deleted. Show the
        // version it deleted (from the first parent) instead of a dead end.
        Err(first) => show(format!("{id}^:{path}")).await.map_err(|_| first)?,
    };

    // Binary sniff, the way git itself does it: a NUL in the first 8000 bytes.
    let binary = bytes.iter().take(8000).any(|&b| b == 0);
    let (content, truncated) = if binary {
        (String::new(), false)
    } else {
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        let truncated = text.len() > FILE_CONTENT_CAP;
        if truncated {
            truncate_at_line(&mut text, FILE_CONTENT_CAP);
        }
        (text, truncated)
    };

    let file = git_vista_core::diff::FileContent {
        id,
        path,
        content,
        truncated,
        binary,
    };
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(file)))
}

/// The currently checked-out branch, resolved fresh (Issue #33 follow-up). The
/// merge dialog fetches this the moment the user clicks "Merge", so it names the
/// real target even if the graph on screen is a stale snapshot from before a branch
/// switch. `null` => detached HEAD. Sent `no-store` so it's never served from cache.
pub(crate) async fn head_branch(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(git_vista_git::read_head_branch(&repo))))
}

/// The working-tree status (Activity/Undo feature, step 1): the parsed output
/// of `git status --porcelain=v2 --branch`, resolved fresh on every request.
///
/// Shelling out to `git status` rather than assembling this from gix keeps the
/// B3 posture of the write endpoints — git itself decides what's staged /
/// modified / conflicted, including every corner case (renames, type changes,
/// sparse checkouts) — and the pure parser lives in core where it's unit-
/// tested. A read, so it works on read-only clones too. Sent `no-store` like
/// the other live reads: the answer changes with every edit in the worktree.
pub(crate) async fn worktree_status(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["status", "--porcelain=v2", "--branch"])
        .output()
        .await
        .map_err(|e| {
            eprintln!("git-vista: /api/status couldn't run git: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            )
        })?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            "git status failed.".to_string()
        } else {
            msg
        };
        eprintln!("git-vista: /api/status failed: {msg}");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
    }
    let parsed = parse_porcelain_v2(&String::from_utf8_lossy(&output.stdout));
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(parsed)))
}

/// Print, to the local terminal, a one-line summary of what a `/api/commits` read
/// found: the repo served, the commit count, and — crucially — the local branch
/// names. It's the fastest answer to "I made a branch and it isn't showing":
/// reload the page and look here. If the branch is in this list, the server sees
/// it and the browser is caching a stale graph; if it's absent, the server is
/// reading the wrong repo or couldn't read the ref (see any warnings above).
fn log_commits_summary(repo: &Path, history: &[CommitSummary], refs: &[GitRef]) {
    let mut local = Vec::new();
    let (mut remote, mut tags, mut has_head) = (0usize, 0usize, false);
    for r in refs {
        match r.kind {
            RefKind::Branch => local.push(r.name.as_str()),
            RefKind::RemoteBranch => remote += 1,
            RefKind::Tag => tags += 1,
            RefKind::Head => has_head = true,
        }
    }
    println!(
        "[/api/commits] {} — {} commit(s); {} local branch(es) [{}]; {remote} remote, {tags} tag(s){}",
        repo.display(),
        history.len(),
        local.len(),
        local.join(", "),
        if has_head { "; HEAD" } else { "" },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use git_vista_protocol::RepositoryDescriptor;
    use tower::ServiceExt;

    async fn status_of(app: Router, uri: &str) -> StatusCode {
        let req = axum::http::Request::get(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn repo_selector_rejects_a_malformed_id_as_bad_request() {
        // A `?repo=` that isn't even a valid id never reaches path resolution.
        let app = Router::new().route("/api/head-branch", get(head_branch));
        let status = status_of(app, "/api/head-branch?repo=not-an-id").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn repo_selector_fails_closed_on_an_unknown_id() {
        // A well-formed id the catalog never registered resolves to nothing — the
        // request is refused with a 404 rather than falling back to any path.
        let unknown = WorktreeId::from_git_dir("/no/such/repo/.git").to_string();
        let app = Router::new().route("/api/head-branch", get(head_branch));
        let status = status_of(app, &format!("/api/head-branch?repo={unknown}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn catalog_endpoint_lists_entries_without_leaking_paths() {
        // The capability report is valid JSON and, by default, carries no paths.
        let app = Router::new().route("/api/catalog", get(crate::handlers::catalog::catalog_list));
        let req = axum::http::Request::get("/api/catalog")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        // Deserialises as the descriptor list, and no descriptor carries a path.
        let list: Vec<RepositoryDescriptor> = serde_json::from_slice(&bytes).unwrap();
        assert!(list.iter().all(|d| d.path.is_none()));
    }
}
