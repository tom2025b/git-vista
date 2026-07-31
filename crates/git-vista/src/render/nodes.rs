//! The commit-node layer: the filled dot with its tap menu, and the small glyph
//! drawn beside each dot.
//!
//! Split out of `render.rs`. [`build_node`] draws one commit's dot (plus the
//! large invisible hit target and the context menu it opens); [`build_node_icon`]
//! draws the per-commit glyph in its own toggleable layer.

use leptos::*;

use git_vista_core::model::RefKind;

use crate::features::shell::signals::Shell;
use crate::geometry::{node_cx, node_cy, NODE_RADIUS};
use crate::icons::icon_set;
use crate::state::MenuData;
use git_vista_core::color::branch_color;

use crate::features::graph::core::RenderCtx;

/// Per-commit node builder — a filled dot in the branch colour plus a larger
/// invisible hit target, built by a `<For>` only for rows in the viewport.
/// Every real commit (merges included) is a filled dot; a hollow ring is
/// reserved for branch stubs (a new branch with no commits of its own), so a
/// merge, which has real content, never reads as empty (Issue #30).
pub fn build_node(
    ctx: StoredValue<RenderCtx>,
    shell: Shell,
    moved: StoredValue<bool>,
    i: usize,
) -> View {
    ctx.with_value(|c| {
        // Checked, like every row lookup since paging (M1.10, #63): a `<For>`
        // key can outlive the shape it was built from by one frame.
        let Some(gr) = c.loaded.rows.get(i) else {
            return ().into_view();
        };
        let cx = node_cx(gr.lane);
        let cy = node_cy(gr.row);
        let color = branch_color(gr.color);
        let fill = color;
        let stroke_width = "2";
        // The row's identity in the DOM (M1.10, #63). This `<g>` is the one and
        // only per-row group — the label/meta/icon tiers deliberately add none —
        // so counting `.graph-row` counts live rows exactly, which is how the
        // MAX_LIVE_ROWS cull is observable from outside the app at all.
        let oid = gr.commit.id.0.clone();

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
        // The row carries its own exact answer (`on_remote`): paged history has
        // no whole-repo pushed-commit set, and inferring one from the rows that
        // happen to be loaded would mislabel everything below the last page.
        let github_url = c
            .frame
            .repo_url
            .as_ref()
            .and_then(|base| gr.on_remote.then(|| format!("{base}/commit/{commit_id}")));
        // The repo's GitHub base, carried into the menu for the "Create Pull
        // Request" compare link (independent of whether this commit is pushed).
        let repo_url = c.frame.repo_url.clone();
        // The any-host forge base (ADR 0010), for the non-GitHub branch links.
        let remote_web_url = c.frame.remote_web_url.clone();
        // Issue #139: opened on pointerup, not click — iPad DuckDuckGo doesn't
        // reliably synthesize a click from a touch on these SVG circles, so the
        // menu depends only on raw pointer events. The `moved` gate still
        // swallows the pointerup that ends a pan. Propagation must NOT be
        // stopped: the svg's own pointerup (gesture cleanup) runs after this.
        let open_menu = move |ev: web_sys::PointerEvent| {
            if moved.get_value() {
                return;
            }
            shell.open_menu(MenuData {
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
                repo_url: repo_url.clone(),
                remote_web_url: remote_web_url.clone(),
            });
        };

        view! {
            <g class="graph-row" data-oid=oid>
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
                    on:pointerup=open_menu
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
        let Some(gr) = c.loaded.rows.get(i) else {
            return ().into_view();
        };
        let icon = if gr.commit.parents.len() > 1 {
            ic.merge
        } else {
            ic.commit
        };
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
