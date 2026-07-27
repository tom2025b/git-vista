//! The graph canvas: mount one [`HistorySeed`] and grow it page by page.
//!
//! Split out of `app.rs`. [`graph_canvas`] is the wiring layer between the [`App`]
//! shell and the rest of the frontend: it moves the seed into the one mounted
//! [`RenderCtx`], creates the shared overlay/gesture signals, bundles them
//! ([`Settings`] / [`Overlays`] / [`GestureState`]), installs the window
//! listeners, and assembles the `<svg>` from the [`crate::render`] builders and
//! the overlay views.
//!
//! Since M1.10 (#63) it also owns the **append loop**. History arrives paged, so
//! the canvas is no longer a pure function of its input: the camera reaching the
//! bottom fires one cursor request, and the reply mutates the aggregate *in
//! place*, behind the same `StoredValue` every builder already reads. That is
//! deliberate — a second copy of the history (a `Graph`, a row vector, a
//! resolved-stub cache) would go stale the first time a page landed. What the
//! view gets instead is a handful of epoch signals saying which parts moved.
//!
//! The subtle part is that an append is *asynchronous* while the canvas itself
//! can be unmounted at any moment (a Refresh, a drift reload, a repo switch). So
//! every reply runs a fixed gauntlet before it is allowed to write anything:
//! liveness, then request freshness, then a fallible update on an owner that may
//! already be disposed. See the spawn block below — the order there is the whole
//! safety argument.
//!
//! [`App`]: super::App
//! [`HistorySeed`]: super::HistorySeed

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use leptos::*;

use git_vista_core::model::RefKind;

use crate::api::{fetch_commit_detail, fetch_page, HistoryFetchError};
use crate::camera::Camera;
use crate::features::graph::core::{
    should_prefetch, show_fixed_loading_overlay, GraphCore, PageLoadState, PageRequestKey,
    PageRetry, RenderCtx, DEFAULT_PAGE_LIMIT,
};
use crate::features::operations::core::{IntentSeq, PendingIntent};
use crate::features::operations::signals::Operations;
use crate::geometry::stub_headroom_for;
use crate::gestures::{self, GestureState};
use crate::lod::detail_for;
use crate::print::print_graph_view;
use crate::render;
use crate::state::{CommitDialog, MenuData, Overlays, PendingOp, Settings, ViewerDoc};
use crate::viewport::visible_row_range;
use crate::{activity, detail, dialogs, menu, viewer};

use super::{HistoryPhase, HistorySeed, HistoryUiSignals};

/// Extra rows rendered above and below the visible window so a fast pan doesn't
/// flash a blank strip before the row `Memo` catches up (Phase 8).
const OVERSCAN_ROWS: usize = 6;

/// The cursor was rejected outright. The server answers a forged cursor and one
/// signed by a rotated process key identically on purpose, so the client can't
/// probe the difference — which is also why there is nothing to retry: this
/// cursor is invalid, not unlucky.
const HTTP_BAD_REQUEST: u16 = 400;

/// History moved under the mounted view (a ref changed, or the repository was
/// deepened). The aggregate can't be patched across generations, so the only
/// recovery is a fresh epoch from page 1.
const HTTP_CONFLICT: u16 = 409;

