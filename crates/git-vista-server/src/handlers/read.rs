//! The read endpoints (all `no-store` GETs): the laid-out history graph, one
//! commit's detail and diff, and the two live "state" reads (checked-out branch,
//! working-tree status). Reads, so they work on read-only clones too.

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;

use git_vista_core::identity::{RepositoryHandle, WorktreeId};
use git_vista_core::layout;
use git_vista_core::layout::replay::ReplayClassifier;
use git_vista_core::layout::stream::{strip_resolved_edges, StreamLayout};
use git_vista_core::layout::trunk_reserve_tip;
use git_vista_core::model::{
    CommitDetail, CommitSummary, Edge, FrameStub, GitRef, GraphRow, Oid, RefKind,
};
use git_vista_core::status::parse_porcelain_v2;
use git_vista_git::{read_commit, read_refs, walk_history, walk_history_topo, RepoError};
use git_vista_protocol::{HistoryFrame, HistoryPage};

use crate::git_cmd::git_stdout_capped;
use crate::handlers::reset::has_seed;
use crate::history::{
    if_none_match, read_history_snapshot, representation_etag, require_same_generation,
    CursorCodec, CursorScope, HistoryCursor, RepresentationKind,
};
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

// ---------------------------------------------------------------------------
// Paged history: the Frame and one Page (M1.10, #63)
//
// The `#[allow(dead_code)]` markers below are the same narrow, temporary device
// `main.rs` uses for `mod history`: plan Step 8 adds the cursor-replay handler
// and Step 9 registers `/api/frame` plus the paged `/api/commits` on both
// routers. Until those routes exist nothing in the binary target reaches this
// code, and deleting tested, plan-mandated seams to satisfy `-D warnings` would
// be the wrong trade. Remove every marker in the Step 9 edit.

/// The server's history Frame: the generic transport envelope over core's
/// display refs. The frontend declares its own same-shaped alias (Task 5); a
/// server-private alias is never imported across the crate boundary.
pub type Frame = HistoryFrame<GitRef>;

/// The server's history Page: the generic transport envelope over core's
/// laid-out rows, wire edges, and OID-anchored stubs.
pub type Page = HistoryPage<GraphRow, Edge, FrameStub>;

/// One paged-history read's target, resolved and canonicalized exactly once.
///
/// Everything downstream — the snapshot, the cursor scope, the Frame's metadata
/// — comes from this single resolution, so a request can never mix two
/// repositories, and an absent `?repo=` captures the mutable default selection
/// once instead of re-reading it per stage.
pub(crate) struct ResolvedHistoryTarget {
    /// The canonical on-disk path. Process-internal only: it never enters a
    /// cursor or a response body.
    pub path: PathBuf,
    /// A cloned, view-only repository.
    pub read_only: bool,
    /// The catalog identity pair, or `None` in degraded (unregistered) mode.
    pub handle: Option<RepositoryHandle>,
    /// The opaque scope every cursor for this target is bound to.
    pub scope: CursorScope,
}

/// Resolve the `?repo=` selector to one [`ResolvedHistoryTarget`].
///
/// A registered target's scope binds **both** halves of its
/// [`RepositoryHandle`], so a cursor follows neither another repository nor a
/// sibling worktree of the same one. A degraded target has no ids, so it binds
/// its canonical path through the per-process key instead — which is why the
/// path is canonicalized here rather than taken as spelled: `state::set_current`
/// stores a degraded selection's path verbatim, and two spellings of one
/// directory would otherwise bind two different scopes. Canonicalization is
/// best-effort, matching the launch path's own posture.
#[allow(dead_code)] // wired by plan Step 9 (route registration)
fn resolve_history_target(
    selector: Option<&str>,
    codec: &CursorCodec,
) -> Result<ResolvedHistoryTarget, (StatusCode, String)> {
    let (path, read_only, handle) = resolve_repo(selector)?;
    let path = path.canonicalize().unwrap_or(path);
    let scope = codec.scope_for_target(handle.as_ref(), &path);
    Ok(ResolvedHistoryTarget {
        path,
        read_only,
        handle,
        scope,
    })
}

