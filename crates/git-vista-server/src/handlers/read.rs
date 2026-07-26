//! The read endpoints (all `no-store` GETs): the laid-out history graph, one
//! commit's detail and diff, and the two live "state" reads (checked-out branch,
//! working-tree status). Reads, so they work on read-only clones too.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use git_vista_core::identity::{RepositoryHandle, WorktreeId};
use git_vista_core::layout;
use git_vista_core::model::{CommitDetail, CommitSummary, GitRef, RefKind};
use git_vista_core::status::parse_porcelain_v2;
use git_vista_git::{read_commit, read_refs, walk_history, RepoError};

use crate::git_cmd::git_stdout_capped;
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
    // Any-host web base (ADR 0010) for the general forge links; repo_url above
    // stays GitHub-only for the existing pushed-commit link behavior.
    graph.remote_web_url = git_vista_git::remote_web_base(repo);
    // Mark which commits are on the remote, so the UI only links pushed objects —
    // an unpushed commit/ref would 404 on the forge. Only worth computing when we
    // have a web base to link to (either the GitHub-only base or the any-host
    // one); on failure we leave it empty (nothing linked).
    if graph.repo_url.is_some() || graph.remote_web_url.is_some() {
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
    let detail = commit_detail_for_repo(&repo, &id)?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(detail)))
}

/// Read one commit and stamp its **exact** remote reachability (M1.10, #63).
///
/// Split out from the handler so a test can drive it against a temp repository:
/// the handler itself resolves the repo from the process-wide selection. The
/// remote flag comes from a singleton `remote_membership` query rather than from
/// a capped prefix of remote history — the commit a user opens is routinely far
/// below the loaded page, and a truncated answer would call a pushed commit
/// unpushed and refuse to link it.
///
/// A remote-scan failure leaves the flag `false` (the same lenient posture the
/// graph read takes): the panel loses a link, it does not lose the commit.
fn commit_detail_for_repo(repo: &Path, id: &str) -> Result<CommitDetail, (StatusCode, String)> {
    let mut detail = read_commit(repo, id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/commit/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let requested = HashSet::from([detail.id.clone()]);
    match git_vista_git::remote_membership(repo, &requested) {
        Ok(found) => detail.on_remote = found.contains(&detail.id),
        Err(e) => eprintln!("git-vista: /api/commit/{id} could not scan remotes: {e}"),
    }
    Ok(detail)
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

/// Upper bound on each `-z` metadata read (`--name-status`, `--numstat`).
/// Unlike the patch, this one is *not* a display cap: the `-z` parsers stop
/// cleanly at a short record, so a truncated metadata read would render as a
/// plausible — and wrong — shorter list of changed files. Crossing it is
/// therefore an explicit `413`, never a partial answer (M1.10, #63).
const DIFF_METADATA_CAP: usize = 8 * 1024 * 1024;

/// The cap for the patch read: the panel's, or the full-screen viewer's.
fn patch_cap(full: bool) -> usize {
    if full {
        DIFF_PATCH_CAP_FULL
    } else {
        DIFF_PATCH_CAP
    }
}

/// The refusal for a metadata read that crossed [`DIFF_METADATA_CAP`].
fn diff_metadata_too_large() -> (StatusCode, String) {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        "diff metadata exceeded 8 MiB".to_string(),
    )
}

/// The three argv vectors for one commit's diff, in read order:
/// `[--name-status -z, --numstat -z, --patch]`.
///
/// An ordinary commit is read with `git show --format= <id>` (its diff vs the
/// parent; a root commit diffs against the empty tree). A merge is read with
/// `git diff <id>^1 <id>` — `git show` on a merge prints the usually-empty
/// combined diff, while "what did this merge bring in?" is exactly the
/// first-parent diff.
///
/// Every vector carries `--no-textconv`: a repository-configured textconv
/// filter would otherwise be allowed to expand a binary blob into the patch —
/// arbitrary, unbounded work on somebody else's `.gitattributes`. Options are
/// placed *before* the revisions so git can never read a revision as an
/// option's value, and `--binary` is deliberately absent so binary files stay
/// the one-line "Binary files … differ" and never inline their bytes.
fn diff_argv(id: &str, against_first_parent: bool) -> [Vec<String>; 3] {
    let first_parent = format!("{id}^1");
    let base: Vec<&str> = if against_first_parent {
        vec!["diff", first_parent.as_str(), id]
    } else {
        vec!["show", "--format=", id]
    };
    let with = |extra: &[&str]| -> Vec<String> {
        let mut args = vec![base[0].to_string()];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.extend(base[1..].iter().map(|s| s.to_string()));
        args
    };
    [
        with(&["--name-status", "-z", "--no-textconv"]),
        with(&["--numstat", "-z", "--no-textconv"]),
        with(&["--patch", "--no-color", "--no-textconv"]),
    ]
}

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
    let diff = commit_diff_for_repo(&repo, &id, query.full == Some(1), DIFF_METADATA_CAP).await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(diff)))
}

