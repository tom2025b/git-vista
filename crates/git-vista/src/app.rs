//! Leptos components for git-vista.
//!
//! Phase 4: on startup the frontend `fetch`es the real, laid-out commit
//! [`Graph`] from the backend's `/api/commits` endpoint (same origin) and renders
//! it as inline SVG — one circle per commit, one curved path per commit->parent
//! link, lanes laid out left to right. The whole graph lives inside a single
//! `<g transform>` driven by a [`Camera`]: pointer drags pan, the wheel zooms
//! toward the cursor (Phase 2). Earlier phases used hardcoded demo data; that's
//! gone from the render path now.
//!
//! This module is deliberately just *view assembly* + the data fetch: spatial
//! math lives in [`crate::geometry`], the lane palette in [`crate::color`], the
//! pan/zoom arithmetic in [`crate::camera`], and the lane layout is computed on
//! the backend by `git-vista-core`.

use leptos::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use gloo_net::http::Request;

use git_vista_core::model::{
    BranchRequest, CreateBranchRequest, CreateCommitRequest, Graph, RefKind,
};

use crate::camera::{Camera, ZOOM_STEP};
use crate::color::{
    branch_color, branch_color_distinct_from, BADGE_DARK, HEAD_BADGE, MERGE_FILL, TAG_BADGE,
};
use crate::datetime::local_timestamp;
use crate::geometry::{
    badge_text_dx, badge_text_y, badge_top_y, badge_width, edge_path, label_bottom_y, label_top_y,
    label_x, node_cx, node_cy, stub_node_cy, stub_path, BADGE_GAP, BADGE_HEIGHT, BADGE_RADIUS,
    NODE_RADIUS,
};
use crate::lod::detail_for;
use crate::text::truncate;
use crate::viewport::visible_row_range;

/// Commit messages longer than this are truncated with an ellipsis in the label
/// (the full text stays available via the node/label hover tooltip).
const MAX_SUMMARY_CHARS: usize = 60;

/// State for the per-commit context menu (Issue #18): which commit was tapped,
/// where to draw the menu (client/viewport px, since it's an HTML overlay, not
/// part of the pan/zoomed SVG), and the commit's GitHub URL when it has one.
#[derive(Clone)]
struct MenuData {
    /// Full commit hash — what "Create branch" targets. For a branch stub this is
    /// its tip's commit (the branch owns no commit of its own), so branching from
    /// the stub forks off that commit.
    commit: String,
    /// The menu's header: a commit's short hash, or a stub's branch name — so a
    /// stub reads as the branch it is, not the commit it happens to sit on
    /// (Issue #30).
    header: String,
    /// Viewport x/y of the click, used to position the overlay.
    x: f64,
    y: f64,
    /// GitHub URL for the "Open on GitHub" item — a commit page for a commit dot,
    /// or the branch's tree page for a stub. `Some` only when this repo has a
    /// github.com origin *and* the target is pushed (otherwise it would 404);
    /// `None` renders the item disabled.
    github_url: Option<String>,
    /// Label for the "Open on GitHub" item, so a stub says "branch" and a commit
    /// says "commit".
    github_label: &'static str,
    /// Label for the "Create branch…" item, so a stub (which represents a branch)
    /// reads "from this branch" while a commit dot reads "from this commit".
    create_label: &'static str,
    /// True when this target is the current HEAD tip — the only place a new commit
    /// can land without moving HEAD, so the "Commit …" items are enabled only here
    /// (Issue #33). A branch stub is never the HEAD tip, so it's always false.
    is_head: bool,
    /// Local branch names living at this target: a stub's own name, or every local
    /// branch badge on a commit dot. Each yields a set of merge/push/delete items
    /// (Issue #33 follow-up). Empty => the target carries no branch, so no branch
    /// operations are shown.
    branches: Vec<String>,
}

/// A branch operation awaiting confirmation in the modal (Issue #33 follow-up).
/// Merge and delete change history/refs and push reaches the network, so each is
/// confirmed before it runs — reusing the same in-app modal the commit dialog uses
/// (a native `confirm()` gets blocked/flashed by the webview, same as `prompt()`).
#[derive(Clone)]
enum PendingOp {
    /// Merge `branch` into the checked-out branch (`git merge <branch>`). `into` is
    /// the live HEAD branch, fetched when the item is clicked, so the confirmation
    /// names the true target; `None` => detached HEAD (the confirm button is disabled).
    Merge { branch: String, into: Option<String> },
    /// Push `branch` to origin (`git push origin <branch>`).
    Push { branch: String },
    /// Delete `branch` (`git branch -d <branch>`). `current` is the live HEAD branch,
    /// fetched on click; when it equals `branch` the confirm button is disabled (git
    /// refuses to delete the checked-out branch). `None` => detached HEAD (deletable).
    Delete { branch: String, current: Option<String> },
}

/// Pointer travel (CSS px) past which a press becomes a pan/drag rather than a
/// tap. Keeps a tap-to-open-link from being eaten by the pan handler.
const DRAG_THRESHOLD: f64 = 4.0;

/// How long (ms) after the commit modal opens to ignore a backdrop dismiss, so
/// iOS's synthesized post-tap "ghost click" can't close the modal it just opened.
const DIALOG_GUARD_MS: f64 = 400.0;

/// Everything the per-row / per-edge view builders need, bundled behind a
/// `StoredValue` so the reactive `<For>` closures (Phase 8 viewport
/// virtualization) can reach the graph and its derived lookups cheaply — without
/// cloning the graph into each closure or rebuilding these tables per row.
struct RenderCtx {
    graph: Graph,
    /// Per-row branch-colour slot (row index → palette slot), so an edge can pick
    /// up the coloured line of the row it belongs to.
    row_color: Vec<usize>,
    /// Commit ids present on the remote, for the "is this pushed?" link gating.
    remote_set: std::collections::HashSet<String>,
    /// Remote branch short-names, for gating local-branch links.
    remote_branches: std::collections::HashSet<String>,
    /// GitHub web base (e.g. "https://github.com/owner/repo"), when this repo has
    /// a github.com origin; `None` => labels stay plain text.
    repo_url: Option<String>,
    /// Left edge (x) of the aligned label column.
    text_x: i32,
}

/// Extra rows rendered above and below the visible window so a fast pan doesn't
/// flash a blank strip before the row `Memo` catches up (Phase 8).
const OVERSCAN_ROWS: usize = 6;

