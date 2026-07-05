//! The graph canvas: wire a loaded [`Graph`] into the pan/zoomable SVG.
//!
//! Split out of `app.rs`. [`graph_canvas`] is the wiring layer between the [`App`]
//! shell and the rest of the frontend: it derives the [`RenderCtx`], creates the
//! shared overlay/gesture signals, bundles them ([`Settings`] / [`Overlays`] /
//! [`GestureState`]), installs the window listeners, and assembles the `<svg>`
//! from the [`crate::render`] builders and the overlay views.
//!
//! [`App`]: super::App

use leptos::*;

use git_vista_core::model::{Graph, RefKind};

use crate::api::fetch_commit_detail;
use crate::camera::Camera;
use crate::geometry::{label_x_per_row, stub_headroom};
use crate::gestures::{self, GestureState};
use crate::lod::detail_for;
use crate::render::{self, RenderCtx};
use crate::state::{CommitDialog, MenuData, Overlays, PendingOp, Settings, ViewerDoc};
use crate::viewport::visible_row_range;
use crate::{activity, detail, dialogs, menu, viewer};

/// Extra rows rendered above and below the visible window so a fast pan doesn't
/// flash a blank strip before the row `Memo` catches up (Phase 8).
const OVERSCAN_ROWS: usize = 6;

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
pub(super) fn graph_canvas(
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
    // The open commit-message dialog, if any (Issue #33): which kind of commit
    // (empty vs staged) and, for a branch stub, which branch it lands on.
    // A real in-app modal, not `window.prompt()`, which webviews block/flash.
    let commit_dialog = create_rw_signal(None::<CommitDialog>);
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
    // The full-screen viewer's document (viewer.rs): the full diff or one
    // file's content, opened from the detail panel, with Print / Save PDF.
    let viewer_doc = create_rw_signal(None::<ViewerDoc>);
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
    // Per-row label x, hugging the graph (see label_x_per_row): each row's text
    // sits just right of what's actually drawn at that row, so labels stay
    // snug against the dots however many stub lanes the repo has grown.
    let text_x = label_x_per_row(&graph);
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
        viewer: viewer_doc,
        activity_open,
        scroll_diff,
        dialog_opened_at,
        reload,
    };

    // Camera (pan/zoom) state. Its home position leaves headroom for any stub
    // cascade overshooting the top of the canvas (a branch created on the
    // newest commit tips *above* row 0), so new branches aren't born
    // half-clipped and unreachable until the user thinks to pan up.
    let home = Camera::home(ctx.with_value(|c| stub_headroom(&c.graph.stubs)));
    let camera = create_rw_signal(home);
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
    gestures::install_key_listener(camera, home, reload, overlays);
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
            on:click=move |_| camera.set(home)
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
            {viewer::viewer_view(overlays, settings)}
        </div>
    }
}