/// [`commit_diff`] against an explicit repository — everything the endpoint does
/// once `?repo=` has been resolved.
///
/// Split out for two reasons. It is the only way to drive this code under test:
/// the handler resolves its repository from the process-wide `CURRENT`
/// selection, which panics when unset and has no test-time setter. And it takes
/// `metadata_cap` as an argument so a test can cross that ceiling with a
/// four-file commit instead of a fixture that really emits 8 MiB of `-z`
/// records; production always passes [`DIFF_METADATA_CAP`].
///
/// All three reads are *bounded*: each one streams into a buffer that never
/// reserves more than its cap, and git is killed and reaped the moment the cap
/// is full (M1.10, #63). The patch cap is a display cap — cutting it is normal
/// and reported as `truncated`. The metadata caps are not: crossing one is a
/// `413`, because a short `-z` read parses into a plausible, wrong file list.
async fn commit_diff_for_repo(
    repo: &Path,
    id: &str,
    full: bool,
    metadata_cap: usize,
) -> Result<git_vista_core::diff::CommitDiff, (StatusCode, String)> {
    // Belt-and-braces before the id goes anywhere near argv: real ids are hex.
    if id.len() < 4 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }
    let detail = read_commit(repo, id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/diff/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let against_first_parent = detail.parents.len() >= 2;
    let [name_args, numstat_args, patch_args] = diff_argv(id, against_first_parent);

    let (name_status, names_truncated) =
        git_stdout_capped(repo, &name_args, "/api/diff", metadata_cap).await?;
    if names_truncated {
        return Err(diff_metadata_too_large());
    }
    let (numstat, numstat_truncated) =
        git_stdout_capped(repo, &numstat_args, "/api/diff", metadata_cap).await?;
    if numstat_truncated {
        return Err(diff_metadata_too_large());
    }
    let (patch_bytes, truncated) =
        git_stdout_capped(repo, &patch_args, "/api/diff", patch_cap(full)).await?;

    let mut files = git_vista_core::diff::parse_name_status_z(&name_status);
    git_vista_core::diff::fold_numstat_z(&numstat, &mut files);

    // `truncated` is the *reader's* byte-level fact, and it is authoritative.
    // It is deliberately not re-derived from the decoded string's length:
    // `from_utf8_lossy` expands each invalid byte to a 3-byte U+FFFD, so a
    // complete sub-cap patch can decode to more than the cap and would then be
    // reported as cut short when nothing was cut. On the truncated path
    // `truncate_at_line` only tidies the already-bounded bytes, so the panel
    // never gets half a line.
    let mut patch = String::from_utf8_lossy(&patch_bytes).into_owned();
    if truncated {
        truncate_at_line(&mut patch, patch_cap(full));
    }

    Ok(git_vista_core::diff::CommitDiff {
        id: detail.id.0,
        files,
        patch,
        truncated,
        against_first_parent,
    })
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
    let file = file_at_commit_for_repo(&repo, &id, &path).await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(file)))
}

