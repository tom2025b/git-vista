//! The top-level Leptos components for git-vista.
//!
//! Phase 4: on startup the frontend `fetch`es the real, laid-out commit
//! [`Graph`] from the backend's `/api/commits` endpoint (same origin) and renders
//! it as inline SVG — one circle per commit, one curved path per commit->parent
//! link, lanes laid out left to right. The whole graph lives inside a single
//! `<g transform>` driven by a [`Camera`]: pointer drags pan, the wheel zooms
//! toward the cursor (Phase 2).
//!
//! This module is now just the two top-level pieces — the [`App`] shell
//! (topbar, toggles, status line, the data fetch) and [`graph_canvas`], which
//! wires the shared signals into the SVG. The heavy lifting lives in focused
//! sibling modules: the HTTP calls in [`crate::api`], persisted toggles in
//! [`crate::prefs`], the shared types and signal bundles in [`crate::state`],
//! the SVG builders in [`crate::render`], pan/zoom in [`crate::gestures`], and
//! the overlays in [`crate::menu`] / [`crate::dialogs`] / [`crate::detail`].
//! Spatial math is in [`crate::geometry`], the lane palette in [`crate::color`].

use leptos::*;

use git_vista_core::model::{Graph, RefKind};

use crate::api::{fetch_commit_detail, fetch_graph, fetch_head_branch, fetch_status};
use crate::camera::Camera;
use crate::geometry::label_x;
use crate::gestures::{self, GestureState};
use crate::icons::icon_set;
use crate::lod::detail_for;
use crate::prefs::{
    load_icon_pref, load_node_icons_pref, store_icon_pref, store_node_icons_pref,
};
use crate::render::{self, RenderCtx};
use crate::state::{MenuData, Overlays, PendingOp, Settings};
use crate::viewport::visible_row_range;
use crate::{activity, detail, dialogs, menu};

