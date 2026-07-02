//! The SVG graph builders — edges, commit dots, the per-node icons, the two
//! label tiers, and the branch stubs.
//!
//! Everything here turns one row/edge of the laid-out [`Graph`] into SVG. The
//! builders are free functions (not closures) so `app.rs` can hand each one to a
//! virtualizing `<For>`; they read the graph and its derived lookups back out of
//! a shared [`RenderCtx`] behind a `StoredValue`, so a per-row closure never
//! clones the graph. Spatial math lives in [`crate::geometry`], colours in
//! [`crate::color`]; this module is just view assembly.

use std::collections::HashSet;

use leptos::*;

use git_vista_core::model::{Graph, RefKind};

use crate::color::{
    branch_color, branch_color_distinct_from, BADGE_DARK, HEAD_BADGE, MERGE_FILL, TAG_BADGE,
};
use crate::datetime::local_timestamp;
use crate::geometry::{
    badge_text_dx, badge_text_y, badge_top_y, badge_width, edge_path, label_bottom_y, label_top_y,
    node_cx, node_cy, stub_node_cy, stub_path, BADGE_GAP, BADGE_HEIGHT, BADGE_RADIUS, NODE_RADIUS,
};
use crate::icons::icon_set;
use crate::state::MenuData;
use crate::text::truncate;

/// Commit messages longer than this are truncated with an ellipsis in the label
/// (the full text stays available via the node/label hover tooltip).
const MAX_SUMMARY_CHARS: usize = 60;

/// Everything the per-row / per-edge view builders need, bundled behind a
/// `StoredValue` so the reactive `<For>` closures (Phase 8 viewport
/// virtualization) can reach the graph and its derived lookups cheaply — without
/// cloning the graph into each closure or rebuilding these tables per row.
pub struct RenderCtx {
    pub graph: Graph,
    /// Per-row branch-colour slot (row index → palette slot), so an edge can pick
    /// up the coloured line of the row it belongs to.
    pub row_color: Vec<usize>,
    /// Commit ids present on the remote, for the "is this pushed?" link gating.
    pub remote_set: HashSet<String>,
    /// Remote branch short-names, for gating local-branch links.
    pub remote_branches: HashSet<String>,
    /// GitHub web base (e.g. "https://github.com/owner/repo"), when this repo has
    /// a github.com origin; `None` => labels stay plain text.
    pub repo_url: Option<String>,
    /// Left edge (x) of the aligned label column.
    pub text_x: i32,
}

/// Cancel a link's navigation only when the "click" is actually the tail of a
/// drag/pan (desktop). Links are real SVG `<a target="_blank">` anchors, so a tap
/// is native link navigation — which works on iOS WebKit, where the scripted
/// `window.open` pop-ups we used before are silently blocked. `moved` is the
/// gesture's drag flag (set in pointermove).
pub fn suppress(moved: StoredValue<bool>, ev: web_sys::MouseEvent) {
    if moved.get_value() {
        ev.prevent_default();
    }
}

/// Indices of edges whose row span intersects the visible row window `[start,
/// end)`. An edge is kept whenever any part of it could cross the viewport — even
/// when both endpoints are off-screen (a long merge line passing through) — so an
/// edge never visibly disappears at the window's edge. Edges always run downward
/// (`from_row` < `to_row`), so the span is `[from_row, to_row]`.
pub fn visible_edges(ctx: StoredValue<RenderCtx>, range: (usize, usize)) -> Vec<usize> {
    let (start, end) = range;
    ctx.with_value(|c| {
        c.graph
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.from_row < end && e.to_row >= start)
            .map(|(i, _)| i)
            .collect()
    })
}

/// Per-edge view builder — invoked by a `<For>` only for edges whose row span
/// intersects the viewport. Colour a link by the branch *line* it belongs to,
/// so it matches the dots it connects:
///  * a first-parent link is part of the child's own branch — a side branch
///    forking off main is drawn in the side branch's colour all the way down to
///    its fork point, not main's blue;
///  * a merge link (any non-first parent) is part of the merged-in branch, so
///    it takes that parent's colour as it curves in.
/// Only main (colour slot 0) ever stays blue this way.
pub fn build_edge(ctx: StoredValue<RenderCtx>, ei: usize) -> View {
    ctx.with_value(|c| {
        let e = &c.graph.edges[ei];
        let d = edge_path(e);
        let child = &c.graph.rows[e.from_row].commit;
        let parent_oid = &c.graph.rows[e.to_row].commit.id;
        let is_first_parent = child.parents.first() == Some(parent_oid);
        let color_row = if is_first_parent { e.from_row } else { e.to_row };
        let color = branch_color(c.row_color[color_row]);
        view! {
            <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
        }
        .into_view()
    })
}

