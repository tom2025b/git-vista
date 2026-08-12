//! The read endpoints (all `no-store` GETs): protocol v4's stateless paged
//! history — `GET /api/frame` (refs/branch-colours, no commits) and the paged
//! `GET /api/commits` (one cursor-signed window of rows/edges/stubs) — plus one
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
use git_vista_core::layout::replay::ReplayClassifier;
use git_vista_core::layout::stream::{strip_resolved_edges, StreamLayout};
use git_vista_core::layout::trunk_reserve_tip;
use git_vista_core::model::{CommitDetail, Edge, FrameStub, GitRef, GraphRow, Oid};
use git_vista_core::status::parse_porcelain_v2;
use git_vista_git::{read_commit, walk_history_topo, RepoError};
use git_vista_protocol::{HistoryFrame, HistoryPage};

use crate::git_cmd::git_stdout_capped;
use crate::handlers::reset::has_seed;
use crate::history::{
    if_none_match, read_history_snapshot, representation_etag, require_same_generation,
    CursorCodec, CursorError, CursorScope, HistoryCursor, RepresentationKind,
};
use crate::state::{current, current_handle, repo_label, resolve_worktree};

/// The optional opaque repository selector shared by the read endpoints (M1.03):
/// `?repo=<worktree-id>` addresses one servable worktree by its opaque id. When
/// absent, the endpoint acts on the server's current default selection — the
/// backward-compatible behaviour the existing single-repo frontend relies on
/// until it adopts ids (M1.11).
#[derive(Deserialize)]
pub(crate) struct RepoQuery {
    #[serde(default)]
    pub(crate) repo: Option<String>,
}

/// Resolve the `?repo=` selector to a concrete repository, failing closed. A
/// malformed id is a `400`; an id the catalog does not hold is a `404` — the
/// server only ever resolves an id it itself registered, never a path from the
/// request. An absent selector falls back to the current default selection.
pub(crate) fn resolve_repo(
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
// `GET /api/frame` and the paged `GET /api/commits` register these below on
// both the loopback and LAN routers (plan Step 9); the whole-graph handler
// this route used to serve is gone.

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
/// Frame read never counts as one. It is an ordinary parameter, not a
/// `cfg(test)` device: production passes a real counter through the handler, so
/// nothing about the pipeline's behaviour changes between profiles.
///
/// Paging is stateless and honestly quadratic over a full scroll: a page at row
/// `n` re-walks `[0,n)` from the same seeds, because the entire server-side
/// state is one signed row number.
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

    // 2. A cursor is authenticated, scope-compared and generation-compared here
    //    — strictly before `walks` moves or Topo opens, so a forged, foreign or
    //    stale cursor costs nothing but an HMAC. Scope mismatch is deliberately
    //    the codec's own generic 400: a probing client must not learn whether it
    //    guessed a real target. Generation drift is the 409 the frontend keys
    //    "restart the aggregate at page 1" on.
    let start_row = match cursor {
        None => 0_usize,
        Some(encoded) => {
            let decoded = codec
                .decode::<HistoryCursor>(encoded)
                .map_err(CursorError::response)?;
            if decoded.scope != target.scope {
                return Err(CursorError.response());
            }
            require_same_generation(&decoded.generation, &snapshot.generation)?;
            decoded.state.next_row
        }
    };
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
    // `Option` only so the checkpoint can consume the layout mid-walk and put the
    // resumed one back; it is `Some` at every observable moment.
    let mut stream = Some(StreamLayout::new(trunk_tip));
    let mut prefix_rows: Vec<GraphRow> = Vec::new();
    let mut walked = 0_usize;
    walks.fetch_add(1, Ordering::Relaxed);
    let walk = walk_history_topo(repo, &tips, &snapshot.shallow_boundaries, |summary| {
        // 4. Checkpoint immediately *before* row `n`, never after: lanes and the
        //    unresolved `PendingEdge` list ride across in the checkpoint, while
        //    the prefix chunk's own resolved edges belong to pages this request
        //    does not own and are dropped here. `walked` is the absolute row the
        //    next push takes, because the walk always starts at row 0.
        if start_row > 0 && walked == start_row {
            let (prefix, checkpoint) = stream.take().expect("the layout is live").checkpoint();
            prefix_rows = prefix.rows;
            stream = Some(StreamLayout::resume(checkpoint));
        }
        // A recorded boundary commit's parents are cut: they may not even be in
        // the object database, so they reserve no lane and wire no edge. Every
        // non-boundary parent stays required.
        let cut = boundaries.contains(&summary.id);
        stream
            .as_mut()
            .expect("the layout is live")
            .push(summary, |_parent| !cut);
        walked += 1;
        if walked >= end_row {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    // 6a. A traversal or object-read failure is ambiguous until the snapshot is
    //     re-read: a ref that moved mid-walk can strand the walk on an object
    //     that is no longer reachable. Drift takes precedence, so the client is
    //     told to restart rather than shown a phantom corruption.
    if let Err(e) = walk {
        let fresh = read_history_snapshot(repo).await?;
        require_same_generation(&snapshot.generation, &fresh.generation)?;
        eprintln!("git-vista: /api/commits failed walking history: {e}");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    let chunk = stream.take().expect("the layout is live").finish();

    // 5. Decorate every row in ascending row order, exactly once — the classifier
    //    propagates claims along first parents and hands out stub columns
    //    cumulatively, so a skipped or out-of-order row corrupts both. The prefix
    //    advances all of that state with emission suppressed; only `[n,n+k)`
    //    produces badges and stubs.
    let mut classifier = ReplayClassifier::new(&snapshot.refs, snapshot.head_branch.as_deref());
    for mut row in prefix_rows {
        classifier.decorate(&mut row, false);
    }
    let mut rows: Vec<GraphRow> = Vec::with_capacity(chunk.rows.len());
    let mut stubs: Vec<FrameStub> = Vec::new();
    for mut row in chunk.rows {
        // Normally every row here is in the window. The exception is a history
        // that ended *before* row `n`: the checkpoint never fired, so this chunk
        // is entirely prefix and the window is legitimately empty.
        let emit = row.row >= start_row;
        let produced = classifier.decorate(&mut row, emit);
        if emit {
            stubs.extend(produced);
            rows.push(row);
        }
    }

    // 6b. Exact remote reachability, for the emitted OIDs only — one walk, never
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
    //    page, so it carries no cursor; a walk stopped exactly at the window's
    //    end does carry one, and the page it opens is legitimately empty.
    let next_cursor = if walked >= end_row {
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

    // This page owns all and only the edges whose *destination* row it holds —
    // `from_row < n` is normal and expected, a merge parent can resolve pages
    // below its child. Resuming from the checkpoint already guarantees that, so
    // the filter only bites in the empty-window case above, where the walk ended
    // before row `n` and every resolved edge is prefix. Ordering comes from the
    // chunk's own `ResolvedEdge` sidecars — `(from_row, parent_ordinal, …)`,
    // never a row index — which is what lets a page that does not start at row 0
    // sort itself at all.
    let edges = strip_resolved_edges(
        chunk
            .resolved_edges
            .into_iter()
            .filter(|resolved| resolved.edge.to_row >= start_row)
            .collect(),
    );

    let page = Page {
        rows,
        edges,
        stubs,
        // Commit-lane high-water only; stub columns sit past it at
        // `lane_count + FrameStub::lane_offset`.
        lane_count: chunk.lane_count,
        cursor: next_cursor,
        generation: snapshot.generation.clone(),
    };

    // 8. Page 1 answers `If-None-Match` against its own current tag; a cursor
    //    page ignores the precondition and always returns 200 with its own
    //    body-derived tag. The rule is keyed on the *request* — "did the client
    //    present a cursor?" — not on the resolved row, because only the cursorless
    //    page 1 is a stable, addressable representation a client can revalidate.
    let body = serde_json::to_vec(&page).map_err(history_serialization_failed)?;
    Ok(representation_response(
        RepresentationKind::Page,
        body,
        headers,
        cursor.is_none(),
    ))
}

/// One page of the checked-out repository's laid-out history — protocol v4,
/// replacing the whole-graph `Graph` this route used to return.
///
/// `?repo=` selects the target the same way every other read endpoint does;
/// `?cursor=` resumes a prior page (absent => page 1); `?limit=` overrides the
/// [`DEFAULT_PAGE_LIMIT`], clamped to [`MAX_PAGE_LIMIT`] by [`page_limit`]. See
/// [`page_for_target`]'s doc comment for the full eight-part construction order.
pub(crate) async fn commits(
    Extension(codec): Extension<Arc<CursorCodec>>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Response, (StatusCode, String)> {
    let target = resolve_history_target(query.repo.as_deref(), codec.as_ref())?;
    let limit = page_limit(query.limit);
    // Discarded after the call: this handler has no test double to prove
    // anything against, unlike `page_for_target`'s own tests, which pass their
    // own counter directly. Production still exercises the exact same counted
    // code path — nothing about the pipeline's behaviour changes here.
    let walks = AtomicUsize::new(0);
    page_for_target(
        &target,
        query.cursor.as_deref(),
        limit,
        codec.as_ref(),
        &headers,
        &walks,
    )
    .await
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
/// `truncated: true` means "this file exists and is bigger than we will
/// serve", which is a truncated 200 and emphatically **not** the missing-object
/// case. Only a genuinely missing spec retries `<id>^:<path>` — that retry
/// exists for a file this commit deleted, and answering a cap with the
/// parent's older content would be a wrong answer wearing a 200 (M1.10, #63).
///
/// Only a **blob** may answer this endpoint (#168). `git show <rev>:<path>`
/// (and, since #221, `git cat-file --batch`'s own content read) happily
/// "succeeds" on a tree (prints a directory listing) or a commit entry — a
/// submodule gitlink — (prints the referenced commit's log), and both would
/// otherwise come back as a `200 FileContent` with git's human-facing output
/// sitting in `content`, wearing a shape that promises real file bytes. A
/// tree is a different resource, not a different representation of this one
/// — the honest fix for tree browsing is a dedicated endpoint, not a
/// discriminator bolted onto this DTO (which would also force a wire-format
/// bump for a capability nothing currently uses) — so the decision here is
/// reject, not describe.
///
/// The type is resolved *before* any content is read, and the `<id>^:<path>`
/// retry ladder is built out of type resolutions, not content reads: the
/// type check must never be applied only to the first attempt and skipped on
/// the fallback, or a path that is a **file** in the parent but a
/// **directory** in this commit would resolve as "not found" on the first
/// attempt, fall through to the parent, and come back as a `200` with real
/// file bytes from the wrong commit — the same failure mode `FileContent`'s
/// cap logic was written to avoid (see above), one layer up. #221 moved both
/// the type check and the content read onto one `git cat-file --batch`
/// process (`crate::git_cmd::git_cat_file_batch`), including through the
/// fallback: the batch protocol's own header field (type, then size) always
/// precedes the content bytes it describes, so "type resolved before
/// content, on every attempt" is now a fact about the order fields appear on
/// the wire, not an invariant this function has to maintain by hand across
/// two separate spawns.
async fn file_at_commit_for_repo(
    repo: &Path,
    id: &str,
    path: &str,
) -> Result<git_vista_core::diff::FileContent, (StatusCode, String)> {
    // Same belt-and-braces as the diff: real ids are hex, and the id leads the
    // `<id>:<path>` spec, so neither half can ever read as an option.
    if id.len() < 4 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }

    let found =
        crate::git_cmd::git_cat_file_batch(repo, id, path, FILE_CONTENT_CAP, "/api/file").await?;
    let (bytes, truncated) = match found {
        crate::git_cmd::BatchFileRead::NotABlob { kind } => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("'{path}' is a {kind}, not a file."),
            ));
        }
        crate::git_cmd::BatchFileRead::Blob { bytes, truncated } => (bytes, truncated),
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
    let output = crate::git_cmd::git_output(&repo, &["status", "--porcelain=v2", "--branch"])
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
    let mut parsed = parse_porcelain_v2(&String::from_utf8_lossy(&output.stdout));
    // Stamped here, not in the parser: this is the instant closest to when
    // `git status` actually ran, so the client can show how old the reading
    // is without asking (the bug this fixes — a status held in memory with
    // no age looks identical whether it's 1 second or 19 hours stale).
    parsed.scanned_at = crate::activity::now_secs();
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(parsed)))
}

/// Upper bound on `git status --porcelain=v2 --branch -z`'s stdout. A cap hit
/// here is a `413`, never a best-effort parse (see [`worktree_status_v2_for_repo`]'s
/// doc comment for why) — 8 MiB is the same fail-safe ceiling
/// `git_cmd::DEFAULT_GIT_STDOUT_CAP` uses for callers that haven't reasoned
/// about size, named locally since that constant is private to `git_cmd`.
const STATUS_V2_STDOUT_CAP: usize = 8 * 1024 * 1024;

/// `GET /api/status/v2` (#68c): the generation-tagged [`WorktreeStatus`] DTO
/// (#68a) built by [`parse_porcelain_v2_z`] (#68b) from a live
/// `git status --porcelain=v2 --branch -z` read. Additive, not a replacement
/// for [`worktree_status`] — the existing v1 shape stays exactly as it is
/// (the live frontend depends on it today); migrating to this shape is 68d's
/// job. Sent `no-store`, same as every other live read in this file.
pub(crate) async fn worktree_status_v2(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let status = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP).await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(status)))
}

