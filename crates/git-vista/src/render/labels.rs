//! The label layer: the two per-row text tiers to the right of the graph.
//!
//! Split out of `render.rs`. [`build_msg`] is the message tier — the ref badges
//! laid out left-to-right then the (linkable, truncated) commit message — and
//! [`build_meta`] is the dimmed `hash · author · date` tier. They're independent
//! builders so the view can hide each at a different zoom and a `<For>` can build
//! each only for on-screen rows.

use leptos::*;

use git_vista_core::model::RefKind;

use git_vista_core::color::{branch_color, BADGE_DARK, HEAD_BADGE, TAG_BADGE};
use crate::datetime::local_timestamp;
use crate::geometry::{
    badge_text_dx, badge_text_y, badge_top_y, badge_width, label_bottom_y, label_top_y, BADGE_GAP,
    BADGE_HEIGHT, BADGE_RADIUS,
};
use crate::icons::icon_set;
use crate::text::truncate;

use super::{suppress, RenderCtx};

/// Commit messages longer than this are truncated with an ellipsis in the label
/// (the full text stays available via the node/label hover tooltip).
const MAX_SUMMARY_CHARS: usize = 60;

/// Commit labels — message tier: any ref badges laid out left-to-right from the
/// label column, then the (truncated, linkable) commit message just past them.
///
/// Phase 9 (level of detail) + Phase 8 (virtualization): the two label lines are
/// two independent builders — this message tier (badges + message) and the
/// dimmed [`build_meta`] tier — so the view can hide each at a different zoom
/// (LOD) and a `<For>` can build each only for the rows on screen. They're
/// independent because the meta line doesn't depend on the badge layout.
pub fn build_msg(
    ctx: StoredValue<RenderCtx>,
    nerd_icons: RwSignal<bool>,
    moved: StoredValue<bool>,
    i: usize,
) -> View {
    ctx.with_value(|c| {
        // Untracked read, same as build_node: the <For> keys carry the icon
        // mode, so a toggle rebuilds the rows.
        let ic = icon_set(nerd_icons.get_untracked());
        let gr = &c.graph.rows[i];
        let mut bx = c.text_x;
        // The row's label colour. Normally the commit's own branch colour, so the
        // label matches the dot it describes. But when an open-circle stub — a
        // branch with no commits of its own — forks off this row, the label follows
        // that branch's colour instead, faded, so the empty-branch row reads as its
        // hollow ring rather than the line it happens to sit on.
        let stub_slot = c.graph.stubs.iter().find(|s| s.anchor_row == gr.row).map(|s| s.color);
        let row_color = branch_color(stub_slot.unwrap_or(gr.color));
        let faded = stub_slot.is_some();
        // Is this row's commit on the remote? Drives whether its message, HEAD
        // badge and tag badges link out (an unpushed commit would 404).
        let commit_on_remote = c.remote_set.contains(&gr.commit.id.0);
        let badges = gr
            .refs
            .iter()
            .map(|r| {
                // Each badge leads with its kind's glyph (icons.rs): local
                // branches get the branch icon, remote branches the alternate
                // one — so local vs remote pills differ at a glance even
                // before reading the name — tags the tag icon, and HEAD the
                // commit icon (it marks the commit you're on). The glyph
                // counts into the pill's width like any other monospace char.
                let icon = match r.kind {
                    RefKind::Head => ic.commit,
                    RefKind::Tag => ic.tag,
                    RefKind::Branch => ic.branch,
                    RefKind::RemoteBranch => ic.branch_alt,
                };
                let w = badge_width(&format!("{icon} {}", r.name));
                let x = bx;
                bx += w + BADGE_GAP;
                // Branch badges take the row's label colour (filled for local,
                // outlined for remote); HEAD and tags get fixed colours.
                let branch = row_color;
                let (fill, stroke, text_fill) = match r.kind {
                    RefKind::Head => (HEAD_BADGE, HEAD_BADGE, BADGE_DARK),
                    RefKind::Tag => (TAG_BADGE, TAG_BADGE, BADGE_DARK),
                    RefKind::Branch => (branch, branch, BADGE_DARK),
                    RefKind::RemoteBranch => ("none", branch, branch),
                };
                let name = r.name.clone();
                // Where this badge links on GitHub (Issue #12) — but only when
                // the target is actually on the remote, so a tap never 404s:
                //  * HEAD / tag -> the commit they sit on, when it's pushed. (A
                //    tag's own page can't be verified offline, so we link the
                //    commit it points at, which resolves whenever it's pushed.)
                //  * local branch -> its tree page, only if a remote branch of
                //    the same name exists.
                //  * remote branch -> its tree page (it's on the remote by
                //    definition); its leading "<remote>/" is stripped.
                let badge_url = c.repo_url.as_ref().and_then(|base| match r.kind {
                    RefKind::Head | RefKind::Tag => {
                        commit_on_remote.then(|| format!("{base}/commit/{}", gr.commit.id.0))
                    }
                    RefKind::Branch => c
                        .remote_branches
                        .contains(&r.name)
                        .then(|| format!("{base}/tree/{}", r.name)),
                    RefKind::RemoteBranch => {
                        let branch = r.name.split_once('/').map_or(r.name.as_str(), |(_, b)| b);
                        Some(format!("{base}/tree/{branch}"))
                    }
                });
                let clickable = badge_url.is_some();
                // A GitHub repo where this ref simply isn't pushed: show it, but
                // dimmed and unlinked, so it's clear it has no GitHub page yet.
                let unpushed = c.repo_url.is_some() && badge_url.is_none();
                let pill = view! {
                    <rect
                        x=x
                        y=badge_top_y(gr.row)
                        width=w
                        height=BADGE_HEIGHT
                        rx=BADGE_RADIUS
                        ry=BADGE_RADIUS
                        fill=fill
                        stroke=stroke
                        stroke-width="1"
                        class:clickable=clickable
                        class:unpushed=unpushed
                    />
                    <text
                        x=x + badge_text_dx()
                        y=badge_text_y(gr.row)
                        class="badge-text"
                        class:clickable=clickable
                        class:unpushed=unpushed
                        fill=text_fill
                    >
                        // The kind glyph, then the name. The tspan only swaps
                        // the font stack (.nf); it inherits the pill's text fill,
                        // so the icon never fights the badge colour.
                        <tspan class="nf">{icon}</tspan>
                        {format!(" {name}")}
                    </text>
                };
                // Wrap in a real SVG anchor when this repo has a GitHub base.
                // The `<g>` puts the ambiguous `<a>` in an SVG-parent context so
                // Leptos resolves it to the SVG-namespaced anchor (an HTML `<a>`
                // wouldn't navigate inside the SVG tree).
                match badge_url {
                    Some(url) => view! {
                        <g>
                            <a href=url target="_blank" rel="noopener" on:click=move |ev| suppress(moved, ev)>
                                {pill}
                            </a>
                        </g>
                    }
                    .into_view(),
                    None => pill.into_view(),
                }
            })
            .collect_view();
        let msg_x = bx; // past the last badge, or text_x when there were none

        let msg = truncate(&gr.commit.summary, MAX_SUMMARY_CHARS);
        // The message links to the commit page on GitHub (Issue #12), but only
        // when the commit is on the remote — otherwise it's dimmed and the
        // tooltip says why, rather than linking to a page that would 404.
        let msg_url = c.repo_url.as_ref().and_then(|base| {
            commit_on_remote.then(|| format!("{base}/commit/{}", gr.commit.id.0))
        });
        let msg_clickable = msg_url.is_some();
        let msg_unpushed = c.repo_url.is_some() && msg_url.is_none();
        let title = if msg_unpushed {
            format!("{} — not pushed to GitHub", gr.commit.summary)
        } else {
            gr.commit.summary.clone()
        };
        let msg_text = view! {
            <text
                x=msg_x
                y=label_top_y(gr.row)
                class="label-msg"
                class:clickable=msg_clickable
                class:unpushed=msg_unpushed
                class:faded=faded
                fill=row_color
            >
                {msg}
                <title>{title}</title>
            </text>
        };
        // Same SVG-anchor wrapping as the badges, so the commit link is a real
        // tap-navigable link rather than a pop-up.
        let msg_view = match msg_url {
            Some(url) => view! {
                <g>
                    <a href=url target="_blank" rel="noopener" on:click=move |ev| suppress(moved, ev)>
                        {msg_text}
                    </a>
                </g>
            }
            .into_view(),
            None => msg_text.into_view(),
        };
        view! {
            {badges}
            {msg_view}
        }
        .into_view()
    })
}

