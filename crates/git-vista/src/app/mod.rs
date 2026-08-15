//! The top-level Leptos components for git-vista.
//!
//! Phase 4 fetched one whole laid-out `Graph`; M1.10 (#63) replaced that with
//! *paged* history — a cheap once-per-view [`Frame`] (refs, colours, repo
//! metadata) plus cursor-paginated pages of rows/edges/stubs, assembled by
//! [`LoadedHistory`](crate::features::graph::core::LoadedHistory). So the App no longer holds a graph: it
//! holds a **seed** (Frame + page 1) and an explicit [`HistoryPhase`], and every
//! repo-metadata consumer reads the Frame rather than a row payload.
//!
//! The phase exists because a Leptos resource keeps serving its previous value
//! while the next one loads. Rendering off `seed.get()` alone would let the
//! *old* history stay mounted across a reload — and, worse, mask the drift
//! notice after a `409`. So the phase branch is consulted first and the resource
//! second, and every result carries the reload epoch it was fetched for.
//!
//! This module is the two top-level pieces — the [`App`] shell (topbar, toggles,
//! status line, the seed fetch) here, and
//! [`graph_canvas`](canvas::graph_canvas) in [`canvas`], which wires the shared
//! signals into the SVG. The heavy lifting lives in focused sibling modules: the
//! HTTP calls in [`crate::api`], persisted toggles in [`crate::prefs`], the
//! shared types and signal bundles in [`crate::state`], the SVG builders in
//! [`crate::render`], pan/zoom in [`crate::gestures`], and the overlays in
//! [`crate::menu`] / [`crate::dialogs`] / [`crate::detail`]. Spatial math is in
//! [`crate::geometry`], the lane palette in [`git_vista_core::color`].
//!
//! [`Camera`]: crate::camera::Camera

use std::fmt;

use leptos::*;

use git_vista_protocol::{check_compatibility, PROTOCOL_VERSION};

use crate::api::{fetch_frame, fetch_page, fetch_protocol, HistoryFetchError};
use crate::datetime;
use crate::dialogs;
use crate::features::a11y::core::GRAPH_REGION_LABEL;
use crate::features::activity::signals::Activity;
use crate::features::dialogs::core::Dialog;
use crate::features::dialogs::signals::Dialogs;
use crate::features::graph::core::{
    print_button_copy, Frame, GraphCore, HistoryInvariantError, LoadedHistory, DEFAULT_PAGE_LIMIT,
};
use crate::features::operations::core::OperationsCore;
use crate::features::operations::signals::Operations;
use crate::features::operations::view::operations_status_view;
use crate::features::session::core::seed_retry_attempts_for;
use crate::features::session::core::seed_retry_delay_ms;
use crate::features::session::core::session_retry_delay_ms;
use crate::features::session::core::SessionEvent;
use crate::features::session::signals as session_state;
use crate::features::shell::signals::{
    install_connectivity_signal, install_mode_signal, SheetController, Shell,
};
use crate::features::status::core as status_core;
use crate::features::status::signals as status_seam;
use crate::hook_policy_banner::hook_policy_banner_view;
use crate::icons::icon_set;
use crate::prefs::{
    load_collapse_wip_pref, load_icon_pref, load_node_icons_pref, store_collapse_wip_pref,
    store_icon_pref, store_node_icons_pref,
};
use crate::session::{establish_session, not_connected_view, recheck_session};
use crate::state::{Features, Settings};
use crate::update_required::update_required_view;

mod canvas;

use canvas::graph_canvas;

/// Frame + page 1 for one reload epoch: everything needed to mount a graph.
///
/// The `epoch` is what makes a late reply safe to drop. A seed fetched before a
/// Refresh (or before a drift reload) names the epoch it was fetched *for*, so
/// it can be recognised as stale instead of being mounted over the live one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySeed {
    pub epoch: u64,
    pub frame: Frame,
    pub loaded: LoadedHistory,
}

/// Why a seed didn't land: the HTTP hop, or the aggregate refusing page 1.
///
/// Both halves carry the underlying error rather than a rendered string, so the
/// status path can still tell drift from a decode failure, and so the message
/// the user sees is the specific one — never an enum name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HistorySeedError {
    Fetch(HistoryFetchError),
    Invariant(HistoryInvariantError),
}

impl fmt::Display for HistorySeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch(e) => write!(f, "{e}"),
            Self::Invariant(e) => write!(f, "{e}"),
        }
    }
}

/// What the graph panel is doing, independent of what the seed resource happens
/// to be holding. Each variant carries the reload epoch it belongs to, so a
/// reply for an earlier epoch can never advance the phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryPhase {
    SeedLoading { epoch: u64 },
    Ready { epoch: u64 },
    DriftReloading { epoch: u64 },
    SeedError { epoch: u64 },
}