/// Extra rows rendered above and below the visible window so a fast pan doesn't
/// flash a blank strip before the row `Memo` catches up (Phase 8).
const OVERSCAN_ROWS: usize = 6;

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

    // The checked-out branch, shown next to the repo name in the status line.
    // Fetched from the same endpoint the merge/delete confirmations use — not
    // inferred from the graph's badges, where several branches on the HEAD
    // commit would make "which one is checked out?" a guess. Keyed on `reload`
    // so Refresh (or any post-operation reload) re-reads it too. `None`/pending
    // => the branch chip is simply omitted (detached HEAD, or still loading).
    let head_branch = create_local_resource(
        move || reload.get(),
        |_| async { fetch_head_branch().await.unwrap_or(None) },
    );

    // The live working-tree status behind the topbar chip (Activity/Undo
    // step 1): clean/dirty/conflicted at a glance, plus ahead/behind vs the
    // upstream. Keyed on `reload` so Refresh — and every post-operation
    // reload — re-reads it alongside the graph. A fetch failure resolves to
    // `None`, which simply hides the chip: a broken status probe shouldn't
    // take the topbar down with it.
    let status = create_local_resource(
        move || reload.get(),
        |_| async { fetch_status().await.ok() },
    );

    // Icon style (icons.rs): Nerd Font glyphs vs the plain-text fallback. A
    // signal so every icon in the app switches live when toggled; persisted in
    // localStorage so a device without a Nerd Font stays on text across loads.
    let nerd_icons = create_rw_signal(load_icon_pref());
    let toggle_icons = move |_| {
        let nerd = !nerd_icons.get_untracked();
        nerd_icons.set(nerd);
        store_icon_pref(nerd);
    };

    // Whether the per-node icons (the glyph beside each commit dot) are shown.
    // Always-on by default; the topbar toggle hides them for anyone who prefers
    // bare dots. Persisted like the icon style.
    let show_node_icons = create_rw_signal(load_node_icons_pref());
    let toggle_node_icons = move |_| {
        let on = !show_node_icons.get_untracked();
        show_node_icons.set(on);
        store_node_icons_pref(on);
    };

    // Whether the Activity panel is open (Activity/Undo feature). Lives here —
    // not in graph_canvas — because its button sits in the topbar, which
    // exists even while the graph is still loading; threaded into the
    // overlays bundle inside graph_canvas.
    let activity_open = create_rw_signal(false);

    // Phase 12 — "Open URL": clone a public repo and view it read-only. `open_url`
    // toggles the modal; `clone_url` holds the field; `cloning` disables the button
    // while git works so a slow clone can't be fired twice. `open_opened_at` guards
    // the backdrop against the iOS ghost-click, same trick as the commit modal.
    // The modal itself lives in `dialogs::open_url_view`.
    let open_url = create_rw_signal(false);
    let clone_url = create_rw_signal(String::new());
    let cloning = create_rw_signal(false);
    let open_opened_at = store_value(0f64);

    view! {
        <main class="app">
            <header class="topbar">
                // The git mark brands the title (icons.rs). Reactive so the
                // topbar switches with the icon-style toggle like everything else.
                <h1>
                    <span class="nf app-icon">{move || icon_set(nerd_icons.get()).git}</span>
                    "git-vista"
                </h1>
                <span class="subtitle">"vertical git history — drag to pan, pinch or scroll to zoom"</span>
                // The working-tree status chip: conflicts trump dirt trumps
                // clean, so the chip always shows the most urgent truth about
                // the tree. Ahead/behind use plain unicode arrows (not Nerd
                // glyphs) so they render identically in both icon modes; the
                // hover title carries the full breakdown.
                {move || status.get().flatten().map(|s| {
                    let ic = icon_set(nerd_icons.get());
                    let (class, icon, label) = if !s.conflicted.is_empty() {
                        ("status-chip conflict", ic.conflict,
                         format!("{} conflicted", s.conflicted.len()))
                    } else if !s.is_clean() {
                        let n = s.change_count();
                        ("status-chip dirty", ic.dirty,
                         format!("{n} change{}", if n == 1 { "" } else { "s" }))
                    } else {
                        ("status-chip clean", ic.clean, "clean".to_string())
                    };
                    let mut sync = String::new();
                    if s.ahead > 0 { sync.push_str(&format!(" ↑{}", s.ahead)); }
                    if s.behind > 0 { sync.push_str(&format!(" ↓{}", s.behind)); }
                    let title = format!(
                        "{} staged · {} unstaged · {} untracked · {} conflicted{}",
                        s.staged.len(), s.unstaged.len(), s.untracked.len(),
                        s.conflicted.len(),
                        s.upstream.as_deref()
                            .map(|u| format!(" · vs {u}"))
                            .unwrap_or_default(),
                    );
                    view! {
                        <span class=class title=title>
                            <span class="nf">{icon}</span>
                            {format!(" {label}{sync}")}
                        </span>
                    }
                })}
                <button
                    class="refresh"
                    on:click=toggle_icons
                    title="Switch between Nerd Font glyph icons and plain-text icons \
                           (use text on devices without a Nerd Font installed)"
                >
                    {move || if nerd_icons.get() { "Icons: glyphs" } else { "Icons: text" }}
                </button>
                <button
                    class="refresh"
                    on:click=toggle_node_icons
                    title="Show or hide the small icons beside each commit dot"
                >
                    {move || if show_node_icons.get() { "Dot icons: on" } else { "Dot icons: off" }}
                </button>
                <button
                    class="refresh"
                    on:click=move |_| {
                        clone_url.set(String::new());
                        open_opened_at.set_value(js_sys::Date::now());
                        open_url.set(true);
                    }
                    title="Clone a public repository from a URL and view its history (read-only)"
                >
                    "Open URL…"
                </button>
                <button
                    class="refresh"
                    on:click=move |_| activity_open.update(|open| *open = !*open)
                    title="Everything that happened in this repo — commits, merges, \
                           branch operations — with what was done through the app \
                           marked, and undo where possible"
                >
                    "Activity"
                </button>
                <button
                    class="refresh"
                    on:click=refresh
                    title="Re-read the repository — shows branches and commits created since the page loaded"
                >
                    "Refresh"
                </button>
            </header>
            // The "Open URL" modal (Phase 12), factored into `dialogs`.
            {dialogs::open_url_view(open_url, clone_url, cloning, open_opened_at, reload)}
            <section class="graph">
                {move || {
                    // Read the icon set here, inside the reactive block, so the
                    // status lines re-render when the icon style is toggled.
                    let ic = icon_set(nerd_icons.get());
                    match graph.get() {
                        None => view! { <p class="status">"Loading history…"</p> }.into_view(),
                        // The conflict/warning glyph flags the failure at a glance.
                        Some(Err(e)) => view! {
                            <p class="status error">
                                <span class="nf">{ic.conflict}</span>
                                {format!(" Failed to load history: {e}")}
                            </p>
                        }
                        .into_view(),
                        Some(Ok(g)) => {
                            // Show which repo this page is actually displaying, straight
                            // from the API response. If it disagrees with the terminal,
                            // the browser is pointed at a stale server/tab — now visible.
                            let repo = g.repo_label.clone();
                            view! {
                                // Repo glyph + name, then branch glyph + checked-out
                                // branch — the icons carry what the old "repository:"
                                // prefix used to spell out.
                                {repo.map(|r| view! {
                                    <p class="status repo">
                                        <span class="nf ic-repo">{ic.repository}</span>
                                        {format!(" {r}")}
                                        {head_branch.get().flatten().map(|b| view! {
                                            <span class="repo-branch">
                                                <span class="nf ic-branch">{ic.branch}</span>
                                                {format!(" {b}")}
                                            </span>
                                        })}
                                    </p>
                                })}
                                {graph_canvas(g, reload, nerd_icons, show_node_icons, activity_open)}
                            }
                            .into_view()
                        }
                    }
                }}
            </section>
        </main>
    }
}