/// Mount one epoch's history and render it as a pan/zoomable SVG canvas.
///
/// `seed` is Frame + page 1 for the epoch this canvas belongs to; it is *moved*
/// into the single [`RenderCtx`] below and never copied. `reload` is the App's
/// fetch counter — also the history epoch — bumped after a successful branch
/// creation so the new branch shows without a full reload (Issue #18, reusing
/// the Issue #16 refresh path) and, since M1.10, by the drift path. `history_ui`
/// is the App's phase/complete/print bundle, which the append loop drives.
/// `nerd_icons` picks the icon set (icons.rs) for the badges, labels and menus;
/// `show_node_icons` shows/hides the glyph beside each commit dot.
pub(super) fn graph_canvas(
    seed: HistorySeed,
    graph: RwSignal<GraphCore>,
    history_ui: HistoryUiSignals,
    nerd_icons: RwSignal<bool>,
    show_node_icons: RwSignal<bool>,
    activity_open: RwSignal<bool>,
    operations: Operations,
) -> impl IntoView {
    let HistorySeed {
        epoch,
        frame,
        loaded,
    } = seed;

    // Phase 12: a repo cloned from a URL is view-only, so every write action in the
    // context menu (create branch, commit, merge, push, delete) is suppressed. The
    // server also refuses these with 403, but hiding them keeps the menu honest.
    // Straight off the Frame — the paged rows carry no repo metadata at all.
    let read_only = frame.read_only;

    // Which branches exist on the remote (the part after the "<remote>/"
    // prefix), so a local branch badge links out only when a remote branch
    // shares its name — an unpushed branch would 404. Derived from the Frame's
    // refs, once, because that is a property of the repository: with paging,
    // whichever rows happen to be loaded say nothing about which branches the
    // remote has. (Whether a *commit* is pushed is per-row: `GraphRow.on_remote`.)
    let remote_branches: HashSet<String> = frame
        .refs
        .iter()
        .filter(|rf| rf.kind == RefKind::RemoteBranch)
        .filter_map(|rf| rf.name.split_once('/').map(|(_, b)| b.to_string()))
        .collect();

    // Read out of the aggregate before it moves: how many rows page 1 brought,
    // and how far down the home camera must sit to keep the stub cascade on
    // screen. Both are re-derived from `ctx` after every append.
    let initial_rows = loaded.rows.len();
    let initial_headroom = stub_headroom_for(
        loaded
            .resolved_stubs()
            .into_iter()
            .map(|s| (s.anchor_row, s.stub.depth)),
    );

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
    // Click ordering for branch operations (M1.11, #64). Every menu item that
    // opens the confirm modal first awaits a live `fetch_head_branch()`, so two
    // quick taps can resolve out of order; `intent_seq` records which tap came
    // first and `pending_intent` records which one actually opened the dialog.
    // Both reset when the canvas is rebuilt on an epoch bump, which is correct:
    // an intent from the previous epoch fails its `RequestKey::is_current` check
    // anyway, so there is nothing older left to compare against.
    let intent_seq = store_value(IntentSeq::default());
    let pending_intent = store_value(None::<PendingIntent>);

    // The mounted canvas's **single owner** of history (M1.10, #63). Frame and
    // aggregate move in here; every builder, the append loop and the overlays
    // read them back out. `StoredValue` (not a signal) because the aggregate is
    // far too big to clone per row, and because appends mutate it *in place* —
    // what the view reacts to is the epoch signals below, not the data itself.
    let ctx = store_value(RenderCtx {
        epoch,
        frame,
        loaded,
        remote_branches,
    });

    // The signal bundles the split view modules take (see `crate::state`): one
    // `Copy` handle each instead of a fistful of separate signals.
    let settings = Settings {
        nerd_icons,
        show_node_icons,
    };
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
        intent_seq,
        pending_intent,
        graph,
        operations,
    };

    // What an append changes, published as signals so the view repaints the
    // minimum. Everything else about the history stays inside `ctx`:
    //   * `row_count`   — how many rows exist, the culler's upper bound;
    //   * `layout_epoch`— bumped only when an already-rendered row's label moved
    //                     (a later page widened the lanes it hangs under);
    //   * `stub_epoch`  — bumped when the resolved stubs moved, which the two
    //                     eager stub layers key off;
    //   * `page_load`   — the single-flight state of the cursor request;
    //   * `home`        — the reset-view camera, which shifts down when a stub
    //                     cascade grows past the top edge.
    let row_count = create_rw_signal(initial_rows);
    let layout_epoch = create_rw_signal(0u32);
    let stub_epoch = create_rw_signal(0u32);
    let page_load = create_rw_signal(PageLoadState::Idle);
    let home = create_rw_signal(Camera::home(initial_headroom));

    // Camera (pan/zoom) state, starting at the home position so a new branch
    // isn't born half-clipped above the top of the canvas.
    let camera = create_rw_signal(home.get_untracked());
    // Whether any pointer is currently pressed (drives the grab/grabbing cursor).
    let dragging = create_rw_signal(false);

    // Phase 8 — viewport virtualization. Track the viewport height and derive the
    // window of rows currently on screen; the `<For>`s in the view render only
    // those (plus a small overscan margin). Using a `Memo` means a sub-row pan
    // doesn't rebuild anything — the row set changes only when a row actually
    // enters or leaves the viewport, and the keyed `<For>` then adds/removes just
    // that row's DOM rather than re-rendering the screenful. Since M1.10 the row
    // count is a *signal*: an accepted page widens the window without remounting
    // anything, and `visible_row_range` caps what it hands back at MAX_LIVE_ROWS.
    let vp_h = create_rw_signal(gestures::window_inner_height());
    // Window listeners (resize → viewport height; keydown → shortcuts), each
    // removed on cleanup so a graph reload doesn't stack duplicate handlers.
    gestures::install_resize_listener(vp_h);
    // `home` goes in as the signal, not its current value: an accepted page can
    // move the home camera down (a taller stub cascade), and the `0` key must
    // land on wherever it is *now* — same rule as the Reset-view button.
    gestures::install_key_listener(camera, home, graph, overlays);
    let visible = create_memo(move |_| {
        visible_row_range(camera.get(), vp_h.get(), row_count.get(), OVERSCAN_ROWS)
    });

    // Is this canvas still mounted? A page request outlives its canvas whenever
    // the user refreshes, switches repo, or history drifts — and the reply then
    // lands on a `StoredValue` that has already been disposed. `try_*` alone
    // isn't enough: Leptos can hand a *reused* slot back, so a stale reply could
    // in principle be applied to somebody else's aggregate. A plain `Rc<Cell>`
    // flipped in `on_cleanup` is the unambiguous answer, and it is checked first.
    let alive = Rc::new(Cell::new(true));
    on_cleanup({
        let alive = Rc::clone(&alive);
        move || alive.set(false)
    });

    // The append loop. A pure threshold (`should_prefetch`) decides *whether* to
    // ask; `page_load` makes it single-flight — `Loading` suppresses the next
    // firing until the reply lands, and `Error` suppresses it until the user
    // explicitly retries. There is deliberately no timer and no automatic retry:
    // a failing cursor hammered on a loop is how a graph the user is reading
    // turns into a request storm.
    create_effect(move |_| {
        let (_, visible_end) = visible.get();
        let rows = row_count.get();
        let viewport_h = vp_h.get();
        let scale = camera.get().scale;
        let load = page_load.get();
        // The aggregate's own answer to "is there more?" — read untracked, since
        // it changes only as part of an append we are about to publish anyway.
        let has_cursor = ctx
            .try_with_value(|c| c.loaded.cursor.is_some())
            .unwrap_or(false);
        if !should_prefetch(visible_end, rows, viewport_h, scale, &load, has_cursor) {
            return;
        }

        // Everything identifying this request is read *before* the await, out of
        // the owner it will be applied to: the epoch this canvas was mounted for,
        // the generation the aggregate is pinned to, the cursor being spent, and
        // the Frame's worktree selector — so a server whose default selection
        // changes mid-scroll can't splice another repository's rows on.
        let Some(Some((request_key, worktree_id))) = ctx.try_with_value(|c| {
            c.loaded.cursor.clone().map(|cursor| {
                (
                    PageRequestKey {
                        epoch: c.epoch,
                        generation: c.loaded.generation.clone(),
                        cursor,
                    },
                    c.frame.worktree_id.clone(),
                )
            })
        }) else {
            return;
        };
        page_load.set(PageLoadState::Loading {
            cursor: request_key.cursor.clone(),
        });

        let alive = Rc::clone(&alive);
        spawn_local(async move {
            let fetched = fetch_page(
                worktree_id.as_deref(),
                Some(&request_key.cursor),
                DEFAULT_PAGE_LIMIT,
            )
            .await;

            // (1) The canvas is gone. Nothing below may run: the signals belong
            // to a disposed reactive scope and the aggregate has been dropped.
            if !alive.get() {
                return;
            }
            // (2) Is this reply still the live view's? All three parts of the key
            // must still match — the epoch (no Refresh since), the generation (no
            // drift), and the cursor (the aggregate hasn't advanced past it).
            // (3) A retired reply is dropped *silently*: writing so much as
            // `page_load` here would stamp one view's request state onto another.
            if ctx.try_with_value(|c| {
                request_key.is_current(
                    graph.get_untracked().epoch(),
                    &c.loaded.generation,
                    c.loaded.cursor.as_deref(),
                )
            }) != Some(true)
            {
                return;
            }

            let page = match fetched {
                Ok(page) => page,
                // History moved under this canvas. Announce the drift *before*
                // bumping the epoch: the App's reload effect refuses to overwrite
                // `DriftReloading` for the same epoch, so setting them the other
                // way round would replace the copy explaining why the graph
                // vanished with a bare "Loading…". Print can't span two
                // generations, so it closes with the epoch it was opened over.
                // The canvas is unmounted by the phase branch that follows, which
                // is what disposes this aggregate — never a manual `dispose()`.
                Err(HistoryFetchError::Http {
                    status: HTTP_CONFLICT,
                    ..
                }) => {
                    let next = graph.update(|g| g.force_bump());
                    history_ui
                        .phase
                        .set(HistoryPhase::DriftReloading { epoch: next });
                    history_ui.print_open.set(false);
                    history_ui.complete.set(false);
                    return;
                }
                Err(err) => {
                    // A cursor the server *rejected* can never be spent again, so
                    // the only honest recovery is a reseed from page 1; anything
                    // else (network, 5xx, a garbled body) left the cursor valid
                    // and may be re-asked with it. Either way the user asks.
                    let retry = match &err {
                        HistoryFetchError::Http {
                            status: HTTP_BAD_REQUEST,
                            ..
                        } => PageRetry::Reseed,
                        _ => PageRetry::SameCursor,
                    };
                    page_load.set(PageLoadState::Error {
                        cursor: request_key.cursor.clone(),
                        message: err.to_string(),
                        retry,
                    });
                    return;
                }
            };

            // The one mutation. `try_update_value` because the owner can still be
            // disposed between the check above and here; `Some(Ok(..))` is the
            // *only* outcome that may publish anything, and it means this
            // canvas's own aggregate accepted the page whole.
            match ctx.try_update_value(|c| c.loaded.append_page(&request_key.cursor, page)) {
                Some(Ok(delta)) => {
                    let (rows, complete) =
                        ctx.with_value(|c| (c.loaded.rows.len(), c.loaded.is_complete()));
                    row_count.set(rows);
                    history_ui.complete.set(complete);
                    // Only rows that existed before this page can have been
                    // re-keyed; a straight append leaves their labels exactly
                    // where they were, so the rebuild is bought only when a
                    // later page actually widened the lanes above them.
                    if delta.prefix_geometry_changed {
                        layout_epoch.update(|n| *n = n.wrapping_add(1));
                    }
                    // A raised lane high-water shifts every resolved stub right
                    // and its cascade up, so the two eager stub layers repaint
                    // and the home camera is re-derived in the same pass.
                    if delta.stub_geometry_changed {
                        stub_epoch.update(|n| *n = n.wrapping_add(1));
                        let headroom = ctx.with_value(|c| {
                            stub_headroom_for(
                                c.loaded
                                    .resolved_stubs()
                                    .into_iter()
                                    .map(|s| (s.anchor_row, s.stub.depth)),
                            )
                        });
                        home.set(Camera::home(headroom));
                    }
                    // Back to Idle re-arms the threshold effect for the next page.
                    // An accepted page is *not* a new epoch: bumping `reload` here
                    // would throw away the very rows just appended.
                    page_load.set(PageLoadState::Idle);
                }
                // The aggregate refused the page, and refused it whole — it is
                // byte-for-byte what it was, so the same cursor is still the
                // right thing to ask for, once the user says so.
                Some(Err(err)) => page_load.set(PageLoadState::Error {
                    cursor: request_key.cursor.clone(),
                    message: err.to_string(),
                    retry: PageRetry::SameCursor,
                }),
                // Disposed between the freshness check and the update: there is
                // no view left for a signal to reach.
                None => {}
            }
        });
    });

    // The Retry affordance's handler — the *only* way out of `Error`, since
    // nothing retries on its own.
    let retry_page = move |_| {
        let Some(retry) = page_load.with_untracked(|state| match state {
            PageLoadState::Error { retry, .. } => Some(retry.clone()),
            _ => None,
        }) else {
            return;
        };
        match retry {
            // Recoverable: hand the same cursor back to the threshold effect,
            // which re-fires immediately if the camera still wants more rows.
            PageRetry::SameCursor => page_load.set(PageLoadState::Idle),
            // The server rejected this cursor, so there is nothing to re-ask —
            // only a fresh epoch from page 1. `page_load` deliberately stays in
            // `Error`: returning it to `Idle` would let the threshold effect
            // spend the rejected cursor again before the replacement mounts.
            PageRetry::Reseed => {
                let next = graph.update(|g| g.force_bump());
                history_ui
                    .phase
                    .set(HistoryPhase::SeedLoading { epoch: next });
            }
        }
    };

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
                //
                // The edge set also reads `row_count` (M1.10, #63): a page can
                // deliver an edge that *starts* in rows already on screen and ends
                // far below, so the visible-edge set can change while the visible
                // *row* window doesn't — and the memo alone would never re-run.
                <For
                    each=move || {
                        row_count.get();
                        render::visible_edges(ctx, visible.get())
                    }
                    key=|ei| *ei
                    children=move |ei| render::build_edge(ctx, ei)
                />
                // Every row tier is keyed on its layout epoch as well as its index,
                // so a later page that pushes an already-drawn row's label right
                // rebuilds that row rather than leaving it at its old x.
                <For
                    each=move || {
                        let (s, e) = visible.get();
                        let le = layout_epoch.get();
                        (s..e).map(|i| (i, le)).collect::<Vec<_>>()
                    }
                    key=|k| *k
                    children=move |(i, _)| render::build_node(ctx, menu, moved, i)
                />
                // Phase 9 (level of detail): the two label tiers, each hidden as the
                // graph is zoomed out. The message tier (badges + message) drops
                // below MESSAGE_SCALE; the dimmed meta line drops below FULL_SCALE,
                // so it's shown only at the closest zoom. Hidden via `.lod-hidden`
                // (display:none), keeping the node/edge structure readable when the
                // text would just be an unreadable smear. Rows are keyed on
                // (index, icon mode, layout epoch): the glyphs and the label x are
                // read untracked inside the builders, so flipping the "Icons"
                // toggle — or widening the lanes — changes every key and rebuilds
                // the visible rows against the new value.
                <g class:lod-hidden=move || !detail_for(camera.get().scale).shows_message()>
                    <For
                        each=move || {
                            let (s, e) = visible.get();
                            let nerd = nerd_icons.get();
                            let le = layout_epoch.get();
                            (s..e).map(|i| (i, nerd, le)).collect::<Vec<_>>()
                        }
                        key=|k| *k
                        children=move |(i, _, _)| render::build_msg(ctx, nerd_icons, moved, i)
                    />
                </g>
                <g class:lod-hidden=move || !detail_for(camera.get().scale).shows_meta()>
                    <For
                        each=move || {
                            let (s, e) = visible.get();
                            let nerd = nerd_icons.get();
                            let le = layout_epoch.get();
                            (s..e).map(|i| (i, nerd, le)).collect::<Vec<_>>()
                        }
                        key=|k| *k
                        children=move |(i, _, _)| render::build_meta(ctx, nerd_icons, i)
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
                            let le = layout_epoch.get();
                            (s..e).map(|i| (i, nerd, le)).collect::<Vec<_>>()
                        }
                        key=|k| *k
                        children=move |(i, _, _)| render::build_node_icon(ctx, nerd_icons, i)
                    />
                    // The two stub layers stay eager (there are only a handful, and
                    // their cascade fans *upward* off the anchor, so they don't map
                    // onto the row window) — but they are no longer static: reading
                    // `stub_epoch` here is what repaints them at their new columns
                    // when a page raises the lane high-water.
                    {move || { stub_epoch.get(); render::stub_icons(ctx, nerd_icons) }}
                </g>
                {move || { stub_epoch.get(); render::stubs(ctx, menu, moved) }}
            </g>
        </svg>
        // The paging affordance (M1.10, #63). A sibling of the `<svg>`, never a
        // child of the camera's `<g transform=…>`: it has to stay put and stay
        // visible however far the user has panned or zoomed away from the rows
        // it is talking about. It never replaces the canvas — the graph on
        // screen stays readable while the next page loads or fails.
        {move || {
            let state = page_load.get();
            if show_fixed_loading_overlay(&state) {
                return view! {
                    <div class="history-page-status">"Loading more history…"</div>
                }
                .into_view();
            }
            match state {
                // The server's own words (or the aggregate's) are kept in the
                // tooltip; the line itself says the one thing that matters and
                // offers the only way forward, which is always explicit.
                PageLoadState::Error { message, .. } => view! {
                    <div class="history-page-status error" title=message>
                        "Couldn't load more history."
                        <button class="refresh" on:click=retry_page>"Retry"</button>
                    </div>
                }
                .into_view(),
                _ => ().into_view(),
            }
        }}
        // Phase 13: a floating "Reset view" control. Pan/zoom can carry the graph
        // off-screen with no obvious way back — easy to do with the iPad trackpad or
        // a pinch — so this recenters the camera (same as the `0` key) with one tap,
        // which is the only way back for anyone driving purely by touch/trackpad.
        // Read at activation time, not at mount: a page that grows the stub
        // cascade moves home, and resetting to the old one would clip it.
        <button
            class="reset-view"
            title="Reset pan & zoom (keyboard: 0)"
            on:click=move |_| camera.set(home.get_untracked())
        >
            "Reset view"
        </button>
        // "Print Graph" (crate::print), mounted here rather than in the App
        // shell since M1.10 (#63): it draws the *whole* history, which now only
        // exists inside this canvas's aggregate. Mounting it here means it can
        // borrow `ctx` instead of being handed a copy that a later page would
        // silently contradict — and that it is disposed together with the epoch
        // it belongs to. Its own overlay is `position: fixed`, so it adds no
        // layout to the canvas it sits beside. The topbar button that sets
        // `print_open` is disabled until the last page lands.
        {print_graph_view(ctx, history_ui.print_open, nerd_icons)}
        // The overlays: the context menu, the two modals, and the detail panel.
        // They're mutually exclusive (opening either modal closes the menu), and
        // each is `position: fixed`, so this wrapper adds no layout. Each view is a
        // reactive closure that renders only when its signal is set.
        <div class="overlays">
            {menu::menu_view(overlays, settings, read_only)}
            {dialogs::commit_dialog_view(overlays)}
            {dialogs::confirm_modal_view(overlays)}
            {detail::detail_panel_view(overlays, settings, detail, ctx)}
            {activity::activity_panel_view(overlays, settings, read_only)}
            {viewer::viewer_view(overlays, settings)}
        </div>
    }
}