/// Current browser window inner height in CSS px, or a sane default when it can't
/// be read. The window is always at least as tall as the SVG (the topbar sits
/// above it), so this is a safe *upper* bound on the viewport height — the
/// virtualizer may draw a few extra rows just past the bottom, never too few.
fn window_inner_height() -> f64 {
    web_sys::window()
        .and_then(|w| w.inner_height().ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(800.0)
}

/// Indices of edges whose row span intersects the visible row window `[start,
/// end)`. An edge is kept whenever any part of it could cross the viewport — even
/// when both endpoints are off-screen (a long merge line passing through) — so an
/// edge never visibly disappears at the window's edge. Edges always run downward
/// (`from_row` < `to_row`), so the span is `[from_row, to_row]`.
fn visible_edges(ctx: StoredValue<RenderCtx>, range: (usize, usize)) -> Vec<usize> {
    let (start, end) = range;
    ctx.with_value(|c| {
        c.graph
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.from_row < end && e.to_row >= start)
            .map(|(i, _)| i)
            .collect()
    })
}

/// Fetch the laid-out graph from the backend. Relative URL → same origin as the
/// served SPA, so no CORS and no hardcoded host.
///
/// The URL carries a per-load cache-busting `t=<ms>` param: the backend already
/// sends `Cache-Control: no-store`, but a unique URL each launch is belt-and-
/// braces against iOS Safari's persistent cache serving a stale graph (so a branch
/// created since the last launch never shows). The backend ignores the param.
async fn fetch_graph() -> Result<Graph, String> {
    let url = format!("/api/commits?t={}", js_sys::Date::now());
    Request::get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Graph>()
        .await
        .map_err(|e| e.to_string())
}

