//! The repo picker and Visualize/Active mode screens (ADR 0006/0009/0010).
//!
//! Both are blocking full-screen overlays in the iPad-proven inline-style
//! pattern of `session::not_connected_view`. The picker lists what the server's
//! catalog offers — launch repo, root-scanned repos, persistent clones
//! (deletable in place, ADR 0008) — as opaque descriptors (never paths);
//! picking one opens the mode screen; choosing a mode POSTs `/api/select` and
//! bumps `reload` so the graph re-reads. The picker sits at z-index 900, the
//! mode screen at 901, both *below* the sign-in and protocol overlays (1000)
//! so authentication always wins the stack.

use leptos::*;

use git_vista_protocol::{RepoMode, RepositoryDescriptor, RepositoryKind};

use crate::features::dialogs::core::Dialog;
use crate::features::dialogs::signals::Dialogs;
use crate::features::graph::core::GraphCore;
use crate::features::operations::core::{result_is_newest, IntentSeq};
use crate::features::operations::signals as ops;
use crate::features::session::core::SessionEvent;
use crate::features::session::signals as session_state;
use crate::features::shell::signals as shell_state;

use crate::api::{delete_clone_request, fetch_catalog, rescan_request, select_request};
use crate::hook_policy_disclosure;

/// Styling for the per-row hook-policy badge (INV-15, #208). Amber — the same
/// palette as the session-wide `hook_policy_banner` — for anything the
/// descriptor says needs the banner, muted grey for the one tier that earns
/// quiet. Inline-styled like the rest of this file, which deliberately keeps
/// out of `styles.css`.
fn hook_policy_badge_style(warn: bool) -> &'static str {
    if warn {
        // Amber-on-brown, legible without being an alarm the user learns to
        // ignore: until the strict tier is the norm most repositories will
        // carry this badge, so it has to stay readable rather than shout.
        "margin-top:6px; display:inline-block; font-size:0.78em; padding:2px 6px; \
         border-radius:4px; background:#3a2a0a; color:#f0c674; border:1px solid #5a4210;"
    } else {
        "margin-top:6px; display:inline-block; font-size:0.78em; padding:2px 6px; \
         border-radius:4px; background:#161b22; color:#8b949e; border:1px solid #30363d;"
    }
}

/// The same disclosure at full length on the mode screen, where the user is one
/// tap from opening the repository. Left-aligned inside a centred dialog on
/// purpose — it is a sentence to read, not a caption.
fn hook_policy_notice_style(warn: bool) -> &'static str {
    if warn {
        "margin:0 0 16px; padding:8px 10px; border-radius:6px; text-align:left; \
         font-size:0.85em; background:#3a2a0a; color:#f0c674; border:1px solid #5a4210;"
    } else {
        "margin:0 0 16px; padding:8px 10px; border-radius:6px; text-align:left; \
         font-size:0.85em; background:#0d1117; color:#8b949e; border:1px solid #30363d;"
    }
}

