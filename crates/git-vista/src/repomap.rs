//! The mindmap repo picker's pure core (#380): grouping and radial layout.
//!
//! Framework-free and host-tested, matching `collapse.rs` / `core.rs`
//! convention — no Leptos, no wasm gate, so `cargo test` actually executes
//! it. The SVG wiring that consumes it lives in `picker.rs` and is verified
//! by a Playwright spec instead.

// On the native target the only consumer of this module is its own test
// suite (the SVG view below is wasm-gated), so every pub item reads as dead
// there. That is the module's design — pure core host-tested, wiring
// wasm-only — not an accident to fix by gating the module (which would
// un-compile the tests).
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use git_vista_protocol::dto::RepositoryDescriptor;

/// One branch of the map: a named group of catalog entries.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub label: &'static str,
    /// Indices into the catalog slice this group was built from — indices,
    /// not clones, so the view can hand the ORIGINAL descriptor to the same
    /// open path the list rows use.
    pub members: Vec<usize>,
}

/// Group catalog entries by name convention.
///
/// Client-side and static on purpose: the server has no category field, and
/// the atlas `categories.toml` retirement (LOS ADR 0003) is the standing
/// argument against building a server-side taxonomy for a display concern.
/// A wrong group here mislabels a node; it never hides one — the invariant
/// the census test below pins is that every entry lands in exactly one group.
pub fn group(entries: &[RepositoryDescriptor]) -> Vec<Group> {
    let mut git_vista = Vec::new();
    let mut mcp = Vec::new();
    let mut ops = Vec::new();
    let mut other = Vec::new();

    for (i, d) in entries.iter().enumerate() {
        let n = d.name.to_ascii_lowercase();
        if n.starts_with("git-vista") || n.starts_with("gv-") {
            git_vista.push(i);
        } else if n.ends_with("-mcp") || n.contains("mcp") {
            mcp.push(i);
        } else if n.contains("ops")
            || n.starts_with("workboard")
            || n.starts_with("hookpack")
            || n.starts_with("backupsage")
        {
            ops.push(i);
        } else {
            other.push(i);
        }
    }

    // Empty groups are dropped: a branch with no leaves is noise, and the
    // layout below spaces branches evenly, so an empty one would waste arc.
    [
        ("Git-Vista", git_vista),
        ("MCP servers", mcp),
        ("Ops & tools", ops),
        ("Other", other),
    ]
    .into_iter()
    .filter(|(_, m)| !m.is_empty())
    .map(|(label, members)| Group { label, members })
    .collect()
}

/// A positioned node in the map's fixed viewBox.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub x: f64,
    pub y: f64,
}

pub const VIEW_W: f64 = 1200.0;
pub const VIEW_H: f64 = 900.0;
const CX: f64 = VIEW_W / 2.0;
const CY: f64 = VIEW_H / 2.0;
// Elliptical radii, not circular: the viewBox is 4:3, and a circle wide
// enough to use the width pushes the twelve-o'clock leaf off the top — the
// bounds test caught exactly that with a circular first draft.
const BRANCH_RX: f64 = 230.0;
const BRANCH_RY: f64 = 175.0;
const LEAF_RX: f64 = 455.0;
const LEAF_RY: f64 = 335.0;
fn polar(angle_deg: f64, rx: f64, ry: f64) -> Placed {
    let a = angle_deg.to_radians();
    Placed {
        x: CX + a.cos() * rx,
        y: CY + a.sin() * ry,
    }
}

/// Branch centre positions: groups spaced evenly around the hub, starting at
/// twelve o'clock.
pub fn branch_positions(group_count: usize) -> Vec<Placed> {
    (0..group_count)
        .map(|i| {
            let angle = -90.0 + (360.0 / group_count.max(1) as f64) * i as f64;
            polar(angle, BRANCH_RX, BRANCH_RY)
        })
        .collect()
}