/// [`file_at_commit`] against an explicit repository — split out for the same
/// reason as [`commit_diff_for_repo`]: the handler's repository comes from the
/// process-wide `CURRENT` selection, which no test can set.
///
/// The read is bounded at [`FILE_CONTENT_CAP`], and a cap hit is a *success*:
/// `Ok((bytes, true))` means "this file exists and is bigger than we will
/// serve", which is a truncated 200 and emphatically **not** the missing-object
/// case. Only a genuine error retries `<id>^:<path>` — that retry exists for a
/// file this commit deleted, and answering a cap with the parent's older
/// content would be a wrong answer wearing a 200 (M1.10, #63).
async fn file_at_commit_for_repo(
    repo: &Path,
    id: &str,
    path: &str,
) -> Result<git_vista_core::diff::FileContent, (StatusCode, String)> {
    // Same belt-and-braces as the diff: real ids are hex, and the id leads the
    // `<id>:<path>` argument, so neither half can ever read as an option.
    if id.len() < 4 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }
    let show = |spec: String| async move {
        git_stdout_capped(
            repo,
            &["show".to_string(), spec],
            "/api/file",
            FILE_CONTENT_CAP,
        )
        .await
    };
    let (bytes, truncated) = match show(format!("{id}:{path}")).await {
        Ok(read) => read,
        // Not in this commit's tree — a file this commit deleted. Show the
        // version it deleted (from the first parent) instead of a dead end.
        Err(first) => show(format!("{id}^:{path}")).await.map_err(|_| first)?,
    };

    // Binary sniff, the way git itself does it: a NUL in the first 8000 bytes.
    // The cap always retains far more than that, so bounding the read cannot
    // change this verdict.
    let binary = bytes.iter().take(8000).any(|&b| b == 0);
    let (content, truncated) = if binary {
        (String::new(), false)
    } else {
        // `truncated` is the reader's byte-level fact; it is not re-derived from
        // the decoded length (see `commit_diff_for_repo`). `truncate_at_line`
        // only tidies bytes the cap already bounded, so the viewer never gets
        // half a line.
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            truncate_at_line(&mut text, FILE_CONTENT_CAP);
        }
        (text, truncated)
    };

    Ok(git_vista_core::diff::FileContent {
        id: id.to_string(),
        path: path.to_string(),
        content,
        truncated,
        binary,
    })
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

    // ---- bounded diff/file reads (M1.10, #63) --------------------------------
    //
    // These drive the `*_for_repo` seams directly. They cannot go through the
    // axum handlers: those resolve the repository from the process-wide
    // `CURRENT` selection, which panics when unset and has no test-time setter.

    /// `git <args…>` in `repo`; asserts success. Same shape as the planner
    /// suites' fixtures, duplicated because those helpers are private to their
    /// own modules and unreachable from here.
    fn run(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {repo:?}");
    }

    /// `git <args…>` in `repo`, returning trimmed stdout; asserts success.
    fn out(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {repo:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// The exact byte length of `git -C <repo> <args…>`'s stdout. The metadata
    /// tests size their injected cap off this, so the fixture never has to grow
    /// to the real 8 MiB ceiling to exercise both cap branches.
    fn stdout_len(repo: &Path, args: &[String]) -> usize {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {repo:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout.len()
    }

    /// A fresh repository on branch `main` with one committed file.
    fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "seed"]);
        (dir, repo)
    }

    /// A repository whose HEAD commit modifies several files — enough `-z`
    /// metadata to cross a test-sized cap, nowhere near enough to need a real
    /// 8 MiB fixture.
    fn repo_with_multi_file_commit() -> (tempfile::TempDir, PathBuf, String) {
        let (dir, repo) = seeded_repo();
        for i in 0..4 {
            std::fs::write(repo.join(format!("file-{i}.txt")), "one\n").unwrap();
        }
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "add files"]);
        for i in 0..4 {
            std::fs::write(repo.join(format!("file-{i}.txt")), "two\n").unwrap();
        }
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "modify files"]);
        let id = out(&repo, &["rev-parse", "HEAD"]);
        (dir, repo, id)
    }

    /// Every diff read disables textconv (a configured textconv filter could
    /// otherwise dump a binary blob into the patch), keeps options ahead of the
    /// revisions, and the caps are the explicit, named ones — not whatever the
    /// fail-safe wrapper happens to use.
    #[test]
    fn bounded_diff_argv_uses_explicit_caps_and_no_textconv() {
        let id = "a".repeat(40);
        let ordinary = diff_argv(&id, false);
        let merge = diff_argv(&id, true);

        for argv in ordinary.iter().chain(merge.iter()) {
            assert!(
                argv.contains(&"--no-textconv".to_string()),
                "every diff read must disable textconv: {argv:?}"
            );
            assert!(
                !argv.contains(&"--binary".to_string()),
                "binary content must never be inlined: {argv:?}"
            );
        }

        // Read order is [name-status, numstat, patch], each with its own shape.
        assert!(ordinary[0].contains(&"--name-status".to_string()));
        assert!(ordinary[0].contains(&"-z".to_string()));
        assert!(ordinary[1].contains(&"--numstat".to_string()));
        assert!(ordinary[1].contains(&"-z".to_string()));
        assert!(ordinary[2].contains(&"--patch".to_string()));
        assert!(ordinary[2].contains(&"--no-color".to_string()));

        // Ordinary commit: `show … --format= <id>`; the revision is last, so no
        // option can ever swallow it.
        for argv in ordinary.iter() {
            assert_eq!(argv[0], "show");
            assert_eq!(argv.last().unwrap(), &id);
            assert_eq!(argv[argv.len() - 2], "--format=");
        }
        // Merge: `diff … <id>^1 <id>`, again with the revisions trailing.
        for argv in merge.iter() {
            assert_eq!(argv[0], "diff");
            assert_eq!(argv[argv.len() - 2], format!("{id}^1"));
            assert_eq!(argv.last().unwrap(), &id);
        }

        // The caps the reads are handed are explicit and named.
        assert_eq!(patch_cap(false), DIFF_PATCH_CAP);
        assert_eq!(patch_cap(true), DIFF_PATCH_CAP_FULL);
        assert_eq!(DIFF_PATCH_CAP, 200_000);
        assert_eq!(DIFF_PATCH_CAP_FULL, 5_000_000);
        assert_eq!(DIFF_METADATA_CAP, 8 * 1024 * 1024);
        assert_eq!(FILE_CONTENT_CAP, 2_000_000);
    }

    /// A `--name-status -z` read that hits the metadata cap is an explicit 413.
    /// It must never come back as a *partial* file list: the `-z` parsers stop
    /// cleanly on a short record, so a silently truncated read would render as a
    /// plausible, wrong, shorter list of changed files.
    #[tokio::test]
    async fn bounded_diff_name_status_cap_returns_413() {
        let (_dir, repo, id) = repo_with_multi_file_commit();
        let [name_args, ..] = diff_argv(&id, false);
        let names_len = stdout_len(&repo, &name_args);
        assert!(names_len > 4, "fixture must exceed the injected cap");

        // Exactly what the guard exists to prevent: those same 4 bytes parse —
        // without complaint, because the `-z` parsers stop cleanly at a short
        // record — into a plausible file list that is simply wrong.
        let (partial, truncated) = git_stdout_capped(&repo, &name_args, "test", 4)
            .await
            .unwrap();
        assert!(truncated);
        let plausible = git_vista_core::diff::parse_name_status_z(&partial);
        assert!(
            plausible.len() < 4,
            "a short read parses to a shorter list: {plausible:?}"
        );

        let (status, msg) = commit_diff_for_repo(&repo, &id, false, 4)
            .await
            .expect_err("a truncated name-status read is an error, not a short list");

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(msg, "diff metadata exceeded 8 MiB");
    }

    /// The same for `--numstat -z`, reached with a cap sized to exactly the
    /// name-status output: that read fills the cap without truncating (the
    /// reader's probe byte tells "exactly cap" from "more"), so only the
    /// strictly larger numstat read crosses it.
    #[tokio::test]
    async fn bounded_diff_numstat_cap_returns_413() {
        let (_dir, repo, id) = repo_with_multi_file_commit();
        let [name_args, numstat_args, _patch_args] = diff_argv(&id, false);
        let names_len = stdout_len(&repo, &name_args);
        let numstat_len = stdout_len(&repo, &numstat_args);
        assert!(
            numstat_len > names_len,
            "fixture invariant: numstat ({numstat_len}) must outgrow name-status ({names_len})"
        );

        let (status, msg) = commit_diff_for_repo(&repo, &id, false, names_len)
            .await
            .expect_err("a truncated numstat read is an error, not missing counts");
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(msg, "diff metadata exceeded 8 MiB");

        // Control: one byte of headroom past the larger read and the very same
        // commit succeeds — so the 413 above was the numstat cap, not a
        // name-status read that mis-reports an exactly-cap-sized output.
        let diff = commit_diff_for_repo(&repo, &id, false, numstat_len)
            .await
            .expect("both metadata reads fit at the larger cap");
        assert_eq!(diff.files.len(), 4);
        assert!(diff.files.iter().all(|f| f.additions == Some(1)));
    }

    /// Write a file of about `len` bytes: `header` first, then deterministic
    /// fixed-size rows. Streamed through a `BufWriter` rather than built in
    /// memory so a 50 MiB fixture costs the test almost nothing, and generated
    /// from the running offset so a longer file is a byte-identical *prefix*
    /// extension of a shorter one (which is what makes an "append" diff cheap
    /// for git to compute). No shell helper is involved — `yes`/`dd`/`head` are
    /// banned by the argv boundary, and every child these tests spawn is
    /// literally `git`.
    fn write_rows(path: &Path, header: &str, len: usize, tag: &str) {
        use std::io::Write;
        let mut w = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        w.write_all(header.as_bytes()).unwrap();
        let mut written = header.len();
        while written < len {
            let row = format!("{written:012} {tag} bounded-read fixture row\n");
            let take = row.len().min(len - written);
            w.write_all(&row.as_bytes()[..take]).unwrap();
            written += take;
        }
        w.flush().unwrap();
    }

    /// A file read that hits the content cap is a *successful truncated file*,
    /// not a missing object. It must therefore never fall through to the
    /// `<id>^:<path>` fallback — that fallback exists for a file this commit
    /// *deleted*, and silently answering a cap with the parent's older content
    /// would be a wrong answer wearing a 200.
    #[tokio::test]
    async fn bounded_file_read_caps_without_parent_fallback() {
        let (_dir, repo) = seeded_repo();
        // The parent's version is small and unmistakable: if a cap ever fell
        // through to the fallback, this is what would come back.
        std::fs::write(repo.join("big.txt"), "PARENT-VERSION\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "parent"]);
        // The commit under test replaces it with a file past the 2 MB cap,
        // carrying its own marker on line one.
        write_rows(
            &repo.join("big.txt"),
            "CHILD-VERSION\n",
            FILE_CONTENT_CAP + 500_000,
            "child",
        );
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "child"]);
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let file = file_at_commit_for_repo(&repo, &id, "big.txt")
            .await
            .expect("a capped read of an existing file is a success, not an error");

        assert!(file.truncated, "the cap hit must be reported");
        assert!(!file.binary);
        assert!(
            file.content.len() <= FILE_CONTENT_CAP,
            "content kept {} bytes, cap is {FILE_CONTENT_CAP}",
            file.content.len()
        );
        assert!(
            file.content.starts_with("CHILD-VERSION\n"),
            "the cap must not fall back to the parent's version"
        );
        assert!(!file.content.contains("PARENT-VERSION"));
        assert_eq!(file.id, id);
        assert_eq!(file.path, "big.txt");

        // The fallback itself still works, for the case it was written for: a
        // file this commit deleted is served from the first parent.
        run(&repo, &["rm", "-q", "big.txt"]);
        run(&repo, &["commit", "-q", "-m", "delete"]);
        let deleted_at = out(&repo, &["rev-parse", "HEAD"]);
        let deleted = file_at_commit_for_repo(&repo, &deleted_at, "big.txt")
            .await
            .expect("a file deleted by this commit is served from its parent");
        assert!(deleted.content.starts_with("CHILD-VERSION\n"));
        assert!(deleted.truncated);
    }

    /// Roughly the size of the text fixture's first version.
    const BIG_TEXT_BYTES: usize = 50 * 1024 * 1024;
    /// How much the second version appends — comfortably past both patch caps.
    const BIG_TEXT_APPEND: usize = 8 * 1024 * 1024;
    /// A string that appears only inside the binary blob. If it ever shows up in
    /// a patch, binary bytes reached the wire.
    const BINARY_SENTINEL: &str = "GV-BINARY-SENTINEL-PAYLOAD";

    fn on_disk_len(path: &Path) -> usize {
        std::fs::metadata(path).unwrap().len() as usize
    }

    /// `len` bytes of binary content: NUL-delimited sentinel runs. The leading
    /// NUL is inside the first 8000 bytes, which is what makes both git and our
    /// own sniff call this binary.
    fn binary_blob(tag: &str, len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len);
        bytes.push(0u8);
        bytes.extend_from_slice(tag.as_bytes());
        while bytes.len() < len {
            bytes.push(0u8);
            bytes.extend_from_slice(BINARY_SENTINEL.as_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    /// A repository whose HEAD commit modifies both a ~50 MiB text file and a
    /// NUL-bearing binary blob.
    ///
    /// Two deliberate choices. `bin.dat` sorts before `zbig.txt`, so git's patch
    /// leads with the binary section — otherwise the 200 KB panel cap would cut
    /// away the very "Binary files … differ" line the test is about. And the
    /// text change is an *append*: git trims the identical 50 MiB prefix in one
    /// pass, so the fixture stays a fixture instead of a minutes-long diff,
    /// while still producing a patch far past both patch caps.
    fn pathological_repo() -> (tempfile::TempDir, PathBuf, String) {
        let (dir, repo) = seeded_repo();
        write_rows(&repo.join("zbig.txt"), "ZBIG\n", BIG_TEXT_BYTES, "alpha");
        std::fs::write(repo.join("bin.dat"), binary_blob("one", 64 * 1024)).unwrap();
        assert_eq!(on_disk_len(&repo.join("zbig.txt")), BIG_TEXT_BYTES);
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "add pathological content"]);

        write_rows(
            &repo.join("zbig.txt"),
            "ZBIG\n",
            BIG_TEXT_BYTES + BIG_TEXT_APPEND,
            "alpha",
        );
        assert_eq!(
            on_disk_len(&repo.join("zbig.txt")),
            BIG_TEXT_BYTES + BIG_TEXT_APPEND,
            "the fixture must really be ~50 MiB, not a silently short write"
        );
        std::fs::write(repo.join("bin.dat"), binary_blob("two", 96 * 1024)).unwrap();
        run(&repo, &["add", "-A"]);
        run(
            &repo,
            &["commit", "-q", "-m", "modify pathological content"],
        );

        let id = out(&repo, &["rev-parse", "HEAD"]);
        (dir, repo, id)
    }

    /// The whole point of the milestone, driven through the real handler helper:
    /// a commit no iPad could ever render still comes back bounded, honestly
    /// flagged, and with the binary blob's bytes nowhere near the wire.
    #[tokio::test]
    async fn bounded_diff_handles_large_text_and_binary_without_blob_leak() {
        let (_dir, repo, id) = pathological_repo();

        let panel = commit_diff_for_repo(&repo, &id, false, DIFF_METADATA_CAP)
            .await
            .expect("a pathological commit still answers, bounded");

        assert!(panel.truncated, "a 50 MiB change must report truncation");
        assert!(
            panel.patch.len() <= DIFF_PATCH_CAP,
            "panel patch kept {} bytes, cap is {DIFF_PATCH_CAP}",
            panel.patch.len()
        );
        // git *names* the binary file rather than printing it — with neither
        // `--binary` nor textconv, its bytes have no way onto the wire.
        assert!(
            panel
                .patch
                .contains("Binary files a/bin.dat and b/bin.dat differ"),
            "git's binary line must survive the cap; patch starts: {:?}",
            &panel.patch[..panel.patch.len().min(300)]
        );
        assert!(
            !panel.patch.contains(BINARY_SENTINEL),
            "the blob's bytes leaked into the patch"
        );
        assert!(
            !panel.patch.contains('\0'),
            "NUL bytes leaked into the patch"
        );

        // The metadata is complete even though the patch was cut: the binary
        // file carries git's `-`/`-` counts (i.e. `None`), the text file real
        // ones. The text file is the positive control — without it, `None`
        // could equally mean the numstat fold matched nothing at all.
        let bin = panel
            .files
            .iter()
            .find(|f| f.path == "bin.dat")
            .expect("bin.dat is in the file list");
        assert_eq!(bin.additions, None);
        assert_eq!(bin.deletions, None);
        let text = panel
            .files
            .iter()
            .find(|f| f.path == "zbig.txt")
            .expect("zbig.txt is in the file list");
        assert!(
            text.additions.unwrap_or(0) > 0,
            "the numstat fold must have matched the text file: {text:?}"
        );

        // `?full=1` lifts the panel cap to the viewer's, and no further.
        let full = commit_diff_for_repo(&repo, &id, true, DIFF_METADATA_CAP)
            .await
            .expect("the full-screen read is bounded too");
        assert!(full.truncated);
        assert!(
            full.patch.len() <= DIFF_PATCH_CAP_FULL,
            "full patch kept {} bytes, cap is {DIFF_PATCH_CAP_FULL}",
            full.patch.len()
        );
        assert!(
            full.patch.len() > DIFF_PATCH_CAP,
            "?full=1 must actually lift the panel cap"
        );
    }

    /// The file viewer against the same fixture: a 58 MiB blob comes back at the
    /// 2 MB cap, and the binary file keeps its existing "flagged, empty" shape.
    #[tokio::test]
    async fn bounded_file_handler_caps_large_existing_file() {
        let (_dir, repo, id) = pathological_repo();

        let big = file_at_commit_for_repo(&repo, &id, "zbig.txt")
            .await
            .expect("a huge existing file is a truncated success");
        assert!(!big.binary);
        assert!(big.truncated, "a 58 MiB file must report the 2 MB cap");
        assert!(
            big.content.len() <= FILE_CONTENT_CAP,
            "kept {} bytes, cap is {FILE_CONTENT_CAP}",
            big.content.len()
        );
        assert!(
            big.content.starts_with("ZBIG\n"),
            "the retained prefix is the file's own beginning"
        );

        // Bounding the read left the binary representation exactly as it was.
        let bin = file_at_commit_for_repo(&repo, &id, "bin.dat")
            .await
            .expect("the binary blob still resolves");
        assert!(bin.binary);
        assert!(bin.content.is_empty());
        assert!(!bin.truncated);
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

    // ---- exact remote reachability for one commit (M1.10, #63) ---------------

    /// A **real** repository of `count` linear commits with
    /// `refs/remotes/origin/main` at the chain tip and one further local-only
    /// commit on top. Built through a single `git fast-import` so a fixture
    /// deeper than the retained 5,000-commit cap costs a second, not minutes.
    fn deep_remote_repo(count: usize) -> (tempfile::TempDir, PathBuf) {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);

        let mut stream = String::new();
        for n in 1..=count {
            let message = format!("commit {n}\n");
            stream.push_str("commit refs/heads/main\n");
            stream.push_str(&format!("mark :{n}\n"));
            stream.push_str(&format!(
                "committer t <t@example.invalid> {} +0000\n",
                1_000 + n
            ));
            stream.push_str(&format!("data {}\n{message}", message.len()));
            if n > 1 {
                stream.push_str(&format!("from :{}\n", n - 1));
            }
            stream.push('\n');
        }
        stream.push_str("reset refs/remotes/origin/main\n");
        stream.push_str(&format!("from :{count}\n\n"));
        stream.push_str("done\n");

        let mut child = std::process::Command::new("git")
            .args(["fast-import", "--quiet", "--done"])
            .current_dir(&repo)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "git fast-import failed");

        // One local commit past the remote tip: "on a remote" stays a real question.
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "local tip"]);
        (dir, repo)
    }

    /// The detail panel's remote flag is exact for an arbitrary commit, however
    /// deep. A two-row page holds only the local tip and the remote tip; the deep
    /// root and an arbitrary unloaded parent are both still reported as pushed,
    /// which a `HISTORY_LIMIT`-capped remote walk could not manage.
    #[test]
    fn commit_detail_marks_unloaded_remote_parent() {
        let (_dir, repo) = deep_remote_repo(5_001);

        let local_tip = out(&repo, &["rev-parse", "HEAD"]);
        let remote_tip = out(&repo, &["rev-parse", "refs/remotes/origin/main"]);
        let arbitrary = out(&repo, &["rev-parse", "refs/remotes/origin/main~3"]);
        let root = out(
            &repo,
            &["rev-list", "--max-parents=0", "refs/remotes/origin/main"],
        );
        let depth: usize = out(&repo, &["rev-list", "--count", "refs/remotes/origin/main"])
            .parse()
            .unwrap();
        assert!(depth > 5_000, "fixture must exceed the cap, got {depth}");

        // The rows a two-row page would own; neither request below is among them.
        let page = [local_tip.as_str(), remote_tip.as_str()];
        assert!(!page.contains(&arbitrary.as_str()));
        assert!(!page.contains(&root.as_str()));

        for id in [&root, &arbitrary] {
            let detail = commit_detail_for_repo(&repo, id).expect("detail read");
            assert_eq!(&detail.id.0, id);
            assert!(detail.on_remote, "an unloaded parent is on the remote");
        }

        let unpushed = commit_detail_for_repo(&repo, &local_tip).expect("detail read");
        assert!(
            !unpushed.on_remote,
            "the local tip was never pushed anywhere"
        );
    }
}
