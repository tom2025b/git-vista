//! The top-level Leptos components for git-vista.
//!
//! Phase 4 fetched one whole laid-out `Graph`; M1.10 (#63) replaced that with
//! *paged* history — a cheap once-per-view [`Frame`] (refs, colours, repo
//! metadata) plus cursor-paginated pages of rows/edges/stubs, assembled by
//! [`crate::history::LoadedHistory`]. So the App no longer holds a graph: it
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

use crate::api::{fetch_frame, fetch_page, fetch_protocol, fetch_status, HistoryFetchError};
use crate::dialogs;
use crate::features::operations::core::OperationsCore;
use crate::features::operations::signals::Operations;
use crate::features::session::core::SessionEvent;
use crate::features::session::signals as session_state;
use crate::history::{Frame, HistoryInvariantError, LoadedHistory, DEFAULT_PAGE_LIMIT};
use crate::icons::icon_set;
use crate::prefs::{load_icon_pref, load_node_icons_pref, store_icon_pref, store_node_icons_pref};
use crate::session::{establish_session, not_connected_view};
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
    pub epoch: u32,
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
    SeedLoading { epoch: u32 },
    Ready { epoch: u32 },
    DriftReloading { epoch: u32 },
    SeedError { epoch: u32 },
}

/// The three history signals the App owns and the canvas drives: which phase the
/// panel is in, whether the whole history is loaded (Print needs all of it), and
/// whether the print overlay is open (any epoch change closes it).
#[derive(Clone, Copy)]
pub struct HistoryUiSignals {
    pub phase: RwSignal<HistoryPhase>,
    pub complete: RwSignal<bool>,
    pub print_open: RwSignal<bool>,
}

/// Fetch one epoch's seed, tagged with the epoch it was fetched for.
///
/// The tag rides on the *result*, not only on the success value: a failure has
/// to be attributable to an epoch too, or a slow error from a retired epoch
/// would raise [`HistoryPhase::SeedError`] over an epoch that is still loading.
async fn load_seed(epoch: u32) -> (u32, Result<HistorySeed, HistorySeedError>) {
    (epoch, seed_for_epoch(epoch).await)
}