/// Render a loaded [`Graph`] as a pan/zoomable SVG canvas. `reload` is the App's
/// fetch counter, bumped after a successful branch creation so the new branch
/// shows without a full reload (Issue #18, reusing the Issue #16 refresh path).
/// `nerd_icons` picks the icon set (icons.rs) for the badges, labels and menus;
/// `show_node_icons` shows/hides the glyph beside each commit dot.
///
/// This is the wiring layer: it derives the [`RenderCtx`], creates the shared
/// overlay/gesture signals, bundles them ([`Settings`] / [`Overlays`] /
/// [`GestureState`]), installs the window listeners, and assembles the `<svg>`
/// from the [`crate::render`] builders and the overlay views.
fn graph_canvas(
    graph: Graph,
    reload: RwSignal<u32>,
    nerd_icons: RwSignal<bool>,
    show_node_icons: RwSignal<bool>,
    activity_open: RwSignal<bool>,
) -> impl IntoView {
    // Per-branch colour slot for each row, indexed by row number (rows are stored
    // in row order), so an edge can pick up its parent's branch colour.
    let row_color: Vec<usize> = graph.rows.iter().map(|gr| gr.color).collect();

    // Phase 12: a repo cloned from a URL is view-only, so every write action in the
    // context menu (create branch, commit, merge, push, delete) is suppressed. The
    // server also refuses these with 403, but hiding them keeps the menu honest.
    let read_only = graph.read_only;

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
    // here so the link click handlers (render) and gesture handlers share one flag.
    let moved = store_value(false);
    // The open context menu, if any (Issue #18). `None` => no menu. Set when a dot
    // is tapped (render::build_node), cleared on a pan/tap-elsewhere (pointerdown).
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
    // The commit whose detail panel is open (Phase 10), by full hash. `None` => no
    // panel. Set from the context menu's "View details" item; cleared by the
    // panel's close button. A `Resource` keyed on it fetches the full commit lazily
    // — so the graph payload stays lean and the panel shows the whole message body
    // and both signatures, which the row summary doesn't carry.
    let detail_id = create_rw_signal(None::<String>);
    let detail = create_local_resource(
        move || detail_id.get(),
        |id| async move {
            match id {
                Some(id) => Some(fetch_commit_detail(&id).await),
                None => None,
            }
        },
    );
    // When the commit modal was opened (ms). iOS synthesizes a `click` a few ms
    // after a tap; opening the modal puts its full-screen backdrop under that tap
    // point, so the ghost click hits the backdrop and closes the modal instantly.
    // The backdrop ignores a dismiss that lands within `DIALOG_GUARD_MS` of opening.
    let dialog_opened_at = store_value(0.0_f64);
    // One-shot "scroll the Changes section into view" instruction, set by the
    // menu's "Show diff" item and consumed by the detail panel's next render.
    let scroll_diff = store_value(false);

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

    // The signal bundles the split view modules take (see `crate::state`): one
    // `Copy` handle each instead of a fistful of separate signals.
    let settings = Settings { nerd_icons, show_node_icons };
    let overlays = Overlays {
        menu,
        commit_dialog,
        commit_msg,
        confirm_op,
        detail_id,
        activity_open,
        scroll_diff,
        dialog_opened_at,
        reload,
    };

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
    let vp_h = create_rw_signal(gestures::window_inner_height());
    // Window listeners (resize → viewport height; keydown → shortcuts), each
    // removed on cleanup so a graph reload doesn't stack duplicate handlers.
    gestures::install_resize_listener(vp_h);
    gestures::install_key_listener(camera, reload, overlays);
    let visible =
        create_memo(move |_| visible_row_range(camera.get(), vp_h.get(), row_count, OVERSCAN_ROWS));

    // Gesture tracking on Pointer Events (see `crate::gestures`): the live pointer
    // list, the previous pinch distance, and where the gesture started, all held in
    // `store_value` cells and bundled with the camera/dragging/menu/moved signals.
    let pointers = store_value(Vec::<(i32, f64, f64)>::new());
    let pinch_dist = store_value(Option::<f64>::None);
    let down_xy = store_value(Option::<(f64, f64)>::None);
    let gs = GestureState {
        camera,
        dragging,
        menu,
        moved,
        pointers,
        pinch_dist,
        down_xy,
    };

    view! {
        <svg
            class="graph-svg"
            class:grabbing=move || dragging.get()
            on:pointerdown=move |ev| gestures::on_pointer_down(gs, ev)
            on:pointermove=move |ev| gestures::on_pointer_move(gs, ev)
            on:pointerup=move |ev| gestures::on_pointer_up(gs, ev)
            on:pointercancel=move |ev| gestures::on_pointer_up(gs, ev)
            on:wheel=move |ev| gestures::on_wheel(camera, ev)
        >
            <g transform=move || camera.get().transform()>
                // Phase 8 (viewport virtualization): only the rows — and the edges —
                // currently on screen are rendered. `visible` is the row window as a
                // `Memo`, so panning within a row doesn't churn the DOM; each keyed
                // `<For>` adds/removes only the rows that actually cross the viewport
                // edge. Order matters for painting: edges first, then nodes on top,
                // then the label tiers, then stubs (unchanged from before).
                <For
                    each=move || render::visible_edges(ctx, visible.get())
                    key=|ei| *ei
                    children=move |ei| render::build_edge(ctx, ei)
                />
                <For
                    each=move || { let (s, e) = visible.get(); (s..e).collect::<Vec<usize>>() }
                    key=|i| *i
                    children=move |i| render::build_node(ctx, menu, moved, i)
                />
                // Phase 9 (level of detail): the two label tiers, each hidden as the
                // graph is zoomed out. The message tier (badges + message) drops
                // below MESSAGE_SCALE; the dimmed meta line drops below FULL_SCALE,
                // so it's shown only at the closest zoom. Hidden via `.lod-hidden`
                // (display:none), keeping the node/edge structure readable when the
                // text would just be an unreadable smear. Rows are keyed on
                // (index, icon mode): the glyphs are read untracked inside the
                // builders, so flipping the "Icons" toggle changes every key and
                // rebuilds the visible rows with the other set.
                <g class:lod-hidden=move || !detail_for(camera.get().scale).shows_message()>
                    <For
                        each=move || {
                            let (s, e) = visible.get();
                            let nerd = nerd_icons.get();
                            (s..e).map(|i| (i, nerd)).collect::<Vec<_>>()
                        }
                        key=|k| *k
                        children=move |(i, _)| render::build_msg(ctx, nerd_icons, moved, i)
                    />
                </g>
                <g class:lod-hidden=move || !detail_for(camera.get().scale).shows_meta()>
                    <For
                        each=move || {
                            let (s, e) = visible.get();
                            let nerd = nerd_icons.get();
                            (s..e).map(|i| (i, nerd)).collect::<Vec<_>>()
                        }
                        key=|k| *k
                        children=move |(i, _)| render::build_meta(ctx, nerd_icons, i)
                    />
                </g>
                // The per-node icons: one layer holding the glyph beside every
                // dot and stub ring, always on unless the user hides it via the
                // topbar "Dot icons" toggle (unlike the label tiers above, this
                // is a preference, not a zoom level). Same virtualization and
                // keying as the label tiers.
                <g class:lod-hidden=move || !settings.show_node_icons.get()>
                    <For
                        each=move || {
                            let (s, e) = visible.get();
                            let nerd = nerd_icons.get();
                            (s..e).map(|i| (i, nerd)).collect::<Vec<_>>()
                        }
                        key=|k| *k
                        children=move |(i, _)| render::build_node_icon(ctx, nerd_icons, i)
                    />
                    {render::stub_icons(ctx, nerd_icons)}
                </g>
                {render::stubs(ctx, menu, moved)}
            </g>
        </svg>
        // Phase 13: a floating "Reset view" control. Pan/zoom can carry the graph
        // off-screen with no obvious way back — easy to do with the iPad trackpad or
        // a pinch — so this recenters the camera (same as the `0` key) with one tap,
        // which is the only way back for anyone driving purely by touch/trackpad.
        <button
            class="reset-view"
            title="Reset pan & zoom (keyboard: 0)"
            on:click=move |_| camera.set(Camera::default())
        >
            "Reset view"
        </button>
        // The overlays: the context menu, the two modals, and the detail panel.
        // They're mutually exclusive (opening either modal closes the menu), and
        // each is `position: fixed`, so this wrapper adds no layout. Each view is a
        // reactive closure that renders only when its signal is set.
        <div class="overlays">
            {menu::menu_view(overlays, settings, read_only)}
            {dialogs::commit_dialog_view(overlays)}
            {dialogs::confirm_modal_view(overlays)}
            {detail::detail_panel_view(overlays, settings, detail, ctx)}
            {activity::activity_panel_view(overlays, settings)}
        </div>
    }
}