/// The blocking repo list. `open` shows/hides it; picking a repo hands its
/// descriptor to `mode_for` (the mode screen); "Clone URL…" opens the existing
/// Phase-12 clone modal via the same signals the topbar button uses.
pub fn picker_view(
    open: RwSignal<bool>,
    mode_for: RwSignal<Option<RepositoryDescriptor>>,
    open_url: RwSignal<bool>,
    clone_url: RwSignal<String>,
    dialogs: Dialogs,
    graph: RwSignal<GraphCore>,
) -> impl IntoView {
    // Refetch every time the picker opens (and after a rescan) — the catalog
    // changes at runtime (clones, rescans), so a cached list would mislead.
    // Also keyed on `reload`: the picker opens on load BEFORE the session
    // lands, so its first fetch 401s; the session effect's reload bump retries
    // it (the same recovery the graph/status reads use).
    let bump = create_rw_signal(0u32);
    let catalog = create_local_resource(
        move || (open.get(), bump.get(), graph.get().epoch()),
        |(is_open, _, _)| async move {
            if is_open {
                Some(fetch_catalog().await)
            } else {
                None
            }
        },
    );
    // The one status line under the picker, written by two independent async actions
    // (delete-clone and Rescan). Stamped with the click sequence that produced it so a
    // slower earlier reply cannot overwrite the newer one (M1.11, #64); sequence 0 means
    // nothing has been shown yet.
    let rescan_msg = create_rw_signal((0_u64, String::new()));
    let msg_seq = store_value(IntentSeq::default());
    move || {
        open.get().then(|| {
            view! {
                <div style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                            z-index:900; display:flex; align-items:center; \
                            justify-content:center; background:rgba(1,4,9,0.85);">
                    // Flex column with the repo list as the only scrolling region,
                    // so the Cancel/actions row below stays visible however long
                    // the list gets (a 20-repo root buried it off-screen on iPad).
                    <div style="min-width:320px; max-width:90vw; max-height:85vh; \
                                display:flex; flex-direction:column; \
                                padding:24px; background:#161b22; border:1px solid #30363d; \
                                border-radius:10px; color:var(--fg);">
                        <div style="font-weight:600; font-size:1.2em; margin-bottom:12px;">
                            "Open a repository"
                        </div>
                        <div style="overflow-y:auto; -webkit-overflow-scrolling:touch; \
                                    flex:1 1 auto; min-height:0;">
                        {move || match catalog.get().flatten() {
                            None => view! { <p>"Loading repositories…"</p> }.into_view(),
                            Some(Err(e)) => view! {
                                <p>{format!("Couldn't list repositories: {e}")}</p>
                            }
                            .into_view(),
                            Some(Ok(entries)) => entries
                                .into_iter()
                                .map(|d| {
                                    let is_clone = d.read_only;
                                    // INV-15 (#208): every row discloses the
                                    // hook policy the server computed for that
                                    // repository. Read before `d` is moved
                                    // into `pick` below. The warn/quiet call is
                                    // the descriptor's own — see
                                    // `hook_policy_disclosure`'s module docs for
                                    // why it is never re-derived here.
                                    let disclosure = hook_policy_disclosure::for_repository(&d);
                                    let label = match d.kind {
                                        RepositoryKind::Bare => format!("{} (bare)", d.name),
                                        RepositoryKind::LinkedWorktree => {
                                            format!("{} (worktree)", d.name)
                                        }
                                        RepositoryKind::MainWorktree if is_clone => {
                                            format!("{} (clone)", d.name)
                                        }
                                        RepositoryKind::MainWorktree => d.name.clone(),
                                    };
                                    let worktree = d.worktree.clone();
                                    let name = d.name.clone();
                                    // ADR 0005: a LAN-view session can't select —
                                    // the row stays as a label, not a dead-end.
                                    let pick = move |_| {
                                        if !session_state::is_lan() {
                                            mode_for.set(Some(d.clone()));
                                        }
                                    };
                                    // Delete a persistent clone (ADR 0008): native
                                    // confirm, then the guarded endpoint; feedback
                                    // reuses the status line under the buttons.
                                    let del = move |_| {
                                        let confirmed = web_sys::window()
                                            .map(|w| {
                                                w.confirm_with_message(&format!(
                                                    "Delete the clone \u{2018}{name}\u{2019} from disk?"
                                                ))
                                                .unwrap_or(false)
                                            })
                                            .unwrap_or(false);
                                        if !confirmed {
                                            return;
                                        }
                                        let worktree = worktree.clone();
                                        let seq = ops::next_seq(msg_seq);
                                        spawn_local(async move {
                                            let text = match delete_clone_request(&worktree)
                                                .await
                                            {
                                                Ok(msg) => {
                                                    bump.update(|n| *n = n.wrapping_add(1));
                                                    msg
                                                }
                                                Err(e) => e,
                                            };
                                            if result_is_newest(
                                                rescan_msg.get_untracked().0,
                                                seq,
                                            ) {
                                                rescan_msg.set((seq, text));
                                            }
                                        });
                                    };
                                    view! {
                                        // A big touch row per repo: tap → mode
                                        // screen; clones carry a Delete beside.
                                        <div style="display:flex; gap:4px; margin:4px 0;">
                                            <button
                                                style="flex:1; text-align:left; \
                                                       padding:12px; font:inherit; \
                                                       color:var(--fg); background:#0d1117; \
                                                       border:1px solid #30363d; \
                                                       border-radius:6px;"
                                                on:click=pick
                                            >
                                                <div>{label}</div>
                                                // Visible text, never a tooltip
                                                // or a `title=` attribute: a
                                                // disclosure nobody notices is
                                                // the failure INV-15 names.
                                                <div style=hook_policy_badge_style(
                                                    disclosure.warn,
                                                )>
                                                    {disclosure.warn.then_some("\u{26A0} ")}
                                                    {disclosure.label}
                                                </div>
                                            </button>
                                            // ADR 0005: no Delete on a LAN-view
                                            // session — the route doesn't even
                                            // exist on the LAN listener. Rows
                                            // re-render post-session (catalog is
                                            // keyed on `reload`), so the flag is
                                            // settled by the time it's read.
                                            // M2.22b (#242): hidden offline too —
                                            // the tracked `online_signal` read
                                            // re-renders this row list when
                                            // connectivity flips, and the banner
                                            // above the graph says why it went.
                                            // `navigator.onLine` can read true
                                            // over a dead tunnel; the write
                                            // boundary stays `api.rs`'s guard.
                                            {(is_clone
                                                && !session_state::is_lan()
                                                && shell_state::online_signal().get())
                                            .then(|| view! {
                                                <button
                                                    style="padding:12px; font:inherit; \
                                                           color:#f85149; background:#0d1117; \
                                                           border:1px solid #30363d; \
                                                           border-radius:6px;"
                                                    on:click=del
                                                >
                                                    "Delete"
                                                </button>
                                            })}
                                        </div>
                                    }
                                })
                                .collect_view(),
                        }}
                        </div>
                        <div style="display:flex; gap:8px; margin-top:16px;">
                            // ADR 0005: Clone URL…/Rescan hit routes the LAN
                            // listener never registers — hide them there. Keyed
                            // on `reload` so the buttons re-evaluate once the
                            // session (and its via_lan flag) lands, the same
                            // recovery the catalog fetch above uses.
                            // M2.22b (#242): hidden offline too, reactively —
                            // both trigger writes (`/api/clone`, `/api/rescan`)
                            // that the offline guard would only refuse.
                            {move || {
                                graph.get().epoch();
                                (!session_state::is_lan() && shell_state::online_signal().get())
                                    .then(|| view! {
                                    <button
                                        style="padding:8px 16px; font:inherit; color:var(--fg); \
                                               background:#21262d; border:1px solid #30363d; \
                                               border-radius:6px;"
                                        on:click=move |_| {
                                            clone_url.set(String::new());
                                            dialogs.open(Dialog::OpenUrl);
                                            open_url.set(true);
                                        }
                                    >
                                        "Clone URL…"
                                    </button>
                                    <button
                                        style="padding:8px 16px; font:inherit; color:var(--fg); \
                                               background:#21262d; border:1px solid #30363d; \
                                               border-radius:6px;"
                                        on:click=move |_| {
                                            let seq = ops::next_seq(msg_seq);
                                            spawn_local(async move {
                                                let text = match rescan_request().await {
                                                    Ok(msg) => {
                                                        bump.update(|n| *n = n.wrapping_add(1));
                                                        msg
                                                    }
                                                    Err(e) => e,
                                                };
                                                if result_is_newest(
                                                    rescan_msg.get_untracked().0,
                                                    seq,
                                                ) {
                                                    rescan_msg.set((seq, text));
                                                }
                                            });
                                        }
                                    >
                                        "Rescan"
                                    </button>
                                })
                            }}
                            // The picker blocks the app, so it must always be
                            // dismissable: Cancel keeps the current repo/mode.
                            <button
                                style="margin-left:auto; padding:8px 16px; font:inherit; \
                                       color:var(--fg); background:#21262d; \
                                       border:1px solid #30363d; border-radius:6px;"
                                on:click=move |_| open.set(false)
                            >
                                "Cancel"
                            </button>
                        </div>
                        {move || {
                            (!rescan_msg.get().1.is_empty()).then(|| {
                                view! {
                                    <div style="margin-top:8px; font-size:0.85em; opacity:0.7;">
                                        {rescan_msg.get().1}
                                    </div>
                                }
                            })
                        }}
                    </div>
                </div>
            }
        })
    }
}