/// Commit labels — meta tier: the dimmed `hash · author · local date+time` line,
/// so the timeline is visible per row. Independent of the badge layout, so it
/// doesn't recompute the badges.
pub fn build_meta(ctx: StoredValue<RenderCtx>, nerd_icons: RwSignal<bool>, i: usize) -> View {
    ctx.with_value(|c| {
        // Untracked read, same as build_node: the <For> keys carry the icon
        // mode, so a toggle rebuilds the rows.
        let ic = icon_set(nerd_icons.get_untracked());
        let gr = &c.graph.rows[i];
        // Same open-circle rule as build_msg: a stub's anchor row takes the stub's
        // branch colour instead of the line it sits on (the meta line's own opacity
        // already gives it the faded, secondary look).
        let stub_slot = c.graph.stubs.iter().find(|s| s.anchor_row == gr.row).map(|s| s.color);
        let meta = format!(
            " {} · {} · {}",
            gr.commit.id.short(),
            gr.commit.author,
            local_timestamp(gr.commit.time),
        );
        view! {
            // The commit glyph leads the meta line, marking each entry of the
            // "commit list" tier. Like the message, the line takes its row's
            // branch colour (the glyph inherits it), faded to stay secondary
            // (see .label-meta's opacity).
            <text
                x=c.text_x
                y=label_bottom_y(gr.row)
                class="label-meta"
                fill=branch_color(stub_slot.unwrap_or(gr.color))
            >
                <tspan class="nf">{ic.commit}</tspan>
                {meta}
            </text>
        }
        .into_view()
    })
}