/// Ask the backend to create `name` at `commit` (Issue #18, `POST /api/branch`).
/// On a non-2xx response the body is git's own error text, returned as `Err` so
/// the caller can show the real reason (branch exists, bad name, …).
async fn create_branch_request(name: &str, commit: &str) -> Result<(), String> {
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
async fn create_commit_request(message: &str, allow_empty: bool) -> Result<(), String> {
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
async fn fetch_head_branch() -> Result<Option<String>, String> {
    let url = format!("/api/head-branch?t={}", js_sys::Date::now());
    Request::get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Option<String>>()
        .await
        .map_err(|e| e.to_string())
}

/// Ask the backend to run a branch operation on `branch` (Issue #33 follow-up).
/// `path` is the endpoint — `/api/merge`, `/api/push`, or `/api/delete-branch` —
/// all of which take the same `{ branch }` body. As with the other requests, a
/// non-2xx body is git's own error text, returned as `Err` for the caller to show.
async fn branch_op_request(path: &str, branch: &str) -> Result<(), String> {
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

#[component]
pub fn App() -> impl IntoView {
    // A bump counter is the resource's reactive source, so changing it re-runs the
    // fetch. The frontend used to fetch exactly once on load (`|| ()`, a constant
    // source that never re-fires), so a branch or commit created *after* the page
    // loaded never appeared until a full reload — the heart of issue #16. The
    // Refresh button below bumps this to re-read the repo on demand. (Each fetch
    // also cache-busts its URL, so the re-read reaches the server, not the cache.)
    // `create_local_resource` because the fetch future isn't `Send` (wasm).
    let reload = create_rw_signal(0u32);
    let graph = create_local_resource(move || reload.get(), |_| fetch_graph());
    let refresh = move |_| reload.update(|n| *n = n.wrapping_add(1));

    view! {
        <main class="app">
            <header class="topbar">
                <h1>"git-vista"</h1>
                <span class="subtitle">"vertical git history — drag to pan, pinch or scroll to zoom"</span>
                <button
                    class="refresh"
                    on:click=refresh
                    title="Re-read the repository — shows branches and commits created since the page loaded"
                >
                    "Refresh"
                </button>
            </header>
            <section class="graph">
                {move || match graph.get() {
                    None => view! { <p class="status">"Loading history…"</p> }.into_view(),
                    Some(Err(e)) => view! {
                        <p class="status error">{format!("Failed to load history: {e}")}</p>
                    }
                    .into_view(),
                    Some(Ok(g)) => {
                        // Show which repo this page is actually displaying, straight
                        // from the API response. If it disagrees with the terminal,
                        // the browser is pointed at a stale server/tab — now visible.
                        let repo = g.repo_label.clone();
                        view! {
                            {repo.map(|r| view! {
                                <p class="status repo">{format!("repository: {r}")}</p>
                            })}
                            {graph_canvas(g, reload)}
                        }
                        .into_view()
                    }
                }}
            </section>
        </main>
    }
}

/// Render a loaded [`Graph`] as a pan/zoomable SVG canvas. `reload` is the App's
/// fetch counter, bumped after a successful branch creation so the new branch
/// shows without a full reload (Issue #18, reusing the Issue #16 refresh path).
fn graph_canvas(graph: Graph, reload: RwSignal<u32>) -> impl IntoView {
    // Per-branch colour slot for each row, indexed by row number (rows are stored
    // in row order), so an edge can pick up its parent's branch colour.
    let row_color: Vec<usize> = graph.rows.iter().map(|gr| gr.color).collect();

    // GitHub web base (e.g. "https://github.com/owner/repo"), if this repo has a
    // github.com origin. `Some` => commit messages and ref badges become links;
    // `None` => they stay plain text. (Issue #12.)
    let repo_url = graph.repo_url.clone();
    // Which objects are actually on the remote, so we only link pushed ones (an
    // unpushed commit/ref 404s on GitHub). `remote_set` = commit ids on the
    // remote; `remote_branches` = remote branch names (the part after the
    // "<remote>/" prefix), derived from the RemoteBranch badges the graph already
    // carries — a local branch is linkable only when a remote branch shares its
    // name. Everything not linkable is shown dimmed (see the `.unpushed` style).
    let remote_set: std::collections::HashSet<String> =
        graph.remote_commits.iter().cloned().collect();
    let remote_branches: std::collections::HashSet<String> = graph
        .rows
        .iter()
        .flat_map(|r| &r.refs)
        .filter(|rf| rf.kind == RefKind::RemoteBranch)
        .filter_map(|rf| rf.name.split_once('/').map(|(_, b)| b.to_string()))
        .collect();
    // Whether the current gesture has become a drag (set in pointermove). Defined
    // here so the link click handlers below can ignore the click that ends a drag.
    let moved = store_value(false);
    // The open context menu, if any (Issue #18). `None` => no menu. Set when a dot
    // is tapped (below), cleared on a pan/tap-elsewhere (the SVG pointerdown).
    let menu = create_rw_signal(None::<MenuData>);
    // The open commit-message dialog, if any (Issue #33). `Some(allow_empty)` =>
    // the modal is showing; the bool picks `--allow-empty` vs a staged commit.
    // A real in-app modal, not `window.prompt()`, which webviews block/flash.
    let commit_dialog = create_rw_signal(None::<bool>);
    // The text currently typed into that dialog's message box.
    let commit_msg = create_rw_signal(String::new());
    // The branch operation awaiting confirmation, if any (Issue #33 follow-up).
    // `Some` => the confirm modal is showing; confirming runs the op then refreshes.
    // Mutually exclusive with the commit dialog (only one overlay is ever open).
    let confirm_op = create_rw_signal(None::<PendingOp>);
    // When the commit modal was opened (ms). iOS synthesizes a `click` a few ms
    // after a tap; opening the modal puts its full-screen backdrop under that tap
    // point, so the ghost click hits the backdrop and closes the modal instantly.
    // The backdrop ignores a dismiss that lands within `DIALOG_GUARD_MS` of opening.
    let dialog_opened_at = store_value(0.0_f64);
    // Links are real SVG `<a target="_blank">` anchors (built below), so a tap is
    // native link navigation. That works on iOS WebKit — which every iPad browser
    // (Safari, Chrome, DuckDuckGo) runs on, and which silently blocks the scripted
    // `window.open` pop-ups we used before. This handler only cancels the
    // navigation when the "click" is actually the tail of a drag/pan (desktop).
    let suppress = move |ev: web_sys::MouseEvent| {
        if moved.get_value() {
            ev.prevent_default();
        }
    };

    // Phase 8 (viewport virtualization): bundle the graph and its derived lookups
    // behind a `StoredValue` so the reactive per-row `<For>` closures below can
    // reach them cheaply — without cloning the graph into each closure or
    // rebuilding these tables per row. The graph moves in here; everything
    // downstream reads it back out of `ctx`.
    let text_x = label_x(graph.lane_count);
    let ctx = store_value(RenderCtx {
        graph,
        row_color,
        remote_set,
        remote_branches,
        repo_url,
        text_x,
    });

    // Per-edge view builder — invoked by a `<For>` only for edges whose row span
    // intersects the viewport. Colour a link by the branch *line* it belongs to,
    // so it matches the dots it connects:
    //  * a first-parent link is part of the child's own branch — a side branch
    //    forking off main is drawn in the side branch's colour all the way down to
    //    its fork point, not main's blue;
    //  * a merge link (any non-first parent) is part of the merged-in branch, so
    //    it takes that parent's colour as it curves in.
    // Only main (colour slot 0) ever stays blue this way.
    let build_edge = move |ei: usize| -> View {
        ctx.with_value(|c| {
            let e = &c.graph.edges[ei];
            let d = edge_path(e);
            let child = &c.graph.rows[e.from_row].commit;
            let parent_oid = &c.graph.rows[e.to_row].commit.id;
            let is_first_parent = child.parents.first() == Some(parent_oid);
            let color_row = if is_first_parent { e.from_row } else { e.to_row };
            let color = branch_color(c.row_color[color_row]);
            view! {
                <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
            }
            .into_view()
        })
    };

    // Per-commit node builder — a filled dot in the branch colour plus a larger
    // invisible hit target, built by a `<For>` only for rows in the viewport.
    // Every real commit (merges included) is a filled dot; a hollow ring is
    // reserved for branch stubs (a new branch with no commits of its own), so a
    // merge, which has real content, never reads as empty (Issue #30).
    let build_node = move |i: usize| -> View {
        ctx.with_value(|c| {
            let gr = &c.graph.rows[i];
            let cx = node_cx(gr.lane);
            let cy = node_cy(gr.row);
            let color = branch_color(gr.color);
            let fill = color;
            let stroke_width = "2";

            // Issue #18: tapping a dot opens a context menu. Gather this commit's
            // menu data now; the click handler clones it in (it may fire repeatedly).
            let commit_id = gr.commit.id.0.clone();
            let short = gr.commit.id.short().to_string();
            let title = format!("{} — {}", gr.commit.id.short(), gr.commit.summary);
            // Only the commit HEAD points at can take a new commit without moving
            // HEAD, so the "Commit …" items are enabled only here (Issue #33).
            let is_head = gr.refs.iter().any(|r| r.kind == RefKind::Head);
            // Local branch badges on this commit — each offers merge/push/delete
            // (Issue #33 follow-up). A commit can carry several; a bare commit none.
            let branches: Vec<String> = gr
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::Branch)
                .map(|r| r.name.clone())
                .collect();
            // Link target only when the repo is on GitHub *and* this commit is
            // pushed — same rule the labels use, so the menu never offers a 404.
            let github_url = c.repo_url.as_ref().and_then(|base| {
                c.remote_set.contains(&commit_id).then(|| format!("{base}/commit/{commit_id}"))
            });
            let open_menu = move |ev: web_sys::MouseEvent| {
                // Ignore the click that ends a pan; a real tap opens the menu where
                // the pointer is (viewport coords for the fixed-position overlay).
                if moved.get_value() {
                    return;
                }
                ev.stop_propagation();
                menu.set(Some(MenuData {
                    commit: commit_id.clone(),
                    header: short.clone(),
                    x: ev.client_x() as f64,
                    y: ev.client_y() as f64,
                    github_url: github_url.clone(),
                    github_label: "Open commit on GitHub",
                    create_label: "Create branch from this commit",
                    is_head,
                    branches: branches.clone(),
                }));
            };

            view! {
                <g>
                    <circle
                        cx=cx
                        cy=cy
                        r=NODE_RADIUS
                        fill=fill
                        stroke=color
                        stroke-width=stroke_width
                    >
                        <title>{title}</title>
                    </circle>
                    // A larger, invisible hit target on top so the small dot is easy
                    // to tap (especially on the iPad). `transparent` (not `none`) so
                    // it still receives the click.
                    <circle
                        cx=cx
                        cy=cy
                        r=NODE_RADIUS + 8
                        fill="transparent"
                        class="node-hit"
                        on:click=open_menu
                    />
                </g>
            }
            .into_view()
        })
    };

    // Commit labels: a fixed text column to the right of the lanes, two lines per
    // row — any ref badges then the (truncated) message on top, the short hash and
    // author dimmed below. The full message stays available on hover.
    //
    // Phase 9 (level of detail) + Phase 8 (virtualization): the two lines are two
    // independent builders — the message tier (badges + message) and the dimmed
    // meta tier (hash · author · date) — so the view can hide each at a different
    // zoom (LOD) and a `<For>` can build each only for the rows on screen. They're
    // independent because the meta line doesn't depend on the badge layout.
    //
    // Message tier: any ref badges laid out left-to-right from the label column,
    // then the (truncated, linkable) commit message just past them.
    let build_msg = move |i: usize| -> View {
        ctx.with_value(|c| {
            let gr = &c.graph.rows[i];
            let mut bx = c.text_x;
            // Is this row's commit on the remote? Drives whether its message, HEAD
            // badge and tag badges link out (an unpushed commit would 404).
            let commit_on_remote = c.remote_set.contains(&gr.commit.id.0);
            let badges = gr
                .refs
                .iter()
                .map(|r| {
                    let w = badge_width(&r.name);
                    let x = bx;
                    bx += w + BADGE_GAP;
                    // Branch badges take their branch's colour (filled for local,
                    // outlined for remote); HEAD and tags get fixed colours.
                    let branch = branch_color(gr.color);
                    let (fill, stroke, text_fill) = match r.kind {
                        RefKind::Head => (HEAD_BADGE, HEAD_BADGE, BADGE_DARK),
                        RefKind::Tag => (TAG_BADGE, TAG_BADGE, BADGE_DARK),
                        RefKind::Branch => (branch, branch, BADGE_DARK),
                        RefKind::RemoteBranch => ("none", branch, branch),
                    };
                    let name = r.name.clone();
                    // Where this badge links on GitHub (Issue #12) — but only when
                    // the target is actually on the remote, so a tap never 404s:
                    //  * HEAD / tag -> the commit they sit on, when it's pushed. (A
                    //    tag's own page can't be verified offline, so we link the
                    //    commit it points at, which resolves whenever it's pushed.)
                    //  * local branch -> its tree page, only if a remote branch of
                    //    the same name exists.
                    //  * remote branch -> its tree page (it's on the remote by
                    //    definition); its leading "<remote>/" is stripped.
                    let badge_url = c.repo_url.as_ref().and_then(|base| match r.kind {
                        RefKind::Head | RefKind::Tag => {
                            commit_on_remote.then(|| format!("{base}/commit/{}", gr.commit.id.0))
                        }
                        RefKind::Branch => c
                            .remote_branches
                            .contains(&r.name)
                            .then(|| format!("{base}/tree/{}", r.name)),
                        RefKind::RemoteBranch => {
                            let branch = r.name.split_once('/').map_or(r.name.as_str(), |(_, b)| b);
                            Some(format!("{base}/tree/{branch}"))
                        }
                    });
                    let clickable = badge_url.is_some();
                    // A GitHub repo where this ref simply isn't pushed: show it, but
                    // dimmed and unlinked, so it's clear it has no GitHub page yet.
                    let unpushed = c.repo_url.is_some() && badge_url.is_none();
                    let pill = view! {
                        <rect
                            x=x
                            y=badge_top_y(gr.row)
                            width=w
                            height=BADGE_HEIGHT
                            rx=BADGE_RADIUS
                            ry=BADGE_RADIUS
                            fill=fill
                            stroke=stroke
                            stroke-width="1"
                            class:clickable=clickable
                            class:unpushed=unpushed
                        />
                        <text
                            x=x + badge_text_dx()
                            y=badge_text_y(gr.row)
                            class="badge-text"
                            class:clickable=clickable
                            class:unpushed=unpushed
                            fill=text_fill
                        >
                            {name}
                        </text>
                    };
                    // Wrap in a real SVG anchor when this repo has a GitHub base.
                    // The `<g>` puts the ambiguous `<a>` in an SVG-parent context so
                    // Leptos resolves it to the SVG-namespaced anchor (an HTML `<a>`
                    // wouldn't navigate inside the SVG tree).
                    match badge_url {
                        Some(url) => view! {
                            <g>
                                <a href=url target="_blank" rel="noopener" on:click=suppress>
                                    {pill}
                                </a>
                            </g>
                        }
                        .into_view(),
                        None => pill.into_view(),
                    }
                })
                .collect_view();
            let msg_x = bx; // past the last badge, or text_x when there were none

            let msg = truncate(&gr.commit.summary, MAX_SUMMARY_CHARS);
            // The message links to the commit page on GitHub (Issue #12), but only
            // when the commit is on the remote — otherwise it's dimmed and the
            // tooltip says why, rather than linking to a page that would 404.
            let msg_url = c.repo_url.as_ref().and_then(|base| {
                commit_on_remote.then(|| format!("{base}/commit/{}", gr.commit.id.0))
            });
            let msg_clickable = msg_url.is_some();
            let msg_unpushed = c.repo_url.is_some() && msg_url.is_none();
            let title = if msg_unpushed {
                format!("{} — not pushed to GitHub", gr.commit.summary)
            } else {
                gr.commit.summary.clone()
            };
            let msg_text = view! {
                <text
                    x=msg_x
                    y=label_top_y(gr.row)
                    class="label-msg"
                    class:clickable=msg_clickable
                    class:unpushed=msg_unpushed
                >
                    {msg}
                    <title>{title}</title>
                </text>
            };
            // Same SVG-anchor wrapping as the badges, so the commit link is a real
            // tap-navigable link rather than a pop-up.
            let msg_view = match msg_url {
                Some(url) => view! {
                    <g>
                        <a href=url target="_blank" rel="noopener" on:click=suppress>
                            {msg_text}
                        </a>
                    </g>
                }
                .into_view(),
                None => msg_text.into_view(),
            };
            view! {
                {badges}
                {msg_view}
            }
            .into_view()
        })
    };

    // Meta tier: the dimmed `hash · author · local date+time` line, so the
    // timeline is visible per row. Independent of the badge layout, so it doesn't
    // recompute the badges.
    let build_meta = move |i: usize| -> View {
        ctx.with_value(|c| {
            let gr = &c.graph.rows[i];
            let meta = format!(
                "{} · {} · {}",
                gr.commit.id.short(),
                gr.commit.author,
                local_timestamp(gr.commit.time),
            );
            view! {
                <text x=c.text_x y=label_bottom_y(gr.row) class="label-meta">
                    {meta}
                </text>
            }
            .into_view()
        })
    };

    // Branch stubs: a local branch with no commits of its own (e.g. one just
    // created from an existing commit) is drawn GitHub-network-graph style — a
    // short, uniquely-coloured line forking off its commit into its own lane,
    // with the branch badge on the fork tip, instead of a second badge crowding
    // the shared commit.
    // Branch stubs (Phase 8: kept eager and always rendered). There are only a
    // handful — one per commit-less new branch — and their cascade fans *upward*
    // off the anchor commit, so they don't map onto the row window as cleanly as
    // nodes/edges/labels; rendering them all is cheap and avoids that edge case.
    let stubs = ctx.with_value(|c| c.graph
        .stubs
        .iter()
        .map(|s| {
            // A new branch must never share the colour of the branch it forked off
            // (Issue #30): the palette collapses many slots onto few colours, so a
            // stub's raw slot can collide with its anchor branch's colour. Bump it
            // until it differs from the anchor commit's colour.
            let anchor_slot = c.graph.rows[s.anchor_row].color;
            let color = branch_color_distinct_from(s.color, anchor_slot);
            let d = stub_path(s.anchor_lane, s.anchor_row, s.lane, s.depth);
            let sx = node_cx(s.lane);
            let sy = stub_node_cy(s.anchor_row, s.depth);
            let name = s.name.clone();

            // The stub is a *branch*, not the commit it happens to sit on, so its
            // menu takes the branch's identity (Issue #30): the header is the
            // branch name and "Open on GitHub" goes to the branch's tree page (only
            // when a remote branch of the same name exists, so it never 404s) — the
            // same rule the branch badges use. "Create branch" still targets the
            // stub's tip commit, so forking from the stub forks off that commit
            // (Issue #24).
            let anchor = &c.graph.rows[s.anchor_row].commit;
            let commit_id = anchor.id.0.clone();
            let header = s.name.clone();
            let branch_name = s.name.clone();
            let github_url = c.repo_url.as_ref().and_then(|base| {
                c.remote_branches
                    .contains(&s.name)
                    .then(|| format!("{base}/tree/{}", s.name))
            });
            let open_menu = move |ev: web_sys::MouseEvent| {
                // Ignore the click that ends a pan; a real tap opens the menu.
                if moved.get_value() {
                    return;
                }
                ev.stop_propagation();
                menu.set(Some(MenuData {
                    commit: commit_id.clone(),
                    header: header.clone(),
                    x: ev.client_x() as f64,
                    y: ev.client_y() as f64,
                    github_url: github_url.clone(),
                    github_label: "Open branch on GitHub",
                    create_label: "Create branch from this branch",
                    // A stub is a new empty branch, never the HEAD tip.
                    is_head: false,
                    // The stub *is* one branch, so its ops act on that single name.
                    branches: vec![branch_name.clone()],
                }));
            };
            view! {
                <path
                    d=d
                    fill="none"
                    stroke=color
                    stroke-width="2"
                    stroke-linecap="round"
                />
                // Hollow, clickable ring (Issue #28) — a stub branch owns no
                // commits of its own yet, so it reads as an empty ring in the
                // branch's colour rather than a filled dot, signalling "nothing
                // committed here yet" at a glance. Still tappable to branch from.
                <circle cx=sx cy=sy r=NODE_RADIUS fill=MERGE_FILL stroke=color stroke-width="2">
                    <title>{format!("{name} — new branch (no commits yet); tap to branch from here")}</title>
                </circle>
                // A larger, invisible hit target on top so the tip is easy to tap,
                // exactly like the commit dots.
                <circle
                    cx=sx
                    cy=sy
                    r=NODE_RADIUS + 8
                    fill="transparent"
                    class="node-hit"
                    on:click=open_menu
                />
            }
        })
        .collect_view());

    // Camera (pan/zoom) state.
    let camera = create_rw_signal(Camera::default());
    // Whether any pointer is currently pressed (drives the grab/grabbing cursor).
    let dragging = create_rw_signal(false);

    // Phase 8 — viewport virtualization. Track the viewport height and derive the
    // window of rows currently on screen; the `<For>`s in the view render only
    // those (plus a small overscan margin). Using a `Memo` means a sub-row pan
    // doesn't rebuild anything — the row set changes only when a row actually
    // enters or leaves the viewport, and the keyed `<For>` then adds/removes just
    // that row's DOM rather than re-rendering the screenful.
    let row_count = ctx.with_value(|c| c.graph.rows.len());
    let vp_h = create_rw_signal(window_inner_height());
    // Refresh the viewport height on window resize (rotate the iPad, resize the
    // desktop window). `forget()` keeps the callback alive for the app's lifetime.
    if let Some(win) = web_sys::window() {
        let cb = Closure::<dyn FnMut()>::new(move || vp_h.set(window_inner_height()));
        let _ = win.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    let visible =
        create_memo(move |_| visible_row_range(camera.get(), vp_h.get(), row_count, OVERSCAN_ROWS));

    // --- Gesture tracking on Pointer Events ---------------------------------
    // Pointer Events unify mouse, pen and touch — crucially they fire for touch
    // on iOS Safari, where the old `movementX/Y`-on-mousemove + `wheel` approach
    // was dead (Safari reports `movementX/Y` as 0 for touch, and pinch never
    // raises a wheel event). We track every pressed pointer's position ourselves
    // and derive the gesture from how many are down:
    //   * one pointer  -> pan by the change in its position (finger or mouse);
    //   * two pointers -> pinch-zoom by the change in the distance between them,
    //                     anchored at their midpoint.
    // `touch-action: none` (styles.css) hands the browser's default touch
    // gestures to us so these fire cleanly.
    //
    // State is held in `store_value` (plain mutable cells, no reactivity needed):
    // the live list of `(pointer_id, x, y)` in client coords, the previous finger
    // distance during a pinch, where the gesture started, and whether it has yet
    // moved far enough to count as a drag (vs a tap).
    let pointers = store_value(Vec::<(i32, f64, f64)>::new());
    let pinch_dist = store_value(Option::<f64>::None);
    let down_xy = store_value(Option::<(f64, f64)>::None);
    // `moved` (declared at the top of this fn) distinguishes a drag/pinch from a
    // tap: until the pointer travels past DRAG_THRESHOLD we neither pan nor
    // capture, so a tap reaches the child element's link click handler.

    // The SVG's top-left in client coords, so a client position can be made
    // SVG-local for zoom anchoring (1 unit = 1px, no viewBox, so it's a shift).
    let svg_origin = |ev: &web_sys::PointerEvent| -> (f64, f64) {
        ev.current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .map(|el| {
                let r = el.get_bounding_client_rect();
                (r.left(), r.top())
            })
            .unwrap_or((0.0, 0.0))
    };

    // Press: just record the pointer. We deliberately do NOT capture it or start a
    // drag yet — that waits until the pointer actually moves (see pointermove), so
    // a plain tap stays a tap and its click reaches the link underneath.
    let on_pointerdown = move |ev: web_sys::PointerEvent| {
        // Any press on the canvas dismisses an open menu. A tap on a dot reopens it
        // on the click that follows (pointerdown fires before click), so this just
        // handles "tap empty space / start panning to close".
        menu.set(None);
        let id = ev.pointer_id();
        let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
        let first = pointers.with_value(|ps| ps.is_empty());
        pointers.update_value(|ps| match ps.iter_mut().find(|p| p.0 == id) {
            Some(slot) => *slot = (id, x, y),
            None => ps.push((id, x, y)),
        });
        if first {
            down_xy.set_value(Some((x, y)));
            moved.set_value(false);
        }
        // A new finger starting a pinch: reset the baseline so the first
        // two-pointer move just samples the distance rather than jumping.
        pinch_dist.set_value(None);
    };

    // Move: update this pointer's position, then pan or pinch by how it changed.
    let on_pointermove = move |ev: web_sys::PointerEvent| {
        let id = ev.pointer_id();
        let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
        let (ox, oy) = svg_origin(&ev);

        // Previous position of this pointer, then store the new one.
        let prev = pointers.with_value(|ps| ps.iter().find(|p| p.0 == id).map(|p| (p.1, p.2)));
        pointers.update_value(|ps| {
            if let Some(slot) = ps.iter_mut().find(|p| p.0 == id) {
                *slot = (id, x, y);
            }
        });

        // Capture now that the gesture is live, so moves keep arriving even if the
        // pointer leaves the SVG. (Deferred to first move so taps don't capture.)
        let capture = |ev: &web_sys::PointerEvent| {
            if let Some(t) = ev.current_target() {
                if let Ok(el) = t.dyn_into::<web_sys::Element>() {
                    let _ = el.set_pointer_capture(ev.pointer_id());
                }
            }
        };

        let count = pointers.with_value(|ps| ps.len());
        if count >= 2 {
            // Two fingers => a pinch, never a tap.
            moved.set_value(true);
            dragging.set(true);
            capture(&ev);
            // Zoom by the change in distance between the first two pointers,
            // anchored at their (SVG-local) midpoint.
            let (a, b) = pointers.with_value(|ps| (ps[0], ps[1]));
            let dist = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt();
            let (mx, my) = ((a.1 + b.1) / 2.0 - ox, (a.2 + b.2) / 2.0 - oy);
            let prev_dist = pinch_dist.get_value().unwrap_or(0.0);
            camera.update(|c| *c = c.pinched(prev_dist, dist, mx, my));
            pinch_dist.set_value(Some(dist));
        } else if let Some((px, py)) = prev {
            // Single pointer: only treat it as a drag once it crosses the
            // threshold from where it started; below that it's still a tap.
            if !moved.get_value() {
                let far = down_xy.get_value().is_some_and(|(sx, sy)| {
                    ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() > DRAG_THRESHOLD
                });
                if far {
                    moved.set_value(true);
                    dragging.set(true);
                    capture(&ev);
                }
            }
            if moved.get_value() {
                // Pan 1:1 with the pointer's movement, independent of zoom.
                camera.update(|c| *c = c.panned(x - px, y - py));
            }
        }
    };

    // Release / cancel: drop the pointer; end the drag once none remain. Reset
    // the pinch baseline so lifting one of two fingers doesn't make the next move
    // jump. `moved` is left for the click that may follow, and reset on next press.
    let on_pointerup = move |ev: web_sys::PointerEvent| {
        let id = ev.pointer_id();
        pointers.update_value(|ps| ps.retain(|p| p.0 != id));
        pinch_dist.set_value(None);
        if pointers.with_value(|ps| ps.is_empty()) {
            dragging.set(false);
        }
    };

    // Wheel: zoom toward the cursor on desktop (trackpad/mouse). Up/away zooms
    // in, down/toward zooms out. Touch pinch is handled above, not here.
    let on_wheel = move |ev: web_sys::WheelEvent| {
        ev.prevent_default(); // don't let the page scroll
        let factor = if ev.delta_y() < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        let (sx, sy) = (ev.offset_x() as f64, ev.offset_y() as f64);
        camera.update(|c| *c = c.zoomed_at(factor, sx, sy));
    };

    // The context menu overlay (Issue #18): a plain HTML pop-up positioned at the
    // click, rendered outside the SVG so it never pans/zooms and isn't clipped.
    let menu_view = move || {
        menu.get().map(|m| {
            let label = m.github_label;
            let open_github = match m.github_url.clone() {
                // Live link: a real anchor, opening GitHub in a new tab. Tapping it
                // also closes the menu.
                Some(url) => view! {
                    <a
                        class="ctx-item"
                        href=url
                        target="_blank"
                        rel="noopener"
                        on:click=move |_| menu.set(None)
                    >
                        {label}
                    </a>
                }
                .into_view(),
                // No GitHub page for this target (no github remote, or unpushed):
                // show the option but disabled, with a reason on hover.
                None => view! {
                    <span
                        class="ctx-item disabled"
                        title="No GitHub page (no github.com remote, or it isn't pushed)"
                    >
                        {label}
                    </span>
                }
                .into_view(),
            };
            // "Create branch from this commit": prompt for a name, POST it, then
            // refresh the graph on success or show git's error on failure (B3).
            let commit = m.commit.clone();
            let on_branch = move |_| {
                menu.set(None);
                let Some(win) = web_sys::window() else { return };
                // A native prompt — simple and works in iPad Safari. Empty / cancel
                // does nothing.
                let name = match win.prompt_with_message("Name for the new branch:") {
                    Ok(Some(n)) => n.trim().to_string(),
                    _ => return,
                };
                if name.is_empty() {
                    return;
                }
                let commit = commit.clone();
                spawn_local(async move {
                    match create_branch_request(&name, &commit).await {
                        // Bump the fetch counter so the new branch appears.
                        Ok(()) => reload.update(|n| *n = n.wrapping_add(1)),
                        Err(e) => {
                            if let Some(w) = web_sys::window() {
                                let _ = w.alert_with_message(&format!("Couldn't create branch:\n{e}"));
                            }
                        }
                    }
                });
            };
            let create_label = m.create_label;
            // The two "Commit …" items (Issue #33). Clicking one closes the menu
            // and opens the commit-message modal (below); the actual POST + refresh
            // happens when the user confirms there. They're enabled only on the
            // HEAD tip (the only place a commit can land without moving HEAD);
            // elsewhere they render disabled with a reason.
            let is_head = m.is_head;
            let make_commit_item = move |label: &'static str, allow_empty: bool| {
                if !is_head {
                    return view! {
                        <span
                            class="ctx-item disabled"
                            title="Only available on the current HEAD commit"
                        >
                            {label}
                        </span>
                    }
                    .into_view();
                }
                let on_commit = move |_| {
                    // Open the dialog *before* closing the menu: `menu.set(None)`
                    // synchronously disposes this handler's own reactive owner, so
                    // any signal write after it is unreliable. Set the dialog first.
                    commit_msg.set(String::new());
                    dialog_opened_at.set_value(js_sys::Date::now());
                    commit_dialog.set(Some(allow_empty));
                    menu.set(None);
                };
                view! { <button class="ctx-item" on:click=on_commit>{label}</button> }.into_view()
            };
            let commit_staged = make_commit_item("Commit staged changes", false);
            let commit_empty = make_commit_item("Create empty commit", true);
            // The branch operations (Issue #33 follow-up): merge / push / delete, one
            // set per local branch living at this target. Each opens the confirm modal
            // rather than acting immediately — the actual POST + refresh happens there.
            // Set `confirm_op` *before* `menu.set(None)`, which disposes this handler's
            // reactive owner (same ordering caveat as the commit items above).
            let branch_items = m
                .branches
                .iter()
                .flat_map(|b| {
                    let b = b.clone();
                    // Merge into the checked-out branch. The target is resolved *live*
                    // on click (not from the possibly-stale graph), so the item stays
                    // generic — "into current branch" — and the confirm dialog names
                    // the real HEAD branch once the fetch returns. Whether it's a
                    // no-op self-merge or a detached HEAD is decided there too.
                    let merge_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            let branch = branch.clone();
                            menu.set(None);
                            spawn_local(async move {
                                let into = fetch_head_branch().await.unwrap_or(None);
                                // Start the ghost-click guard when the modal opens.
                                dialog_opened_at.set_value(js_sys::Date::now());
                                confirm_op.set(Some(PendingOp::Merge { branch, into }));
                            });
                        };
                        view! {
                            <button class="ctx-item" on:click=on>
                                {format!("Merge ‘{b}’ into current branch")}
                            </button>
                        }
                        .into_view()
                    };
                    // Push: always available; git reports if there's no origin/upstream.
                    let push_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            dialog_opened_at.set_value(js_sys::Date::now());
                            confirm_op.set(Some(PendingOp::Push { branch: branch.clone() }));
                            menu.set(None);
                        };
                        view! { <button class="ctx-item" on:click=on>{format!("Push ‘{b}’")}</button> }
                            .into_view()
                    };
                    // Delete: like merge, the "is this the checked-out branch?" test is
                    // resolved live on click, not from the possibly-stale graph. The
                    // confirm dialog blocks deleting the current branch; git's safe
                    // `-d` still refuses an unmerged one server-side.
                    let delete_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            let branch = branch.clone();
                            menu.set(None);
                            spawn_local(async move {
                                let current = fetch_head_branch().await.unwrap_or(None);
                                // Start the ghost-click guard when the modal opens.
                                dialog_opened_at.set_value(js_sys::Date::now());
                                confirm_op.set(Some(PendingOp::Delete { branch, current }));
                            });
                        };
                        view! {
                            <button class="ctx-item danger" on:click=on>{format!("Delete ‘{b}’")}</button>
                        }
                        .into_view()
                    };
                    [merge_item, push_item, delete_item]
                })
                .collect_view();
            view! {
                <div class="ctx-menu" style=format!("left: {}px; top: {}px;", m.x, m.y)>
                    <div class="ctx-menu-header">{m.header.clone()}</div>
                    {open_github}
                    <button class="ctx-item" on:click=on_branch>
                        {create_label}
                    </button>
                    {commit_staged}
                    {commit_empty}
                    {branch_items}
                </div>
            }
        })
    };

    // The commit-message modal (Issue #33). Shown while `commit_dialog` is `Some`;
    // a real overlay with a focused text box, so it prompts reliably where a native
    // `window.prompt()` gets blocked/flashed by the webview. Confirming POSTs the
    // commit and refreshes the graph; cancelling just closes it.
    let submit_commit = move || {
        let Some(allow_empty) = commit_dialog.get_untracked() else {
            return;
        };
        let message = commit_msg.get_untracked().trim().to_string();
        if message.is_empty() {
            return; // Keep the dialog open; the Commit button is disabled anyway.
        }
        commit_dialog.set(None);
        spawn_local(async move {
            match create_commit_request(&message, allow_empty).await {
                Ok(()) => reload.update(|n| *n = n.wrapping_add(1)),
                Err(e) => {
                    if let Some(w) = web_sys::window() {
                        let _ = w.alert_with_message(&format!("Couldn't create commit:\n{e}"));
                    }
                }
            }
        });
    };
    let commit_dialog_view = move || {
        commit_dialog.get().map(|allow_empty| {
            let title = if allow_empty { "Create empty commit" } else { "Commit staged changes" };
            // The message field is a <textarea>, NOT an <input>: the void <input>
            // element breaks Leptos' CSR <template> node-walk on iOS WebKit (which
            // parses void elements differently than Blink/Gecko), panicking the whole
            // view so the modal never mounts on iPad. A textarea is non-void — and is
            // fine for a commit message. Styles are inline and viewport-sized
            // (100vw/100vh) since that's what proved to render reliably on iOS.
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if js_sys::Date::now() - dialog_opened_at.get_value()
                            > DIALOG_GUARD_MS
                        {
                            commit_dialog.set(None);
                        }
                    }
                >
                    <div
                        style="min-width:300px; max-width:90vw; padding:16px; \
                               background:#161b22; border:1px solid #30363d; \
                               border-radius:10px; color:var(--fg); \
                               box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="font-weight:600; margin-bottom:12px;">{title}</div>
                        <textarea
                            style="width:100%; box-sizing:border-box; padding:10px; \
                                   font:inherit; color:var(--fg); background:#0d1117; \
                                   border:1px solid #30363d; border-radius:6px; \
                                   resize:none;"
                            rows="2"
                            placeholder="Commit message"
                            prop:value=move || commit_msg.get()
                            on:input=move |ev| commit_msg.set(event_target_value(&ev))
                        ></textarea>
                        <div style="display:flex; gap:8px; justify-content:flex-end; \
                                    margin-top:14px;">
                            <button
                                style="padding:6px 14px; font:inherit; color:var(--fg); \
                                       background:#21262d; border:1px solid #30363d; \
                                       border-radius:6px;"
                                on:click=move |_| commit_dialog.set(None)
                            >
                                "Cancel"
                            </button>
                            <button
                                style="padding:6px 14px; font:inherit; color:#fff; \
                                       background:#238636; border:1px solid #2ea043; \
                                       border-radius:6px;"
                                prop:disabled=move || commit_msg.get().trim().is_empty()
                                on:click=move |_| submit_commit()
                            >
                                "Commit"
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    };

    // The branch-op confirmation modal (Issue #33 follow-up). Reuses the commit
    // modal's iPad-proven inline-styled overlay, minus any text input (so no void
    // `<input>` to trip the WebKit CSR bug). Confirming runs the pending op and
    // refreshes; cancelling or a backdrop tap closes it.
    let run_confirmed = move || {
        let Some(op) = confirm_op.get_untracked() else {
            return;
        };
        confirm_op.set(None);
        let (path, branch, verb) = match op {
            PendingOp::Merge { branch, .. } => ("/api/merge", branch, "merge"),
            PendingOp::Push { branch } => ("/api/push", branch, "push"),
            PendingOp::Delete { branch, .. } => ("/api/delete-branch", branch, "delete"),
        };
        spawn_local(async move {
            match branch_op_request(path, &branch).await {
                Ok(()) => reload.update(|n| *n = n.wrapping_add(1)),
                Err(e) => {
                    if let Some(w) = web_sys::window() {
                        let _ = w.alert_with_message(&format!("Couldn't {verb} ‘{branch}’:\n{e}"));
                    }
                }
            }
        });
    };
    let confirm_modal_view = move || {
        confirm_op.get().map(|op| {
            // `enabled` gates the confirm button: a merge into itself or a detached
            // HEAD has no valid target, so the dialog is informational (Cancel only).
            let (title, body, confirm_label, danger, enabled) = match &op {
                PendingOp::Merge { branch, into } => match into {
                    Some(into) if into != branch => (
                        "Merge branch",
                        format!("Merge ‘{branch}’ into ‘{into}’? This updates ‘{into}’ in the working tree."),
                        "Merge",
                        false,
                        true,
                    ),
                    Some(into) => (
                        "Merge branch",
                        format!("‘{into}’ is the branch you're on — there's nothing to merge into itself."),
                        "Merge",
                        false,
                        false,
                    ),
                    None => (
                        "Merge branch",
                        format!("HEAD is detached, so there's no branch to merge ‘{branch}’ into. Check out a branch first."),
                        "Merge",
                        false,
                        false,
                    ),
                },
                PendingOp::Push { branch } => (
                    "Push branch",
                    format!("Push ‘{branch}’ to origin?"),
                    "Push",
                    false,
                    true,
                ),
                PendingOp::Delete { branch, current } => match current {
                    Some(current) if current == branch => (
                        "Delete branch",
                        format!("‘{branch}’ is the branch you're on — check out another branch before deleting it."),
                        "Delete",
                        true,
                        false,
                    ),
                    // A different branch, or detached HEAD: safe to offer the delete.
                    _ => (
                        "Delete branch",
                        format!("Delete branch ‘{branch}’? Only a fully-merged branch can be deleted here."),
                        "Delete",
                        true,
                        true,
                    ),
                },
            };
            // The confirm button is muted when disabled, red for a destructive
            // delete, green otherwise.
            let confirm_style = if !enabled {
                "padding:6px 14px; font:inherit; color:var(--muted); \
                 background:#21262d; border:1px solid #30363d; border-radius:6px; \
                 opacity:0.6;"
            } else if danger {
                "padding:6px 14px; font:inherit; color:#fff; \
                 background:#da3633; border:1px solid #f85149; border-radius:6px;"
            } else {
                "padding:6px 14px; font:inherit; color:#fff; \
                 background:#238636; border:1px solid #2ea043; border-radius:6px;"
            };
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if js_sys::Date::now() - dialog_opened_at.get_value() > DIALOG_GUARD_MS {
                            confirm_op.set(None);
                        }
                    }
                >
                    <div
                        style="min-width:300px; max-width:90vw; padding:16px; \
                               background:#161b22; border:1px solid #30363d; \
                               border-radius:10px; color:var(--fg); \
                               box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="font-weight:600; margin-bottom:12px;">{title}</div>
                        <div style="margin-bottom:14px; line-height:1.4;">{body}</div>
                        <div style="display:flex; gap:8px; justify-content:flex-end;">
                            <button
                                style="padding:6px 14px; font:inherit; color:var(--fg); \
                                       background:#21262d; border:1px solid #30363d; \
                                       border-radius:6px;"
                                on:click=move |_| confirm_op.set(None)
                            >
                                "Cancel"
                            </button>
                            <button
                                style=confirm_style
                                prop:disabled=!enabled
                                on:click=move |_| run_confirmed()
                            >
                                {confirm_label}
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    };

    view! {
        <svg
            class="graph-svg"
            class:grabbing=move || dragging.get()
            on:pointerdown=on_pointerdown
            on:pointermove=on_pointermove
            on:pointerup=on_pointerup
            on:pointercancel=on_pointerup
            on:wheel=on_wheel
        >
            <g transform=move || camera.get().transform()>
                // Phase 8 (viewport virtualization): only the rows — and the edges —
                // currently on screen are rendered. `visible` is the row window as a
                // `Memo`, so panning within a row doesn't churn the DOM; each keyed
                // `<For>` adds/removes only the rows that actually cross the viewport
                // edge. Order matters for painting: edges first, then nodes on top,
                // then the label tiers, then stubs (unchanged from before).
                <For
                    each=move || visible_edges(ctx, visible.get())
                    key=|ei| *ei
                    children=move |ei| build_edge(ei)
                />
                <For
                    each=move || { let (s, e) = visible.get(); (s..e).collect::<Vec<usize>>() }
                    key=|i| *i
                    children=move |i| build_node(i)
                />
                // Phase 9 (level of detail): the two label tiers, each hidden as the
                // graph is zoomed out. The message tier (badges + message) drops
                // below MESSAGE_SCALE; the dimmed meta line drops below FULL_SCALE,
                // so it's shown only at the closest zoom. Hidden via `.lod-hidden`
                // (display:none), keeping the node/edge structure readable when the
                // text would just be an unreadable smear.
                <g class:lod-hidden=move || !detail_for(camera.get().scale).shows_message()>
                    <For
                        each=move || { let (s, e) = visible.get(); (s..e).collect::<Vec<usize>>() }
                        key=|i| *i
                        children=move |i| build_msg(i)
                    />
                </g>
                <g class:lod-hidden=move || !detail_for(camera.get().scale).shows_meta()>
                    <For
                        each=move || { let (s, e) = visible.get(); (s..e).collect::<Vec<usize>>() }
                        key=|i| *i
                        children=move |i| build_meta(i)
                    />
                </g>
                {stubs}
            </g>
        </svg>
        // The overlays: the context menu, the commit modal, and the branch-op
        // confirm modal. They're mutually exclusive (opening either modal closes the
        // menu), so one reactive block renders whichever is active — all are
        // `position: fixed`, so this wrapper adds no layout.
        <div class="overlays">
            {move || {
                let menu = menu_view();
                let modal = commit_dialog_view();
                let confirm = confirm_modal_view();
                view! { {menu} {modal} {confirm} }
            }}
        </div>
    }
}