/// The two-button mode screen (ADR 0006): Visualize / Active for one repo.
/// Selecting POSTs `/api/select`, mirrors the mode into the api chokepoint,
/// closes both overlays, and bumps `reload` so every resource re-reads.
pub fn mode_view(
    mode_for: RwSignal<Option<RepositoryDescriptor>>,
    picker_open: RwSignal<bool>,
    graph: RwSignal<GraphCore>,
) -> impl IntoView {
    let busy = create_rw_signal(false);
    let err = create_rw_signal(String::new());
    move || {
        mode_for.get().map(|d| {
            let name = d.name.clone();
            // INV-15 (#208): the full sentence, at the point of commitment. The
            // picker row's badge is a glance; this is where the user is about
            // to hand a repository the ability to run its hooks, so the reason
            // is spelled out rather than abbreviated.
            let disclosure = hook_policy_disclosure::for_repository(&d);
            let choose = move |mode: RepoMode| {
                let worktree = d.worktree.clone();
                move |_| {
                    if busy.get_untracked() {
                        return;
                    }
                    busy.set(true);
                    err.set(String::new());
                    let worktree = worktree.clone();
                    spawn_local(async move {
                        match select_request(&worktree, mode).await {
                            Ok(()) => {
                                // The server accepted the selection, so this is the user's
                                // choice taking effect. A LAN session cannot reach here —
                                // `select_request` refuses first (ADR 0005) — so the core's
                                // rejection is unreachable and deliberately ignored.
                                let _ = session_state::apply(SessionEvent::UiModeSelected(mode));
                                mode_for.set(None);
                                picker_open.set(false);
                                graph.update(|g| {
                                    g.force_bump();
                                });
                            }
                            Err(e) => err.set(e),
                        }
                        busy.set(false);
                    });
                }
            };
            view! {
                <div style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                            z-index:901; display:flex; align-items:center; \
                            justify-content:center; background:rgba(1,4,9,0.85);">
                    <div style="min-width:300px; max-width:90vw; padding:24px; \
                                background:#161b22; border:1px solid #30363d; \
                                border-radius:10px; color:var(--fg); text-align:center;">
                        <div style="font-weight:600; font-size:1.2em; margin-bottom:16px;">
                            {format!("Open ‘{name}’ as…")}
                        </div>
                        <div
                            role="status"
                            style=hook_policy_notice_style(disclosure.warn)
                        >
                            {disclosure.warn.then_some("\u{26A0} ")}
                            {disclosure.detail}
                        </div>
                        <button
                            style="display:block; width:100%; padding:16px; margin:8px 0; \
                                   font:inherit; font-size:1.05em; color:#fff; \
                                   background:#1f6feb; border:1px solid #388bfd; \
                                   border-radius:8px;"
                            disabled=move || busy.get()
                            on:click=choose(RepoMode::Visualize)
                        >
                            "Visualize — look only, with links out"
                        </button>
                        {(!session_state::is_lan()).then(|| view! {
                            <button
                                style="display:block; width:100%; padding:16px; margin:8px 0; \
                                       font:inherit; font-size:1.05em; color:#fff; \
                                       background:#238636; border:1px solid #2ea043; \
                                       border-radius:8px;"
                                disabled=move || busy.get()
                                on:click=choose(RepoMode::Active)
                            >
                                "Active — full git operations"
                            </button>
                        })}
                        {move || {
                            (!err.get().is_empty()).then(|| {
                                view! {
                                    <div style="margin-top:8px; color:#f85149;">{err.get()}</div>
                                }
                            })
                        }}
                        <button
                            style="margin-top:12px; padding:8px 16px; font:inherit; \
                                   color:var(--fg); background:#21262d; \
                                   border:1px solid #30363d; border-radius:6px;"
                            on:click=move |_| mode_for.set(None)
                        >
                            "Back"
                        </button>
                    </div>
                </div>
            }
        })
    }
}