/// Leaf positions for one branch: members fanned across an arc centred on
/// the branch's own angle. The arc widens with member count but is capped so
/// a big group cannot sweep into its neighbour's sector.
pub fn leaf_positions(group_index: usize, group_count: usize, member_count: usize) -> Vec<Placed> {
    let base = -90.0 + (360.0 / group_count.max(1) as f64) * group_index as f64;
    let sector = 360.0 / group_count.max(1) as f64;
    let spread = (member_count.saturating_sub(1) as f64 * 9.0).min(sector * 0.82);
    let step = if member_count > 1 {
        spread / (member_count - 1) as f64
    } else {
        0.0
    };
    (0..member_count)
        .map(|i| {
            // Alternate ring: neighbours 9 degrees apart would overlap
            // (chips are 170pt wide, the arc gap at this radius is ~70pt),
            // and an overlapped chip swallows its neighbour's click -- the
            // browser spec caught exactly that. Staggering every other leaf
            // 52pt inward separates them without narrowing the fan.
            let stagger = if i % 2 == 1 { 52.0 } else { 0.0 };
            polar(
                base - spread / 2.0 + step * i as f64,
                LEAF_RX - stagger,
                LEAF_RY - stagger,
            )
        })
        .collect()
}

/// The SVG mindmap view (#380), wasm-only so the pure core above stays
/// host-testable. Renders the grouped catalog radially: dark hub, one hue per
/// branch, curved spines, repo leaves — the diagram grammar Tom standardised
/// (rounded pills, branch-hue consistency, curved connectors). A leaf click
/// runs the SAME `mode_for.set(...)` path a list row runs; LAN sessions get
/// labels only, same as the list (ADR 0005).
#[cfg(target_arch = "wasm32")]
pub fn map_view(
    entries: Vec<RepositoryDescriptor>,
    mode_for: leptos::RwSignal<Option<RepositoryDescriptor>>,
) -> leptos::View {
    use crate::features::session::signals as session_state;
    use leptos::*;

    // One hue per branch, fixed order matching `group()`'s output order.
    const HUES: [&str; 4] = ["#58a6ff", "#3fb950", "#d29922", "#bc8cff"];

    let groups = group(&entries);
    let branches = branch_positions(groups.len());
    let mut nodes: Vec<View> = Vec::new();
    let mut edges: Vec<View> = Vec::new();

    for (gi, (g, bp)) in groups.iter().zip(&branches).enumerate() {
        let hue = HUES[gi % HUES.len()];
        // Hub -> branch spine.
        edges.push(
            view! {
                <path
                    d=format!(
                        "M {} {} Q {} {} {} {}",
                        VIEW_W / 2.0, VIEW_H / 2.0,
                        (VIEW_W / 2.0 + bp.x) / 2.0, (VIEW_H / 2.0 + bp.y) / 2.0,
                        bp.x, bp.y,
                    )
                    fill="none" stroke=hue stroke-width="6" stroke-linecap="round" opacity="0.9"
                />
            }
            .into_view(),
        );
        for (li, (mi, lp)) in g
            .members
            .iter()
            .zip(leaf_positions(gi, groups.len(), g.members.len()))
            .enumerate()
        {
            let _ = li;
            let d = entries[*mi].clone();
            let label = d.name.clone();
            // The connector's DASH carries information, not decoration
            // (Tom's ask): solid = a normal working repo, long dashes = a
            // view-only clone, short dots = a linked worktree. Same encoding
            // the KEYS box below spells out in words.
            let dash = if d.read_only {
                "14 9"
            } else if matches!(
                d.kind,
                git_vista_protocol::dto::RepositoryKind::LinkedWorktree
            ) {
                "3 7"
            } else {
                "0"
            };
            edges.push(
                view! {
                    <path
                        d=format!(
                            "M {} {} Q {} {} {} {}",
                            bp.x, bp.y,
                            (bp.x + lp.x) / 2.0, (bp.y + lp.y) / 2.0,
                            lp.x, lp.y,
                        )
                        fill="none" stroke=hue stroke-width="3.5"
                        stroke-linecap="round" opacity="0.7"
                        stroke-dasharray=dash
                    />
                }
                .into_view(),
            );
            let open_kb = {
                let d = d.clone();
                move |ev: web_sys::KeyboardEvent| {
                    if (ev.key() == "Enter" || ev.key() == " ") && !session_state::is_lan() {
                        ev.prevent_default();
                        mode_for.set(Some(d.clone()));
                    }
                }
            };
            let aria = format!("Open repository {}", d.name);
            let open = move |_| {
                if !session_state::is_lan() {
                    mode_for.set(Some(d.clone()));
                }
            };
            nodes.push(
                view! {
                    <g
                        class="repomap-leaf"
                        on:click=open
                        on:keydown=open_kb
                        tabindex="0"
                        role="button"
                        aria-label=aria
                        style="cursor:pointer;"
                    >
                        <rect
                            class="repomap-chip"
                            x=lp.x - 92.0 y=lp.y - 23.0 width="184" height="46" rx="14"
                            fill="#0d1117" stroke=hue stroke-width="2.5"
                        />
                        // A whisper of the branch hue inside the chip — reads
                        // as grouping from across the room without shouting.
                        <rect
                            x=lp.x - 92.0 y=lp.y - 23.0 width="184" height="46" rx="14"
                            fill=hue opacity="0.10" style="pointer-events:none;"
                        />
                        <text
                            x=lp.x y=lp.y + 6.0 text-anchor="middle"
                            fill="var(--fg)" font-size="18" font-weight="600"
                            style="pointer-events:none;"
                        >
                            {label}
                        </text>
                    </g>
                }
                .into_view(),
            );
        }
        nodes.push(
            view! {
                <g>
                    <rect
                        x=bp.x - 92.0 y=bp.y - 31.0 width="184" height="62" rx="31"
                        fill=hue opacity="0.95" filter="url(#rm-glow)"
                    />
                    <text
                        x=bp.x y=bp.y - 3.0 text-anchor="middle"
                        fill="#0d1117" font-size="21" font-weight="800"
                    >
                        {g.label}
                    </text>
                    <text
                        x=bp.x y=bp.y + 20.0 text-anchor="middle"
                        fill="#0d1117" font-size="15" font-weight="600"
                    >
                        {format!("{} repos", g.members.len())}
                    </text>
                </g>
            }
            .into_view(),
        );
    }

    view! {
        <svg
            class="repomap"
            viewBox=format!("0 0 {VIEW_W} {VIEW_H}")
            style="width:100%; height:auto; min-width:700px; display:block;"
        >
            // 50-inch-at-two-feet treatment (#380 follow-up): a soft glow on
            // the coloured nodes and a radial hub gradient. SVG filters, not
            // images — a handful of nodes, so the filter cost is nothing.
            <defs>
                <filter id="rm-glow" x="-40%" y="-40%" width="180%" height="180%">
                    <feGaussianBlur stdDeviation="6" result="b" />
                    <feMerge>
                        <feMergeNode in="b" />
                        <feMergeNode in="SourceGraphic" />
                    </feMerge>
                </filter>
                <radialGradient id="rm-hub" cx="50%" cy="42%" r="65%">
                    <stop offset="0%" stop-color="#1f2f4a" />
                    <stop offset="100%" stop-color="#0d1117" />
                </radialGradient>
            </defs>
            <g>{edges}</g>
            <g>{nodes}</g>
            // KEYS: the dash encoding in words (the standing diagram rule —
            // when an encoding repeats, spell it out in the picture).
            <g style="pointer-events:none;">
                <rect x="18" y=VIEW_H - 118.0 width="255" height="100" rx="10"
                    fill="#161b22" stroke="#30363d" stroke-width="1.5" opacity="0.95" />
                <text x="34" y=VIEW_H - 92.0 fill="var(--fg)" font-size="15" font-weight="700">
                    "KEYS — line style"
                </text>
                <line x1="34" y1=VIEW_H - 68.0 x2="86" y2=VIEW_H - 68.0
                    stroke="#8b949e" stroke-width="3.5" stroke-linecap="round" />
                <text x="96" y=VIEW_H - 63.0 fill="#8b949e" font-size="14">"working repo"</text>
                <line x1="34" y1=VIEW_H - 46.0 x2="86" y2=VIEW_H - 46.0
                    stroke="#8b949e" stroke-width="3.5" stroke-linecap="round"
                    stroke-dasharray="14 9" />
                <text x="96" y=VIEW_H - 41.0 fill="#8b949e" font-size="14">"clone (view-only)"</text>
                <line x1="34" y1=VIEW_H - 24.0 x2="86" y2=VIEW_H - 24.0
                    stroke="#8b949e" stroke-width="3.5" stroke-linecap="round"
                    stroke-dasharray="3 7" />
                <text x="96" y=VIEW_H - 19.0 fill="#8b949e" font-size="14">"linked worktree"</text>
            </g>
            <circle cx=VIEW_W / 2.0 cy=VIEW_H / 2.0 r="84" fill="url(#rm-hub)"
                stroke="#58a6ff" stroke-width="3.5" filter="url(#rm-glow)" />
            <text x=VIEW_W / 2.0 y=VIEW_H / 2.0 - 8.0 text-anchor="middle"
                fill="var(--fg)" font-size="22" font-weight="800">
                "Repositories"
            </text>
            <text x=VIEW_W / 2.0 y=VIEW_H / 2.0 + 22.0 text-anchor="middle"
                fill="#58a6ff" font-size="19" font-weight="700">
                {format!("{}", entries.len())}
            </text>
        </svg>
    }
    .into_view()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Half-width/height of the largest node chip: a centre this close to
    /// the edge keeps the whole chip on the canvas. Test-side on purpose —
    /// it is an assertion about the layout, not part of it.
    const MARGIN: f64 = 95.0;

    fn in_bounds(p: &Placed) -> bool {
        p.x >= MARGIN && p.x <= VIEW_W - MARGIN && p.y >= MARGIN && p.y <= VIEW_H - MARGIN
    }
    use git_vista_protocol::dto::{RepositoryDescriptor, RepositoryKind};

    fn d(name: &str) -> RepositoryDescriptor {
        RepositoryDescriptor {
            repository: format!("r-{name}"),
            worktree: format!("w-{name}"),
            name: name.to_string(),
            kind: RepositoryKind::MainWorktree,
            read_only: false,
            path: None,
            remote_web_url: None,
            hook_policy: Default::default(),
        }
    }

    #[test]
    fn every_entry_lands_in_exactly_one_group() {
        // The census invariant: grouping is display-only and must never hide
        // or duplicate a repo. Mutation target: drop a push in group() and
        // this goes red on the count; double one and it goes red on dedup.
        let entries: Vec<_> = [
            "Git-Vista",
            "gv-testkit",
            "printpdf-mcp",
            "corpus-mcp",
            "Linux-Ops-Suite",
            "workboard",
            "backupsage",
            "writing",
            "gluco",
        ]
        .iter()
        .map(|n| d(n))
        .collect();

        let groups = group(&entries);

        let mut seen: Vec<usize> = groups.iter().flat_map(|g| g.members.clone()).collect();
        seen.sort_unstable();
        let expected: Vec<usize> = (0..entries.len()).collect();
        assert_eq!(seen, expected, "{groups:?}");
    }

    #[test]
    fn grouping_follows_the_name_conventions() {
        let entries: Vec<_> = [
            "Git-Vista",
            "gv-testkit",
            "printpdf-mcp",
            "workboard",
            "writing",
        ]
        .iter()
        .map(|n| d(n))
        .collect();

        let groups = group(&entries);
        let labels: Vec<_> = groups.iter().map(|g| g.label).collect();

        assert_eq!(labels, ["Git-Vista", "MCP servers", "Ops & tools", "Other"]);
        assert_eq!(groups[0].members, vec![0, 1]);
        assert_eq!(groups[1].members, vec![2]);
        assert_eq!(groups[2].members, vec![3]);
        assert_eq!(groups[3].members, vec![4]);
    }

    #[test]
    fn empty_groups_are_dropped_not_rendered_bare() {
        let entries: Vec<_> = ["writing"].iter().map(|n| d(n)).collect();

        let groups = group(&entries);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "Other");
    }

    #[test]
    fn branches_are_evenly_spaced_and_in_bounds() {
        for count in 1..=6 {
            let placed = branch_positions(count);
            assert_eq!(placed.len(), count);
            for p in &placed {
                assert!(in_bounds(p), "count={count} p={p:?}");
            }
        }
    }

    #[test]
    fn a_fifty_leaf_branch_stays_inside_the_canvas() {
        // The realistic worst case: the whole ~54-repo catalog with most
        // entries in one group. Every leaf centre must stay on the canvas —
        // the chip half-size margin makes this "the whole chip is visible".
        for gi in 0..4 {
            for p in leaf_positions(gi, 4, 50) {
                assert!(in_bounds(&p), "group {gi} leaf {p:?}");
            }
        }
    }

    #[test]
    fn a_single_leaf_sits_on_its_branch_axis() {
        let leaf = &leaf_positions(0, 4, 1)[0];
        let branch = &branch_positions(4)[0];
        // Same angle (twelve o'clock): x equal, leaf further out (smaller y).
        assert!((leaf.x - branch.x).abs() < 0.001);
        assert!(leaf.y < branch.y);
    }
}
