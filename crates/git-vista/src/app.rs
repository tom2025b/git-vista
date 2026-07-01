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
use wasm_bindgen::JsCast;

use gloo_net::http::Request;

use git_vista_core::model::{CreateBranchRequest, Graph, RefKind};

use crate::camera::{Camera, ZOOM_STEP};
use crate::color::{branch_color, BADGE_DARK, HEAD_BADGE, MERGE_FILL, TAG_BADGE};
use crate::datetime::local_timestamp;
use crate::geometry::{
    badge_text_dx, badge_text_y, badge_top_y, badge_width, edge_path, label_bottom_y, label_top_y,
    label_x, node_cx, node_cy, stub_node_cy, stub_path, BADGE_GAP, BADGE_HEIGHT, BADGE_RADIUS,
    NODE_RADIUS,
};
use crate::text::truncate;

/// Commit messages longer than this are truncated with an ellipsis in the label
/// (the full text stays available via the node/label hover tooltip).
const MAX_SUMMARY_CHARS: usize = 60;

/// State for the per-commit context menu (Issue #18): which commit was tapped,
/// where to draw the menu (client/viewport px, since it's an HTML overlay, not
/// part of the pan/zoomed SVG), and the commit's GitHub URL when it has one.
#[derive(Clone)]
struct MenuData {
    /// Full commit hash — what "Create branch" targets.
    commit: String,
    /// Short hash, shown as the menu's header.
    short: String,
    /// Viewport x/y of the click, used to position the overlay.
    x: f64,
    y: f64,
    /// GitHub commit URL, `Some` only when this repo has a github.com origin *and*
    /// the commit is pushed (otherwise the page would 404). Drives whether the
    /// "Open on GitHub" item is a live link or a disabled entry.
    github_url: Option<String>,
}