/// [`worktree_status_v2`] against an explicit repository — split out for the
/// same reason as [`commit_diff_for_repo`]/[`file_at_commit_for_repo`]: the
/// handler's repository comes from the process-wide `CURRENT` selection,
/// which no test can set. `cap` is likewise explicit rather than the module
/// constant baked in, the same shape `commit_diff_for_repo` already uses for
/// its metadata caps — it lets a cap-hit test use a small, cheap cap instead
/// of constructing gigabytes (or, for this endpoint, hundreds of thousands of
/// filenames) of real porcelain output to exceed the production ceiling.
///
/// **A cap hit is refused, not parsed.** Unlike a file read — where a
/// truncated prefix is still a valid, useful answer — a truncated
/// porcelain-v2 stream can cut a record in half, and `parse_porcelain_v2_z`
/// has no way to know that happened: it would either drop the partial last
/// record (an honest undercount elsewhere in this file's parsers) or, worse,
/// misparse it into something that looks like a complete but wrong entry.
/// Serving that as a `200 WorktreeStatus` would be a wrong answer wearing a
/// success status — the exact failure mode `FILE_CONTENT_CAP`'s "cap hit is a
/// *success*" design deliberately avoids for file reads by *reporting* the
/// truncation instead of hiding it. Status has no equivalent partial-content
/// contract to report through, so refusing outright is the honest choice
/// until one exists. True large-worktree responsiveness (bounding cost, not
/// just correctness) is #68e's job, not this one's.
async fn worktree_status_v2_for_repo(
    repo: &Path,
    cap: usize,
) -> Result<git_vista_protocol::WorktreeStatus, (StatusCode, String)> {
    let (bytes, truncated) = git_stdout_capped(
        repo,
        &[
            "status".to_string(),
            "--porcelain=v2".to_string(),
            "--branch".to_string(),
            "-z".to_string(),
        ],
        "/api/status/v2",
        cap,
    )
    .await?;
    if truncated {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "worktree status exceeded the read cap".to_string(),
        ));
    }

    let parsed = git_vista_protocol::parse_porcelain_v2_z(&bytes);

    // The generation (ADR 0001): HEAD + refs + index from a real repository
    // read, plus a digest of *these exact bytes* as the worktree slot — so
    // the generation changes on precisely the tracked/untracked edits this
    // very read observed, per `status.rs`'s own module doc. `status-v1:` is
    // the namespace prefix that doc comment already committed to, mirroring
    // `history.rs`'s `history-v1:` precedent, so a status generation can
    // never be confused with (or compared against) a history generation.
    let mut inputs = git_vista_git::read_generation_inputs(repo).map_err(|e| {
        eprintln!("git-vista: /api/status/v2 couldn't read generation inputs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    };
    inputs.worktree(&digest);
    let generation = inputs.generation();
    let token = git_vista_protocol::GenerationToken::new(format!("status-v1:{generation}"))
        .expect("a formatted digest is always non-empty");

    Ok(parsed.into_worktree_status(token))
}

/// The staging-base diff (M2.17b, #213): the patch text a [`PatchPlan`]
/// selects from, plus the `diff-v1:` generation it is pinned under. Lives
/// here — the diff-argv home — so `handlers::staging` never builds git argv
/// of its own (the #66 single-funnel posture, same reason the executor owns
/// the apply argv).
///
/// The base follows the direction→diff contract `patch_plan`'s module doc
/// pins: `stage` reads `git diff` (worktree-vs-index), `unstage` reads
/// `git diff --cached` (index-vs-HEAD). Same hardening as `diff_argv`:
/// `--no-textconv`, no `--binary`, `--no-color`; capped at
/// [`DIFF_PATCH_CAP_FULL`] and cut at a line boundary with the truncation
/// reported, so selections can only ever address what was actually served.
///
/// The token mirrors `worktree_status_v2_for_repo`'s recipe exactly (HEAD +
/// refs + index inputs, worktree slot = digest of the bytes this read
/// observed) with two deliberate differences: the digest is of *these patch
/// bytes*, and the direction is folded in before the bytes — the two base
/// diffs are different documents even when their text happens to match, and
/// a token minted for one must never admit a selection made against the
/// other. Namespace `diff-v1:` per the `status-v1:`/`history-v1:` precedent.
///
/// [`PatchPlan`]: git_vista_protocol::PatchPlan
pub(crate) async fn staging_diff_for_repo(
    repo: &Path,
    direction: git_vista_protocol::StageDirection,
) -> Result<git_vista_protocol::StagingDiff, (StatusCode, String)> {
    use git_vista_protocol::StageDirection;
    let mut args = vec!["diff".to_string()];
    if matches!(direction, StageDirection::Unstage) {
        args.push("--cached".to_string());
    }
    args.extend(
        ["--patch", "--no-color", "--no-textconv"]
            .into_iter()
            .map(String::from),
    );
    let (bytes, over_cap) =
        git_stdout_capped(repo, &args, "/api/staging/diff", DIFF_PATCH_CAP_FULL).await?;
    let mut patch = String::from_utf8_lossy(&bytes).into_owned();
    if over_cap {
        // Unlike the display diff, this text is *addressable* — a hunk cut
        // mid-body would parse, be selectable, preview, and then always
        // refuse at apply (its header counts exceed its body). Cut at the
        // last complete file section instead, so everything served is
        // genuinely appliable; the truncation flag still discloses the cut.
        truncate_at_line(&mut patch, DIFF_PATCH_CAP_FULL);
        if let Some(last_file) = patch.rfind("\ndiff --git ") {
            patch.truncate(last_file);
        }
    }

    let mut inputs = git_vista_git::read_generation_inputs(repo).map_err(|e| {
        eprintln!("git-vista: /api/staging/diff couldn't read generation inputs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(match direction {
            StageDirection::Stage => b"staging-base:worktree-vs-index\0".as_slice(),
            StageDirection::Unstage => b"staging-base:index-vs-head\0".as_slice(),
        });
        hasher.update(patch.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    inputs.worktree(&digest);
    let generation = inputs.generation();
    let token = git_vista_protocol::GenerationToken::new(format!("diff-v1:{generation}"))
        .expect("a formatted digest is always non-empty");

    Ok(git_vista_protocol::StagingDiff {
        generation: token,
        patch,
        truncated: over_cap,
    })
}

/// One explicit source/target diff (`POST /api/diff/spec`, M2.16 #69) — the
/// endpoint that makes #69's *"diff source and target are explicit"* criterion
/// real by accepting a [`DiffSpec`] rather than assuming "one commit vs its
/// parent".
///
/// # Why POST for a read
///
/// [`DiffSpec`] is an internally-tagged enum whose variants carry different
/// fields; a query string cannot express that shape without flattening it back
/// into loose optional parameters — which is exactly the un-explicit form the
/// type exists to remove. `POST /api/plan` set this precedent for the same
/// reason (`handlers/plan.rs`), and `api.rs`'s `preview_push` states it
/// plainly: it is a read in every sense but the HTTP verb the CSRF gate
/// demands.
///
/// # Why loopback-only
///
/// Registered inside `main.rs`'s `full_routes` block, so a LAN visualize
/// session cannot reach it. Two of the four modes ([`DiffSpec::WorktreeVsIndex`]
/// and [`DiffSpec::IndexVsCommit`]) expose **uncommitted** worktree and index
/// content, which ADR 0005's read-only LAN profile deliberately withholds —
/// `/api/staging/diff` is gated the same way and for the same reason. The
/// commit-vs-commit modes would be safe on the LAN listener, but splitting one
/// endpoint across two exposure classes by variant would put a security
/// boundary inside a match arm, where a later added variant inherits whichever
/// side someone forgets to think about. One endpoint, the stricter placement.
pub(crate) async fn spec_diff(
    Query(query): Query<RepoQuery>,
    Json(spec): Json<git_vista_protocol::diff::DiffSpec>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(query.repo.as_deref())?.0;
    let diff = spec_diff_for_repo(&repo, spec).await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(diff)))
}

/// The seam [`spec_diff`] does its work through, with the repository injected
/// rather than resolved from the process-global selection — the same shape
/// `staging_diff_for_repo` has, and for the same reason: it is what lets a test
/// drive the real endpoint body against a throwaway repository.
pub(crate) async fn spec_diff_for_repo(
    repo: &Path,
    spec: git_vista_protocol::diff::DiffSpec,
) -> Result<git_vista_protocol::diff::SpecDiff, (StatusCode, String)> {
    // `diff_spec_argv_with`, not `diff_spec_argv`: the bare mode mapping
    // carries no read options, and `--no-textconv` is not optional here. A
    // repository's own `.gitattributes` can bind a `diff=<driver>` textconv
    // filter, which git then *executes* to render file contents — a diff read
    // without the flag hands a repository the ability to run a command of its
    // choosing. `--no-color` keeps a `color.ui = always` config from injecting
    // ANSI escapes into text rendered as-is. Same flag set every other diff
    // read in this file uses; the helper places them before the revisions so
    // git can never read a revision as an option's value.
    let args = git_vista_protocol::diff::diff_spec_argv_with(
        &spec,
        &["--patch", "--no-color", "--no-textconv"],
    );

    let (bytes, over_cap) =
        git_stdout_capped(repo, &args, "/api/diff/spec", DIFF_PATCH_CAP_FULL).await?;

    // Same decode-then-tidy order as `commit_diff_for_repo`: `over_cap` is the
    // reader's byte-level fact and is authoritative, never re-derived from the
    // decoded string's length (`from_utf8_lossy` expands each invalid byte to a
    // 3-byte U+FFFD, so a complete sub-cap patch can decode to more than the
    // cap and would then be reported as cut when nothing was).
    let mut patch = String::from_utf8_lossy(&bytes).into_owned();
    if over_cap {
        truncate_at_line(&mut patch, DIFF_PATCH_CAP_FULL);
    }

    Ok(git_vista_protocol::diff::SpecDiff {
        spec,
        patch,
        truncated: over_cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use git_vista_core::identity::RepositoryId;
    use git_vista_core::layout::stream::canonicalize_edges;
    use git_vista_core::model::CommitSummary;
    use git_vista_protocol::diff::DiffSpec;
    use git_vista_protocol::{
        ApiError, ChangeKind, ChangeSides, ErrorCode, RepositoryDescriptor, StatusEntry,
        PROTOCOL_HEADER, PROTOCOL_VERSION,
    };
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

    // ---- truncate_at_line's multi-byte safety (#69, M2.16) --------------
    //
    // `truncate_at_line`'s own doc comment names the hazard: "The cap is first
    // walked back to a char boundary so a multi-byte character straddling it
    // can't panic the slice." That walk-back had no test until these — every
    // truncation fixture in this file is pure ASCII, where the hazard cannot
    // occur, so the guard was load-bearing and entirely unexercised. Its three
    // call sites (`commit_diff_for_repo`, the file reader, `staging_diff_for_repo`)
    // all feed it text decoded by `from_utf8_lossy`, which emits multi-byte
    // U+FFFD for every invalid input byte — so non-ASCII at the cap boundary is
    // not an exotic case, it is what malformed input decodes *to*.

    /// The specific panic the walk-back exists to prevent: a cap landing
    /// **inside** a multi-byte character. Without the boundary walk, `text[..end]`
    /// slices mid-character and panics.
    ///
    /// Byte layout of the fixture, counted by hand rather than derived:
    /// `o`=0, `k`=1, `\n`=2, then `日`=3..6, `日`=6..9, `\n`=9. A cap of 5 lands
    /// on the *third byte* of the first `日` — not a boundary.
    #[test]
    fn truncate_at_line_walks_back_off_a_multibyte_char_instead_of_panicking() {
        let mut text = String::from("ok\n日日\n");
        assert_eq!(
            text.len(),
            10,
            "fixture byte length changed; recount the cap"
        );
        assert!(
            !text.is_char_boundary(5),
            "cap 5 must land mid-character or this test proves nothing"
        );

        truncate_at_line(&mut text, 5);

        // Walk 5 → 4 → 3 (the start of the first `日`), then cut at the last
        // newline before it.
        assert_eq!(text, "ok");
    }

    /// The control: a cap that already sits on a char boundary must behave
    /// identically, so the test above is measuring the walk-back rather than
    /// truncation in general.
    #[test]
    fn truncate_at_line_on_an_exact_char_boundary_needs_no_walk_back() {
        let mut text = String::from("ok\n日日\n");
        assert!(text.is_char_boundary(3), "3 is the start of the first 日");

        truncate_at_line(&mut text, 3);

        assert_eq!(text, "ok");
    }

    /// With no newline before the cap, the function falls back to the
    /// walked-back byte position — which must still be a char boundary, or the
    /// `truncate` call panics. Keeps one whole character rather than a partial.
    #[test]
    fn truncate_at_line_with_no_newline_keeps_whole_characters() {
        let mut text = String::from("日日日");
        assert_eq!(text.len(), 9);
        assert!(!text.is_char_boundary(4));

        truncate_at_line(&mut text, 4);

        assert_eq!(text, "日", "cut mid-character instead of walking back");
    }

    /// The property the walk-back actually guarantees, stated directly: for
    /// **every** cap position over multi-byte text, the call completes and
    /// leaves valid UTF-8.
    ///
    /// A cap is not a value this code chooses — it is `DIFF_PATCH_CAP` measured
    /// against whatever bytes git emitted, so which byte it lands on is
    /// effectively arbitrary. The three cases above pin specific known-bad
    /// offsets; this one closes the gaps between them, and is what would catch a
    /// future rewrite that handles some boundary cases but not all.
    #[test]
    fn truncate_at_line_never_panics_at_any_cap_over_multibyte_text() {
        // Deliberately mixed: ASCII, 2-byte (é), 3-byte (日), 4-byte (🦀), and
        // U+FFFD — the character `from_utf8_lossy` actually produces from
        // invalid input, which is how this text arises in production.
        let original = "a\né日\n🦀b\u{FFFD}\nzz";

        for cap in 0..=original.len() + 4 {
            let mut text = String::from(original);
            truncate_at_line(&mut text, cap);

            // `String` cannot hold invalid UTF-8, so surviving the call at all
            // is most of the proof; assert the result is a real prefix too, so
            // a "fix" that sanitised by rewriting bytes would not pass.
            assert!(
                original.starts_with(text.as_str()),
                "cap {cap} produced {text:?}, which is not a prefix of the input"
            );
        }
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

    // ---- POST /api/diff/spec: the four explicit modes (M2.16, #69) -------

    /// A repository where **each of the four `DiffSpec` modes sees a different
    /// change**, so a test can prove a mode diffed what it claims rather than
    /// merely returning some non-empty patch.
    ///
    /// `v.txt` moves through four values, each parked in a different place:
    ///
    /// ```text
    ///   one    commit 1 (branch `base`)
    ///   two    commit 2 (branch `main`, HEAD)
    ///   three  staged in the index, not committed
    ///   four   in the working tree, not staged
    /// ```
    ///
    /// So `WorktreeVsIndex` must see three→four, `IndexVsCommit(HEAD)` two→three,
    /// and `CommitVsCommit`/`RefVsRef` one→two. Four modes, four distinguishable
    /// answers — a mode that silently ran the wrong argv shows up as the wrong
    /// pair, not as a pass.
    fn four_mode_repo() -> (tempfile::TempDir, PathBuf, String, String) {
        let (dir, repo) = seeded_repo();

        std::fs::write(repo.join("v.txt"), "one\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "v = one"]);
        let c1 = out(&repo, &["rev-parse", "HEAD"]);
        run(&repo, &["branch", "base"]);

        std::fs::write(repo.join("v.txt"), "two\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "v = two"]);
        let c2 = out(&repo, &["rev-parse", "HEAD"]);

        // Staged but uncommitted.
        std::fs::write(repo.join("v.txt"), "three\n").unwrap();
        run(&repo, &["add", "-A"]);

        // Working tree, on top of the staged value and not added.
        std::fs::write(repo.join("v.txt"), "four\n").unwrap();

        (dir, repo, c1, c2)
    }

    /// Assert a patch changes exactly `from` → `to`, and **not** any of the
    /// other values in play. The negative half is the point: without it, a
    /// patch containing every value (what `git diff` against the wrong base
    /// would produce) satisfies the positive assertions too.
    fn assert_changes(patch: &str, from: &str, to: &str, mode: &str) {
        assert!(
            patch.contains(&format!("-{from}")),
            "{mode}: expected removal of {from:?}; patch was:\n{patch}"
        );
        assert!(
            patch.contains(&format!("+{to}")),
            "{mode}: expected addition of {to:?}; patch was:\n{patch}"
        );
        for other in ["one", "two", "three", "four"] {
            if other == from || other == to {
                continue;
            }
            assert!(
                !patch.contains(&format!("-{other}\n")) && !patch.contains(&format!("+{other}\n")),
                "{mode}: patch mentions {other:?}, so it diffed the wrong pair;\
                 \npatch was:\n{patch}"
            );
        }
    }

    #[tokio::test]
    async fn spec_diff_worktree_vs_index_sees_the_unstaged_edit_only() {
        let (_dir, repo, _c1, _c2) = four_mode_repo();
        let out = spec_diff_for_repo(&repo, DiffSpec::WorktreeVsIndex)
            .await
            .expect("worktree-vs-index answers");
        assert_changes(&out.patch, "three", "four", "WorktreeVsIndex");
        assert!(!out.truncated);
        assert_eq!(out.spec, DiffSpec::WorktreeVsIndex, "spec must echo back");
    }

    #[tokio::test]
    async fn spec_diff_index_vs_commit_sees_the_staged_edit_only() {
        let (_dir, repo, _c1, c2) = four_mode_repo();
        let spec = DiffSpec::IndexVsCommit {
            commit: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
        };
        let out = spec_diff_for_repo(&repo, spec.clone())
            .await
            .expect("index-vs-commit answers");
        assert_changes(&out.patch, "two", "three", "IndexVsCommit");
        assert_eq!(out.spec, spec);
    }

    #[tokio::test]
    async fn spec_diff_commit_vs_commit_sees_only_what_is_committed() {
        let (_dir, repo, c1, c2) = four_mode_repo();
        let spec = DiffSpec::CommitVsCommit {
            base: git_vista_protocol::plan::CommitOid::new(&c1).unwrap(),
            target: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
        };
        let out = spec_diff_for_repo(&repo, spec.clone())
            .await
            .expect("commit-vs-commit answers");
        // Neither the staged nor the worktree value may appear: this mode
        // reads committed history only.
        assert_changes(&out.patch, "one", "two", "CommitVsCommit");
        assert_eq!(out.spec, spec);
    }

    #[tokio::test]
    async fn spec_diff_ref_vs_ref_resolves_names_to_the_same_answer() {
        let (_dir, repo, _c1, _c2) = four_mode_repo();
        let spec = DiffSpec::RefVsRef {
            base: git_vista_protocol::plan::RefName::new("base").unwrap(),
            target: git_vista_protocol::plan::RefName::new("main").unwrap(),
        };
        let out = spec_diff_for_repo(&repo, spec.clone())
            .await
            .expect("ref-vs-ref answers");
        assert_changes(&out.patch, "one", "two", "RefVsRef");
        assert_eq!(out.spec, spec);
    }

    /// The four modes must not collapse into each other. Stated as a direct
    /// comparison because every per-mode test above could pass while two modes
    /// quietly ran identical argv — `CommitVsCommit` and `RefVsRef` genuinely
    /// *do* produce identical argv shapes by design, so "they differ" cannot be
    /// assumed from the type alone.
    #[tokio::test]
    async fn the_worktree_index_and_commit_modes_return_genuinely_different_patches() {
        let (_dir, repo, c1, c2) = four_mode_repo();

        let worktree = spec_diff_for_repo(&repo, DiffSpec::WorktreeVsIndex)
            .await
            .unwrap();
        let index = spec_diff_for_repo(
            &repo,
            DiffSpec::IndexVsCommit {
                commit: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
            },
        )
        .await
        .unwrap();
        let committed = spec_diff_for_repo(
            &repo,
            DiffSpec::CommitVsCommit {
                base: git_vista_protocol::plan::CommitOid::new(&c1).unwrap(),
                target: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
            },
        )
        .await
        .unwrap();

        assert_ne!(worktree.patch, index.patch);
        assert_ne!(index.patch, committed.patch);
        assert_ne!(worktree.patch, committed.patch);
    }

    /// `--no-textconv` is a security property, not a formatting preference: a
    /// repository's own `.gitattributes` can bind a `diff=<driver>` textconv
    /// filter, and git *executes* that configured program to render file
    /// contents. This proves the flag actually reaches git.
    ///
    /// # What removing the flag actually does here — measured, not assumed
    ///
    /// Mutation-checked by deleting `--no-textconv` from `spec_diff_for_repo`'s
    /// argv and re-running: baseline 5 pass, mutated 4 pass and **this test
    /// alone** fails. So it does guard the flag specifically rather than
    /// tripping on any change.
    ///
    /// But it fails by a different route than the assertion below suggests, and
    /// that is worth stating rather than leaving for someone to rediscover.
    /// Without the flag, git tries to run the filter, needs a temp file to do
    /// it, and **the sandbox refuses**:
    ///
    /// ```text
    /// (500, "fatal: unable to create temp-file: Permission denied")
    /// ```
    ///
    /// The call errors before any patch exists, so `unwrap()` panics and the
    /// marker assertion never evaluates. That is a genuinely good finding — the
    /// sandbox blocks textconv execution independently of this flag, so the two
    /// are defence in depth rather than one guard. It also means **this test
    /// would still go red if the marker assertion were deleted**, which is
    /// exactly the kind of overlap that makes a test look stronger than it is.
    ///
    /// The assertion is kept because it is the one that stays meaningful if the
    /// sandbox is ever loosened, or on a filter that needs no temp file — but
    /// on this box, today, the sandbox fires first.
    #[tokio::test]
    async fn spec_diff_never_runs_a_repository_configured_textconv_filter() {
        let (_dir, repo, _c1, c2) = four_mode_repo();

        // A textconv driver that replaces any file's rendered content. If it
        // runs, the marker appears instead of the real diff text.
        std::fs::write(repo.join(".gitattributes"), "v.txt diff=pwned\n").unwrap();
        run(
            &repo,
            &["config", "diff.pwned.textconv", "echo TEXTCONV_RAN"],
        );

        for (label, spec) in [
            ("WorktreeVsIndex", DiffSpec::WorktreeVsIndex),
            (
                "IndexVsCommit",
                DiffSpec::IndexVsCommit {
                    commit: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
                },
            ),
        ] {
            // `expect`, not `unwrap`: with the flag removed this is where the
            // failure actually lands (sandbox-denied temp file), so the message
            // should name the cause rather than printing a bare Err.
            let out = spec_diff_for_repo(&repo, spec).await.unwrap_or_else(|e| {
                panic!(
                    "{label}: the diff read failed instead of answering: {e:?}. \
                     If this is a temp-file permission error, git attempted a \
                     textconv filter — meaning --no-textconv is missing from \
                     this mode's argv and the sandbox caught what the flag \
                     should have prevented."
                )
            });
            assert!(
                !out.patch.contains("TEXTCONV_RAN"),
                "{label}: a repository-configured textconv filter executed — \
                 --no-textconv is missing from this mode's argv"
            );
        }
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

    // ---- malicious `{*path}` against GET /api/file/{id}/{*path} (#67) --------
    //
    // `file_at_commit_for_repo` turns `path` into a `<rev>:<path>` git revision
    // spec and shells out to `git -C <repo> show <spec>`. `-C <repo>` is
    // equivalent to `cd <repo> && git show <spec>` — the process's effective cwd
    // for git's own `<rev>:./path` / `<rev>:../path` resolution (documented in
    // gitrevisions(7)) is therefore always `repo`, which every real caller
    // (`resolve_repo`/`resolve_worktree`/`current()`) sets to a registered
    // worktree's own root, never a subdirectory of one. So the cwd-relative
    // resolution these tests probe is always rooted at the tree root in
    // production, and — as the tests below establish — git itself refuses a
    // `../` that would walk above that cwd ("outside repository"), independent
    // of anything this server does. That is the fact this whole battery exists
    // to pin down instead of assume.

    /// A repository shaped to exercise the malicious-path battery: a root file,
    /// a subdirectory (so a tree-vs-blob path exists), and a **committed
    /// symlink** whose target must come back as blob content, never followed.
    fn path_battery_repo() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = seeded_repo();
        std::fs::write(repo.join("secret.txt"), "root-secret\n").unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/file.txt"), "sub-file\n").unwrap();
        std::os::unix::fs::symlink("file.txt", repo.join("sub/link.txt")).unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "path battery fixture"]);
        (dir, repo)
    }

    /// `../../../etc/passwd`, and a same-depth `../` from the tree root: git's
    /// own boundary check refuses to resolve a `<rev>:../path` that would walk
    /// above the cwd it resolved `-C repo` to (the worktree root), independent
    /// of the tree object. This is the uncertain case the task exists to
    /// establish, and it comes back a hard refusal, not a path.
    #[tokio::test]
    async fn file_read_relative_traversal_cannot_walk_above_repo_root() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        for path in ["../../../etc/passwd", "../secret.txt", "../../secret.txt"] {
            let err = file_at_commit_for_repo(&repo, &id, path)
                .await
                .expect_err(&format!("{path} must not resolve"));
            assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(
                err.1.contains("outside repository"),
                "path {path:?} produced unexpected message: {}",
                err.1
            );
        }
    }

    /// `./secret.txt` resolves from the same cwd (the repo root) precisely as
    /// the bare tree-relative path does — the positive control for the
    /// traversal test above: `./` and root-relative agree because cwd == tree
    /// root in production.
    #[tokio::test]
    async fn file_read_dot_slash_prefix_matches_tree_relative_path() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let dotted = file_at_commit_for_repo(&repo, &id, "./secret.txt")
            .await
            .expect("./secret.txt must resolve, cwd is the tree root");
        let bare = file_at_commit_for_repo(&repo, &id, "secret.txt")
            .await
            .expect("control read");
        assert_eq!(dotted.content, bare.content);
        assert_eq!(dotted.content, "root-secret\n");
    }

    /// A leading `/` is not tree-root shorthand — git treats it as a literal
    /// path component and reports the object missing, the same shape as any
    /// other not-found path.
    #[tokio::test]
    async fn file_read_leading_slash_is_not_found_not_root_shorthand() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let err = file_at_commit_for_repo(&repo, &id, "/secret.txt")
            .await
            .expect_err("a leading slash must not silently mean the tree root");
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// axum's `{*path}` wildcard percent-decodes the captured string before the
    /// handler ever sees it (verified here against the real extractor, not
    /// assumed), so `%2e%2e%2f` arrives at `file_at_commit_for_repo` already
    /// turned into a literal `../` — no double-decoding boundary for an
    /// attacker to exploit, and the traversal refusal above still applies to
    /// whatever comes out the other side.
    #[tokio::test]
    async fn axum_wildcard_decodes_percent_encoding_before_the_handler() {
        async fn echo(AxumPath(path): AxumPath<String>) -> String {
            path
        }
        let app = Router::new().route("/f/{*path}", get(echo));
        let req = axum::http::Request::get("/f/%2e%2e%2fsecret.txt%2e%2e")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let decoded = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(decoded, "../secret.txt..");

        // Double-encoded (`%252e` -> literal `%2e`, not `.`) must NOT decode a
        // second time anywhere in the pipeline — it should reach the handler
        // still percent-escaped text and fail as a not-found path, not as a
        // second-order traversal.
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);
        let err = file_at_commit_for_repo(&repo, &id, "%252e%252e%252fsecret.txt")
            .await
            .expect_err("double-encoded traversal must not resolve");
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A path that names a **tree**, not a blob, is now a `404` (#168) — not
    /// the `200` this test used to pin. `git show <rev>:<dir>` happily prints
    /// a directory listing, and until this change the handler forwarded it
    /// verbatim as if it were file content (no NUL, so it wasn't even flagged
    /// binary). A tree is a different resource from a file, not another
    /// representation of the same one, so the fix is a clean rejection
    /// rather than a discriminator bolted onto `FileContent` — see the
    /// doc comment on `file_at_commit_for_repo`. This test previously pinned
    /// the listing-as-200 behaviour under a name saying so; it is now the
    /// regression test for the rejection instead, renamed to match.
    #[tokio::test]
    async fn file_read_of_a_tree_path_is_rejected_not_returned_as_content() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let err = file_at_commit_for_repo(&repo, &id, "sub")
            .await
            .expect_err("a tree path must not answer as file content");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(
            err.1.contains("tree"),
            "reason should name the object kind: {}",
            err.1
        );
    }

    /// An empty path segment (`<id>:`) means the root tree in git, and is
    /// rejected for exactly the same reason as the named-tree case above, one
    /// level up — deliberately, not by accident: nothing distinguishes "no
    /// path given" from "path names the root tree" once the type check is in
    /// place, and the root tree is exactly as much "not a file" as `sub` is.
    #[tokio::test]
    async fn file_read_of_empty_path_is_rejected_as_the_root_tree() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let err = file_at_commit_for_repo(&repo, &id, "")
            .await
            .expect_err("an empty path names the root tree, not a file");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(
            err.1.contains("tree"),
            "reason should name the object kind: {}",
            err.1
        );
    }

    /// The trap this task exists to close: a path that is a regular **file**
    /// in the parent commit and becomes a **directory** in the child commit.
    /// A naive fix that made the tree case "fail" the existing `<id>:<path>`
    /// vs `<id>^:<path>` content-read ladder would fall through to the
    /// parent on the child's tree and hand back the parent's *file* bytes
    /// with a 200 — silently answering a request for commit `X` with content
    /// from `X^`. The type check must resolve against `X` first and reject
    /// immediately on a tree, never reaching the parent at all.
    #[tokio::test]
    async fn a_file_that_becomes_a_directory_is_rejected_not_served_from_the_parent() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("was-a-file"), "PARENT-FILE-CONTENT\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(
            &repo,
            &["commit", "-q", "-m", "parent: was-a-file is a file"],
        );
        let parent_id = out(&repo, &["rev-parse", "HEAD"]);

        // The child replaces the file with a directory of the same name.
        run(&repo, &["rm", "-q", "was-a-file"]);
        std::fs::create_dir_all(repo.join("was-a-file")).unwrap();
        std::fs::write(repo.join("was-a-file/inner.txt"), "inner\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(
            &repo,
            &["commit", "-q", "-m", "child: was-a-file is now a directory"],
        );
        let child_id = out(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(child_id, parent_id);

        let err = file_at_commit_for_repo(&repo, &child_id, "was-a-file")
            .await
            .expect_err("a directory in the requested commit must be rejected, not silently answered from the parent's file");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(!err.1.contains("PARENT-FILE-CONTENT"));

        // Control: the parent's own read of the same path still works and
        // still returns the file it always did — the fix changed only the
        // child's answer, not the parent's.
        let parent_file = file_at_commit_for_repo(&repo, &parent_id, "was-a-file")
            .await
            .expect("the parent's own read of the file is unaffected");
        assert_eq!(parent_file.content, "PARENT-FILE-CONTENT\n");
    }

    /// The mirror of the trap test above, for the case #167's original
    /// fallback exists to serve: a path this commit **deleted** (so the
    /// first type resolution genuinely finds nothing) whose *parent* version
    /// was a **tree**, not a file. Before #168 this returned the parent's
    /// directory listing as a 200 — the same wart as the direct case, one
    /// commit removed, reached only through the fallback ladder. The type
    /// check must apply to the fallback's resolution too, not just the first
    /// attempt.
    #[tokio::test]
    async fn a_deleted_path_whose_parent_was_a_tree_is_rejected_through_the_fallback() {
        let (_dir, repo) = path_battery_repo();
        let parent_id = out(&repo, &["rev-parse", "HEAD"]);

        // The child deletes `sub` entirely, so `<child>:sub` resolves to
        // nothing and the fallback is what actually answers.
        run(&repo, &["rm", "-q", "-r", "sub"]);
        run(&repo, &["commit", "-q", "-m", "child: delete sub"]);
        let child_id = out(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(child_id, parent_id);

        let err = file_at_commit_for_repo(&repo, &child_id, "sub")
            .await
            .expect_err("the parent's tree must not leak through the fallback as a 200");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(
            !err.1.contains("file.txt"),
            "no listing content should appear in the error"
        );
    }

    /// A submodule (a `commit`-typed tree entry) is exactly as much "not a
    /// file" as a directory — `git show <rev>:<submodule-path>` prints the
    /// referenced commit's own log/diff, not the submodule's own bytes, which
    /// is an even more misleading 200 than a directory listing would be.
    #[tokio::test]
    async fn a_submodule_entry_is_rejected_not_shown_as_the_referenced_commits_log() {
        let (_dir, repo) = seeded_repo();
        let inner_commit = out(&repo, &["rev-parse", "HEAD"]);
        // A gitlink tree entry (mode 160000) pointing at some commit — enough
        // to make git treat the path as type `commit`, with no real
        // submodule checkout required for this handler-level test.
        run(
            &repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                &inner_commit,
                "vendor/lib",
            ],
        );
        run(&repo, &["commit", "-q", "-m", "add a submodule gitlink"]);
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let err = file_at_commit_for_repo(&repo, &id, "vendor/lib")
            .await
            .expect_err("a submodule gitlink must not answer as file content");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(
            err.1.contains("commit"),
            "reason should name the object kind: {}",
            err.1
        );
    }

    /// A path with an embedded newline can never name a real git object, so it
    /// must fail as a clean not-found — not panic, and not somehow be
    /// interpreted as two arguments (it travels as a single argv element, same
    /// belt-and-braces as the id check above it).
    #[tokio::test]
    async fn file_read_embedded_newline_is_a_clean_not_found() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let err = file_at_commit_for_repo(&repo, &id, "secret.txt\nsub/file.txt")
            .await
            .expect_err("a newline-bearing path cannot name a real object");
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A several-KB path is refused cleanly by git (no such blob) rather than
    /// causing unbounded allocation or a hang on this server's side — the read
    /// is still going through `git_stdout_capped`, the same bounded reader as
    /// every other file/diff read.
    #[tokio::test]
    async fn file_read_very_long_path_is_refused_cleanly() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);
        let long_path = "a".repeat(8_000);

        let err = file_at_commit_for_repo(&repo, &id, &long_path)
            .await
            .expect_err("no several-KB path exists in the fixture");
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A committed symlink's blob **is** its target string — `git show` must
    /// report that literal text, not follow the link and return the linked
    /// file's content. Confirms path resolution never leaves git's own object
    /// model to touch the filesystem's symlink semantics.
    #[tokio::test]
    async fn file_read_of_a_committed_symlink_returns_target_text_not_dereferenced() {
        let (_dir, repo) = path_battery_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let link = file_at_commit_for_repo(&repo, &id, "sub/link.txt")
            .await
            .expect("the symlink blob itself resolves");
        assert_eq!(link.content, "file.txt");
        assert!(!link.content.contains("sub-file"));
    }

    /// Every case above, again against the `<id>^:path>` fallback: build a
    /// commit whose tree lacks all of the fixture's paths (so the first `show`
    /// attempt always misses and the retry against the parent is what actually
    /// answers), then repeat the security-relevant assertions. A malicious path
    /// that only got exercised on the happy path would miss this second attempt
    /// entirely.
    #[tokio::test]
    async fn malicious_paths_behave_identically_through_the_parent_fallback() {
        let (_dir, repo) = path_battery_repo();
        let parent_id = out(&repo, &["rev-parse", "HEAD"]);

        // A child commit that deletes everything path_battery_repo added, so
        // `<child>:<path>` always misses and every read below is answered by
        // the `<child>^:<path>` retry against `parent_id`'s tree.
        run(&repo, &["rm", "-q", "-r", "secret.txt", "sub"]);
        run(&repo, &["commit", "-q", "-m", "delete everything"]);
        let child_id = out(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(
            child_id, parent_id,
            "the fallback must actually cross a commit"
        );

        // Traversal is still refused.
        let err = file_at_commit_for_repo(&repo, &child_id, "../secret.txt")
            .await
            .expect_err("traversal must be refused through the fallback too");
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.1.contains("outside repository"));

        // The symlink still comes back as its target text, not dereferenced.
        // `FileContent.id` echoes back the *requested* commit id even when the
        // content was actually read from its parent's tree — that's the
        // existing contract (see `bounded_file_read_caps_without_parent_fallback`
        // above), not something this test introduces.
        let link = file_at_commit_for_repo(&repo, &child_id, "sub/link.txt")
            .await
            .expect("the fallback must reach the parent's symlink blob");
        assert_eq!(link.content, "file.txt");
        assert_eq!(link.id, child_id);

        // A tree path is rejected through the fallback too (#168) — covered
        // in full, including the "no listing leaks into the error" check, by
        // `a_deleted_path_whose_parent_was_a_tree_is_rejected_through_the_fallback`.
        let tree_err = file_at_commit_for_repo(&repo, &child_id, "sub")
            .await
            .expect_err("a tree must not answer as content, fallback or not");
        assert_eq!(tree_err.0, StatusCode::NOT_FOUND);

        // `truncated` must never be true for this tiny fixture — a sign the
        // cap logic didn't misfire on the retry path.
        assert!(!link.truncated);
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

    /// One page read for `repo` at `cursor`/`limit` under `headers`, plus its
    /// walk count. `history_codec` is keyed deterministically, so a cursor minted
    /// by one call opens on the next exactly as it would inside one process.
    async fn page_parts(
        repo: &Path,
        cursor: Option<&str>,
        limit: usize,
        headers: &HeaderMap,
    ) -> (StatusCode, HeaderValue, Vec<u8>, usize) {
        let codec = history_codec();
        let target = history_target(repo, &codec);
        let walks = AtomicUsize::new(0);
        let response = page_for_target(&target, cursor, limit, &codec, headers, &walks)
            .await
            .expect("page read");
        let (status, etag, body) = parts_of(response).await;
        (status, etag, body, walks.load(Ordering::Relaxed))
    }

    /// The page-1 read for `repo` at `limit` under `headers`, plus its walk count.
    async fn page_one_parts(
        repo: &Path,
        limit: usize,
        headers: &HeaderMap,
    ) -> (StatusCode, HeaderValue, Vec<u8>, usize) {
        page_parts(repo, None, limit, headers).await
    }

    /// Follow the cursor chain from page 1 to exhaustion at `limit`, decoding
    /// every page. The last page a history yields is the one that carries no
    /// cursor — which may legitimately be an empty page, when the previous walk
    /// stopped exactly at the window's end.
    async fn all_pages(repo: &Path, limit: usize) -> Vec<Page> {
        let headers = HeaderMap::new();
        let mut pages: Vec<Page> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (status, _, body, walks) =
                page_parts(repo, cursor.as_deref(), limit, &headers).await;
            assert_eq!(status, StatusCode::OK, "every page in a chain is a 200");
            assert_eq!(walks, 1, "one page, one Topo walk");
            let page: Page = serde_json::from_slice(&body).expect("Page decodes");
            cursor = page.cursor.clone();
            pages.push(page);
            assert!(
                pages.len() <= 64,
                "paging at limit {limit} must terminate on a fixture this small"
            );
            if cursor.is_none() {
                return pages;
            }
        }
    }

    /// An `If-None-Match:` header map carrying exactly `value`.
    fn if_none_match_header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        headers
    }

    /// The history target resolves through the same fail-closed selector arms
    /// the other read endpoints use: a malformed id never reaches path
    /// resolution, and an id the catalog never registered resolves to nothing
    /// rather than falling back to any path. (Not a plan-named test — it exists
    /// so the new resolution seam's refusals are pinned, since the nine tests
    /// below construct their targets directly.)
    #[test]
    fn resolve_history_target_fails_closed_on_a_bad_selector() {
        let codec = history_codec();

        // Matched rather than `expect_err`-ed on purpose: the Ok variant holds a
        // canonical filesystem path, and a `Debug` bound would put it in a panic
        // message.
        let Err((status, _)) = resolve_history_target(Some("not-an-id"), &codec) else {
            panic!("a malformed selector must be refused");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let unknown = WorktreeId::from_git_dir("/no/such/repo/.git").to_string();
        let Err((status, _)) = resolve_history_target(Some(&unknown), &codec) else {
            panic!("an unregistered id must be refused");
        };
        assert_eq!(status, StatusCode::NOT_FOUND);
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

    // ---- paged replay: contiguity, edge ownership, stub ownership -------------

    /// One commit in `repo` at a fixed author/committer timestamp, so the Topo
    /// `DateOrder` these fixtures depend on is not a function of wall-clock time.
    fn commit_at(repo: &Path, file: &str, message: &str, epoch: i64) {
        std::fs::write(repo.join(file), format!("{message}\n")).unwrap();
        run(repo, &["add", "-A"]);
        let stamp = format!("{epoch} +0000");
        run_env(
            repo,
            &["commit", "-q", "-m", message],
            &[("GIT_AUTHOR_DATE", &stamp), ("GIT_COMMITTER_DATE", &stamp)],
        );
    }

    /// The plan's adversarial edge fixture, and the reason it is adversarial:
    ///
    /// ```text
    ///   row 0  M   merge, parents [A, B]
    ///   row 1  A   parent [R]
    ///   row 2  R   recorded shallow boundary — its parent Z is cut
    ///   row 3  B   unrelated root, older than R
    /// ```
    ///
    /// Topo `DateOrder` emits `M(0) -> [A(1), B(3)]` and `A(1) -> R(2)`, so the
    /// `M -> B` edge resolves three rows below its own row: any page containing
    /// row 3 owns an edge whose `from_row` is 0. That is the shape a page-local
    /// row index cannot express, which is what `ResolvedEdge.parent_ordinal` and
    /// the checkpointed `PendingEdge` list exist for.
    ///
    /// Returns `(repo, z_oid)` — `Z` must never appear in any page.
    fn adversarial_edge_repo(parent: &Path) -> (PathBuf, String) {
        let repo = parent.join("edges");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);

        commit_at(&repo, "z.txt", "z", 1_700_001_000);
        let z = out(&repo, &["rev-parse", "HEAD"]);
        commit_at(&repo, "r.txt", "r", 1_700_003_000);
        let r = out(&repo, &["rev-parse", "HEAD"]);
        commit_at(&repo, "a.txt", "a", 1_700_004_000);

        // B: an unrelated root, deliberately *older* than R so `DateOrder` puts
        // it last even though it is the merge's second parent.
        run(&repo, &["checkout", "-q", "--orphan", "bside"]);
        run(&repo, &["rm", "-r", "-f", "-q", "--cached", "."]);
        for stale in ["z.txt", "r.txt", "a.txt"] {
            std::fs::remove_file(repo.join(stale)).unwrap();
        }
        commit_at(&repo, "b.txt", "b", 1_700_002_000);

        run(&repo, &["checkout", "-q", "main"]);
        let stamp = format!("{} +0000", 1_700_005_000_i64);
        run_env(
            &repo,
            &[
                "merge",
                "-q",
                "--no-ff",
                "--allow-unrelated-histories",
                "-m",
                "m",
                "bside",
            ],
            &[("GIT_AUTHOR_DATE", &stamp), ("GIT_COMMITTER_DATE", &stamp)],
        );

        // Record R as a shallow boundary *after* every commit is written: from
        // here on Z is unreachable to the traversal, cut rather than missing.
        std::fs::write(repo.join(".git").join("shallow"), format!("{r}\n")).unwrap();
        (repo, z)
    }

    /// A linear history carrying two stub anchors: one local branch demoted at
    /// row 1, and a three-branch cascade demoted at row 3. Local `main` outranks
    /// every one of them, so each is a [`FrameStub`] rather than a badge.
    fn stub_cascade_repo(parent: &Path) -> PathBuf {
        let repo = deterministic_repo(parent, "stubs", 6);
        run(&repo, &["branch", "zeta", "HEAD~1"]);
        run(&repo, &["branch", "alpha", "HEAD~3"]);
        run(&repo, &["branch", "beta", "HEAD~3"]);
        run(&repo, &["branch", "gamma", "HEAD~3"]);
        repo
    }

    /// Paging is a partition of the same replay, not a different one: at any page
    /// size, the concatenated pages are the uninterrupted walk's rows, in order,
    /// with absolute row numbers that never repeat and never skip.
    #[tokio::test]
    async fn pages_are_contiguous_at_limits_one_and_seven() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 8);

        let (_, _, oracle_body, _) = page_one_parts(&repo, MAX_PAGE_LIMIT, &HeaderMap::new()).await;
        let oracle: Page = serde_json::from_slice(&oracle_body).unwrap();
        assert_eq!(oracle.rows.len(), 8, "the uninterrupted page holds it all");
        assert!(
            oracle.cursor.is_none(),
            "a walk that ended before the window filled opens no next page"
        );

        for limit in [1_usize, 7] {
            let pages = all_pages(&repo, limit).await;

            let mut expected_start = 0_usize;
            let mut union: Vec<GraphRow> = Vec::new();
            for (index, page) in pages.iter().enumerate() {
                assert!(
                    page.rows.len() <= limit,
                    "limit {limit}: page {index} overran the window"
                );
                for (offset, row) in page.rows.iter().enumerate() {
                    assert_eq!(
                        row.row,
                        expected_start + offset,
                        "limit {limit}: page {index} row {offset} is not contiguous"
                    );
                }
                expected_start += page.rows.len();
                union.extend(page.rows.iter().cloned());
            }

            assert_eq!(
                union.iter().map(|r| r.row).collect::<Vec<_>>(),
                (0..8).collect::<Vec<_>>(),
                "limit {limit}: absolute rows are 0..8 exactly once, in order"
            );
            assert_eq!(
                union
                    .iter()
                    .map(|r| r.commit.id.clone())
                    .collect::<Vec<_>>(),
                oracle
                    .rows
                    .iter()
                    .map(|r| r.commit.id.clone())
                    .collect::<Vec<_>>(),
                "limit {limit}: the pages replay the uninterrupted walk"
            );
            assert_eq!(
                union.iter().map(|r| r.lane).collect::<Vec<_>>(),
                oracle.rows.iter().map(|r| r.lane).collect::<Vec<_>>(),
                "limit {limit}: lanes survive the checkpoint/resume boundary"
            );
            assert_eq!(
                union.iter().map(|r| r.color).collect::<Vec<_>>(),
                oracle.rows.iter().map(|r| r.color).collect::<Vec<_>>(),
                "limit {limit}: the prefix replay rebuilt the same claims"
            );
            assert_eq!(
                union
                    .iter()
                    .map(|r| r.refs.iter().map(|x| x.name.clone()).collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
                oracle
                    .rows
                    .iter()
                    .map(|r| r.refs.iter().map(|x| x.name.clone()).collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
                "limit {limit}: badges land on their own row, once"
            );

            let generations: HashSet<_> = pages.iter().map(|p| p.generation.clone()).collect();
            assert_eq!(
                generations.len(),
                1,
                "limit {limit}: one stable history, one generation"
            );
            assert!(
                pages.last().unwrap().cursor.is_none(),
                "limit {limit}: the chain ends without a cursor"
            );
        }
    }

    /// Edge ownership at every page boundary, over the plan's adversarial graph.
    ///
    /// Each edge is delivered exactly once, on the page that owns its *parent*
    /// row, even when the child row is pages away. Raw concatenation is therefore
    /// deliberately **not** canonical order — only a canonicalized clone of the
    /// completed union is, and it must equal the uninterrupted walk's own edges.
    #[tokio::test]
    async fn paged_edge_union_canonicalizes_to_uninterrupted_oracle_at_every_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, z) = adversarial_edge_repo(dir.path());

        let (_, _, oracle_body, _) = page_one_parts(&repo, MAX_PAGE_LIMIT, &HeaderMap::new()).await;
        let oracle: Page = serde_json::from_slice(&oracle_body).unwrap();

        // The fixture really is `M(0) -> [A(1), B(3)]`, `A(1) -> R(2)`, cut at R.
        let summaries: Vec<&str> = oracle
            .rows
            .iter()
            .map(|r| r.commit.summary.as_str())
            .collect();
        assert_eq!(
            summaries,
            vec!["m", "a", "r", "b"],
            "the adversarial Topo DateOrder the plan specifies"
        );
        assert!(
            oracle.rows[2].commit.parents.is_empty(),
            "a recorded shallow boundary reaches the layout as a root"
        );
        assert!(
            !oracle.rows.iter().any(|r| r.commit.id.0 == z),
            "the commit below the boundary is cut, not paged"
        );
        assert_eq!(
            oracle.edges,
            vec![
                Edge {
                    from_row: 0,
                    from_lane: 0,
                    to_row: 1,
                    to_lane: 0
                },
                Edge {
                    from_row: 0,
                    from_lane: 0,
                    to_row: 3,
                    to_lane: 1
                },
                Edge {
                    from_row: 1,
                    from_lane: 0,
                    to_row: 2,
                    to_lane: 0
                },
            ],
            "the uninterrupted oracle is canonical (from_row, parent ordinal, …)"
        );

        let mut saw_edge_from_an_earlier_page = false;
        let mut saw_noncanonical_raw_union = false;

        for limit in 1..=oracle.rows.len() {
            let pages = all_pages(&repo, limit).await;

            let mut start = 0_usize;
            let mut union_rows: Vec<GraphRow> = Vec::new();
            let mut raw_union: Vec<Edge> = Vec::new();
            for (index, page) in pages.iter().enumerate() {
                let end = start + page.rows.len();
                for edge in &page.edges {
                    assert!(
                        (start..end).contains(&edge.to_row),
                        "limit {limit}: page {index} [{start},{end}) must own only \
                         edges whose destination row it holds, got {edge:?}"
                    );
                    if edge.from_row < start {
                        saw_edge_from_an_earlier_page = true;
                    }
                }
                start = end;
                union_rows.extend(page.rows.iter().cloned());
                raw_union.extend(page.edges.iter().cloned());
            }

            assert_eq!(
                union_rows.len(),
                oracle.rows.len(),
                "limit {limit}: the union is the whole history"
            );
            assert_eq!(
                raw_union.len(),
                oracle.edges.len(),
                "limit {limit}: every edge exactly once — no duplicate, no drop"
            );
            let distinct: HashSet<_> = raw_union
                .iter()
                .map(|e| (e.from_row, e.from_lane, e.to_row, e.to_lane))
                .collect();
            assert_eq!(
                distinct.len(),
                oracle.edges.len(),
                "limit {limit}: the union holds no repeated edge"
            );

            if raw_union != oracle.edges {
                saw_noncanonical_raw_union = true;
            }

            // Only *this* — a canonicalized clone of the completed union, indexed
            // against absolute rows starting at zero — is required to equal the
            // oracle. `canonicalize_edges` is never called on page-local rows.
            let mut canonical = raw_union.clone();
            canonicalize_edges(&union_rows, &mut canonical);
            assert_eq!(
                canonical, oracle.edges,
                "limit {limit}: the completed union canonicalizes to the \
                 uninterrupted new-pipeline oracle"
            );
        }

        assert!(
            saw_edge_from_an_earlier_page,
            "the fixture must exercise a page owning an edge with from_row < n"
        );
        assert!(
            saw_noncanonical_raw_union,
            "raw concatenated page edge order is deliberately not required to be \
             canonical order; a fixture where it always happens to be proves nothing"
        );
    }

    /// Stub ownership at every page boundary: a stub rides the page that owns its
    /// anchor row and no other, a suppressed prefix emits none, and the cumulative
    /// column numbering survives the prefix replay.
    ///
    /// Per accepted decision D18, paged `lane_offset` is **row**-order numbering:
    /// the streaming classifier emits each stub on its anchor's page and cannot
    /// see later rows, so it cannot reproduce the whole-graph pass's
    /// priority-sorted seed order. The oracle here is the uninterrupted *new*
    /// pipeline, which is exactly what the frontend will render.
    #[tokio::test]
    async fn page_stubs_emit_once_on_anchor_page_with_stable_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let repo = stub_cascade_repo(dir.path());

        let (_, _, oracle_body, _) = page_one_parts(&repo, MAX_PAGE_LIMIT, &HeaderMap::new()).await;
        let oracle: Page = serde_json::from_slice(&oracle_body).unwrap();
        assert_eq!(oracle.rows.len(), 6);

        let anchor_one = oracle.rows[1].commit.id.clone();
        let anchor_three = oracle.rows[3].commit.id.clone();
        assert_eq!(
            oracle
                .stubs
                .iter()
                .map(|s| (
                    s.name.as_str(),
                    s.anchor_commit.clone(),
                    s.lane_offset,
                    s.depth
                ))
                .collect::<Vec<_>>(),
            vec![
                ("zeta", anchor_one.clone(), 0, 0),
                ("alpha", anchor_three.clone(), 1, 0),
                ("beta", anchor_three.clone(), 2, 1),
                ("gamma", anchor_three.clone(), 3, 2),
            ],
            "row-order cumulative offsets, name-sorted within one anchor (D18)"
        );
        for name in ["zeta", "alpha", "beta", "gamma"] {
            assert!(
                !oracle
                    .rows
                    .iter()
                    .any(|r| r.refs.iter().any(|x| x.name == name)),
                "{name} is drawn as a stub line, never as a second badge"
            );
        }

        for limit in 1..=oracle.rows.len() {
            let pages = all_pages(&repo, limit).await;

            let mut start = 0_usize;
            let mut union: Vec<FrameStub> = Vec::new();
            for (index, page) in pages.iter().enumerate() {
                let end = start + page.rows.len();
                let owned: HashSet<Oid> = page.rows.iter().map(|r| r.commit.id.clone()).collect();
                for stub in &page.stubs {
                    assert!(
                        owned.contains(&stub.anchor_commit),
                        "limit {limit}: page {index} [{start},{end}) carries a stub \
                         whose anchor row it does not own: {stub:?}"
                    );
                }
                start = end;
                union.extend(page.stubs.iter().cloned());
            }

            assert_eq!(
                union, oracle.stubs,
                "limit {limit}: each stub once, on its anchor page, with the \
                 cumulative offsets the uninterrupted classification hands out"
            );
        }
    }

    // ---- cursor drift, tamper, scope, and error precedence (Step 8, part B) ---

    /// A `count`-commit linear history built via `git fast-import`, carrying no
    /// `M` (modify) commands — every commit shares one empty tree, so the batch
    /// is small enough that fast-import writes it as individually addressable
    /// **loose** objects rather than one pack. The two walk-error fixtures below
    /// need that: they force a traversal failure by deleting one specific
    /// commit's object file, and a duplicate copy sitting in a pack would defeat
    /// the deletion.
    fn deep_linear_repo(parent: &Path, count: usize) -> (PathBuf, String, String) {
        use std::io::Write;
        assert!(count >= 2, "a walk-error fixture needs a root and a child");

        let repo = parent.join(format!("deep-{count}"));
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        // `git fast-import` only unpacks a batch this size automatically up to
        // `transfer.unpackLimit` (default 100); above that it always writes one
        // pack regardless of how few "M" commands the stream carries. Raise the
        // limit so a fixture of any size the tests below choose stays loose.
        run(&repo, &["config", "transfer.unpackLimit", "1000000"]);

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
        assert!(
            std::fs::read_dir(repo.join(".git/objects/pack"))
                .unwrap()
                .next()
                .is_none(),
            "no \"M\" commands means fast-import must never pack this fixture"
        );

        let tip = out(&repo, &["rev-parse", "refs/heads/main"]);
        let root = out(&repo, &["rev-list", "--max-parents=0", "refs/heads/main"]);
        (repo, tip, root)
    }

    /// Move `repo`'s `ref_name` to `new_oid` from a background thread, after a
    /// short fixed delay. The delay is comfortably longer than the calling
    /// test's already-in-flight snapshot read (a handful of small file reads,
    /// microseconds) and comfortably shorter than the multi-hundred/thousand
    /// commit walk these tests give it to race against, so the mutation lands
    /// strictly between the two.
    fn race_ref_move(
        repo: &Path,
        ref_name: &'static str,
        new_oid: &str,
    ) -> std::thread::JoinHandle<()> {
        let repo = repo.to_path_buf();
        let new_oid = new_oid.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            run(&repo, &["update-ref", ref_name, &new_oid]);
        })
    }

    /// A cursor page never revalidates against `If-None-Match`, even when the
    /// client presents that exact page's own current tag: only a Frame and page
    /// 1 are stable, addressable representations.
    #[tokio::test]
    async fn cursor_page_ignores_if_none_match() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 3);

        let (status_one, _, body_one, _) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
        assert_eq!(status_one, StatusCode::OK);
        let page_one: Page = serde_json::from_slice(&body_one).unwrap();
        let cursor = page_one.cursor.clone().expect("more rows remain");

        let (status_two, tag_two, body_two, walks_two) =
            page_parts(&repo, Some(&cursor), 1, &HeaderMap::new()).await;
        assert_eq!(status_two, StatusCode::OK);
        assert_eq!(walks_two, 1);

        // Presenting that exact, freshly computed tag back must still 200.
        let presented = if_none_match_header(tag_two.to_str().unwrap());
        let (status_three, tag_three, body_three, walks_three) =
            page_parts(&repo, Some(&cursor), 1, &presented).await;
        assert_eq!(
            status_three,
            StatusCode::OK,
            "a cursor page always 200s despite a matching If-None-Match"
        );
        assert_eq!(tag_three, tag_two);
        assert_eq!(body_three, body_two);
        assert_eq!(walks_three, 1);
    }

    /// A ref moving between the page that mints a cursor and the page that
    /// consumes it is refused as a 409 — caught by the cursor's own generation
    /// comparison, strictly before any traversal.
    #[tokio::test]
    async fn ref_move_between_pages_returns_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 4);

        let (status_one, _, body_one, walks_one) =
            page_one_parts(&repo, 1, &HeaderMap::new()).await;
        assert_eq!(status_one, StatusCode::OK);
        assert_eq!(walks_one, 1);
        let page_one: Page = serde_json::from_slice(&body_one).unwrap();
        let cursor = page_one.cursor.clone().expect("more rows remain");

        // The branch this cursor was minted against moves: a new commit lands.
        commit_at(&repo, "extra.txt", "extra", 1_700_009_000);

        let codec = history_codec();
        let target = history_target(&repo, &codec);
        let walks = AtomicUsize::new(0);
        let error = page_for_target(&target, Some(&cursor), 1, &codec, &HeaderMap::new(), &walks)
            .await
            .expect_err("a cursor pinned to a generation the repository has left must be refused");
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(error.1, "history moved");
        assert_eq!(
            walks.load(Ordering::Relaxed),
            0,
            "generation drift is caught before any walk"
        );
    }

    /// The generation can move *during* a page that never presented a cursor at
    /// all: the walk itself completes (against the seeds the initial snapshot
    /// captured), but the repository has moved by the time the success-path
    /// combined re-read runs, and that re-read still refuses the page. Driven
    /// through the real `api_contract` middleware, so the wire JSON — not just
    /// the handler's own tuple — is proved to carry `error.code == "conflict"`.
    #[tokio::test]
    async fn generation_move_during_page_returns_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, tip, _root) = deep_linear_repo(dir.path(), 1_500);

        // A new commit, built but referenced by nothing yet: the racer below
        // moves `main` onto it only after this request is already under way.
        let tree = out(&repo, &["rev-parse", &format!("{tip}^{{tree}}")]);
        let extra = out(&repo, &["commit-tree", &tree, "-p", &tip, "-m", "extra"]);
        let racer = race_ref_move(&repo, "refs/heads/main", &extra);

        let walks = Arc::new(AtomicUsize::new(0));
        let repo_for_route = repo.clone();
        let walks_for_route = Arc::clone(&walks);
        let app = Router::new()
            .route(
                "/api/commits",
                get(move || {
                    let repo_for_route = repo_for_route.clone();
                    let walks_for_route = Arc::clone(&walks_for_route);
                    async move {
                        let codec = history_codec();
                        let target = history_target(&repo_for_route, &codec);
                        page_for_target(
                            &target,
                            None,
                            1_500,
                            &codec,
                            &HeaderMap::new(),
                            walks_for_route.as_ref(),
                        )
                        .await
                    }
                }),
            )
            .layer(axum::middleware::from_fn(crate::middleware::api_contract));

        let req = axum::http::Request::get("/api/commits")
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        racer.join().unwrap();

        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "the walk ran against the old seeds; the repository moved before the re-read"
        );
        assert_eq!(
            walks.load(Ordering::Relaxed),
            1,
            "the walk itself ran exactly once, unlike a rejected cursor"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let err: ApiError = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            err.error.code,
            ErrorCode::Conflict,
            "the real middleware envelope, not just the handler's own tuple"
        );
        assert_eq!(err.error.message, "history moved");
    }

    /// A cursor whose signature no longer verifies — one flipped character — is
    /// the same generic 400 as every other codec failure, and costs nothing but
    /// the failed HMAC check.
    #[tokio::test]
    async fn tampered_cursor_is_bad_request_before_walk() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 3);

        let (_, _, body, _) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
        let page: Page = serde_json::from_slice(&body).unwrap();
        let cursor = page.cursor.clone().expect("more rows remain");

        let mut chars: Vec<char> = cursor.chars().collect();
        chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert_ne!(tampered, cursor);

        let codec = history_codec();
        let target = history_target(&repo, &codec);
        let walks = AtomicUsize::new(0);
        let error = page_for_target(
            &target,
            Some(&tampered),
            1,
            &codec,
            &HeaderMap::new(),
            &walks,
        )
        .await
        .expect_err("a tampered cursor must be refused");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1, "invalid history cursor");
        assert_eq!(walks.load(Ordering::Relaxed), 0);
    }

    /// A cursor minted for one repository must not open on a different one, even
    /// when the two happen to share a generation (byte-identical committed
    /// topology): the codec's own signature still verifies, so only the scope
    /// comparison — the same generic 400 — catches it.
    #[tokio::test]
    async fn same_generation_other_repository_cursor_is_rejected_before_walk() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = deterministic_repo(dir.path(), "alpha", 3);
        let beta = deterministic_repo(dir.path(), "beta", 3);

        let (_, _, alpha_body, _) = page_one_parts(&alpha, 1, &HeaderMap::new()).await;
        let alpha_page: Page = serde_json::from_slice(&alpha_body).unwrap();
        let cursor = alpha_page.cursor.clone().expect("more rows remain");

        let (_, _, beta_body, _) = page_one_parts(&beta, 1, &HeaderMap::new()).await;
        let beta_page: Page = serde_json::from_slice(&beta_body).unwrap();
        assert_eq!(
            alpha_page.generation, beta_page.generation,
            "identical committed topology shares one generation"
        );

        let codec = history_codec();
        let target = history_target(&beta, &codec);
        let walks = AtomicUsize::new(0);
        let error = page_for_target(&target, Some(&cursor), 1, &codec, &HeaderMap::new(), &walks)
            .await
            .expect_err(
                "a cursor minted for one repository must not open on another, \
                 even at the same generation",
            );
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1, "invalid history cursor");
        assert_eq!(walks.load(Ordering::Relaxed), 0);
    }

    /// A registered target's scope binds both halves of its `RepositoryHandle`:
    /// a cursor minted for one worktree of a repository must not open on a
    /// sibling worktree of that same repository, even though both share the
    /// same generation (they are the same committed history).
    #[tokio::test]
    async fn same_repository_sibling_worktree_cursor_is_rejected_before_walk() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 3);
        let path = repo.canonicalize().expect("a temp repo path resolves");
        let common = path.join(".git");
        let common_str = common.to_str().expect("a temp path is valid utf-8");

        let repository = RepositoryId::from_common_dir(common_str);
        let worktree_main = WorktreeId::from_git_dir(common_str);
        let worktree_other = WorktreeId::from_git_dir(&format!("{common_str}/worktrees/other"));
        let handle_main = RepositoryHandle::new(repository, worktree_main);
        let handle_other = RepositoryHandle::new(repository, worktree_other);
        assert_ne!(handle_main.worktree, handle_other.worktree);

        let codec = history_codec();
        let scope_main = codec.scope_for_target(Some(&handle_main), &path);
        let target_other = ResolvedHistoryTarget {
            path: path.clone(),
            read_only: false,
            handle: Some(handle_other),
            scope: codec.scope_for_target(Some(&handle_other), &path),
        };
        assert_ne!(
            scope_main, target_other.scope,
            "sibling worktrees of one repository bind different scopes"
        );

        let snapshot = read_history_snapshot(&path).await.expect("snapshot read");
        let cursor = codec
            .encode(
                scope_main,
                &snapshot.generation,
                &HistoryCursor { next_row: 1 },
            )
            .expect("signing a cursor for the main worktree's scope");

        let walks = AtomicUsize::new(0);
        let error = page_for_target(
            &target_other,
            Some(&cursor),
            1,
            &codec,
            &HeaderMap::new(),
            &walks,
        )
        .await
        .expect_err("a cursor scoped to a sibling worktree must not open on this one");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1, "invalid history cursor");
        assert_eq!(walks.load(Ordering::Relaxed), 0);
    }

    /// A shallow boundary set changing — deepening, then unshallowing — moves
    /// the generation without moving a single ref or either HEAD half. A cursor
    /// pinned before either move is a stale, rejected-before-walk 409 in both
    /// directions, and every fresh Frame/Page tag moves with it.
    #[tokio::test]
    async fn deepen_without_ref_move_rejects_stale_cursor_with_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 5);
        let path = repo.canonicalize().unwrap();
        let head_path = path.join(".git").join("HEAD");
        let ref_path = path.join(".git").join("refs").join("heads").join("main");
        let head_before = std::fs::read(&head_path).unwrap();
        let ref_before = std::fs::read(&ref_path).unwrap();

        let (_, tag_frame_before, _) = frame_parts(&repo, &HeaderMap::new()).await;
        let (_, tag_page_before, body_before, _) =
            page_one_parts(&repo, 1, &HeaderMap::new()).await;
        let page_before: Page = serde_json::from_slice(&body_before).unwrap();
        let cursor_before = page_before.cursor.clone().expect("more rows remain");
        let generation_before = page_before.generation.clone();

        let codec = history_codec();
        let target = history_target(&repo, &codec);

        // --- deepen: record a shallow boundary. Only `.git/shallow` changes.
        let boundary = out(&repo, &["rev-parse", "HEAD~2"]);
        std::fs::write(path.join(".git").join("shallow"), format!("{boundary}\n")).unwrap();
        assert_eq!(
            std::fs::read(&head_path).unwrap(),
            head_before,
            "HEAD is untouched by a deepen"
        );
        assert_eq!(
            std::fs::read(&ref_path).unwrap(),
            ref_before,
            "the branch ref is untouched by a deepen"
        );

        let walks_deepen = AtomicUsize::new(0);
        let error_deepen = page_for_target(
            &target,
            Some(&cursor_before),
            1,
            &codec,
            &HeaderMap::new(),
            &walks_deepen,
        )
        .await
        .expect_err("a cursor pinned before a deepen must be refused");
        assert_eq!(error_deepen.0, StatusCode::CONFLICT);
        assert_eq!(walks_deepen.load(Ordering::Relaxed), 0);

        let (_, tag_frame_deepened, _) = frame_parts(&repo, &HeaderMap::new()).await;
        let (_, tag_page_deepened, body_deepened, _) =
            page_one_parts(&repo, 1, &HeaderMap::new()).await;
        let page_deepened: Page = serde_json::from_slice(&body_deepened).unwrap();
        assert_ne!(
            generation_before, page_deepened.generation,
            "the shallow boundary is part of the history generation"
        );
        assert_ne!(tag_frame_before, tag_frame_deepened);
        assert_ne!(tag_page_before, tag_page_deepened);
        let cursor_deepened = page_deepened.cursor.clone().expect("more rows remain");

        // --- unshallow: clear the boundary. Again, only `.git/shallow` moves.
        std::fs::remove_file(path.join(".git").join("shallow")).unwrap();
        assert_eq!(
            std::fs::read(&head_path).unwrap(),
            head_before,
            "HEAD is untouched by an unshallow"
        );
        assert_eq!(
            std::fs::read(&ref_path).unwrap(),
            ref_before,
            "the branch ref is untouched by an unshallow"
        );

        let walks_unshallow = AtomicUsize::new(0);
        let error_unshallow = page_for_target(
            &target,
            Some(&cursor_deepened),
            1,
            &codec,
            &HeaderMap::new(),
            &walks_unshallow,
        )
        .await
        .expect_err("a cursor pinned before an unshallow must be refused");
        assert_eq!(error_unshallow.0, StatusCode::CONFLICT);
        assert_eq!(walks_unshallow.load(Ordering::Relaxed), 0);

        let (_, tag_frame_final, _) = frame_parts(&repo, &HeaderMap::new()).await;
        let (_, tag_page_final, body_final, _) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
        let page_final: Page = serde_json::from_slice(&body_final).unwrap();
        assert_ne!(
            page_deepened.generation, page_final.generation,
            "unshallowing moves the generation again"
        );
        assert_ne!(tag_frame_deepened, tag_frame_final);
        assert_ne!(tag_page_deepened, tag_page_final);
    }

    /// Malformed `.git/shallow` content fails the very first combined snapshot
    /// read `page_for_target` performs — before any cursor is even looked at —
    /// so it is the handler-level twin of
    /// `history::tests::malformed_shallow_metadata_is_snapshot_error`: an
    /// explicit read error, never a silent "unshallow".
    #[tokio::test]
    async fn malformed_shallow_metadata_is_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = deterministic_repo(dir.path(), "alpha", 2);
        std::fs::write(repo.join(".git").join("shallow"), "not-hex\n").unwrap();

        let codec = history_codec();
        let target = history_target(&repo, &codec);
        let walks = AtomicUsize::new(0);
        let error = page_for_target(
            &target,
            None,
            DEFAULT_PAGE_LIMIT,
            &codec,
            &HeaderMap::new(),
            &walks,
        )
        .await
        .expect_err("malformed shallow metadata must be an explicit error, not a silent unshallow");
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.1.contains("shallow"), "{}", error.1);
        assert_eq!(
            walks.load(Ordering::Relaxed),
            0,
            "the snapshot read fails before the walk counter ever moves"
        );
    }

    /// A traversal failure and a concurrent repository move can happen
    /// together; the combined re-read this triggers must report the move, not
    /// the walk's own error — a 409 always outranks a simultaneous read error.
    #[tokio::test]
    async fn walk_error_after_snapshot_move_returns_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, tip, root) = deep_linear_repo(dir.path(), 1_500);
        // The root is visited last under `DateOrder`, so the walk must process
        // nearly the entire history before failing — the racer's whole window.
        std::fs::remove_file(loose_object(&repo, &root)).expect("the root is a real loose object");

        let tree = out(&repo, &["rev-parse", &format!("{tip}^{{tree}}")]);
        let extra = out(&repo, &["commit-tree", &tree, "-p", &tip, "-m", "extra"]);
        let racer = race_ref_move(&repo, "refs/heads/main", &extra);

        let codec = history_codec();
        let target = history_target(&repo, &codec);
        let walks = AtomicUsize::new(0);
        let error = page_for_target(&target, None, 1_500, &codec, &HeaderMap::new(), &walks)
            .await
            .expect_err("a walk that fails while the repository has moved must report the move");
        racer.join().unwrap();

        assert_eq!(
            error.0,
            StatusCode::CONFLICT,
            "drift takes precedence over the walk's own error: {error:?}"
        );
        assert_eq!(error.1, "history moved");
        assert_eq!(
            walks.load(Ordering::Relaxed),
            1,
            "the walk ran once before failing"
        );
    }

    /// The same missing-object failure, but nothing else moves: the combined
    /// re-read finds the identical generation, so the explicit read error is
    /// surfaced rather than an invented conflict.
    #[tokio::test]
    async fn walk_error_with_stable_snapshot_returns_explicit_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, _tip, root) = deep_linear_repo(dir.path(), 30);
        std::fs::remove_file(loose_object(&repo, &root)).expect("the root is a real loose object");

        let codec = history_codec();
        let target = history_target(&repo, &codec);
        let walks = AtomicUsize::new(0);
        let error = page_for_target(
            &target,
            None,
            MAX_PAGE_LIMIT,
            &codec,
            &HeaderMap::new(),
            &walks,
        )
        .await
        .expect_err("a missing commit object must surface as an explicit read error");
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_ne!(
            error.0,
            StatusCode::CONFLICT,
            "nothing moved, so this must never be reported as drift"
        );
        assert_eq!(
            walks.load(Ordering::Relaxed),
            1,
            "the walk counted its one attempt before failing"
        );
    }

    // ---- shared router registration and the response budget (Step 9) --------

    /// Establish a session against `router` (whichever host it expects) and
    /// return just the `Cookie` header value. Duplicated from `main.rs`'s own
    /// test helper of the same shape (private to that module, unreachable from
    /// here) rather than exposed across the crate for one shared test helper.
    async fn bootstrap_cookie_for(router: Router, host: &str, token: &str) -> String {
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/session")
                    .header(header::HOST, host)
                    .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                    .header(header::CONTENT_TYPE, "application/json")
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [127, 0, 0, 1],
                        55000,
                    ))))
                    .body(axum::body::Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "bootstrap should succeed");
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        set_cookie.split(';').next().unwrap().to_string()
    }

    /// Both `/api/frame` and the paged `/api/commits` are registered on *both*
    /// listener profiles (loopback and LAN, ADR 0005), while a representative
    /// write route (`/api/commit`, POST) exists only on loopback — proving the
    /// two new reads were added to `api_router`'s always-registered section,
    /// not inside the `full_routes` write block. Follows the shape of
    /// `main::tests::the_lan_router_has_no_write_routes` /
    /// `..._loopback_router_still_has_write_routes_registered`, driven at the
    /// real route table (not `page_for_target` directly) because route
    /// *registration* is exactly what's under test here.
    #[tokio::test]
    async fn history_routes_exist_on_loopback_and_lan_read_profile() {
        for (via_lan, host, full_routes) in [
            (false, "localhost:8080", true),
            (true, "192.168.1.42:8080", false),
        ] {
            let sessions = std::sync::Arc::new(crate::session::SessionManager::new(None));
            let token = sessions.current_bootstrap();
            let session_state = crate::handlers::session::SessionState {
                manager: sessions,
                via_lan,
                rate_limiter: None,
            };
            let hosts = if via_lan {
                crate::security::HostPolicy::lan(
                    "192.168.1.42".parse().unwrap(),
                    crate::state::PORT,
                )
            } else {
                crate::security::HostPolicy::loopback(crate::state::PORT)
            };
            let router =
                crate::api_router(session_state, hosts, full_routes, Arc::new(history_codec()));
            let cookie = bootstrap_cookie_for(router.clone(), host, &token).await;

            for (method, uri) in [("GET", "/api/frame"), ("GET", "/api/commits")] {
                let resp = router
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .method(method)
                            .uri(uri)
                            .header(header::HOST, host)
                            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                            .header(header::COOKIE, cookie.clone())
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_ne!(
                    resp.status(),
                    StatusCode::NOT_FOUND,
                    "{method} {uri} must be a registered route on the {} profile (it may still \
                     fail for other reasons, e.g. no repository selected)",
                    if via_lan { "LAN" } else { "loopback" }
                );
            }

            // The representative write: registered POST-only on loopback,
            // never registered at all on LAN (ADR 0005). A GET reaches real
            // routing either way, so 404 (never built) is distinguishable
            // from 405 (built, wrong method).
            let resp = router
                .oneshot(
                    axum::http::Request::builder()
                        .method("GET")
                        .uri("/api/commit")
                        .header(header::HOST, host)
                        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                        .header(header::COOKIE, cookie)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if via_lan {
                assert_eq!(
                    resp.status(),
                    StatusCode::NOT_FOUND,
                    "the LAN profile must never register a write route"
                );
            } else {
                assert_eq!(
                    resp.status(),
                    StatusCode::METHOD_NOT_ALLOWED,
                    "the loopback profile keeps its write routes registered"
                );
            }
        }
    }

    /// A deliberately pathological, but not unrealistic, default-size Page: 250
    /// rows (the un-overridden `?limit=`), long real-world author/summary
    /// fields, a scatter of merges, several rows carrying refs, and a cascade
    /// of stubs. This is a **fixture budget, not a universal metadata ceiling**
    /// — a real repository with even longer commit messages or many more refs
    /// on one page could exceed 512 KiB; this only proves today's realistic
    /// worst case stays comfortably inside it.
    #[test]
    fn default_page_pathological_fixture_is_at_most_512_kib() {
        let long_author = "Alexandra Christodoulopoulou-Fitzgerald-Nakamura-Petrov \
             <alexandra.christodoulopoulou-fitzgerald-nakamura-petrov@\
             an-extremely-long-corporate-engineering-subdomain.example-enterprises.co.uk>";
        let long_summary = "refactor(auth,session): replace the legacy cookie-based session \
             token validation path with the new HMAC-SHA256-signed scheme, closing out the \
             follow-up work items from the January security review and satisfying checklist \
             item 7.3 (rotation, constant-time compare, and origin binding)";

        let hex = |n: u32| format!("{n:040x}");

        let mut rows = Vec::with_capacity(DEFAULT_PAGE_LIMIT);
        let mut edges = Vec::new();
        let mut stubs = Vec::new();
        let lanes = 6usize;

        for row in 0..DEFAULT_PAGE_LIMIT {
            let is_merge = row % 17 == 0 && row > 0;
            let lane = row % lanes;
            let mut parents = vec![Oid(hex(row as u32 + 1))];
            if is_merge {
                parents.push(Oid(hex(row as u32 + 1000)));
            }
            let mut refs = Vec::new();
            if row % 23 == 0 {
                refs.push(GitRef {
                    name: format!(
                        "feature/a-very-descriptive-long-lived-branch-name-for-team-{row}"
                    ),
                    kind: git_vista_core::model::RefKind::Branch,
                    target: Oid(hex(row as u32)),
                });
                refs.push(GitRef {
                    name: format!("origin/feature/a-very-descriptive-long-lived-branch-name-{row}"),
                    kind: git_vista_core::model::RefKind::RemoteBranch,
                    target: Oid(hex(row as u32)),
                });
            }
            rows.push(GraphRow {
                commit: CommitSummary {
                    id: Oid(hex(row as u32)),
                    parents,
                    summary: long_summary.to_string(),
                    author: long_author.to_string(),
                    time: 1_700_000_000 + row as i64,
                },
                row,
                lane,
                refs,
                color: row % 8,
                on_remote: row % 3 == 0,
            });
            if row > 0 {
                edges.push(Edge {
                    from_row: row - 1,
                    from_lane: (row - 1) % lanes,
                    to_row: row,
                    to_lane: lane,
                });
            }
            if is_merge {
                edges.push(Edge {
                    from_row: row - 1,
                    from_lane: lanes - 1,
                    to_row: row,
                    to_lane: lane,
                });
            }
            if row % 31 == 0 {
                for depth in 0..3 {
                    stubs.push(FrameStub {
                        name: format!(
                            "release/a-long-lived-release-branch-name-row-{row}-depth-{depth}"
                        ),
                        anchor_commit: Oid(hex(row as u32)),
                        lane_offset: lanes + depth,
                        color: (row + depth) % 8,
                        depth,
                    });
                }
            }
        }

        let page = Page {
            rows,
            edges,
            stubs,
            lane_count: lanes,
            cursor: Some(
                "A".repeat(64) + "." + &"b".repeat(96), // a plausible signed-cursor shape
            ),
            generation: git_vista_protocol::GenerationToken::new(format!(
                "history-v1:{}",
                "f".repeat(64)
            ))
            .unwrap(),
        };

        let body = serde_json::to_vec(&page).expect("Page always serializes");
        assert!(
            body.len() <= 512 * 1024,
            "fixture budget exceeded ({} bytes > 512 KiB) — this is a fixture budget for \
             today's pathological-but-realistic default page, not a universal metadata \
             ceiling: a real repository could still produce a larger page than this",
            body.len()
        );
    }

    // ---- GET /api/status/v2: the live handler seam (#68c) ---------------------

    /// The real handler, end to end: a dirty worktree (staged add, unstaged
    /// modify, untracked file) produces a `WorktreeStatus` whose `entries`
    /// actually reflect it, and whose `generation` is a real, non-empty
    /// `status-v1:`-namespaced token — not the DTO's shape alone (task 10's
    /// tests already pin that), but this file's own contribution: that the
    /// three existing pieces (DTO, parser, generation inputs) are actually
    /// wired together correctly.
    #[tokio::test]
    async fn worktree_status_v2_reflects_a_real_dirty_worktree() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        run(&repo, &["add", "new.txt"]);

        let status = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
            .await
            .expect("a real repository read must succeed");

        assert!(
            status.generation.as_str().starts_with("status-v1:"),
            "generation must carry the status-v1 namespace: {:?}",
            status.generation
        );
        assert_eq!(status.branch.as_deref(), Some("main"));

        let unstaged_a = status.entries.iter().any(|e| {
            matches!(
                e,
                StatusEntry::Changed { path, sides: ChangeSides::UnstagedOnly { .. }, .. }
                    if path == "a.txt"
            )
        });
        assert!(
            unstaged_a,
            "a.txt's unstaged edit must appear: {:?}",
            status.entries
        );

        let staged_new = status.entries.iter().any(|e| matches!(
            e,
            StatusEntry::Changed { path, sides: ChangeSides::StagedOnly { staged: ChangeKind::Added }, .. }
                if path == "new.txt"
        ));
        assert!(
            staged_new,
            "new.txt's staged add must appear: {:?}",
            status.entries
        );
    }

    /// The generation changes across a real edit, and is stable when nothing
    /// changed between two reads — the actual guarantee #68's "generation-
    /// tagged and detects external changes" criterion is about, proven
    /// against a real repository rather than assumed from the DTO's shape.
    #[tokio::test]
    async fn worktree_status_v2_generation_changes_with_the_worktree() {
        let (_dir, repo) = seeded_repo();

        let clean = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
            .await
            .unwrap();
        let clean_again = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
            .await
            .unwrap();
        assert_eq!(
            clean.generation, clean_again.generation,
            "two reads of an unchanged worktree must agree"
        );

        std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
        let dirty = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
            .await
            .unwrap();
        assert_ne!(
            clean.generation, dirty.generation,
            "an unstaged edit must change the generation"
        );
    }

    /// A porcelain-v2 stream past the cap is refused outright, not parsed
    /// into a `WorktreeStatus` missing (or mangling) its cut-off last entry —
    /// see `worktree_status_v2_for_repo`'s doc comment for why a status cap
    /// hit cannot be a success the way a file-read cap hit is. Uses a small
    /// injected cap (the same testability seam `commit_diff_for_repo`'s
    /// metadata-cap tests use) rather than constructing enough real
    /// porcelain output to exceed the production 8 MiB ceiling.
    #[tokio::test]
    async fn worktree_status_v2_refuses_rather_than_serving_a_truncated_parse() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();

        let err = worktree_status_v2_for_repo(&repo, 4)
            .await
            .expect_err("a cap hit must be refused, not parsed");
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The production cap is generous enough that an ordinary dirty worktree
    /// never trips it — the control for the test above, so a cap-hit failure
    /// there is known to come from the injected small cap, not from
    /// `STATUS_V2_STDOUT_CAP` itself being too tight for real use.
    #[tokio::test]
    async fn worktree_status_v2_production_cap_does_not_truncate_an_ordinary_worktree() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
            .await
            .expect("an ordinary dirty worktree must not hit the production cap");
    }

    // ---- Large-worktree responsiveness (#68e) ---------------------------------
    //
    // #68's own text — "large worktrees stay responsive" — is unfalsifiable as
    // written. This turns it into: a real measurement at several worktree
    // sizes, a stated cap-boundary file count, and a budget a future change
    // can actually fail against (`worktree_status_v2_budget_holds_at_1k_files`
    // below, which runs in every `cargo test`; the full multi-N ladder is
    // `#[ignore]`d — see that test's own doc comment for why).

    /// `n` freshly created, distinctly named untracked files under `repo` —
    /// the cheapest real worktree-size generator available: untracked
    /// entries are one porcelain-v2 `? <path>` record each (no hash/mode
    /// fields to compute), so file *creation* cost dominates over anything
    /// `git status` itself has to do, keeping the measurement honest about
    /// what it's actually timing.
    fn generate_untracked_files(repo: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(repo.join(format!("bench-{i:06}.txt")), "x\n").unwrap();
        }
    }

    /// One measurement: wall-clock time for the **real** `#68c` handler seam
    /// (git spawn, `-z` porcelain read, `parse_porcelain_v2_z`, and the full
    /// generation derivation — `read_generation_inputs`'s ref walk plus the
    /// sha256 digest) against a worktree with `n` untracked files.
    async fn time_status_v2(repo: &Path, n: usize) -> (std::time::Duration, bool) {
        generate_untracked_files(repo, n);
        let start = std::time::Instant::now();
        let result = worktree_status_v2_for_repo(repo, STATUS_V2_STDOUT_CAP).await;
        let elapsed = start.elapsed();
        (elapsed, result.is_ok())
    }

    /// The real measurement behind `docs/PERFORMANCE_BUDGETS.md`'s numbers —
    /// **not** part of the normal test run. `#[ignore]`d because generating
    /// up to 20,000 real files and shelling out to `git status` repeatedly
    /// costs real wall-clock seconds, which has no place in every `cargo
    /// test`/CI run; `worktree_status_v2_budget_holds_at_1k_files` below is
    /// the fast, always-on regression check derived from what this found.
    ///
    /// Run explicitly to reproduce or update the recorded numbers:
    /// `cargo test -p git-vista-server --bin git-vista-server -- --ignored \
    ///  --nocapture large_worktree_responsiveness_ladder`
    ///
    /// One host, one run each — not a statistically controlled benchmark
    /// suite. `docs/PERFORMANCE_BUDGETS.md` says so explicitly; treat the
    /// printed numbers as "real and reproducible," not "precise to the
    /// millisecond."
    #[tokio::test]
    #[ignore = "generates up to 20k real files and shells out to git repeatedly; run explicitly, see doc comment"]
    async fn large_worktree_responsiveness_ladder() {
        let (_dir, repo) = seeded_repo();
        println!("\n#68e large-worktree responsiveness ladder (one host, one run each):");
        println!("{:>8}  {:>12}  {:>8}", "n_files", "elapsed", "ok?");
        for n in [100usize, 1_000, 5_000, 20_000] {
            let (elapsed, ok) = time_status_v2(&repo, n).await;
            println!("{n:>8}  {elapsed:>12?}  {ok:>8}");
        }
    }

    /// Where the 8 MiB cap (`STATUS_V2_STDOUT_CAP`) actually bites, in file
    /// count — not asserted from arithmetic on an assumed per-record size,
    /// measured against a real, large, uniformly-named worktree (`? bench-
    /// NNNNNN.txt\0` is 20 bytes/record: 2-byte marker+space, 15-byte name,
    /// 1-byte NUL terminator; a real worktree's actual paths will differ, so
    /// this is a lower bound on the file count that trips the cap for
    /// *this* naming scheme, not a universal constant — `docs/
    /// PERFORMANCE_BUDGETS.md` states that caveat explicitly). `#[ignore]`d
    /// for the same reason as the ladder above: real cost, not a normal-run
    /// check.
    #[tokio::test]
    #[ignore = "generates ~450k real files; run explicitly, see doc comment"]
    async fn large_worktree_cap_boundary_in_file_count() {
        let (_dir, repo) = seeded_repo();
        // 20 bytes/record * ~450_000 ~= 8.6 MiB, comfortably past the 8 MiB
        // cap for this naming scheme.
        let (_elapsed, ok) = time_status_v2(&repo, 450_000).await;
        assert!(
            !ok,
            "450,000 uniformly-named untracked files should exceed \
             STATUS_V2_STDOUT_CAP for this record size — if this now \
             succeeds, the cap boundary moved and docs/PERFORMANCE_BUDGETS.md \
             needs its file-count figure re-measured"
        );
    }

    /// The always-on regression check: 1,000 changed files (cheap enough for
    /// every `cargo test`/CI run) must complete well inside a generous
    /// multiple of the budget `docs/PERFORMANCE_BUDGETS.md` states — loose
    /// enough not to flake on a loaded CI runner, tight enough that a real
    /// regression (e.g. the generation derivation's ref walk becoming
    /// accidentally quadratic) would still fail it.
    #[tokio::test]
    async fn worktree_status_v2_budget_holds_at_1k_files() {
        let (_dir, repo) = seeded_repo();
        let (elapsed, ok) = time_status_v2(&repo, 1_000).await;
        assert!(ok, "1,000 untracked files must not hit the read cap");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "1,000-file worktree status took {elapsed:?}, budget is 2s \
             (see docs/PERFORMANCE_BUDGETS.md) — this is a real regression, \
             not flakiness, unless the CI runner is unusually loaded"
        );
    }
}
