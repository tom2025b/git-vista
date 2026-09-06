//! The bisect menu items (M5.34, #87, ADR 0131): mark a commit bad to start,
//! mark another good to launch the search, then good/bad/skip the candidate
//! git checks out, or reset.
//!
//! # An anchor, the same shape `compare_items` uses, for the same reason
//!
//! Starting a bisect needs two commits — a known-bad and a known-good — and
//! nothing else in this menu system holds "the first of two picks" as a
//! visible, nameable piece of state. `compare_items::build_compare_items`
//! solved the identical problem for "compare with": an anchor set on one
//! commit's menu, named in every other commit's menu ("Start bisect: this
//! commit is good"), and clearable from the commit that set it. Copying that
//! shape rather than inventing a bisect-specific one keeps the state legible
//! the same way.
//!
//! # No client-side "is a bisect in progress" tracking
//!
//! Deliberately absent, matching ADR 0131's whole thesis: the app reads
//! git's own state rather than mirroring it. Mark/reset are offered
//! unconditionally; the executor answers 409 ("no bisect in progress") or
//! 200 ("nothing to reset") itself, and the existing `ErrorNotice` path
//! shows a 409 exactly like any other write refusal. A client-side flag here
//! would be exactly the kind of second, driftable copy of git's own fact
//! this app's own architecture argues against.

use leptos::*;

use crate::api::{bisect_mark_request, bisect_reset_request, bisect_start_request};
use crate::features::dialogs::core::{Dialog, ErrorNotice};
use crate::icons::GitIcons;
use crate::state::{Features, MenuData};
use git_vista_protocol::plan::BisectVerdict;
use git_vista_protocol::CommitOid;

/// Builds the bisect items for one commit's context menu.
///
/// Returns `(bad_anchor_or_clear, start_here_as_good, mark_good, mark_bad,
/// mark_skip, reset)`. `start_here_as_good` renders empty when no bad anchor
/// is set, or when the anchor IS this commit — starting a bisect between a
/// commit and itself is an empty range.
pub(super) fn build_bisect_items(
    features: Features,
    ic: &'static GitIcons,
    m: &MenuData,
) -> (View, View, View, View, View, View) {
    let Features {
        shell,
        dialogs,
        bisect_bad_anchor,
        ..
    } = features;
    let this = m.commit.clone();
    let anchor = bisect_bad_anchor.get_untracked();
    let is_anchor = anchor.as_deref() == Some(this.as_str());

    let report_error = move |title: &'static str, body: String| {
        dialogs.open(Dialog::Error);
        shell.open_error(ErrorNotice { title, body });
    };

    // Item 1: mark this commit bad (sets the anchor), or clear it if this
    // commit already is the anchor.
    let anchor_item = {
        let this = this.clone();
        let on = move |_| {
            shell.close_menu();
            if is_anchor {
                bisect_bad_anchor.set(None);
            } else {
                bisect_bad_anchor.set(Some(this.clone()));
            }
        };
        let label = if is_anchor {
            "Clear bisect bad mark"
        } else {
            "Mark bad (start bisect here)"
        };
        view! {
            <button class="ctx-item" on:click=on>
                <span class="nf ctx-icon">{ic.history}</span>
                {label}
            </button>
        }
        .into_view()
    };

    // Item 2: only once a DIFFERENT commit is anchored as bad.
    let start_item = match &anchor {
        Some(bad) if bad.as_str() != this.as_str() => {
            let bad = bad.clone();
            let this = this.clone();
            let on = move |_| {
                shell.close_menu();
                let (Ok(bad), Ok(good)) =
                    (CommitOid::new(bad.clone()), CommitOid::new(this.clone()))
                else {
                    return;
                };
                bisect_bad_anchor.set(None);
                spawn_local(async move {
                    if let Err(e) = bisect_start_request(bad, vec![good]).await {
                        report_error("Couldn't start bisect", e);
                    }
                });
            };
            view! {
                <button class="ctx-item" on:click=on>
                    <span class="nf ctx-icon">{ic.history}</span>
                    "Start bisect: this commit is good"
                </button>
            }
            .into_view()
        }
        _ => ().into_view(),
    };

    // Items 3-5: mark the current candidate. Always offered — see the module
    // doc for why there is no client-side "is one in progress" gate.
    let mark = move |verdict: BisectVerdict, label: &'static str| {
        let on = move |_| {
            shell.close_menu();
            spawn_local(async move {
                if let Err(e) = bisect_mark_request(verdict).await {
                    report_error("Couldn't mark bisect candidate", e);
                }
            });
        };
        view! {
            <button class="ctx-item" on:click=on>
                <span class="nf ctx-icon">{ic.history}</span>
                {label}
            </button>
        }
        .into_view()
    };
    let mark_good_item = mark(BisectVerdict::Good, "Bisect: mark good");
    let mark_bad_item = mark(BisectVerdict::Bad, "Bisect: mark bad");
    let mark_skip_item = mark(BisectVerdict::Skip, "Bisect: skip (can't test)");

    // Item 6: end the bisect. Always offered, always safe — see the module
    // doc; the executor answers 200 "nothing to reset" rather than refusing.
    let reset_item = {
        let on = move |_| {
            shell.close_menu();
            bisect_bad_anchor.set(None);
            spawn_local(async move {
                if let Err(e) = bisect_reset_request().await {
                    report_error("Couldn't reset bisect", e);
                }
            });
        };
        view! {
            <button class="ctx-item" on:click=on>
                <span class="nf ctx-icon">{ic.history}</span>
                "Bisect: reset"
            </button>
        }
        .into_view()
    };

    (
        anchor_item,
        start_item,
        mark_good_item,
        mark_bad_item,
        mark_skip_item,
        reset_item,
    )
}