/// The page size an absent `?limit=` gets.
const DEFAULT_PAGE_LIMIT: usize = 250;

/// The largest page any client can ask for.
const MAX_PAGE_LIMIT: usize = 1_000;

/// Clamp a requested page size into `1..=MAX_PAGE_LIMIT`. Zero would mint a
/// cursor that never advances, and an oversized request is clamped rather than
/// refused — a client asking for too much gets a smaller page plus a cursor,
/// never a 400.
#[allow(dead_code)] // wired by plan Step 9 (route registration)
fn page_limit(raw: Option<usize>) -> usize {
    raw.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

/// The paged-history query: the shared repository selector plus the opaque
/// cursor and the requested page size.
///
/// Deliberately **not** `deny_unknown_fields`: the frontend appends its own
/// `?t=<millis>` cache-buster to every history read (see `crates/git-vista/
/// src/api.rs`), and that must never be answered with a 400.
#[derive(Deserialize)]
#[allow(dead_code)] // wired by plan Step 9 (route registration)
pub(crate) struct PageQuery {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Serializing a representation we just built cannot fail on well-formed data;
/// if it somehow does, it is our bug, not the client's.
fn history_serialization_failed(e: serde_json::Error) -> (StatusCode, String) {
    eprintln!("git-vista: serializing a history representation failed: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "could not serialize history".to_string(),
    )
}

/// Build the `200` — or the empty `304` — for one **already serialized**
/// representation.
///
/// The validator is SHA-256 over exactly these bytes and the response body is
/// this same buffer, so an ETag can never describe a re-serialization or be
/// derived from the generation. Both statuses retain the quoted tag;
/// `Cache-Control: no-store` is stamped centrally by the auth middleware
/// (`security::require_auth`) and is deliberately not repeated here.
///
/// `honor_precondition` is false for a cursor page: only a Frame and page 1 are
/// stable, addressable representations a client can revalidate.
fn representation_response(
    kind: RepresentationKind,
    body: Vec<u8>,
    headers: &HeaderMap,
    honor_precondition: bool,
) -> Response {
    let etag = representation_etag(kind, &body);
    if honor_precondition && if_none_match(headers, &etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    (
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
        ],
        body,
    )
        .into_response()
}

/// Build one repository's Frame response.
///
/// `O(refs)`: branch slots come from [`ReplayClassifier::new`], which never
/// touches the object database, so a Frame answers without walking a single
/// commit and never increments a walk counter. A Frame carries no stubs — only
/// a page's own rows can anchor one.
///
/// Split out from the [`frame`] handler so a test can drive it against a
/// temporary repository; the handler itself resolves the process-wide default
/// selection, which every test in this binary shares.
async fn frame_for_target(
    target: &ResolvedHistoryTarget,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let repo = target.path.as_path();
    let snapshot = read_history_snapshot(repo).await?;
    let frame = Frame {
        generation: snapshot.generation.clone(),
        refs: snapshot.refs.clone(),
        head_branch: snapshot.head_branch.clone(),
        branch_colors: ReplayClassifier::new(&snapshot.refs, snapshot.head_branch.as_deref())
            .branch_colors(),
        // A short non-path label, so the header can say *which* repo without
        // leaking the server's filesystem (M1.03).
        repo_label: Some(repo_label(repo)),
        repo_id: target.handle.map(|handle| handle.repository.to_string()),
        worktree_id: target.handle.map(|handle| handle.worktree.to_string()),
        read_only: target.read_only,
        resettable: !target.read_only && has_seed(repo),
        repo_url: git_vista_git::github_web_base(repo),
        remote_web_url: git_vista_git::remote_web_base(repo),
    };
    // The combined re-read: the metadata above reads config, not refs, but the
    // repository can still move under a Frame read, and a Frame that advertises
    // a generation no longer current would hand the client a cursor seed for a
    // history that has already gone.
    let fresh = read_history_snapshot(repo).await?;
    require_same_generation(&snapshot.generation, &fresh.generation)?;

    let body = serde_json::to_vec(&frame).map_err(history_serialization_failed)?;
    Ok(representation_response(
        RepresentationKind::Frame,
        body,
        headers,
        true,
    ))
}

/// The cheap, once-per-view half of paged history: refs, branch colour slots,
/// and the resolved target's metadata — no commits at all.
#[allow(dead_code)] // registered by plan Step 9
pub(crate) async fn frame(
    Extension(codec): Extension<Arc<CursorCodec>>,
    headers: HeaderMap,
    Query(query): Query<RepoQuery>,
) -> Result<Response, (StatusCode, String)> {
    let target = resolve_history_target(query.repo.as_deref(), codec.as_ref())?;
    frame_for_target(&target, &headers).await
}

/// Build one Page response for `target`.
///
/// The construction order is the plan's and is load-bearing: the target is
/// already resolved, then one combined refs + HEAD + shallow snapshot is read,
/// then a cursor (when present) is authenticated and its scope and generation
/// compared **before** `walks` moves or Topo opens, then the walk runs, then
/// exact remote membership is stamped on the emitted rows only, then the
/// combined snapshot is re-read and drift is a 409, and only then is the body
/// built, serialized exactly once, and hashed into its own `gv4-page` tag.
///
/// `walks` is injected the way `commit_diff_for_repo`'s `metadata_cap` is: so a
/// test can prove a rejected cursor never reaches the traversal, and that a
/// Frame read never counts as one.
///
/// Plan Step 7 implements the page-1 path; the cursor branch — authentication,
/// scope/generation gates and the `[0,n)` prefix replay — is plan Step 8.
#[allow(dead_code)] // wired by plan Step 9 (route registration)
async fn page_for_target(
    target: &ResolvedHistoryTarget,
    cursor: Option<&str>,
    limit: usize,
    codec: &CursorCodec,
    headers: &HeaderMap,
    walks: &AtomicUsize,
) -> Result<Response, (StatusCode, String)> {
    let repo = target.path.as_path();

    // 1. One combined snapshot, read after the target was resolved, so refs,
    //    both HEAD halves and the canonical shallow set describe one moment.
    let snapshot = read_history_snapshot(repo).await?;

    // 2. A cursor is authenticated, scope-compared and drift-compared here,
    //    before anything below runs. Plan Step 8 owns that path and the prefix
    //    replay it enables; until then only page 1 is served.
    if cursor.is_some() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            "paged history replay is not implemented yet".to_string(),
        ));
    }
    let start_row = 0_usize;
    let end_row = start_row.saturating_add(limit);

    // 3. Rebuild the shallow-aware Topo `DateOrder` walk from the snapshot's own
    //    sorted tips and exact boundary set — never from a re-read of the refs.
    let tips: Vec<(String, Oid)> = snapshot
        .tips
        .iter()
        .map(|tip| (tip.full_ref_name.clone(), tip.object_id.clone()))
        .collect();
    let boundaries: HashSet<Oid> = snapshot.shallow_boundaries.iter().cloned().collect();
    let trunk_tip = trunk_reserve_tip(&snapshot.refs, snapshot.head_branch.as_deref());
    let mut stream = StreamLayout::new(trunk_tip);
    let mut walked = 0_usize;
    walks.fetch_add(1, Ordering::Relaxed);
    walk_history_topo(repo, &tips, &snapshot.shallow_boundaries, |summary| {
        // A recorded boundary commit's parents are cut: they may not even be in
        // the object database, so they reserve no lane and wire no edge. Every
        // non-boundary parent stays required.
        let cut = boundaries.contains(&summary.id);
        stream.push(summary, |_parent| !cut);
        walked += 1;
        if walked >= end_row {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .map_err(|e| {
        eprintln!("git-vista: /api/commits failed walking history: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let chunk = stream.finish();

    // 4/5. Decorate every row in ascending row order, exactly once — the
    //      classifier propagates claims along first parents and hands out stub
    //      columns cumulatively, so a skipped or out-of-order row corrupts both.
    //      Page 1 has no prefix, so nothing is suppressed here.
    let mut classifier = ReplayClassifier::new(&snapshot.refs, snapshot.head_branch.as_deref());
    let mut rows: Vec<GraphRow> = Vec::with_capacity(chunk.rows.len());
    let mut stubs: Vec<FrameStub> = Vec::new();
    for mut row in chunk.rows {
        let emit = row.row >= start_row;
        let produced = classifier.decorate(&mut row, emit);
        if emit {
            stubs.extend(produced);
            rows.push(row);
        }
    }

    // 6. Exact remote reachability, for the emitted OIDs only — one walk, never
    //    one per row. A remote-scan failure leaves the flags false, the same
    //    lenient posture the commit-detail read takes: the page loses forge
    //    links, it does not lose the history.
    let requested: HashSet<Oid> = rows.iter().map(|row| row.commit.id.clone()).collect();
    match git_vista_git::remote_membership(repo, &requested) {
        Ok(found) => {
            for row in &mut rows {
                row.on_remote = found.contains(&row.commit.id);
            }
        }
        Err(e) => eprintln!("git-vista: /api/commits could not scan remotes: {e}"),
    }

    // The combined re-read on success: a ref that moved while we walked would
    // otherwise let this page splice two histories together.
    let fresh = read_history_snapshot(repo).await?;
    require_same_generation(&snapshot.generation, &fresh.generation)?;

    // 7. Sign the next absolute row under the same target scope and the stable
    //    generation. A walk that ended before the window filled has no next
    //    page, so it carries no cursor.
    let cursor = if walked >= end_row {
        let signed = codec
            .encode(
                target.scope,
                &snapshot.generation,
                &HistoryCursor { next_row: end_row },
            )
            .map_err(|_| {
                eprintln!("git-vista: /api/commits could not sign a history cursor");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not sign a history cursor".to_string(),
                )
            })?;
        Some(signed)
    } else {
        None
    };

    let page = Page {
        rows,
        // Already in canonical `(from_row, parent_ordinal, …)` order: the chunk
        // sorted its own `ResolvedEdge`s through their sidecars, which is what
        // lets a page that does not start at row 0 sort without row indexing.
        edges: strip_resolved_edges(chunk.resolved_edges),
        stubs,
        // Commit-lane high-water only; stub columns sit past it at
        // `lane_count + FrameStub::lane_offset`.
        lane_count: chunk.lane_count,
        cursor,
        generation: snapshot.generation.clone(),
    };

    // 8. Page 1 answers `If-None-Match` against its own current tag; a cursor
    //    page ignores the precondition and always returns 200.
    let body = serde_json::to_vec(&page).map_err(history_serialization_failed)?;
    Ok(representation_response(
        RepresentationKind::Page,
        body,
        headers,
        start_row == 0,
    ))
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

    // ---- paged history: Frame, page limits, exact-body validators (M1.10, #63) --
    //
    // These drive the repo-parameterized `frame_for_target` / `page_for_target`
    // seams directly, exactly as the bounded diff/file tests above drive
    // `commit_diff_for_repo`. The axum handlers resolve their repository from the
    // process-global `CURRENT` selection, shared by every test in this binary, so
    // a handler-level test would race with `state::tests` and with its own
    // siblings. The only production code skipped is `resolve_history_target`,
    // whose selector arms are already pinned by the two tests at the top of this
    // module.

    /// `git <args…>` in `repo` with `envs` set; asserts success. Fixed
    /// author/committer dates are what make two independently built repositories
    /// share one history generation.
    fn run_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(repo);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let status = cmd.status().unwrap();
        assert!(status.success(), "git {args:?} failed in {repo:?}");
    }

    /// A repository named `name` under `parent`, on `main`, with `commits`
    /// commits whose ids are a pure function of their content — two copies built
    /// this way are byte-identical histories and share one generation.
    fn deterministic_repo(parent: &Path, name: &str, commits: usize) -> PathBuf {
        assert!(commits >= 1, "a history fixture needs at least one commit");
        let repo = parent.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        for i in 0..commits {
            std::fs::write(repo.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            run(&repo, &["add", "-A"]);
            let stamp = format!("{} +0000", 1_700_000_000 + i);
            let message = format!("c{i}");
            run_env(
                &repo,
                &["commit", "-q", "-m", &message],
                &[("GIT_AUTHOR_DATE", &stamp), ("GIT_COMMITTER_DATE", &stamp)],
            );
        }
        repo
    }

    /// A deterministic cursor codec, so nothing here depends on the per-process
    /// random key.
    fn history_codec() -> CursorCodec {
        CursorCodec::with_key([0x27; 32])
    }

    /// The degraded-mode target for `repo`: canonical path, no catalog ids, scope
    /// bound through the codec's key — what `resolve_history_target` builds for a
    /// selection the catalog never registered.
    fn history_target(repo: &Path, codec: &CursorCodec) -> ResolvedHistoryTarget {
        let path = repo.canonicalize().expect("a temp repo path resolves");
        let scope = codec.scope_for_target(None, &path);
        ResolvedHistoryTarget {
            path,
            read_only: false,
            handle: None,
            scope,
        }
    }

    /// Split a history response into `(status, etag, body)`. Every 200 and 304
    /// must carry its quoted representation tag.
    async fn parts_of(response: Response) -> (StatusCode, HeaderValue, Vec<u8>) {
        let status = response.status();
        let etag = response
            .headers()
            .get(header::ETAG)
            .expect("every history response carries its representation tag")
            .clone();
        let body = axum::body::to_bytes(response.into_body(), 8 << 20)
            .await
            .expect("a bounded history body")
            .to_vec();
        (status, etag, body)
    }

    /// The loose-object path for `oid`. Deleting one is how these tests make a
    /// commit traversal impossible while leaving refs and HEAD intact.
    fn loose_object(repo: &Path, oid: &str) -> PathBuf {
        repo.join(".git")
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..])
    }

    /// The Frame read for `repo` under `headers`. There is deliberately no walk
    /// counter to pass: `frame_for_target` takes none because a Frame has
    /// nothing to walk — the claim is proved below by breaking the object
    /// database and watching a Frame answer anyway.
    async fn frame_parts(repo: &Path, headers: &HeaderMap) -> (StatusCode, HeaderValue, Vec<u8>) {
        let codec = history_codec();
        let target = history_target(repo, &codec);
        let response = frame_for_target(&target, headers)
            .await
            .expect("frame read");
        parts_of(response).await
    }

    /// The page-1 read for `repo` at `limit` under `headers`, plus its walk count.
    async fn page_one_parts(
        repo: &Path,
        limit: usize,
        headers: &HeaderMap,
    ) -> (StatusCode, HeaderValue, Vec<u8>, usize) {
        let codec = history_codec();
        let target = history_target(repo, &codec);
        let walks = AtomicUsize::new(0);
        let response = page_for_target(&target, None, limit, &codec, headers, &walks)
            .await
            .expect("page read");
        let (status, etag, body) = parts_of(response).await;
        (status, etag, body, walks.load(Ordering::Relaxed))
    }

    /// An `If-None-Match:` header map carrying exactly `value`.
    fn if_none_match_header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        headers
    }

    /// The default page is the plan's 250; a client may ask for less, may ask for
    /// more and be clamped, and can never ask for a page that would fail to
    /// advance the cursor. An unknown query key (the frontend's `?t=`) is
    /// accepted, never a 400.
    #[test]
    fn page_limit_defaults_and_clamps() {
        assert_eq!(DEFAULT_PAGE_LIMIT, 250);
        assert_eq!(MAX_PAGE_LIMIT, 1_000);

        assert_eq!(
            page_limit(None),
            DEFAULT_PAGE_LIMIT,
            "an absent ?limit= is the default page"
        );
        assert_eq!(
            page_limit(Some(0)),
            1,
            "a zero-row page would never advance the cursor"
        );
        assert_eq!(page_limit(Some(1)), 1);
        assert_eq!(page_limit(Some(7)), 7);
        assert_eq!(page_limit(Some(DEFAULT_PAGE_LIMIT)), DEFAULT_PAGE_LIMIT);
        assert_eq!(page_limit(Some(MAX_PAGE_LIMIT)), MAX_PAGE_LIMIT);
        assert_eq!(
            page_limit(Some(MAX_PAGE_LIMIT + 1)),
            MAX_PAGE_LIMIT,
            "an oversized ?limit= clamps rather than failing the read"
        );
        assert_eq!(page_limit(Some(usize::MAX)), MAX_PAGE_LIMIT);

        // `PageQuery` must not deny unknown fields: the frontend appends its own
        // cache-buster and must not be answered with a 400.
        let parsed: PageQuery = serde_json::from_str(
            r#"{"repo":null,"cursor":"opaque","limit":7,"t":"1737000000000"}"#,
        )
        .expect("PageQuery tolerates the frontend's ?t= cache-buster");
        assert!(parsed.repo.is_none());
        assert_eq!(parsed.cursor.as_deref(), Some("opaque"));
        assert_eq!(page_limit(parsed.limit), 7);
    }

    /// One snapshot, one generation — but two different resources, so two
    /// different, type-prefixed, exact-body validators. The Frame is `O(refs)`:
    /// it must not touch the walk counter at all.
    #[tokio::test]
    async fn frame_and_page_one_share_generation_but_have_distinct_etags() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 4);
        let headers = HeaderMap::new();

        let (frame_status, frame_tag, frame_body) = frame_parts(&repo, &headers).await;
        assert_eq!(frame_status, StatusCode::OK);

        let (page_status, page_tag, page_body, page_walks) =
            page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &headers).await;
        assert_eq!(page_status, StatusCode::OK);
        assert_eq!(page_walks, 1, "page 1 walks exactly once");

        let frame: Frame = serde_json::from_slice(&frame_body).expect("Frame decodes");
        let page: Page = serde_json::from_slice(&page_body).expect("Page decodes");
        assert_eq!(
            frame.generation, page.generation,
            "one combined snapshot, one generation"
        );
        assert_ne!(frame_tag, page_tag);
        assert!(
            frame_tag.to_str().unwrap().starts_with("\"gv4-frame:"),
            "{frame_tag:?}"
        );
        assert!(
            page_tag.to_str().unwrap().starts_with("\"gv4-page:"),
            "{page_tag:?}"
        );

        // The tags are hashes of the exact bytes that were sent, not of a
        // re-serialization and never of the generation.
        assert_eq!(
            representation_etag(RepresentationKind::Frame, &frame_body),
            frame_tag
        );
        assert_eq!(
            representation_etag(RepresentationKind::Page, &page_body),
            page_tag
        );

        // The Frame answers branch slots from refs alone and carries no stubs
        // (the envelope has no such field); the Page carries the rows.
        assert_eq!(
            frame.branch_colors,
            vec![("main".to_string(), 0)],
            "the trunk's stable slot comes from the refs, with no walk"
        );
        assert!(
            !frame_body.windows(7).any(|w| w == b"\"stubs\""),
            "a Frame never carries stubs"
        );
        assert_eq!(page.rows.len(), 4);
        assert_eq!(page.rows[0].row, 0);

        // The `O(refs)` claim, with teeth: remove one interior commit object, so
        // every commit traversal in this repository now fails, and the Frame
        // still answers the identical body. Nothing below the ref tips feeds it,
        // which is why it needs — and is given — no walk counter at all.
        let interior = out(&repo, &["rev-parse", "HEAD~2"]);
        std::fs::remove_file(loose_object(&repo, &interior)).expect("a loose interior commit");
        let walks = AtomicUsize::new(0);
        let codec = history_codec();
        let target = history_target(&repo, &codec);
        page_for_target(&target, None, DEFAULT_PAGE_LIMIT, &codec, &headers, &walks)
            .await
            .expect_err("a Page cannot be built without the commit objects");
        assert_eq!(
            walks.load(Ordering::Relaxed),
            1,
            "the Page read counted its one walk before failing in it"
        );

        let (status, revalidated, body) = frame_parts(&repo, &headers).await;
        assert_eq!(status, StatusCode::OK, "a Frame needs no commit object");
        assert_eq!(
            revalidated, frame_tag,
            "the Frame is a pure function of refs, HEAD and the shallow set"
        );
        assert_eq!(body, frame_body);
    }

    /// A change the generation deliberately excludes — repository config, not a
    /// ref, HEAD, or a shallow boundary — still changes the Frame's body, so it
    /// must change the Frame's validator. Generation and ETag are separate
    /// things.
    #[tokio::test]
    async fn frame_metadata_change_changes_etag_without_generation_change() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 2);
        let headers = HeaderMap::new();

        let (_, before_tag, before_body) = frame_parts(&repo, &headers).await;
        let before: Frame = serde_json::from_slice(&before_body).unwrap();
        assert!(
            before.remote_web_url.is_none(),
            "the fixture starts with no remote"
        );

        run(
            &repo,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );

        let (_, after_tag, after_body) = frame_parts(&repo, &headers).await;
        let after: Frame = serde_json::from_slice(&after_body).unwrap();
        assert!(
            after.remote_web_url.is_some(),
            "adding a remote gives the Frame a forge base"
        );
        assert_eq!(
            before.generation, after.generation,
            "config moves no ref, no HEAD half and no shallow boundary"
        );
        assert_ne!(
            before_tag, after_tag,
            "the validator is derived from the sent body, so metadata moves it"
        );
    }

    /// Two selections over byte-identical histories share a generation but are
    /// different resources: the resolved-target metadata rides in the Frame body,
    /// so switching the default selection must move the Frame's validator — and
    /// the two targets must bind different cursor scopes.
    #[tokio::test]
    async fn default_selection_switch_same_history_changes_frame_etag() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = deterministic_repo(dir.path(), "alpha", 3);
        let beta = deterministic_repo(dir.path(), "beta", 3);
        let headers = HeaderMap::new();

        let (_, alpha_tag, alpha_body) = frame_parts(&alpha, &headers).await;
        let (_, beta_tag, beta_body) = frame_parts(&beta, &headers).await;
        let a: Frame = serde_json::from_slice(&alpha_body).unwrap();
        let b: Frame = serde_json::from_slice(&beta_body).unwrap();

        assert_eq!(
            a.generation, b.generation,
            "identical committed topology is one history generation"
        );
        assert!(a
            .repo_label
            .as_deref()
            .is_some_and(|label| label.ends_with("alpha")));
        assert!(b
            .repo_label
            .as_deref()
            .is_some_and(|label| label.ends_with("beta")));
        assert_ne!(
            alpha_tag, beta_tag,
            "one generation, two selections, two validators"
        );

        let codec = history_codec();
        assert_ne!(
            history_target(&alpha, &codec).scope,
            history_target(&beta, &codec).scope,
            "a cursor minted for one selection must not open on the other"
        );
    }

    /// Page 1 at two different limits is two different representations of one
    /// generation, each with its own exact-body validator and its own cursor.
    #[tokio::test]
    async fn page_one_limits_one_and_seven_have_distinct_etags() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 8);
        let headers = HeaderMap::new();

        let (_, tag_one, body_one, _) = page_one_parts(&repo, 1, &headers).await;
        let (_, tag_seven, body_seven, _) = page_one_parts(&repo, 7, &headers).await;
        let one: Page = serde_json::from_slice(&body_one).unwrap();
        let seven: Page = serde_json::from_slice(&body_seven).unwrap();

        assert_eq!(one.rows.len(), 1);
        assert_eq!(seven.rows.len(), 7);
        assert_eq!(one.rows[0].row, 0, "both pages start at absolute row 0");
        assert_eq!(seven.rows[0].row, 0);
        assert_eq!(one.rows[0].commit.id, seven.rows[0].commit.id);
        assert_eq!(
            one.generation, seven.generation,
            "the page size is not part of the history generation"
        );
        assert_ne!(tag_one, tag_seven);
        assert!(one.cursor.is_some(), "seven more rows remain after limit 1");
        assert!(seven.cursor.is_some(), "one more row remains after limit 7");
        assert_ne!(
            one.cursor, seven.cursor,
            "the two cursors name different next rows"
        );
    }

    /// The two tag namespaces are sealed: a Frame validator can never satisfy a
    /// Page's precondition, nor a Page validator a Frame's. Both requests are
    /// answered 200 with their own tag and a real body.
    #[tokio::test]
    async fn frame_etag_cannot_304_page_one() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 3);
        let none = HeaderMap::new();

        let (_, frame_tag, _) = frame_parts(&repo, &none).await;
        let (_, page_tag, _, _) = page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &none).await;
        assert_ne!(frame_tag, page_tag);

        let presented = if_none_match_header(frame_tag.to_str().unwrap());
        let (status, tag, body, _) = page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &presented).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a Frame tag must not 304 a Page: they are different resources"
        );
        assert_eq!(tag, page_tag);
        assert!(!body.is_empty());

        let presented = if_none_match_header(page_tag.to_str().unwrap());
        let (status, tag, body) = frame_parts(&repo, &presented).await;
        assert_eq!(status, StatusCode::OK, "nor a Page tag a Frame");
        assert_eq!(tag, frame_tag);
        assert!(!body.is_empty());
    }

    /// A Frame whose own current validator is presented is answered with an
    /// empty 304 that still carries that validator.
    #[tokio::test]
    async fn frame_matching_validator_returns_304_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 3);

        let (status, tag, body) = frame_parts(&repo, &HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());

        let presented = if_none_match_header(tag.to_str().unwrap());
        let (status, revalidated, body) = frame_parts(&repo, &presented).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(revalidated, tag, "a 304 keeps the validator it matched");
        assert!(body.is_empty(), "a 304 carries no body");
    }

    /// Page 1 evaluates the precondition against its own current tag, and a
    /// match is an empty 304 carrying that tag.
    #[tokio::test]
    async fn page_one_matching_validator_returns_304_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 3);

        let (status, tag, body, _) =
            page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());

        let presented = if_none_match_header(tag.to_str().unwrap());
        let (status, revalidated, body, _) =
            page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &presented).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(revalidated, tag);
        assert!(body.is_empty(), "a 304 carries no body");
    }

    /// RFC 9110 weak comparison, on both representations: a `W/`-prefixed tag, a
    /// matching member of a comma-separated list, and `*` each revalidate to an
    /// empty 304 carrying the representation's own tag.
    #[tokio::test]
    async fn frame_and_page_one_weak_list_and_star_validators_return_304_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 3);

        let (_, frame_tag, _) = frame_parts(&repo, &HeaderMap::new()).await;
        let (_, page_tag, _, _) =
            page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &HeaderMap::new()).await;

        for (what, tag) in [("frame", &frame_tag), ("page", &page_tag)] {
            let quoted = tag.to_str().unwrap();
            let validators = [
                format!("W/{quoted}"),
                format!("\"gv4-page:0000000000000000000000000000000000000000000000000000000000000000\", {quoted}"),
                "*".to_string(),
            ];
            for validator in validators {
                let presented = if_none_match_header(&validator);
                let (status, revalidated, body) = if what == "frame" {
                    frame_parts(&repo, &presented).await
                } else {
                    let (status, tag, body, _) =
                        page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &presented).await;
                    (status, tag, body)
                };
                assert_eq!(
                    status,
                    StatusCode::NOT_MODIFIED,
                    "{what} must revalidate on {validator}"
                );
                assert_eq!(&revalidated, tag, "{what}: 304 keeps its own validator");
                assert!(body.is_empty(), "{what}: a 304 carries no body");
            }
        }
    }
}
