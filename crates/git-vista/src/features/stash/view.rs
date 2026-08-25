//! The stash drawer's markup — wasm only (M3.24, #77).
//!
//! Mounted as a section of the Activity panel, beside the tag list and the
//! working-tree status it shares a refresh key with.
//!
//! # This file decides nothing
//!
//! It is `#[cfg(target_arch = "wasm32")]`, so `cargo test --workspace` never
//! compiles a line of it and no host test can reach anything written here. Every
//! branch below is therefore a one-to-one mapping from a value
//! [`crate::features::stash::core`] already computed — a variant to an element,
//! an `Availability` to a `<button>` or a `<span>`. In particular:
//!
//! - whether a pop finished is [`PopVerdict::is_complete`], never a check on the
//!   HTTP result here;
//! - which actions a row offers, and in what order, is `action_offers`;
//! - what a push will and will not capture is `push_preview`.
//!
//! The browser suite is what proves this file is *reached*; the Rust suite is
//! what proves the values it renders are right.
//!
//! # No new CSS
//!
//! Reuses `act-file` / `act-pill` / `act-meta` / `act-undo` / `detail-muted` /
//! `detail-status` / `detail-error`, following `activity.rs`'s tag rows. A new
//! tappable class would need a 44x44 decision recorded in
//! `features::a11y::audit`'s `INTERACTIVE_CENSUS` and a `:focus-visible` twin
//! for any `:hover` rule — real obligations, and there is nothing here that
//! earns them. `.act-undo` already carries both.

use leptos::*;

use crate::api::{fetch_stash_patch, fetch_stashes, push_stash_request};
use crate::datetime::time_ago;
use crate::features::stash::core::{
    drawer_view, push_preview, Availability, DrawerView, PushPreview, StashAction, StashRow,
    LOADING_STASHES, NOTHING_TO_STASH, NO_STASHES,
};
use crate::features::stash::signals::{compose_pop, StashDrawer, StashNotice};
use crate::features::status::core::StatusSections;
use crate::icons::icon_set;
use crate::state::{Features, Settings, ViewerDoc};