/// Pointer travel (CSS px) past which a press becomes a pan/drag rather than a
/// tap. Keeps a tap-to-open-link from being eaten by the pan handler.
const DRAG_THRESHOLD: f64 = 4.0;

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

    // Edges first so the nodes paint on top of them.
    let edges = graph
        .edges
        .iter()
        .map(|e| {
            let d = edge_path(e);
            // Colour a link by the branch *line* it belongs to, so it matches the
            // dots it connects:
            //  * a first-parent link is part of the child's own branch — a side
            //    branch forking off main is drawn in the side branch's colour all
            //    the way down to its fork point, not main's blue;
            //  * a merge link (any non-first parent) is part of the merged-in
            //    branch, so it takes that parent's colour as it curves in.
            // Only main (colour slot 0) ever stays blue this way.
            let child = &graph.rows[e.from_row].commit;
            let parent_oid = &graph.rows[e.to_row].commit.id;
            let is_first_parent = child.parents.first() == Some(parent_oid);
            let color_row = if is_first_parent { e.from_row } else { e.to_row };
            let color = branch_color(row_color[color_row]);
            view! {
                <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
            }
        })
        .collect_view();

    let nodes = graph
        .rows
        .iter()
        .map(|gr| {
            let cx = node_cx(gr.lane);
            let cy = node_cy(gr.row);
            let color = branch_color(gr.color);
            // Draw merge commits as a hollow ring so they stand out from the
            // ordinary filled dots.
            let merge = gr.commit.is_merge();
            let fill = if merge { MERGE_FILL } else { color };
            let stroke_width = if merge { "3" } else { "2" };

            // Issue #18: tapping a dot opens a context menu. Gather this commit's
            // menu data now; the click handler clones it in (it may fire repeatedly).
            let commit_id = gr.commit.id.0.clone();
            let short = gr.commit.id.short().to_string();
            // Link target only when the repo is on GitHub *and* this commit is
            // pushed — same rule the labels use, so the menu never offers a 404.
            let github_url = repo_url
                .as_ref()
                .and_then(|base| remote_set.contains(&commit_id).then(|| format!("{base}/commit/{commit_id}")));
            let open_menu = move |ev: web_sys::MouseEvent| {
                // Ignore the click that ends a pan; a real tap opens the menu where
                // the pointer is (viewport coords for the fixed-position overlay).
                if moved.get_value() {
                    return;
                }
                ev.stop_propagation();
                menu.set(Some(MenuData {
                    commit: commit_id.clone(),
                    short: short.clone(),
                    x: ev.client_x() as f64,
                    y: ev.client_y() as f64,
                    github_url: github_url.clone(),
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
                        <title>{format!("{} — {}", gr.commit.id.short(), gr.commit.summary)}</title>
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
        })
        .collect_view();

    // Commit labels: a fixed text column to the right of the lanes, two lines per
    // row — any ref badges then the (truncated) message on top, the short hash and
    // author dimmed below. The full message stays available on hover.
    let text_x = label_x(graph.lane_count);
    let labels = graph
        .rows
        .iter()
        .map(|gr| {
            // Lay any ref badges out left-to-right from the start of the label
            // column; the message then starts just past them (or at the column
            // edge when there are none).
            let mut bx = text_x;
            // Is this row's commit on the remote? Drives whether its message, HEAD
            // badge and tag badges link out (an unpushed commit would 404).
            let commit_on_remote = remote_set.contains(&gr.commit.id.0);
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
                    let badge_url = repo_url.as_ref().and_then(|base| match r.kind {
                        RefKind::Head | RefKind::Tag => {
                            commit_on_remote.then(|| format!("{base}/commit/{}", gr.commit.id.0))
                        }
                        RefKind::Branch => remote_branches
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
                    let unpushed = repo_url.is_some() && badge_url.is_none();
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
            // hash · author · local date+time, so the timeline is visible per row.
            let meta = format!(
                "{} · {} · {}",
                gr.commit.id.short(),
                gr.commit.author,
                local_timestamp(gr.commit.time),
            );
            // The message links to the commit page on GitHub (Issue #12), but only
            // when the commit is on the remote — otherwise it's dimmed and the
            // tooltip says why, rather than linking to a page that would 404.
            let msg_url = repo_url
                .as_ref()
                .and_then(|base| commit_on_remote.then(|| format!("{base}/commit/{}", gr.commit.id.0)));
            let msg_clickable = msg_url.is_some();
            let msg_unpushed = repo_url.is_some() && msg_url.is_none();
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
                <text x=text_x y=label_bottom_y(gr.row) class="label-meta">
                    {meta}
                </text>
            }
        })
        .collect_view();

    // Branch stubs: a local branch with no commits of its own (e.g. one just
    // created from an existing commit) is drawn GitHub-network-graph style — a
    // short, uniquely-coloured line forking off its commit into its own lane,
    // with the branch badge on the fork tip, instead of a second badge crowding
    // the shared commit.
    let stubs = graph
        .stubs
        .iter()
        .map(|s| {
            let color = branch_color(s.color);
            let d = stub_path(s.anchor_lane, s.anchor_row, s.lane);
            let sx = node_cx(s.lane);
            let sy = stub_node_cy(s.anchor_row);
            // Badge sits just right of the fork tip, vertically centred on it.
            let bx = sx + NODE_RADIUS + BADGE_GAP;
            let w = badge_width(&s.name);
            let name = s.name.clone();

            // The stub's tip *is* its anchor commit (the branch owns no commits of
            // its own yet), so tapping the tip opens the same context menu as that
            // commit — letting you fork a further branch straight off this one
            // (Issue #24). We therefore target the anchor commit's id/hash.
            let anchor = &graph.rows[s.anchor_row].commit;
            let commit_id = anchor.id.0.clone();
            let short = anchor.id.short().to_string();
            let github_url = repo_url
                .as_ref()
                .and_then(|base| remote_set.contains(&commit_id).then(|| format!("{base}/commit/{commit_id}")));
            let open_menu = move |ev: web_sys::MouseEvent| {
                // Ignore the click that ends a pan; a real tap opens the menu.
                if moved.get_value() {
                    return;
                }
                ev.stop_propagation();
                menu.set(Some(MenuData {
                    commit: commit_id.clone(),
                    short: short.clone(),
                    x: ev.client_x() as f64,
                    y: ev.client_y() as f64,
                    github_url: github_url.clone(),
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
                // Filled, clickable dot (Issue #24) — matching the commit nodes, so
                // every node in the graph reads as a tappable dot.
                <circle cx=sx cy=sy r=NODE_RADIUS fill=color stroke=color stroke-width="2">
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
                <rect
                    x=bx
                    y=sy - BADGE_HEIGHT / 2
                    width=w
                    height=BADGE_HEIGHT
                    rx=BADGE_RADIUS
                    ry=BADGE_RADIUS
                    fill=color
                    stroke=color
                    stroke-width="1"
                />
                <text x=bx + badge_text_dx() y=sy + 4 class="badge-text" fill=BADGE_DARK>
                    {name}
                </text>
            }
        })
        .collect_view();

    // Camera (pan/zoom) state.
    let camera = create_rw_signal(Camera::default());
    // Whether any pointer is currently pressed (drives the grab/grabbing cursor).
    let dragging = create_rw_signal(false);

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
                        "Open on GitHub"
                    </a>
                }
                .into_view(),
                // No GitHub page for this commit (no github remote, or unpushed):
                // show the option but disabled, with a reason on hover.
                None => view! {
                    <span
                        class="ctx-item disabled"
                        title="This commit has no GitHub page (no github.com remote, or it isn't pushed)"
                    >
                        "Open on GitHub"
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
            view! {
                <div class="ctx-menu" style=format!("left: {}px; top: {}px;", m.x, m.y)>
                    <div class="ctx-menu-header">{m.short.clone()}</div>
                    {open_github}
                    <button class="ctx-item" on:click=on_branch>
                        "Create branch from this commit"
                    </button>
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
                {edges}
                {nodes}
                {labels}
                {stubs}
            </g>
        </svg>
        {menu_view}
    }
}
