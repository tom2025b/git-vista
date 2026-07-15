//! The branch-stub layer: the forked lines, hollow rings, name labels, and tap
//! menus for local branches that own no commits of their own — plus their glyphs.
//!
//! Split out of `render.rs`. [`stubs`] draws the whole cascade (connectors, then
//! rings + hit targets) GitHub-network-graph style; [`stub_icons`] draws the
//! branch glyph beside each ring in the same toggleable layer as the node icons.

use leptos::*;

use crate::geometry::{node_cx, stub_node_cy, stub_path, NODE_RADIUS};
use crate::icons::icon_set;
use crate::state::MenuData;
use crate::text::truncate;
use git_vista_core::color::{branch_color, MERGE_FILL};

use super::RenderCtx;

/// A stub's branch-name label is truncated past this (the full name stays in
/// the ring's hover tooltip and the menu header). Short enough that a deep
/// cascade's label can't reach far across the message column.
const MAX_STUB_NAME_CHARS: usize = 24;

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
                    // Same colour rule as the stub's own line/ring: the branch
                    // name's stable colour.
                    let color = branch_color(s.color);
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
    // Two passes: every connector path first, then every ring + hit target. A
    // cascade's deeper connector starts exactly at the previous tip's centre
    // (and the first at the anchor commit's), so drawn interleaved it would
    // paint over the ring below it — and, worse, sit on top of that ring's hit
    // circle, swallowing taps aimed dead-centre. The paths are decorative, so
    // they also get pointer-events:none; belt and braces with the ordering.
    let paths = ctx.with_value(|c| {
        c.graph
            .stubs
            .iter()
            .map(|s| {
                let color = branch_color(s.color);
                let d = stub_path(s.anchor_lane, s.anchor_row, s.lane, s.depth);
                view! {
                    <path
                        d=d
                        fill="none"
                        stroke=color
                        stroke-width="2"
                        stroke-linecap="round"
                        pointer-events="none"
                    />
                }
            })
            .collect_view()
    });
    let tips = ctx.with_value(|c| c.graph
        .stubs
        .iter()
        .map(|s| {
            // The branch name's stable colour — the same colour this branch's
            // line will wear once it owns commits, so committing on the stub
            // reads as the stub growing into its line.
            let color = branch_color(s.color);
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
            // The repo's GitHub base, for the menu's "Create Pull Request" link.
            let repo_url = c.repo_url.clone();
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
                    repo_url: repo_url.clone(),
                }));
            };
            view! {
                // Hollow, clickable ring (Issue #28) — a stub branch owns no
                // commits of its own yet, so it reads as an empty ring in the
                // branch's colour rather than a filled dot, signalling "nothing
                // committed here yet" at a glance. Still tappable to branch from.
                <circle cx=sx cy=sy r=NODE_RADIUS fill=MERGE_FILL stroke=color stroke-width="2">
                    <title>{format!("{name} — new branch (no commits yet); tap to branch from here")}</title>
                </circle>
                // The branch NAME beside the ring (iPad-testing follow-up: a bare
                // hollow ring was unidentifiable without tapping it). Same colour
                // as the ring/line so name and geometry read as one thing;
                // truncated so a long name can't sprawl across the canvas. Sits
                // at a half-row y, so it clears the commit labels' text lines;
                // pointer-events:none keeps the ring's hit circle the tap target.
                <text
                    x=sx + NODE_RADIUS + 6
                    y=sy + 4
                    class="stub-label"
                    fill=color
                    pointer-events="none"
                >
                    {truncate(&s.name, MAX_STUB_NAME_CHARS)}
                </text>
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
        .collect_view());
    view! { {paths} {tips} }.into_view()
}
