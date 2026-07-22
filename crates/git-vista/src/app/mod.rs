//! The top-level Leptos components for git-vista.
//!
//! Phase 4: on startup the frontend `fetch`es the real, laid-out commit
//! [`Graph`](git_vista_core::model::Graph) from the backend's `/api/commits`
//! endpoint (same origin) and renders it as inline SVG — one circle per commit,
//! one curved path per commit->parent link, lanes laid out left to right. The
//! whole graph lives inside a single `<g transform>` driven by a [`Camera`] —
//! pointer drags pan, the wheel zooms toward the cursor (Phase 2).
//!
//! This module is now just the two top-level pieces — the [`App`] shell
//! (topbar, toggles, status line, the data fetch) here, and
//! [`graph_canvas`](canvas::graph_canvas) in [`canvas`], which wires the shared
//! signals into the SVG. The heavy lifting lives in focused sibling modules: the
//! HTTP calls in [`crate::api`], persisted toggles in [`crate::prefs`], the
//! shared types and signal bundles in [`crate::state`], the SVG builders in
//! [`crate::render`], pan/zoom in [`crate::gestures`], and the overlays in
//! [`crate::menu`] / [`crate::dialogs`] / [`crate::detail`]. Spatial math is in
//! [`crate::geometry`], the lane palette in [`git_vista_core::color`].
//!
//! [`Camera`]: crate::camera::Camera

use leptos::*;

use git_vista_protocol::{check_compatibility, PROTOCOL_VERSION};

use crate::api::{fetch_graph, fetch_head_branch, fetch_protocol, fetch_status};
use crate::dialogs;
use crate::icons::icon_set;
use crate::prefs::{load_icon_pref, load_node_icons_pref, store_icon_pref, store_node_icons_pref};
use crate::print::print_graph_view;
use crate::session::{establish_session, not_connected_view};
use crate::update_required::update_required_view;

mod canvas;

use canvas::graph_canvas;

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

    // M1.04 (#57): establish the loopback session before the API is usable. Run
    // once on load (source `|| ()`, not keyed on `reload`, so re-reads don't
    // re-bootstrap): it exchanges a `#s=<token>` fragment for a session cookie, or
    // checks an existing one. `Some(Ok(false))` — no session and nothing to make
    // one from — drives the blocking sign-in overlay; a network `Err` falls through
    // to the normal load-error path (an unreachable server isn't a sign-in problem).
    let session = create_local_resource(|| (), |_| establish_session());
    let needs_sign_in = move || matches!(session.get(), Some(Ok(false)));
    // The graph/status/head-branch reads fired at load without a cookie and 401'd;
    // once the session lands, bump `reload` once so they refetch authenticated.
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

    // "Reset Test Repo" (iPad-testing follow-up): the button appears only when
    // the graph says this repo carries a seed (`gv --seed`); the confirm modal
    // lives in `dialogs::reset_repo_view`, owned here like the Open-URL one
    // because its button sits in the topbar, not the graph canvas.
    let reset_open = create_rw_signal(false);
    let reset_opened_at = store_value(0f64);

    // "Print Graph": the full static print view of the whole graph
    // (crate::print), opened from the topbar, with Print / Save PDF.
    let print_graph_open = create_rw_signal(false);

    // ADR 0006: ask every time — the repo picker opens on load (the sign-in and
    // protocol overlays sit above it when they apply) and from the topbar
    // "Repos" button. `mode_for` holds the repo awaiting a Visualize/Active
    // choice; the mode screen renders whenever it's Some.
    let picker_open = create_rw_signal(true);
    let mode_for = create_rw_signal(None::<git_vista_protocol::RepositoryDescriptor>);

    // Defense in depth (ADR 0007): mirror the loaded graph's mode into api.rs so
    // write calls refuse client-side too. The server's 403 remains the boundary.
    create_effect(move |_| {
        if let Some(Ok(g)) = graph.get() {
            crate::api::set_ui_mode(Some(if g.read_only {
                git_vista_protocol::RepoMode::Visualize
            } else {
                git_vista_protocol::RepoMode::Active
            }));
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
                {move || graph.get().and_then(|r| r.ok()).map(|g| {
                    let (label, class) = if g.read_only {
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
                                // synthesizing its descriptor from the graph stamp.
                                // Absent ids (degraded mode) => no mode screen.
                                if let Some(worktree) = g.worktree_id.clone() {
                                    mode_for.set(Some(git_vista_protocol::RepositoryDescriptor {
                                        repository: g.repo_id.clone().unwrap_or_default(),
                                        worktree,
                                        name: g.repo_label.clone()
                                            .unwrap_or_else(|| "repository".into()),
                                        kind: git_vista_protocol::RepositoryKind::MainWorktree,
                                        read_only: g.read_only,
                                        path: None,
                                        remote_web_url: g.remote_web_url.clone(),
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
                    (!crate::api::is_lan_session()).then(|| view! {
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
                // "Print Graph" appears once the graph is loaded — it opens
                // the full static print view (every row, light background)
                // with Print / Save PDF controls.
                {move || graph.get().and_then(|r| r.ok()).map(|_| view! {
                    <button
                        class="refresh"
                        on:click=move |_| print_graph_open.set(true)
                        title="A clean, printable view of the whole graph — \
                               print it or save it as a PDF"
                    >
                        "Print Graph"
                    </button>
                })}
                // Only a repo explicitly seeded as a test repo (`gv --seed`)
                // gets this; everything since the seed is discarded on reset,
                // so it's confirmed in its own modal and styled as a hazard.
                {move || graph
                    .get()
                    .and_then(|r| r.ok())
                    .filter(|g| g.resettable)
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
                                        // Forge repo link (ADR 0010): out to wherever
                                        // this repo's origin lives, any host.
                                        {g.remote_web_url.clone().map(|base| {
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
                                {print_graph_view(g.clone(), print_graph_open, nerd_icons)}
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