/// Per-commit node builder — a filled dot in the branch colour plus a larger
/// invisible hit target, built by a `<For>` only for rows in the viewport.
/// Every real commit (merges included) is a filled dot; a hollow ring is
/// reserved for branch stubs (a new branch with no commits of its own), so a
/// merge, which has real content, never reads as empty (Issue #30).
pub fn build_node(
    ctx: StoredValue<RenderCtx>,
    menu: RwSignal<Option<MenuData>>,
    moved: StoredValue<bool>,
    i: usize,
) -> View {
    ctx.with_value(|c| {
        let gr = &c.graph.rows[i];
        let cx = node_cx(gr.lane);
        let cy = node_cy(gr.row);
        let color = branch_color(gr.color);
        let fill = color;
        let stroke_width = "2";

        // Issue #18: tapping a dot opens a context menu. Gather this commit's
        // menu data now; the click handler clones it in (it may fire repeatedly).
        let commit_id = gr.commit.id.0.clone();
        let short = gr.commit.id.short().to_string();
        let title = format!("{} — {}", gr.commit.id.short(), gr.commit.summary);
        // Only the commit HEAD points at can take a new commit without moving
        // HEAD, so the "Commit …" items are enabled only here (Issue #33).
        let is_head = gr.refs.iter().any(|r| r.kind == RefKind::Head);
        // Local branch badges on this commit — each offers merge/push/delete
        // (Issue #33 follow-up). A commit can carry several; a bare commit none.
        let branches: Vec<String> = gr
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Branch)
            .map(|r| r.name.clone())
            .collect();
        // Link target only when the repo is on GitHub *and* this commit is
        // pushed — same rule the labels use, so the menu never offers a 404.
        let github_url = c.repo_url.as_ref().and_then(|base| {
            c.remote_set.contains(&commit_id).then(|| format!("{base}/commit/{commit_id}"))
        });
        let open_menu = move |ev: web_sys::MouseEvent| {
            // Ignore the click that ends a pan; a real tap opens the menu where
            // the pointer is (viewport coords for the fixed-position overlay).
            if moved.get_value() {
                return;
            }
            ev.stop_propagation();
            menu.set(Some(MenuData {
                commit: commit_id.clone(),
                header: short.clone(),
                x: ev.client_x() as f64,
                y: ev.client_y() as f64,
                github_url: github_url.clone(),
                github_label: "Open commit on GitHub",
                create_label: "Create branch from this commit",
                is_head,
                branches: branches.clone(),
                // A commit dot: the menu header shows the commit glyph.
                is_branch: false,
            }));
        };

        view! {
            <g>
                <circle
                    cx=cx
                    cy=cy
                    r=NODE_RADIUS
                    fill=fill
                    stroke=color
                    stroke-width=stroke_width
                >
                    <title>{title}</title>
                </circle>
                // A larger, invisible hit target on top so the small dot is easy
                // to tap (especially on the iPad). `transparent` (not `none`) so
                // it still receives the click.
                <circle
                    cx=cx
                    cy=cy
                    r=NODE_RADIUS + 8
                    fill="transparent"
                    class="node-hit"
                    on:click=open_menu
                />
            </g>
        }
        .into_view()
    })
}

/// Per-node icons: a small glyph just left of each commit dot, in the dot's
/// own branch colour — the merge glyph for merges, the commit glyph
/// otherwise. A separate builder (not part of build_node) so the icons live
/// in their own <g>, shown/hidden as one layer by the "Dot icons" toggle
/// without touching the dots themselves.
pub fn build_node_icon(ctx: StoredValue<RenderCtx>, nerd_icons: RwSignal<bool>, i: usize) -> View {
    ctx.with_value(|c| {
        // Untracked read, same as the other builders: the <For> keys carry
        // the icon mode, so a toggle rebuilds the rows.
        let ic = icon_set(nerd_icons.get_untracked());
        let gr = &c.graph.rows[i];
        let icon = if gr.commit.parents.len() > 1 { ic.merge } else { ic.commit };
        view! {
            <text
                x=node_cx(gr.lane) - NODE_RADIUS - 5
                y=node_cy(gr.row) + 4
                text-anchor="end"
                class="nf node-icon"
                fill=branch_color(gr.color)
            >
                {icon}
            </text>
        }
        .into_view()
    })
}