/// The whole Stashes section.
///
/// `sections` is the panel's own working-tree read, passed in rather than
/// fetched again: the push preview needs the staged/unstaged/untracked counts,
/// and a second `/api/status/v2` resource would be a second "is the panel open"
/// to fall out of sync.
pub fn stash_section_view(
    features: Features,
    settings: Settings,
    read_only: bool,
    sections: Signal<Option<StatusSections>>,
) -> impl IntoView {
    let Features { graph, shell, .. } = features;
    let nerd_icons = settings.nerd_icons;
    let drawer = StashDrawer::new();
    let write_gate = crate::features::stash::core::write_gate(read_only);

    let stashes = create_local_resource(
        move || (shell.activity_is_open(), graph.get().epoch()),
        |(open, _)| async move {
            if open {
                Some(fetch_stashes().await)
            } else {
                None
            }
        },
    );

    // The patch of whichever row is expanded. Keyed on the selector, so
    // collapsing and re-expanding re-reads rather than showing a patch from
    // before an apply moved the tree underneath it.
    let patch = create_local_resource(
        move || drawer.inspecting(),
        |selector| async move {
            match selector {
                None => None,
                Some(entry) => Some(fetch_stash_patch(&entry).await),
            }
        },
    );

    let notice_view = move || {
        drawer.notice().map(|notice| {
            let StashNotice {
                headline,
                complete,
                tree,
                entry_retained: _,
                conflicted,
                unreadable,
            } = notice;
            // `complete` comes from the verdict, not from "the request
            // returned" — see this module's header.
            let class = if complete {
                "detail-status"
            } else {
                "detail-status detail-error"
            };
            // A3: the conflicted paths route into the SHARED conflict view
            // (#428/#429/#432), the same `ViewerDoc::Conflict` the working-tree
            // section's conflicted cards open. No stash-shaped conflict UI.
            let routes = conflicted
                .into_iter()
                .map(|path| {
                    let open_path = path.clone();
                    let open = move |_| {
                        shell.open_viewer(ViewerDoc::Conflict {
                            path: open_path.clone(),
                        });
                    };
                    view! {
                        <div class="act-file">
                            <span class="act-file-path">{path.clone()}</span>
                            <button
                                class="act-undo"
                                aria-label=format!("{path} — resolve this conflict")
                                on:click=open
                            >
                                "Resolve"
                            </button>
                        </div>
                    }
                })
                .collect_view();
            // Paths whose sides could not be read are a fault to report, not
            // work the user can do by picking a side — so they get no Resolve
            // button, which would open a viewer with nothing to choose between.
            let faults = unreadable
                .into_iter()
                .map(|path| {
                    view! {
                        <div class="act-file detail-muted">
                            <span class="act-file-path">
                                {format!("{path} — could not be read; resolve this one by hand")}
                            </span>
                        </div>
                    }
                })
                .collect_view();
            // What happened to the user's data, stated as its own line. For a
            // composed pop this is the part that matters most when the pop did
            // NOT finish.
            let effect = tree.map(|t| view! { <p class="detail-status">{t.line()}</p> });
            let dismiss = move |_| drawer.clear_notice();
            view! {
                <p class=class>{headline}</p>
                {effect}
                {routes}
                {faults}
                <button class="act-undo" on:click=dismiss>"Dismiss"</button>
            }
        })
    };

    // -- The push control (A2). -------------------------------------------
    let keep_index = create_rw_signal(false);
    let include_untracked = create_rw_signal(false);

    let preview = move || {
        sections
            .get()
            .map(|s| push_preview(&s, keep_index.get(), include_untracked.get()))
    };

    let push_control = move || {
        if read_only {
            return view! {}.into_view();
        }
        let Some(prepared) = preview() else {
            // No status read yet. Offering a push here would mean offering it
            // without being able to say what it would capture, which is the
            // thing A2 forbids.
            return view! { <p class="detail-status">"Reading the working tree…"</p> }.into_view();
        };

        // `may_push` is the single predicate for "is this push worth offering",
        // so this file never re-decides it from the fields.
        let offerable = prepared.may_push();
        let PushPreview {
            captures,
            leaves_behind,
            refusal,
        } = prepared;

        let captured = captures
            .into_iter()
            .map(|line| view! { <div class="act-file"><span class="act-file-path">{line}</span></div> })
            .collect_view();
        // The load-bearing half: what git will leave in the working tree.
        let left = leaves_behind
            .into_iter()
            .map(|line| {
                view! {
                    <div class="act-file detail-error">
                        <span class="act-file-path">{line}</span>
                    </div>
                }
            })
            .collect_view();

        let button = if !offerable {
            // The refusal text is the core's, not this file's.
            let why = refusal.unwrap_or(NOTHING_TO_STASH);
            view! { <p class="detail-status">{why}</p> }.into_view()
        } else {
            {
                let on_push = move |_| {
                    let keep = keep_index.get_untracked();
                    let untracked = include_untracked.get_untracked();
                    drawer.begin("", "stashing");
                    spawn_local(async move {
                        let result = push_stash_request(None, keep, untracked).await;
                        drawer.set_notice(StashNotice::from_result(
                            result,
                            "Stashed your working tree changes.",
                        ));
                        drawer.finish();
                        // A5: one bump refreshes the drawer, the feed, the
                        // working-tree read and the graph together — the same
                        // convention every other write in this app follows.
                        graph.update(|g| {
                            g.force_bump();
                        });
                    });
                };
                view! {
                    <button class="act-undo" on:click=on_push>
                        "Stash these changes"
                    </button>
                }
                .into_view()
            }
        };

        view! {
            <div class="act-meta">
                <label>
                    <input
                        type="checkbox"
                        prop:checked=move || include_untracked.get()
                        on:change=move |_| include_untracked.update(|v| *v = !*v)
                    />
                    " Include untracked files"
                </label>
                <label>
                    <input
                        type="checkbox"
                        prop:checked=move || keep_index.get()
                        on:change=move |_| keep_index.update(|v| *v = !*v)
                    />
                    " Keep staged changes staged"
                </label>
            </div>
            {captured}
            {left}
            {button}
        }
        .into_view()
    };

    // -- The rows. ---------------------------------------------------------
    let rows_view = move || match drawer_view(stashes.get().flatten(), write_gate) {
        DrawerView::Loading => view! { <p class="detail-status">{LOADING_STASHES}</p> }.into_view(),
        DrawerView::Failed(line) => {
            view! { <p class="detail-status detail-error">{line}</p> }.into_view()
        }
        DrawerView::Empty => view! { <p class="detail-status">{NO_STASHES}</p> }.into_view(),
        DrawerView::Rows(rows) => rows
            .into_iter()
            .map(|row| stash_row_view(row, drawer, graph, patch, nerd_icons))
            .collect_view(),
    };

    view! {
        <div class="detail-section-title act-feed-title">"Stashes"</div>
        {push_control}
        {notice_view}
        {rows_view}
    }
}