/// Frame, then page 1 pinned to that Frame's worktree, then the aggregate.
///
/// The generation check is the client's half of the drift contract: the Frame
/// and the page are two separate reads, so a commit landing between them would
/// otherwise splice rows onto refs that no longer describe them. Checking here
/// means [`LoadedHistory::from_first_page`] can treat its own generation check
/// as the tautology it is.
async fn seed_for_epoch(epoch: u32) -> Result<HistorySeed, HistorySeedError> {
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
    // A bump counter is the resource's reactive source, so changing it re-runs the
    // fetch. The frontend used to fetch exactly once on load (`|| ()`, a constant
    // source that never re-fires), so a branch or commit created *after* the page
    // loaded never appeared until a full reload — the heart of issue #16. The
    // Refresh button below bumps this to re-read the repo on demand. (Each fetch
    // also cache-busts its URL, so the re-read reaches the server, not the cache.)
    // Since M1.10 it doubles as the history *epoch*: every seed and every page
    // request is stamped with it, and a reply carrying a retired epoch is dropped.
    let reload = create_rw_signal(0u32);
    let refresh = move |_| reload.update(|n| *n = n.wrapping_add(1));

    // The write registry (M1.11, #64). Deliberately created HERE and not inside
    // `graph_canvas`: an epoch bump rebuilds the canvas and every overlay in it, so an
    // operation living down there would be destroyed by the very re-read its own
    // completion triggers. Owning it in the shell is what lets a write survive a panel
    // change (acceptance criterion 2).
    let operations_core = create_rw_signal(OperationsCore::default());
    let operations = Operations::new(operations_core, reload);

    // The history signals the App owns (M1.10, #63). `print_graph_open` opens the
    // full static print view (crate::print) from the topbar; `history_complete`
    // says whether every page has been loaded, which is what Print requires; the
    // phase is what the graph panel actually renders off.
    let history_phase = create_rw_signal(HistoryPhase::SeedLoading { epoch: 0 });
    let history_complete = create_rw_signal(false);
    let print_graph_open = create_rw_signal(false);
    let history_ui = HistoryUiSignals {
        phase: history_phase,
        complete: history_complete,
        print_open: print_graph_open,
    };

    // Frame + page 1, keyed on the epoch. `create_local_resource` because the
    // fetch future isn't `Send` (wasm).
    let seed = create_local_resource(move || reload.get(), load_seed);

    // Every epoch — Refresh, a post-operation reload, a drift reload — retires
    // the mounted history. Print can't span two generations and "complete" must
    // not survive into a graph that hasn't loaded a page yet, so both are reset
    // here rather than at each of the several places that bump `reload`.
    create_effect(move |_| {
        let epoch = reload.get();
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
        let Some((epoch, complete)) = seed
            .map(|(epoch, result)| (*epoch, result.as_ref().ok().map(|s| s.loaded.is_complete())))
        else {
            return;
        };
        if epoch != reload.get_untracked() {
            return;
        }
        match complete {
            Some(complete) => {
                history_ui.complete.set(complete);
                history_ui.phase.set(HistoryPhase::Ready { epoch });
            }
            None => history_ui.phase.set(HistoryPhase::SeedError { epoch }),
        }
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
    let session = create_local_resource(|| (), |_| establish_session());
    let needs_sign_in = move || matches!(session.get(), Some(Ok(false)));
    // The history/status reads fired at load without a cookie and 401'd; once the
    // session lands, bump `reload` once so they refetch authenticated.
    create_effect(move |_| {
        if matches!(session.get(), Some(Ok(true))) {
            reload.update(|n| *n = n.wrapping_add(1));
        }
    });

    // M1.02 (#102): negotiate the protocol before trusting the rest of the API.
    // Keyed on `reload` so every Refresh (and every post-operation reload)
    // re-checks — if the server is redeployed on an incompatible protocol while
    // this tab stays open, the next reload catches it. `protocol_gate` yields the
    // negotiation payload + verdict only when the client is *out* of the server's
    // accepted window; that drives the blocking "Update Required" overlay below.
    let protocol = create_local_resource(move || reload.get(), |_| fetch_protocol());
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

    // The live working-tree status behind the topbar chip (Activity/Undo
    // step 1): clean/dirty/conflicted at a glance, plus ahead/behind vs the
    // upstream. Keyed on `reload` so Refresh — and every post-operation
    // reload — re-reads it alongside the history. A fetch failure resolves to
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

    // "Reset Test Repo" (iPad-testing follow-up): the button appears only when
    // the Frame says this repo carries a seed (`gv --seed`); the confirm modal
    // lives in `dialogs::reset_repo_view`, owned here like the Open-URL one
    // because its button sits in the topbar, not the graph canvas.
    let reset_open = create_rw_signal(false);
    let reset_opened_at = store_value(0f64);

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
        <main class="app">
            // M1.02: the blocking "Update Required" screen, shown (over everything
            // else) only when this client's protocol is incompatible with the server.
            {move || protocol_gate().map(|(info, verdict)| update_required_view(info, verdict))}
            // M1.04: the blocking sign-in screen, shown when there's no session and
            // no bootstrap token to make one — the operator must open `gv`'s link.
            {move || needs_sign_in().then(not_connected_view)}
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
                        // Split into staged vs the rest, so clicking "Stage
                        // Changes" visibly flips the chip ("2 changes" → "2
                        // staged"), then to "clean" once committed.
                        let staged = s.staged.len();
                        let unstaged = s.unstaged.len() + s.untracked.len();
                        let label = match (staged, unstaged) {
                            (st, 0) => format!("{st} staged"),
                            (0, un) => format!("{un} change{}", if un == 1 { "" } else { "s" }),
                            (st, un) => format!("{st} staged · {un} unstaged"),
                        };
                        ("status-chip dirty", ic.dirty, label)
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
                {move || {
                    session.get();
                    (!session_state::is_lan()).then(|| view! {
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
                    })
                }}
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
                {move || frame().map(|_| view! {
                    <button
                        class="refresh"
                        disabled=move || !history_complete.get()
                        on:click=move |_| {
                            if history_complete.get_untracked() {
                                history_ui.print_open.set(true);
                            }
                        }
                        title=move || if history_complete.get() {
                            "A clean, printable view of the whole graph — \
                             print it or save it as a PDF"
                        } else {
                            "Load all history before printing."
                        }
                    >
                        "Print Graph"
                    </button>
                })}
                // Only a repo explicitly seeded as a test repo (`gv --seed`)
                // gets this; everything since the seed is discarded on reset,
                // so it's confirmed in its own modal and styled as a hazard.
                {move || frame()
                    .filter(|f| f.resettable)
                    .map(|_| view! {
                        <button
                            class="refresh danger"
                            on:click=move |_| {
                                reset_opened_at.set_value(js_sys::Date::now());
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
            // The "Open URL" modal (Phase 12), factored into `dialogs`.
            {dialogs::open_url_view(open_url, clone_url, cloning, open_opened_at, reload, mode_for)}
            // The "Reset Test Repo" confirmation (only reachable via the gated
            // topbar button above).
            {dialogs::reset_repo_view(reset_open, reset_opened_at, reload)}
            // The repo picker + mode screens (ADR 0006): blocking overlays under
            // the sign-in/protocol screens, over everything else.
            {crate::picker::picker_view(picker_open, mode_for, open_url, clone_url, open_opened_at, reload)}
            {crate::picker::mode_view(mode_for, picker_open, reload)}
            <section class="graph">
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
                                            reload,
                                            history_ui,
                                            nerd_icons,
                                            show_node_icons,
                                            activity_open,
                                            operations,
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
