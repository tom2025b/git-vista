//! The two-commit comparison items (M4.27, #80).
//!
//! Before this, nothing in the UI could construct a
//! [`DiffSpec::CommitVsCommit`] at all: the variant existed, was argv-tested
//! and had a viewer title arm, but no user gesture produced one. "Compare with
//! HEAD" on a branch stub was the only comparison reachable, and one of its two
//! endpoints was fixed.
//!
//! # Why an anchor rather than a mode
//!
//! Picking two things needs somewhere to keep the first one. A modal "compare
//! mode" would put the whole graph into a state where every click means
//! something different, and one the user can forget they are in. An anchor is
//! visible in the menu of every other commit ("Compare with 1a2b3c4…"), names
//! itself, and can be cleared from the commit that set it — so the state is
//! always legible from the thing in front of you.

use leptos::*;

use crate::features::graph::core::{offer_for, CompareOffer};
use crate::icons::GitIcons;
use crate::state::{Features, MenuData, ViewerDoc};
use git_vista_protocol::diff::{ComparisonBasis, DiffSpec};
use git_vista_protocol::CommitOid;

/// The conventional 7-char short id, matching the server's own truncation.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Builds the comparison items for one commit's context menu.
///
/// Returns `(anchor_or_clear, compare_direct, compare_since_merge_base)`. The
/// two compare items render empty when no anchor is set, or when the anchor IS
/// this commit — comparing a commit with itself is an empty diff and offering
/// it would be a dead end dressed as an action.

pub(super) fn build_compare_items(
    features: Features,
    ic: &'static GitIcons,
    m: &MenuData,
) -> (View, View, View) {
    let Features {
        shell,
        compare_anchor,
        ..
    } = features;
    let this = m.commit.clone();
    let anchor = compare_anchor.get_untracked();

    let offer = offer_for(anchor.as_deref(), &this);
    let is_anchor = offer == CompareOffer::ClearAnchor;

    // Item 1: set the anchor, or clear it if this commit already is it.
    let anchor_item = {
        let this = this.clone();
        let on = move |_| {
            shell.close_menu();
            if is_anchor {
                compare_anchor.set(None);
            } else {
                compare_anchor.set(Some(this.clone()));
            }
        };
        let label = if is_anchor {
            "Clear comparison anchor".to_string()
        } else {
            "Compare from here".to_string()
        };
        view! {
            <button class="ctx-item" on:click=on>
                <span class="nf ctx-icon">{ic.diff}</span>
                {label}
            </button>
        }
        .into_view()
    };

    // Items 2 and 3: only once a DIFFERENT commit is anchored.
    let (direct, since) = match offer {
        CompareOffer::Compare { base: a, .. } => {
            let mk = move |basis: ComparisonBasis, label: String| {
                let a = a.clone();
                let this = this.clone();
                let on = move |_| {
                    shell.close_menu();
                    // A hash straight from `MenuData` should always parse; if it
                    // somehow does not, do nothing rather than open a viewer on a
                    // comparison we could not build.
                    let (Ok(base), Ok(target)) =
                        (CommitOid::new(a.clone()), CommitOid::new(this.clone()))
                    else {
                        return;
                    };
                    shell.open_viewer(ViewerDoc::Spec {
                        spec: DiffSpec::CommitVsCommit {
                            base,
                            target,
                            basis,
                        },
                    });
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        <span class="nf ctx-icon">{ic.diff}</span>
                        {label}
                    </button>
                }
                .into_view()
            };
            let a_short = short(&a).to_string();
            (
                mk(ComparisonBasis::Direct, format!("Compare with ‘{a_short}’")),
                // The merge-base question, reachable from the UI for the first
                // time. `ComparisonBasis` has carried it since ADR 0062 and no
                // gesture produced it — "what did this branch add since they
                // diverged" is a different question from "what differs now",
                // and the two produce patches that look alike.
                mk(
                    ComparisonBasis::SinceMergeBase,
                    format!("Compare with ‘{a_short}’ since they diverged"),
                ),
            )
        }
        _ => (().into_view(), ().into_view()),
    };

    (anchor_item, direct, since)
}