/// One row: what it is, when, and what it offers.
#[allow(clippy::too_many_arguments)]
fn stash_row_view(
    row: StashRow,
    drawer: StashDrawer,
    graph: RwSignal<crate::features::graph::core::GraphCore>,
    patch: Resource<Option<String>, Option<Result<String, String>>>,
    nerd_icons: RwSignal<bool>,
) -> impl IntoView {
    let ic = icon_set(nerd_icons.get_untracked());
    let StashRow {
        selector,
        oid,
        oid_short,
        subject,
        when,
        actions,
    } = row;

    let branch_pill = subject
        .branch
        .clone()
        .map(|b| view! { <span class="act-pill">{b}</span> });
    // Git's own `WIP on …` line is marked, because the user's own words are
    // worth more trust than a generated one and the row should not pass a
    // generated subject off as something they wrote.
    let automatic = subject
        .automatic
        .then(|| view! { <span class="act-pill act-terminal">"auto"</span> });

    let buttons = actions
        .into_iter()
        .map(|offer| {
            let action = offer.action;
            match offer.availability {
                // A refusal is rendered, not removed: a control that silently
                // vanishes teaches the user nothing.
                Availability::Refused(why) => view! {
                    <span class="detail-muted" title=why>{action.label()}</span>
                }
                .into_view(),
                Availability::Offered => action_button(action, &selector, &oid, drawer, graph),
            }
        })
        .collect_view();

    let is_open = {
        let selector = selector.clone();
        move || drawer.inspecting().as_deref() == Some(selector.as_str())
    };

    let patch_view = move || {
        if !is_open() {
            return view! {}.into_view();
        }
        match patch.get().flatten() {
            None => view! { <p class="detail-status">"Reading the stash…"</p> }.into_view(),
            Some(Err(e)) => view! { <p class="detail-status detail-error">{e}</p> }.into_view(),
            // An entry whose patch is empty is a real, readable observation —
            // `git stash show -p` prints nothing for a stash that holds only
            // untracked files, since those are a separate commit it does not
            // diff. Saying so beats an empty box.
            Some(Ok(text)) if text.trim().is_empty() => view! {
                <p class="detail-status">
                    "No tracked-file changes to show. This stash may hold only untracked files."
                </p>
            }
            .into_view(),
            Some(Ok(text)) => view! { <pre class="act-file-path">{text}</pre> }.into_view(),
        }
    };

    let busy_label = {
        let selector = selector.clone();
        move || {
            drawer
                .busy()
                .label(&selector)
                .map(|what| view! { <span class="detail-muted">{what}"…"</span> })
        }
    };

    view! {
        <div class="act-file" title=format!("{selector} → {oid}")>
            <span class="nf ctx-icon">{ic.stash}</span>
            <span class="act-file-path">{subject.subject}</span>
            {branch_pill}
            {automatic}
            <span class="detail-muted">{oid_short}</span>
        </div>
        <div class="act-file detail-muted">
            <span class="act-file-path">{time_ago(when)}</span>
        </div>
        <div class="act-meta">{buttons}{busy_label}</div>
        {patch_view}
    }
}

