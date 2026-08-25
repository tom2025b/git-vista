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
        head_state: snapshot.head_state,
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
///
/// `pub(crate)`: `handlers::conflicts` reuses this for `/api/blob/{oid}` and
/// the result-pane worktree read (#428) — same tidy-already-bounded-bytes
/// contract, so a truncated blob or worktree file never hands the viewer half
/// a line either.
pub(crate) fn truncate_at_line(text: &mut String, cap: usize) {
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
///
/// `pub(crate)`: `handlers::conflicts` reuses this exact cap for
/// `/api/blob/{oid}` and the result-pane worktree read (#428) — one number
/// governing every bounded text read in the app, not a second cap that could
/// silently drift from this one.
pub(crate) const FILE_CONTENT_CAP: usize = 2_000_000;

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

// ---------------------------------------------------------------------------
// Tests — extracted into handlers/read/<topic>_suite.rs (mechanical test
// extraction, keeping this file to its production code). Each suite is a
// child module of `read`, so `use super::*` reaches this file's private
// items exactly as the inline `mod tests` block used to.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod content_suite;

#[cfg(test)]
mod graph_suite;

#[cfg(test)]
mod routing_suite;

#[cfg(test)]
mod status_suite;
