//! The worktree drawer's markup — wasm only (M11.03, #548).
//!
//! Mounted as a section of the Activity panel, beside the stash drawer and the
//! tag list it shares a refresh key with.
//!
//! # This file decides nothing
//!
//! It is `#[cfg(target_arch = "wasm32")]`, so `cargo test --workspace` never
//! compiles a line of it and no host test can reach anything written here.
//! Every branch below is a one-to-one mapping from a value
//! [`crate::features::worktrees::core`] already computed — a `RowOffer` to a
//! `<button>` or a paragraph, a `FactSource` to a CSS modifier. In particular:
//!
//! - whether a row can be opened is [`RowOffer::is_actionable`], never a
//!   `Serviceable` match here;
//! - why a refused row is refused is the `reason` the core carried out of
//!   `Serviceable::refusal`, never a sentence written in this file;
//! - what a row has checked out is `BranchCell::label`.
//!
//! `core_suite.rs`'s source census reads this file back and pins those, because
//! a mapping written here is a mapping no `cargo test` can run (ADR 0115).
//!
//! # No new CSS, and the modifier reuse is deliberate
//!
//! `.act-pill.act-terminal` and `.act-pill.act-app` already exist and already
//! mean exactly what this drawer needs them to. `styles.css` documents them as
//! *"app/terminal attribution. app = accent (done through git-vista),
//! terminal = muted (done outside it)"* — which is the same axis as this
//! feature's: git's `locked`/`prunable`/`bare` are facts from outside this
//! application, and `Serviceable` is this application's own verdict. Reusing
//! the pair gives the two kinds of claim visibly different colours with no new
//! rule, no new `:focus-visible` twin, and no new entry in
//! `features::a11y::audit`'s censuses.
//!
//! The one interactive element is `.act-undo`, which already carries the 44x44
//! floor (#65) and its focus-visible twin.
//!
//! # The refusal is a paragraph, not a tooltip
//!
//! #548's acceptance says so in as many words, and #65 is why: a reason
//! carried only in `title=` never surfaces on a tap and is never announced.
//! The stash drawer's `Availability::Refused` arm uses `title=`; this one
//! deliberately does not follow it.

use leptos::*;

use git_vista_protocol::RepoMode;

use crate::api::{fetch_worktree_census, select_worktree_request};
use crate::features::dialogs::core::ErrorNotice;
use crate::features::session::signals as session_state;
use crate::features::worktrees::core::{
    drawer_view, DrawerView, FactSource, RowFact, RowOffer, WorktreeRow,
};
use crate::state::Features;

/// The drawer's landmark label. A named region, not a bare div — beyond the
/// ordinary accessibility argument, it is what lets the browser spec assert
/// about *this drawer* rather than about the page, the same reason the stash
/// drawer carries one.
pub const DRAWER_REGION_LABEL: &str = "Worktrees";

/// Shown while the census read is in flight.
const LOADING: &str = "Loading worktrees…";

/// The drawer: one row per worktree this repository has.
pub fn worktree_section_view(features: Features) -> impl IntoView {
    let Features { graph, shell, .. } = features;

    // Keyed exactly like the stash drawer beside it: the panel's open state
    // and the graph epoch. A successful switch bumps the epoch, so the drawer
    // re-reads and the row that offered "Open" becomes "you are here" without
    // a manual refresh — and nothing is fetched at all while the panel is shut.
    let census = create_local_resource(
        move || (shell.activity_is_open(), graph.get().epoch()),
        |(open, _)| async move {
            if open {
                Some(fetch_worktree_census().await)
            } else {
                None
            }
        },
    );

    let rows_view = move || match census.get().flatten() {
        None => view! { <p class="detail-status">{LOADING}</p> }.into_view(),
        Some(fetched) => match drawer_view(fetched) {
            // Nothing is decided here: `unreadable_paragraphs` owns both the
            // sentence and whether the path-bearing half appears, so that
            // decision is host-tested in `core.rs` rather than living in a
            // module `cargo test` never compiles (#658, ADR 0115).
            view @ DrawerView::Unreadable { .. } => view
                .unreadable_paragraphs()
                .into_iter()
                .map(|line| view! { <p class="detail-status detail-error">{line}</p> })
                .collect_view(),
            DrawerView::Rows(rows) => rows
                .into_iter()
                .map(|row| worktree_row_view(row, features))
                .collect_view(),
        },
    };

    view! {
        <section class="worktree-drawer" aria-label=DRAWER_REGION_LABEL>
            <div class="detail-section-title act-feed-title">{DRAWER_REGION_LABEL}</div>
            {rows_view}
        </section>
    }
}

/// One badge. The modifier is chosen from the fact's own `source`, so a git
/// flag can never be rendered wearing the app's colour.
fn fact_pill(fact: RowFact) -> View {
    let modifier = match fact.source {
        FactSource::Git => "act-pill act-terminal",
        FactSource::App => "act-pill act-app",
    };
    view! { <span class=modifier>{fact.label}</span> }.into_view()
}

/// One row: which desk, what it holds, what git says, what this app says, and
/// what it offers.
fn worktree_row_view(row: WorktreeRow, features: Features) -> View {
    let WorktreeRow {
        name,
        path,
        branch,
        head,
        is_current,
        git_facts,
        app_fact,
        offer,
    } = row;

    let branch_label = branch.label();
    let git_pills = git_facts.into_iter().map(fact_pill).collect_view();
    let app_pill = fact_pill(app_fact);
    let here = is_current.then(|| view! { <span class="act-pill act-ref">"you are here"</span> });
    let head_cell = head.map(|oid| view! { <span class="act-meta">{oid}</span> });
    // Only when the operator opted into path exposure; the row is complete
    // without it, because every action is by id.
    let path_cell = path.map(|p| view! { <div class="act-meta">{p}</div> });

    let action = match offer {
        // Nothing to switch to. Not a disabled button — there is no action
        // being withheld, so offering one greyed out would invent a refusal.
        RowOffer::Current => View::default(),
        // Visible text, never a tooltip. See the module doc.
        RowOffer::Refused { reason } => {
            view! { <p class="detail-status detail-error">{reason}</p> }.into_view()
        }
        RowOffer::Open { id } => open_button(id, name.clone(), features).into_view(),
    };

    view! {
        <div class="act-file">
            <div>
                <span class="act-pill act-ref">{name}</span>
                <span class="act-meta">{branch_label}</span>
                {head_cell}
                {here}
            </div>
            <div>{git_pills}{app_pill}</div>
            {path_cell}
            {action}
        </div>
    }
    .into_view()
}

/// The one control in this drawer.
fn open_button(id: String, name: String, features: Features) -> impl IntoView {
    let Features { graph, shell, .. } = features;
    let label = format!("Open ‘{name}’");
    let on = move |_| {
        let id = id.clone();
        let name = name.clone();
        spawn_local(async move {
            // The posture the session is already in, and never an escalation:
            // switching desks must not be a way to acquire Active mode. The
            // same rule M11.02's "Open Worktree" offer follows.
            let mode = session_state::ui_mode().unwrap_or(RepoMode::Visualize);
            match select_worktree_request(&id, mode).await {
                Ok(()) => graph.update(|g| {
                    g.force_bump();
                }),
                Err(e) => shell.open_error(ErrorNotice {
                    title: "Couldn't open that worktree",
                    body: format!("‘{name}’ could not be opened: {e}"),
                }),
            }
        });
    };
    view! {
        <button class="act-undo" on:click=on>{label}</button>
    }
}