/// One offered action's control.
fn action_button(
    action: StashAction,
    selector: &str,
    oid: &str,
    drawer: StashDrawer,
    graph: RwSignal<crate::features::graph::core::GraphCore>,
) -> View {
    let selector = selector.to_string();
    let oid = oid.to_string();
    let label = action.label();

    // Disabled only while THIS row is mid-write. `locked` is per-selector on
    // purpose: the drawer lists many entries, and greying all of them out
    // because one is being dropped would be a claim about the others that is
    // not true.
    let locked_selector = selector.clone();
    let locked = move || drawer.busy().locked(&locked_selector);

    let on_click = move |_| {
        let selector = selector.clone();
        let oid = oid.clone();
        match action {
            // A read — no confirmation, no epoch bump, nothing to undo.
            StashAction::Inspect => drawer.toggle_inspect(&selector),
            StashAction::Apply => {
                drawer.begin(&selector, "applying");
                spawn_local(async move {
                    let result = crate::api::apply_stash_request(&selector, &oid).await;
                    drawer.set_notice(StashNotice::from_result(
                        result,
                        "Applied the stash. It is still in your list.",
                    ));
                    drawer.finish();
                    graph.update(|g| {
                        g.force_bump();
                    });
                });
            }
            StashAction::Pop => {
                drawer.begin(&selector, "popping");
                spawn_local(async move {
                    let key = crate::api::new_idempotency_key();
                    // Every decision about whether this finished belongs to
                    // the verdict `compose_pop` returns. Nothing here reads
                    // the HTTP results itself.
                    let verdict = compose_pop(&selector, &oid, key).await;
                    drawer.set_notice(StashNotice::from_pop(&verdict));
                    drawer.finish();
                    graph.update(|g| {
                        g.force_bump();
                    });
                });
            }
            StashAction::Branch => {
                let Some(win) = web_sys::window() else { return };
                let Some(name) = win
                    .prompt_with_message("Name for the new branch:")
                    .ok()
                    .flatten()
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty())
                else {
                    return;
                };
                drawer.begin(&selector, "branching");
                spawn_local(async move {
                    let result =
                        crate::api::branch_from_stash_request(&name, &selector, &oid).await;
                    drawer.set_notice(StashNotice::from_result(
                        result,
                        "Created the branch and applied the stash there.",
                    ));
                    drawer.finish();
                    graph.update(|g| {
                        g.force_bump();
                    });
                });
            }
            StashAction::Drop => {
                // Destructive and irreversible from the user's point of view,
                // so it asks first. The stash's commit stays recoverable until
                // gc via the recovery pin, which is what the wording says
                // rather than promising the change is undoable.
                let Some(win) = web_sys::window() else { return };
                let confirmed = win
                    .confirm_with_message(&format!(
                        "Drop {selector}?\n\nThis removes the entry from your stash list. \
                         The changes stay recoverable from the Recovery Centre until git \
                         garbage-collects them."
                    ))
                    .unwrap_or(false);
                if !confirmed {
                    return;
                }
                drawer.begin(&selector, "dropping");
                spawn_local(async move {
                    let key = crate::api::new_idempotency_key();
                    let receipt = crate::api::drop_stash_request(&selector, &oid, key).await;
                    // A receipt is not a success: `ok` is the HTTP status, and
                    // reading the outer `Ok` as "it worked" would report a
                    // refused drop as a completed one.
                    let result = match receipt {
                        Ok(r) if r.ok => Ok(()),
                        Ok(r) => Err(r.message),
                        Err(why) => Err(why),
                    };
                    drawer.set_notice(StashNotice::from_result(result, "Dropped the stash entry."));
                    drawer.finish();
                    graph.update(|g| {
                        g.force_bump();
                    });
                });
            }
        }
    };

    let class = if action.destructive() {
        "act-undo act-danger"
    } else {
        "act-undo"
    };

    view! {
        <button class=class prop:disabled=locked on:click=on_click>
            {label}
        </button>
    }
    .into_view()
}