/// The history signals the App owns and the canvas drives: which phase the
/// panel is in, whether the whole history is loaded (Print needs all of it),
/// whether the print overlay is open (any epoch change closes it), and whether
/// the append loop should chase completeness in the background regardless of
/// scroll position.
#[derive(Clone, Copy)]
pub struct HistoryUiSignals {
    pub phase: RwSignal<HistoryPhase>,
    pub complete: RwSignal<bool>,
    pub print_open: RwSignal<bool>,
    /// #382: how many WIP runs the current projection holds, published UP by
    /// the canvas so the topbar can say so.
    ///
    /// Without it the topbar can only report the toggle's position, and a
    /// graph whose runs sit thirty commits below the viewport is
    /// indistinguishable from a graph with none — which is exactly how a
    /// working feature got reported as broken. Zero is informative too: it
    /// means this repository genuinely has no runs, not that they are hiding.
    pub wip_runs: RwSignal<usize>,
    /// #217: the `worktree_id` of the repository whose history the user has
    /// already driven to completeness — `None` until that first happens.
    /// Unlike `complete` and `print_open` it is deliberately left out of the
    /// epoch-reset effect below, so it survives every later
    /// Refresh/settle/drift bump. An epoch bump always remounts the canvas
    /// from page 1 (`seed_for_epoch`), which genuinely does drop completeness
    /// — pages are pinned to a generation, so the old ones can't just be kept
    /// — but a user who has already scrolled (or waited) their way to a
    /// complete history once has demonstrated they want the whole thing. This
    /// is what lets the new epoch's append loop (`canvas.rs`) resume
    /// pagination toward completion on its own — `should_prefetch`'s `eager`
    /// parameter — instead of leaving Print Graph dark until the user
    /// manually re-scrolls through however many pages the repository has,
    /// with the camera reset to the top by the very same remount.
    ///
    /// **An id, not a bool** (review finding): `graph.force_bump()` is the
    /// same primitive behind a mere reload *and* behind switching to a
    /// genuinely different repository (`picker.rs`'s select, `open_url.rs`'s
    /// clone-settle). A tab-lifetime bool therefore leaked: complete one
    /// repo's history, then open an unrelated — possibly enormous — repo, and
    /// its canvas would mount with `eager` already true and silently
    /// paginate that repo's entire history with no user signal at all. That
    /// is the same class of unexplained behavior #217 exists to remove, just
    /// pointed the other way. Storing *which* repo earned the latch makes the
    /// comparison at the point of use (`canvas.rs`) authoritative, rather
    /// than depending on someone remembering to clear a bool at every present
    /// and future bump site.
    pub want_full_history: RwSignal<Option<String>>,
}

/// Fetch one epoch's seed, tagged with the epoch it was fetched for.
///
/// The tag rides on the *result*, not only on the success value: a failure has
/// to be attributable to an epoch too, or a slow error from a retired epoch
/// would raise [`HistoryPhase::SeedError`] over an epoch that is still loading.
async fn load_seed(epoch: u64) -> (u64, Result<HistorySeed, HistorySeedError>) {
    (epoch, seed_for_epoch(epoch).await)
}

/// Frame, then page 1 pinned to that Frame's worktree, then the aggregate.
///
/// The generation check is the client's half of the drift contract: the Frame
/// and the page are two separate reads, so a commit landing between them would
/// otherwise splice rows onto refs that no longer describe them. Checking here
/// means [`LoadedHistory::from_first_page`] can treat its own generation check
/// as the tautology it is.
async fn seed_for_epoch(epoch: u64) -> Result<HistorySeed, HistorySeedError> {
    let frame = fetch_frame().await.map_err(HistorySeedError::Fetch)?;
    let page = fetch_page(frame.worktree_id.as_deref(), None, DEFAULT_PAGE_LIMIT)
        .await
        .map_err(HistorySeedError::Fetch)?;
    if frame.generation != page.generation {
        return Err(HistorySeedError::Invariant(
            HistoryInvariantError::GenerationMismatch {
                expected: frame.generation.clone(),
                actual: page.generation,
            },
        ));
    }
    let loaded = LoadedHistory::from_first_page(page).map_err(HistorySeedError::Invariant)?;
    Ok(HistorySeed {
        epoch,
        frame,
        loaded,
    })
}