/// Stub tips get the branch glyph beside their hollow ring, in the same
/// toggleable layer. Stubs are few and eager (see [`stubs`]), so this is
/// a plain reactive closure — reading the icon signal re-renders on toggle.
pub fn stub_icons(ctx: StoredValue<RenderCtx>, nerd_icons: RwSignal<bool>) -> impl IntoView {
    move || {
        let ic = icon_set(nerd_icons.get());
        ctx.with_value(|c| {
            c.graph
                .stubs
                .iter()
                .map(|s| {
                    // Same colour rule as the stub's own line/ring (Issue #30).
                    let anchor_slot = c.graph.rows[s.anchor_row].color;
                    let color = branch_color_distinct_from(s.color, anchor_slot);
                    view! {
                        <text
                            x=node_cx(s.lane) - NODE_RADIUS - 5
                            y=stub_node_cy(s.anchor_row, s.depth) + 4
                            text-anchor="end"
                            class="nf node-icon"
                            fill=color
                        >
                            {ic.branch}
                        </text>
                    }
                })
                .collect_view()
        })
    }
}

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
        // The row's branch colour — the label column takes the colour of the
        // dot it describes, so a row reads as one unit across the canvas.
        let row_color = branch_color(gr.color);
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
                // Branch badges take their branch's colour (filled for local,
                // outlined for remote); HEAD and tags get fixed colours.
                let branch = branch_color(gr.color);
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
                fill=branch_color(gr.color)
            >
                <tspan class="nf">{ic.commit}</tspan>
                {meta}
            </text>
        }
        .into_view()
    })
}

/// Branch stubs: a local branch with no commits of its own (e.g. one just
/// created from an existing commit) is drawn GitHub-network-graph style — a
/// short, uniquely-coloured line forking off its commit into its own lane,
/// with the branch badge on the fork tip, instead of a second badge crowding
/// the shared commit.
///
/// Branch stubs (Phase 8: kept eager and always rendered). There are only a
/// handful — one per commit-less new branch — and their cascade fans *upward*
/// off the anchor commit, so they don't map onto the row window as cleanly as
/// nodes/edges/labels; rendering them all is cheap and avoids that edge case.
pub fn stubs(
    ctx: StoredValue<RenderCtx>,
    menu: RwSignal<Option<MenuData>>,
    moved: StoredValue<bool>,
) -> View {
    ctx.with_value(|c| c.graph
        .stubs
        .iter()
        .map(|s| {
            // A new branch must never share the colour of the branch it forked off
            // (Issue #30): the palette collapses many slots onto few colours, so a
            // stub's raw slot can collide with its anchor branch's colour. Bump it
            // until it differs from the anchor commit's colour.
            let anchor_slot = c.graph.rows[s.anchor_row].color;
            let color = branch_color_distinct_from(s.color, anchor_slot);
            let d = stub_path(s.anchor_lane, s.anchor_row, s.lane, s.depth);
            let sx = node_cx(s.lane);
            let sy = stub_node_cy(s.anchor_row, s.depth);
            let name = s.name.clone();

            // The stub is a *branch*, not the commit it happens to sit on, so its
            // menu takes the branch's identity (Issue #30): the header is the
            // branch name and "Open on GitHub" goes to the branch's tree page (only
            // when a remote branch of the same name exists, so it never 404s) — the
            // same rule the branch badges use. "Create branch" still targets the
            // stub's tip commit, so forking from the stub forks off that commit
            // (Issue #24).
            let anchor = &c.graph.rows[s.anchor_row].commit;
            let commit_id = anchor.id.0.clone();
            let header = s.name.clone();
            let branch_name = s.name.clone();
            let github_url = c.repo_url.as_ref().and_then(|base| {
                c.remote_branches
                    .contains(&s.name)
                    .then(|| format!("{base}/tree/{}", s.name))
            });
            let open_menu = move |ev: web_sys::MouseEvent| {
                // Ignore the click that ends a pan; a real tap opens the menu.
                if moved.get_value() {
                    return;
                }
                ev.stop_propagation();
                menu.set(Some(MenuData {
                    commit: commit_id.clone(),
                    header: header.clone(),
                    x: ev.client_x() as f64,
                    y: ev.client_y() as f64,
                    github_url: github_url.clone(),
                    github_label: "Open branch on GitHub",
                    create_label: "Create branch from this branch",
                    // A stub is a new empty branch, never the HEAD tip.
                    is_head: false,
                    // The stub *is* one branch, so its ops act on that single name.
                    branches: vec![branch_name.clone()],
                    // …and its menu header shows the branch glyph, not the commit's.
                    is_branch: true,
                }));
            };
            view! {
                <path
                    d=d
                    fill="none"
                    stroke=color
                    stroke-width="2"
                    stroke-linecap="round"
                />
                // Hollow, clickable ring (Issue #28) — a stub branch owns no
                // commits of its own yet, so it reads as an empty ring in the
                // branch's colour rather than a filled dot, signalling "nothing
                // committed here yet" at a glance. Still tappable to branch from.
                <circle cx=sx cy=sy r=NODE_RADIUS fill=MERGE_FILL stroke=color stroke-width="2">
                    <title>{format!("{name} — new branch (no commits yet); tap to branch from here")}</title>
                </circle>
                // A larger, invisible hit target on top so the tip is easy to tap,
                // exactly like the commit dots.
                <circle
                    cx=sx
                    cy=sy
                    r=NODE_RADIUS + 8
                    fill="transparent"
                    class="node-hit"
                    on:click=open_menu
                />
            }
        })
        .collect_view())
}