#[component]
pub fn App() -> impl IntoView {
    // The graph epoch (M1.11, #64): `GraphCore` replaces the bare `reload: RwSignal<u32>`
    // counter every writer used to bump unconditionally. The frontend used to fetch
    // exactly once on load (`|| ()`, a constant source that never re-fires), so a
    // branch or commit created *after* the page loaded never appeared until a full
    // reload — the heart of issue #16; a bump counter fixed that, but bumped after
    // EVERY write regardless of whether the repository actually moved. `GraphCore`
    // makes that a tested decision (design spec D3): a settled operation reports its
    // post-execution generation, and `on_invalidate` skips the bump when nothing
    // moved. Explicit actions — Refresh, the "R" key, a 409 drift, session landing —
    // have no generation to compare and call `force_bump` instead, exactly
    // reproducing the old unconditional behaviour for those paths.
    let graph = create_rw_signal(GraphCore::default());
    let refresh = move |_| {
        graph.update(|g| {
            g.force_bump();
        });
    };

    // The write registry (M1.11, #64). Deliberately created HERE and not inside
    // `graph_canvas`: an epoch bump rebuilds the canvas and every overlay in it, so an
    // operation living down there would be destroyed by the very re-read its own
    // completion triggers. Owning it in the shell is what lets a write survive a panel
    // change (acceptance criterion 2).
    let operations_core = create_rw_signal(OperationsCore::default());
    let operations = Operations::new(operations_core, graph);
    // #232, M2.20f: resume watching a Fetch/Pull that was still in flight
    // when this tab reloaded or was suspended and resumed — before
    // anything else reads `operations_core`, so `menu.rs`'s
    // `remote_op_running` gate (which disables Fetch/Pull, with a reason,
    // while either is in flight) sees the resumed entry from the first
    // render, rather than a render-or-two where the resumed op is real but
    // the menu still offers a second one.
    operations.resume_from_storage();

    // The history signals the App owns (M1.10, #63). `print_graph_open` opens the
    // full static print view (crate::print) from the topbar; `history_complete`
    // says whether every page has been loaded, which is what Print requires; the
    // phase is what the graph panel actually renders off.
    let history_phase = create_rw_signal(HistoryPhase::SeedLoading { epoch: 0 });
    let history_complete = create_rw_signal(false);
    let print_graph_open = create_rw_signal(false);
    // #217: `None` until some repository's history is first driven to
    // completeness, then that repository's own `worktree_id` — see the field
    // doc on `HistoryUiSignals` for why an id and not a bool.
    let want_full_history = create_rw_signal(None::<String>);
    let wip_runs = create_rw_signal(0usize);
    let history_ui = HistoryUiSignals {
        phase: history_phase,
        complete: history_complete,
        print_open: print_graph_open,
        want_full_history,
        wip_runs,
    };

    // Frame + page 1, keyed on the epoch. `create_local_resource` because the
    // fetch future isn't `Send` (wasm).
    let seed = create_local_resource(move || graph.get().epoch(), load_seed);

    // Every epoch — Refresh, a post-operation reload, a drift reload — retires
    // the mounted history. Print can't span two generations and "complete" must
    // not survive into a graph that hasn't loaded a page yet, so both are reset
    // here rather than at each of the several places that bump `reload`.
    //
    // #217: `want_full_history` is deliberately NOT reset here, unlike
    // `complete` and `print_open`. It is the one history flag meant to survive
    // the epoch churn, so the new epoch's append loop knows to chase
    // completeness in the background (`should_prefetch`'s `eager`) instead of
    // leaving Print Graph disabled until the user re-scrolls through however
    // much history the repository has.
    create_effect(move |_| {
        let epoch = graph.get().epoch();
        history_ui.print_open.set(false);
        history_ui.complete.set(false);
        // A drift reload has already announced itself with the epoch it is
        // reloading *into*; overwriting that with SeedLoading would drop the
        // "History moved" copy that explains why the graph vanished.
        if history_ui.phase.get_untracked() != (HistoryPhase::DriftReloading { epoch }) {
            history_ui.phase.set(HistoryPhase::SeedLoading { epoch });
        }
    });

    // Promote a seed to its phase — but only the *current* epoch's. The resource
    // keeps its previous value while the next load runs, and an out-of-order
    // completion would otherwise mark a live reload Ready with retired data.
    create_effect(move |_| {
        let Some((epoch, complete, worktree)) = seed.map(|(epoch, result)| {
            (
                *epoch,
                result.as_ref().ok().map(|s| s.loaded.is_complete()),
                // #217: pulled from the same seed read, so the latch below can
                // only ever name the repository this very seed belongs to.
                result
                    .as_ref()
                    .ok()
                    .and_then(|s| s.frame.worktree_id.clone()),
            )
        }) else {
            return;
        };
        if epoch != graph.get_untracked().epoch() {
            return;
        }
        match complete {
            Some(complete) => {
                history_ui.complete.set(complete);
                // #217: a repository small enough to be complete on page 1
                // still counts as "the user has a complete history" — latch
                // `want_full_history` here too, not only in the append loop's
                // multi-page case in `canvas.rs`. Latched to *this* seed's own
                // worktree, so it can never speak for a different repository.
                if complete {
                    history_ui.want_full_history.set(worktree.clone());
                }
                history_ui.phase.set(HistoryPhase::Ready { epoch });
            }
            None => history_ui.phase.set(HistoryPhase::SeedError { epoch }),
        }
    });

    // #218 residual gap: `HistoryPhase::SeedError` used to have no automatic
    // self-heal at all. `seed_for_epoch` makes two sequential `send_read`-backed
    // calls, each already good for one immediate retry (api.rs's `send_read`)
    // bounded by `REQUEST_TIMEOUT_MS`; if a flaky tunnel outlasts both,
    // `seed_for_epoch` returns `Err`, the promotion effect above sets
    // `SeedError`, and nothing retried it — the user was stuck on the single
    // status line above until clicking Refresh (which just calls the same
    // `force_bump` this effect now schedules automatically, with backoff).
    //
    // `(u64, u32)` = (the epoch this chain's own last `force_bump` produced,
    // attempts spent so far). Deliberately a *second* signal, not folded into
    // the epoch-reset effect a few lines up: that effect fires on every epoch
    // change, including the ones this very mechanism causes, so resetting the
    // counter there would zero the budget on every retry and make it
    // unbounded. `seed_retry_attempts_for` carries the reasoning for why
    // "per-epoch" has to mean "per failure chain" instead — read its doc
    // comment before touching this.
    let seed_retry = create_rw_signal((0u64, 0u32));
    create_effect(move |_| {
        let HistoryPhase::SeedError { epoch } = history_ui.phase.get() else {
            return;
        };
        let (expected_epoch, attempts_used) = seed_retry.get_untracked();
        let attempts_used = seed_retry_attempts_for(expected_epoch, epoch, attempts_used);
        let Some(delay) = seed_retry_delay_ms(attempts_used) else {
            return; // Budget spent for this chain: stop, leave the error visible.
        };
        let next_attempt = attempts_used + 1;
        set_timeout(
            move || {
                // Stale-timer guard: if the user already manually refreshed
                // (or a drift reload etc. superseded this failure) since the
                // timer was armed, the panel is no longer showing the epoch
                // this retry was for — firing anyway would race a reload
                // that's already in flight, so skip it.
                if history_ui.phase.get_untracked() != (HistoryPhase::SeedError { epoch }) {
                    return;
                }
                let new_epoch = graph.try_update(|g| g.force_bump()).unwrap_or_default();
                seed_retry.set((new_epoch, next_attempt));
            },
            std::time::Duration::from_millis(delay as u64),
        );
    });

    // The last accepted Frame — the single source of repo metadata now that the
    // paged rows carry none. Deliberately *not* epoch-checked, unlike the graph
    // panel below: the topbar keeps naming the repo it last identified while the
    // next epoch loads, exactly as the retained whole `Graph` used to, so a
    // Refresh doesn't blink every repo-scoped control out of existence.
    let frame = move || {
        seed.map(|(_, result)| result.as_ref().ok().map(|s| s.frame.clone()))
            .flatten()
    };

    // M1.04 (#57): establish the loopback session before the API is usable. Run
    // once on load (source `|| ()`, not keyed on `reload`, so re-reads don't
    // re-bootstrap): it exchanges a `#s=<token>` fragment for a session cookie, or
    // checks an existing one. `Some(Ok(false))` — no session and nothing to make
    // one from — drives the blocking sign-in overlay; a network `Err` falls through
    // to the normal load-error path (an unreachable server isn't a sign-in problem).
    // #218: keyed on an attempt counter rather than `()`, so a transport
    // failure can be retried. Attempt 0 is the real bootstrap (may redeem a
    // `#s=` token); every retry is `recheck_session`, a GET that neither
    // re-spends the single-use token nor consumes the LAN listener's
    // sign-in rate budget — see that function's doc comment.
    let session_attempt = create_rw_signal(0u32);
    let session = create_local_resource(
        move || session_attempt.get(),
        |attempt| async move {
            if attempt == 0 {
                establish_session().await
            } else {
                recheck_session().await
            }
        },
    );
    // #218: nothing in the reactive graph used to react to `Err`, so a
    // transport failure during the first load left the app permanently
    // stuck — every later read 401ing with no cookie ever set, recoverable
    // only by a full browser reload. That matches the reported symptom
    // (history rendering as a single status line until a manual retry).
    // Bounded and backed off; `session_retry_delay_ms` owns the policy and
    // is host-tested.
    create_effect(move |_| {
        if !matches!(session.get(), Some(Err(_))) {
            return;
        }
        let made = session_attempt.get_untracked();
        let Some(delay) = session_retry_delay_ms(made) else {
            return; // Budget spent: stop rather than storm a dead server.
        };
        set_timeout(
            move || session_attempt.update(|a| *a += 1),
            std::time::Duration::from_millis(delay as u64),
        );
    });
    let needs_sign_in = move || matches!(session.get(), Some(Ok(false)));
    // The history/status reads fired at load without a cookie and 401'd; once the
    // session lands, bump `reload` once so they refetch authenticated.
    create_effect(move |_| {
        if matches!(session.get(), Some(Ok(true))) {
            graph.update(|g| {
                g.force_bump();
            });
        }
    });

    // M1.02 (#102): negotiate the protocol before trusting the rest of the API.
    // Keyed on `reload` so every Refresh (and every post-operation reload)
    // re-checks — if the server is redeployed on an incompatible protocol while
    // this tab stays open, the next reload catches it. `protocol_gate` yields the
    // negotiation payload + verdict only when the client is *out* of the server's
    // accepted window; that drives the blocking "Update Required" overlay below.
    let protocol = create_local_resource(move || graph.get().epoch(), |_| fetch_protocol());
    let protocol_gate = move || match protocol.get() {
        Some(Ok(info)) => {
            let verdict = check_compatibility(
                PROTOCOL_VERSION,
                info.min_client_protocol,
                info.max_client_protocol,
            );
            (!verdict.is_compatible()).then_some((info, verdict))
        }
        // Pending, or the negotiation call itself failed (unreachable server):
        // no overlay — the normal load-error path handles an unreachable server.
        _ => None,
    };

    // The app's one iOS ghost-click guard (M1.11, #64). Lives here, not in
    // `graph_canvas`, for two reasons: the topbar's own modals (Open URL, Reset) need it
    // and exist before the graph does, and a canvas rebuilt by an epoch bump would
    // otherwise reset the guard out from under a modal that is still up. Named
    // `dialogs_guard` because the `dialogs` *module* is in scope here too.
    let dialogs_guard = Dialogs::new();

    // #226: keep the commit-draft scope tracking the served repository, so a
    // draft persists per repo and survives an iOS tab suspension (the first
    // Frame after the rebuild restores it). Re-fires on every epoch reload;
    // `set_draft_scope`'s same-repo no-op (host-tested in dialogs/core.rs)
    // is what makes that safe for in-flight typing.
    create_effect(move |_| {
        dialogs_guard.set_draft_scope(frame().and_then(|f| f.worktree_id));
    });

    // The Activity panel's visibility (Activity/Undo feature). Lives here — not in
    // graph_canvas — because its button sits in the topbar, which exists even while the
    // graph is still loading; threaded into the overlays bundle inside graph_canvas.
    // Declared above `status` because that read's key includes it.
    let activity = Activity::new();

    // Every overlay the app can show, and the order they were raised in (M1.11, #64,
    // Task 8). Created here, not in `graph_canvas`, for three reasons: the Activity
    // toggle it owns lives in the topbar, which exists before the graph does; an epoch
    // bump's rebuild would otherwise destroy the six overlay signals mid-interaction; and
    // this is where Task 6's deferred "move the overlay signals out of canvas scope" step
    // actually lands, rather than migrating them twice.
    let shell = Shell::new(activity);

    // #232 follow-up: give `operations` the handles it needs to put its own
    // failures on screen. It is created ~200 lines above this, deliberately —
    // above `graph_canvas`, so an in-flight write survives the epoch bump its
    // completion causes (M1.11) — but `Shell` and `Dialogs` cannot exist that
    // early, so the wiring is a second step rather than a constructor argument.
    //
    // Without this line every refusal `Operations::cancel` receives (offline,
    // an evicted id, either 409, a dropped tunnel) reaches the browser console
    // and nobody else: the user taps Cancel on a stalled fetch and gets
    // silence. The review that caught it found the sink installed nowhere at
    // all — a finished feature shipped inert because its one wiring line fell
    // between two work boundaries, and no test could see it.
    operations.install_error_sink(shell, dialogs_guard);

    // The window's current layout mode (M1.12, #65). Created here, not in
    // graph_canvas, for the same reason `shell` is: an epoch bump's rebuild must
    // not tear down and reinstall the resize listener mid-session.
    let mode = install_mode_signal();
    let sheet = SheetController::new(mode.get_untracked());
    create_effect(move |_| sheet.on_mode_change(mode.get()));
    create_effect(move |_| {
        if shell.detail_id().is_none() {
            sheet.cancel_drag();
        }
    });

    // The browser's own connectivity report (M2.22a, #241). Installed here, not
    // in `graph_canvas`, for the same reason `mode` is: an epoch bump's rebuild
    // must not tear down and reinstall the online/offline listeners mid-session.
    // `api.rs`'s `refuse_if_offline()` reads the plain accessor this seeds
    // (`shell::signals::is_online`), not this signal — the signal drives
    // M2.22b's UI (#242): the offline banner below, and the write controls the
    // menu/picker/Activity views gate through `shell_state::online_signal()`.
    let online = install_connectivity_signal();

    // The topbar chip's status summary (Activity/Undo step 1), plus the shared
    // refetch trigger several write handlers pull after a mutation. Until M1.11
    // (#64, Task 7) the Activity panel kept a second, independently-fetched copy
    // of this same v1 `RepoStatus` read for its own status *section*; M2.15
    // (#68) gave that section its own v2 `WorktreeStatus` read instead (grouped
    // sections, accessible labels — `activity.rs`'s `worktree_status` resource),
    // so the panel's rendering no longer depends on this one. This resource's
    // job is now just the chip and the `.refetch()` calls, not panel rendering.
    let status = status_seam::create(graph, activity);

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

    // Whether runs of auto-checkpoint commits fold into one node (#374).
    // Default on: the checkpointer commits every 30s during a session, so
    // real commits end up buried under dozens of near-identical dots
    // otherwise. Persisted like the icon prefs above.
    let collapse_wip = create_rw_signal(load_collapse_wip_pref());
    let toggle_collapse_wip = move |_| {
        let on = !collapse_wip.get_untracked();
        collapse_wip.set(on);
        store_collapse_wip_pref(on);
    };

    // The two bundles `graph_canvas` takes (see `crate::state`). `features` is every
    // handle created here, above the canvas, precisely so an epoch bump's rebuild cannot
    // drop it; `settings` is the display preferences every icon-drawing view reads.
    let features = Features {
        graph,
        dialogs: dialogs_guard,
        operations,
        status,
        shell,
    };
    let settings = Settings {
        nerd_icons,
        show_node_icons,
        collapse_wip,
    };

    // Phase 12 — "Open URL": clone a public repo and view it read-only. `open_url`
    // toggles the modal; `clone_url` holds the field; `cloning` disables the button
    // while git works so a slow clone can't be fired twice. The shared `dialogs`
    // guard protects the backdrop from the iOS ghost-click, same as every other modal.
    // The modal itself lives in `dialogs::open_url_view`.
    let open_url = create_rw_signal(false);
    let clone_url = create_rw_signal(String::new());
    let cloning = create_rw_signal(false);
    // #278: true only while `clone_request` has fallen back to polling
    // `GET /api/clone-status/{key}` after a lost/timed-out/"already in
    // progress" `POST /api/clone` response — see `dialogs::open_url_view`'s
    // doc comment. `cloning` alone still governs the dismiss pin.
    let checking_clone_status = create_rw_signal(false);

    // "Reset Test Repo" (iPad-testing follow-up): the button appears only when
    // the Frame says this repo carries a seed (`gv --seed`); the confirm modal
    // lives in `dialogs::reset_repo_view`, owned here like the Open-URL one
    // because its button sits in the topbar, not the graph canvas.
    let reset_open = create_rw_signal(false);

    // ADR 0006: ask every time — the repo picker opens on load (the sign-in and
    // protocol overlays sit above it when they apply) and from the topbar
    // "Repos" button. `mode_for` holds the repo awaiting a Visualize/Active
    // choice; the mode screen renders whenever it's Some.
    let picker_open = create_rw_signal(true);
    let mode_for = create_rw_signal(None::<git_vista_protocol::RepositoryDescriptor>);
    // ADR 0005: a LAN-view session can't select a repo or mode, so the
    // ask-every-time picker would only dead-end there — close it once the
    // session lands and show the served repo's graph straight away.
    create_effect(move |_| {
        if matches!(session.get(), Some(Ok(true))) && session_state::is_lan() {
            picker_open.set(false);
        }
    });

    // Defense in depth (ADR 0007): mirror the Frame's mode into the session core so
    // write calls refuse client-side too. The server's 403 remains the boundary.
    // `Observed`, not `Selected`: this is the server's report, not a user choice, so a
    // LAN session must record it rather than refuse it (M1.11, #64).
    create_effect(move |_| {
        if let Some(f) = frame() {
            let _ = session_state::apply(SessionEvent::UiModeObserved(Some(if f.read_only {
                git_vista_protocol::RepoMode::Visualize
            } else {
                git_vista_protocol::RepoMode::Active
            })));
        }
    });

    view! {
        <main
            class=move || {
                let mut classes = format!("app {}", mode.get().css_class());
                if sheet.placement().is_sheet() {
                    classes.push_str(" inspector-sheet");
                }
                if sheet.is_dragging() {
                    classes.push_str(" sheet-dragging");
                }
                classes
            }
            style=move || match sheet.render_metrics() {
                Some(metrics) => format!(
                    "--sheet-full-height:{}dvh;--sheet-rest-offset:{}dvh;--sheet-drag-offset:{}px",
                    metrics.full_height_vh,
                    metrics.rest_offset_vh,
                    sheet.drag_offset_px(),
                ),
                None => String::new(),
            }
        >
            // M1.02: the blocking "Update Required" screen, shown (over everything
            // else) only when this client's protocol is incompatible with the server.
            {move || protocol_gate().map(|(info, verdict)| update_required_view(info, verdict))}
            // M1.04: the blocking sign-in screen, shown when there's no session and
            // no bootstrap token to make one — the operator must open `gv`'s link.
            {move || needs_sign_in().then(not_connected_view)}
            // M1.13a (#66, ADR 0025): the persistent hook-policy disclosure banner.
            // Keyed on `session.get()` purely as the reactive trigger (the same
            // resource `needs_sign_in` reads above) — `hook_policy_banner_visible`
            // itself is a plain, non-reactive read of session_state, matching that
            // module's own documented posture that these per-tab facts are fixed
            // once `establish_session` resolves. Renders nothing until then, so the
            // banner never flashes on with a stale default before the real session
            // state is known.
            {move || {
                session
                    .get()
                    .map(|_| hook_policy_banner_view(
                            session_state::hook_policy_banner_visible(),
                            session_state::hook_policy(),
                        ))
            }}
            // M1.11 (#64): in-flight writes and their outcomes. Mounted in the shell,
            // not the canvas, so it keeps reporting across the epoch bump a completed
            // write triggers — and so a failure is dismissible app state rather than a
            // native alert the app cannot see.
            {operations_status_view(operations)}
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
                    // The label's grouping is decided in one host-tested place
                    // (`features::status::core::chip_label`), shared with the
                    // Activity panel's status sections. Before #348 this arm
                    // folded untracked into "unstaged" while the panel gave it
                    // its own section, so the two disagreed on screen about the
                    // same worktree.
                    let label = status_core::chip_label(
                        s.staged.len(),
                        s.unstaged.len(),
                        s.untracked.len(),
                        s.conflicted.len(),
                    );
                    let (mut class, icon) = if !s.conflicted.is_empty() {
                        ("status-chip conflict".to_string(), ic.conflict)
                    } else if !s.is_clean() {
                        ("status-chip dirty".to_string(), ic.dirty)
                    } else {
                        ("status-chip clean".to_string(), ic.clean)
                    };
                    // How old this reading is — #the-stale-worktree-status-bug:
                    // a reading held in memory since the last fetch looked
                    // pixel-identical whether it was 1 second or 19 hours old.
                    // `scanned_at == 0` means an older server never stamped a
                    // time; that reads as "age unknown", not as a bogus huge
                    // age computed against the unix epoch.
                    let now = (js_sys::Date::now() / 1000.0) as i64;
                    let age = (s.scanned_at > 0).then(|| now - s.scanned_at);
                    let freshness = datetime::freshness_label(age);
                    if datetime::is_stale(age) {
                        class.push_str(" stale");
                    }
                    let mut sync = String::new();
                    if s.ahead > 0 { sync.push_str(&format!(" ↑{}", s.ahead)); }
                    if s.behind > 0 { sync.push_str(&format!(" ↓{}", s.behind)); }
                    let title = format!(
                        "{} staged · {} unstaged · {} untracked · {} conflicted{} · {}",
                        s.staged.len(), s.unstaged.len(), s.untracked.len(),
                        s.conflicted.len(),
                        s.upstream.as_deref()
                            .map(|u| format!(" · vs {u}"))
                            .unwrap_or_default(),
                        freshness,
                    );
                    view! {
                        <span class=class title=title>
                            <span class="nf">{icon}</span>
                            {format!(" {label}{sync}")}
                            <span class="status-age">{format!(" · {freshness}")}</span>
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
                    on:click=toggle_collapse_wip
                    title="Fold runs of auto-checkpoint commits into one node, so real \
                           commits aren't buried under checkpoint noise. Turn off to see \
                           every checkpoint."
                >
                    {move || {
                        // #382: the toggle alone cannot answer "are there runs
                        // here at all", and a graph whose runs sit thirty
                        // commits below the fold looks exactly like a graph
                        // with none — which is how a working feature got
                        // reported as broken. Zero is informative too.
                        let n = wip_runs.get();
                        match (collapse_wip.get(), n) {
                            (true, 0) => "WIP: folded · no runs".to_string(),
                            (true, 1) => "WIP: folded · 1 run".to_string(),
                            (true, n) => format!("WIP: folded · {n} runs"),
                            (false, _) => "WIP: shown".to_string(),
                        }
                    }}
                </button>
                <button
                    class="refresh"
                    on:click=move |_| picker_open.set(true)
                    title="Open another repository — the launch repo, a repo from \
                           the configured root, or a clone"
                >
                    "Repos"
                </button>
                // The mode badge (ADR 0006): which experience the current repo is
                // open in; tapping it re-opens the mode screen for this repo.
                {move || frame().map(|f| {
                    let (label, class) = if f.read_only {
                        ("Visualize", "refresh mode-badge visualize")
                    } else {
                        ("Active", "refresh mode-badge active")
                    };
                    view! {
                        <button
                            class=class
                            title="This repo's mode — tap to change it"
                            on:click=move |_| {
                                // Re-open the mode screen for the current repo by
                                // synthesizing its descriptor from the Frame.
                                // Absent ids (degraded mode) => no mode screen.
                                if let Some(worktree) = f.worktree_id.clone() {
                                    mode_for.set(Some(git_vista_protocol::RepositoryDescriptor {
                                        repository: f.repo_id.clone().unwrap_or_default(),
                                        worktree,
                                        name: f.repo_label.clone()
                                            .unwrap_or_else(|| "repository".into()),
                                        kind: git_vista_protocol::RepositoryKind::MainWorktree,
                                        read_only: f.read_only,
                                        path: None,
                                        remote_web_url: f.remote_web_url.clone(),
                                        // Synthesized from the Frame, which
                                        // carries no hook policy — so this is
                                        // "not disclosed", never a guessed
                                        // value.
                                        //
                                        // #208 made that visible: the mode
                                        // screen now renders this field, so
                                        // re-opening the mode screen from this
                                        // badge says "not disclosed" even for a
                                        // repository whose picker row said
                                        // "sandboxed (strict)". That is the
                                        // truthful reading of this path, which
                                        // genuinely does not know, and it errs
                                        // in the safe direction. Closing the
                                        // gap means carrying the policy on the
                                        // Frame — a protocol change, not a
                                        // client-side guess.
                                        hook_policy: None,
                                    }));
                                }
                            }
                        >
                            {label}
                        </button>
                    }
                })}
                // ADR 0005: no Open URL… on a LAN-view session — `/api/clone`
                // doesn't exist on the LAN listener. Keyed on the session
                // resource so the button re-evaluates once `establish_session`
                // lands (it records via_lan before resolving).
                // M2.22b (#242): hidden offline too — this is the same
                // `/api/clone` entry point as the picker's Clone URL…, and
                // gating one twin but not the other would walk the user into
                // a dialog whose submit can only be refused. `navigator.onLine`
                // can read true over a dead tunnel; `api.rs`'s guard stays the
                // boundary.
                {move || {
                    session.get();
                    (!session_state::is_lan() && online.get()).then(|| view! {
                        <button
                            class="refresh"
                            on:click=move |_| {
                                clone_url.set(String::new());
                                dialogs_guard.open(Dialog::OpenUrl);
                                open_url.set(true);
                            }
                            title="Clone a public repository from a URL and view its history (read-only)"
                        >
                            "Open URL…"
                        </button>
                    })
                }}
                <button
                    class="refresh"
                    // Through the shell, not `activity.toggle()`: the overlay stack is what
                    // keeps the right edge to one panel, and a write that bypassed it would
                    // put the detail panel back underneath (M1.11, #64, Task 8).
                    on:click=move |_| shell.toggle_activity()
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
                // "Print Graph" appears once a Frame is accepted — it opens
                // the full static print view (every loaded row, light
                // background) with Print / Save PDF controls.
                //
                // Paged history makes it *conditional* (M1.10, #63). The
                // printout is a document, and a document of the newest 250
                // commits captioned "the whole graph" is quietly wrong — worse
                // than no printout, because nothing on paper says it was
                // partial. So the button carries the real HTML `disabled`
                // attribute until the last page has landed, and the handler
                // re-reads `complete` regardless: the attribute is an
                // affordance, not the guarantee. A drift reload can un-complete
                // the history between the paint and the tap.
                //
                // #217: the disabled reason used to live only in `title` — CSS
                // dimming plus a native tooltip that never surfaces on tap, so
                // on iPad the button just went dead with no explanation. The
                // label now carries the same reason (`print_button_copy`,
                // host-tested), so it's visible without hover/long-press.
                {move || frame().map(|_| view! {
                    <button
                        class="refresh"
                        disabled=move || !history_complete.get()
                        on:click=move |_| {
                            if history_complete.get_untracked() {
                                history_ui.print_open.set(true);
                            }
                        }
                        title=move || print_button_copy(history_complete.get()).1
                    >
                        {move || print_button_copy(history_complete.get()).0}
                    </button>
                })}
                // Only a repo explicitly seeded as a test repo (`gv --seed`)
                // gets this; everything since the seed is discarded on reset,
                // so it's confirmed in its own modal and styled as a hazard.
                // M2.22b (#242): hidden offline like the rest of the write
                // set — a destructive control whose confirm flow could only
                // dead-end on `api.rs`'s offline guard.
                {move || frame()
                    .filter(|f| f.resettable && online.get())
                    .map(|_| view! {
                        <button
                            class="refresh danger"
                            on:click=move |_| {
                                dialogs_guard.open(Dialog::Reset);
                                reset_open.set(true);
                            }
                            title="Restore this test repo to its recorded seed state — \
                                   discards every commit, branch and change made since \
                                   (recorded with gv --seed)"
                        >
                            "Reset Test Repo"
                        </button>
                    })}
            </header>
            // M2.22b (#242): the offline strip, directly under the topbar in
            // normal flow (see `offline_banner`'s module docs for why it is
            // not a second fixed bar, and why the live region mounts
            // permanently rather than on demand). Purely a disclosure — the
            // write controls it explains are gated where they render, and the
            // real boundary is `api.rs`'s `refuse_if_offline()` either way.
            {crate::offline_banner::offline_banner_view(online)}
            // The "Open URL" modal (Phase 12), factored into `dialogs`.
            {dialogs::open_url_view(
                open_url,
                clone_url,
                cloning,
                checking_clone_status,
                dialogs_guard,
                graph,
                mode_for,
            )}
            // The "Reset Test Repo" confirmation (only reachable via the gated
            // topbar button above).
            {dialogs::reset_repo_view(reset_open, dialogs_guard, graph)}
            // The repo picker + mode screens (ADR 0006): blocking overlays under
            // the sign-in/protocol screens, over everything else.
            {crate::picker::picker_view(picker_open, mode_for, open_url, clone_url, dialogs_guard, graph)}
            {crate::picker::mode_view(mode_for, picker_open, graph)}
            {move || {
                (shell.detail_id().is_some() && sheet.placement().is_sheet()).then(|| view! {
                    <div
                        class="sheet-grab-zone"
                        aria-hidden="true"
                        on:pointerdown=move |ev| sheet.pointer_down(ev)
                        on:pointermove=move |ev| sheet.pointer_move(ev)
                        on:pointerup=move |ev| sheet.pointer_up(ev)
                        on:pointercancel=move |ev| sheet.pointer_cancel(ev)
                    >
                        <span class="sheet-grab-pill"></span>
                    </div>
                })
            }}
            // M1.12 (#65): a bare <section> is not a landmark — it is only exposed as
            // a `region` once it has an accessible name, so without this the graph is
            // an anonymous container and VoiceOver's rotor has nothing to jump to. The
            // name comes from `a11y::core` rather than a literal here so the markup and
            // the tripwire that checks it cannot drift apart.
            <section class="graph" aria-label=GRAPH_REGION_LABEL>
                {move || {
                    // Read the icon set here, inside the reactive block, so the
                    // status lines re-render when the icon style is toggled.
                    let ic = icon_set(nerd_icons.get());
                    // The *phase* decides what this panel shows, and it is read
                    // before the resource on purpose (M1.10, #63). A resource
                    // keeps serving its previous value while the next one loads,
                    // so matching on `seed.get()` first would leave the retired
                    // epoch's graph mounted — and would hide the drift notice
                    // behind a graph the server has already disowned.
                    match history_ui.phase.get() {
                        HistoryPhase::SeedLoading { .. } => {
                            view! { <p class="status">"Loading history…"</p> }.into_view()
                        }
                        // The 409 path: the epoch on screen is gone and its
                        // replacement is already being fetched.
                        HistoryPhase::DriftReloading { .. } => {
                            view! { <p class="status">"History moved — reloading…"</p> }
                                .into_view()
                        }
                        // The conflict/warning glyph flags the failure at a glance.
                        HistoryPhase::SeedError { epoch } => {
                            // The failure is read back out of the resource, but
                            // only the one belonging to *this* epoch: reporting
                            // a retired epoch's error would name a cause that no
                            // longer applies.
                            let message = seed
                                .map(|(e, result)| match result {
                                    Err(err) if *e == epoch => Some(err.to_string()),
                                    _ => None,
                                })
                                .flatten()
                                .unwrap_or_else(|| "the request did not complete".to_string());
                            view! {
                                <p class="status error">
                                    <span class="nf">{ic.conflict}</span>
                                    {format!(" Failed to load history: {message}")}
                                </p>
                            }
                            .into_view()
                        }
                        HistoryPhase::Ready { epoch } => {
                            // Only the phase's own epoch may mount. The guard is
                            // belt-and-braces — the promotion effect above sets
                            // Ready only for a matching seed — but it is what
                            // makes "retained value" harmless rather than subtle.
                            match seed.get() {
                                Some((e, Ok(seed))) if e == epoch => {
                                    // Show which repo this page is actually
                                    // displaying, straight from the Frame. If it
                                    // disagrees with the terminal, the browser is
                                    // pointed at a stale server/tab — now visible.
                                    let repo = seed.frame.repo_label.clone();
                                    let branch = seed.frame.head_branch.clone();
                                    let remote_web_url = seed.frame.remote_web_url.clone();
                                    view! {
                                        // Repo glyph + name, then branch glyph + checked-out
                                        // branch — the icons carry what the old "repository:"
                                        // prefix used to spell out.
                                        {repo.map(|r| view! {
                                            <p class="status repo">
                                                <span class="nf ic-repo">{ic.repository}</span>
                                                {format!(" {r}")}
                                                {branch.map(|b| view! {
                                                    <span class="repo-branch">
                                                        <span class="nf ic-branch">{ic.branch}</span>
                                                        {format!(" {b}")}
                                                    </span>
                                                })}
                                                // Forge repo link (ADR 0010): out to wherever
                                                // this repo's origin lives, any host.
                                                {remote_web_url.map(|base| {
                                                    let host =
                                                        git_vista_core::forge::host_label(&base);
                                                    view! {
                                                        <a
                                                            class="repo-link"
                                                            href=base
                                                            target="_blank"
                                                            rel="noopener"
                                                            style="margin-left:8px; font-size:0.85em;"
                                                        >
                                                            {format!("view on {host} ↗")}
                                                        </a>
                                                    }
                                                })}
                                            </p>
                                        })}
                                        // The print view is no longer mounted
                                        // here: it reads the same aggregate the
                                        // canvas owns, so it lives *inside*
                                        // `graph_canvas` and is disposed with it.
                                        {graph_canvas(
                                            seed,
                                            features,
                                            history_ui,
                                            settings,
                                        )}
                                    }
                                    .into_view()
                                }
                                // Ready without its own seed can't happen; showing
                                // the loading line beats an empty panel if it does.
                                _ => view! { <p class="status">"Loading history…"</p> }.into_view(),
                            }
                        }
                    }
                }}
            </section>
        </main>
    }
}
