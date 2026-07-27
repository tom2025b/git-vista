//! Pure layout geometry for the vertical commit graph.
//!
//! Everything in here maps abstract graph coordinates — a commit's `(row, lane)`
//! — onto concrete SVG user units, with no Leptos/DOM dependency. Splitting it
//! out of the [`crate::app`] component keeps that file about *view assembly* and
//! lets the spatial math be reasoned about (and unit-tested) on its own. Colours
//! live separately in [`git_vista_core::color`].
//!
//! All values are whole numbers so the emitted SVG attributes stay clean.

use git_vista_core::model::{BranchStub, Edge, Graph};

// Geometry of the graph, in SVG user units (px).
pub const ROW_HEIGHT: i32 = 56; // vertical gap between commits
pub const LANE_WIDTH: i32 = 34; // horizontal gap between lanes
                                // Used only by the wasm-only `app` view, so it reads as dead on host/test builds.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const NODE_RADIUS: i32 = 7;
pub const PAD_X: i32 = 28;
pub const PAD_Y: i32 = 28;
// Horizontal gap between the rightmost lane and the start of the label column.
pub const LABEL_GAP: i32 = 18;

// Ref badges (Phase 7): small pills drawn at the start of the label column,
// before the commit message. The font is monospace, so a glyph's advance is a
// fixed fraction of the font size and badge widths can be computed exactly.
// Used only by the wasm-only `app` view, so they read as dead on host/test builds.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const BADGE_HEIGHT: i32 = 16;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const BADGE_RADIUS: i32 = 4;
// Horizontal gap between adjacent badges (and after the last one, before text).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const BADGE_GAP: i32 = 5;
// Per-character advance and inner horizontal padding, in px (monospace @ ~11px).
const BADGE_CHAR_W: i32 = 7;
const BADGE_PAD_X: i32 = 6;

/// Pointer travel (CSS px) past which a press becomes a pan/drag rather than a
/// tap. Touch gets a wider allowance: a natural finger tap wobbles 5-10 px on
/// an iPad, and the strict mouse value silently ate node taps (issue #115).
/// Mouse/pen (and anything unknown) stay precise so a tiny deliberate drag
/// still pans.
pub fn drag_threshold(pointer_type: &str) -> f64 {
    if pointer_type == "touch" {
        12.0
    } else {
        4.0
    }
}

/// Centre x of a node in the given lane.
pub fn node_cx(lane: usize) -> i32 {
    PAD_X + lane as i32 * LANE_WIDTH
}

/// Centre y of a node in the given row.
pub fn node_cy(row: usize) -> i32 {
    PAD_Y + row as i32 * ROW_HEIGHT
}

/// Left edge (x) of the commit-label column: a fixed column just to the right of
/// the widest lane, so every row's text is aligned regardless of its own lane.
/// Superseded in the views by [`label_x_per_row`] (labels hug the graph now);
/// kept as the documented old behaviour, pinned by its test below.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn label_x(lane_count: usize) -> i32 {
    // `lane_count` lanes occupy indices 0..lane_count; sit past the last one.
    node_cx(lane_count.saturating_sub(1)) + LABEL_GAP
}

/// Per-row left edge (x) of the label text, hugging the graph: each row's label
/// starts just right of the rightmost thing actually drawn *at that row* — its
/// own dot, any edge passing vertically through, and any stub ring hovering
/// there — instead of one global column past the widest lane ([`label_x`]).
///
/// The old global column made label distance depend on the whole repo: every
/// commit-less branch stub takes its own lane, so a repo with many branches
/// pushed all labels far right of the trunk dots, while a freshly cloned repo
/// (no local stubs) had them snug against the graph. Hugging per row gives every
/// repo the snug version.
///
/// Edge occupancy is the safe over-approximation of the S-curve
/// ([`edge_path`]): at its two endpoint rows the curve is still within a lane
/// of the endpoint (the bulge allowance below), and on rows strictly between it
/// can be anywhere between the lanes, so those take the outer lane.
///
/// M1.10 (#63): the *paged* views no longer call this — a page-at-a-time graph
/// grows its occupancy incrementally and monotonically inside
/// [`crate::features::graph::core::LoadedHistory`], which reads back through
/// `LoadedHistory::text_x`. This whole-`Graph` form stays as the documented
/// reference the incremental one mirrors, pinned by the tests below.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn label_x_per_row(graph: &Graph) -> Vec<i32> {
    // Every row is at least as wide as its own dot.
    let mut occ: Vec<usize> = graph.rows.iter().map(|r| r.lane).collect();
    if occ.is_empty() {
        return Vec::new();
    }
    let last = occ.len() - 1;
    for e in &graph.edges {
        let (top, bot) = if e.from_row <= e.to_row {
            (e.from_row, e.to_row)
        } else {
            (e.to_row, e.from_row)
        };
        let hi = e.from_lane.max(e.to_lane);
        for (r, occ_r) in occ
            .iter_mut()
            .enumerate()
            .take(bot.min(last) + 1)
            .skip(top.min(last))
        {
            // Endpoint rows: the curve has left its lane by less than one lane
            // within the label's text band, so allow one lane of bulge (capped
            // at the outer lane). Middle rows: anywhere between — take `hi`.
            let lane = if r == e.from_row {
                (e.from_lane + 1).min(hi)
            } else if r == e.to_row {
                (e.to_lane + 1).min(hi)
            } else {
                hi
            };
            *occ_r = (*occ_r).max(lane);
        }
    }
    for s in &graph.stubs {
        // A stub's ring sits (depth+1) half-rows above its anchor, so it hangs
        // over the anchor row and ⌈(depth+1)/2⌉ rows above it.
        let up = (s.depth + 2) / 2;
        let top = s.anchor_row.saturating_sub(up);
        for occ_r in occ.iter_mut().take(s.anchor_row.min(last) + 1).skip(top) {
            *occ_r = (*occ_r).max(s.lane);
        }
    }
    occ.into_iter()
        .map(|lane| node_cx(lane) + LABEL_GAP)
        .collect()
}

/// Baseline y of a row's first (message) label line — just above the node's
/// centre, so the two-line label straddles the node.
pub fn label_top_y(row: usize) -> i32 {
    node_cy(row) - 3
}

/// Baseline y of a row's second (hash · author) label line — just below centre.
pub fn label_bottom_y(row: usize) -> i32 {
    node_cy(row) + 12
}

/// Pixel width of a badge holding `text`, from the monospace glyph advance plus
/// padding on both sides. Used to lay badges out left-to-right and to know how
/// far to push the commit message past them.
pub fn badge_width(text: &str) -> i32 {
    text.chars().count() as i32 * BADGE_CHAR_W + 2 * BADGE_PAD_X
}

/// Left inset of a badge's text from its left edge.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn badge_text_dx() -> i32 {
    BADGE_PAD_X
}

/// Top y of a row's badge pills — they sit on the message (top) label line,
/// clear of the hash·author line below.
pub fn badge_top_y(row: usize) -> i32 {
    node_cy(row) - 12
}

/// Baseline y of a badge's text, vertically centred in the pill.
pub fn badge_text_y(row: usize) -> i32 {
    node_cy(row) - 1
}

/// SVG path data for a commit->parent edge. Same-lane links are a straight
/// vertical line; lane-changing links (branches/merges) get a smooth vertical
/// S-curve so they read as flowing between columns rather than cutting across.
pub fn edge_path(e: &Edge) -> String {
    let (x1, y1) = (node_cx(e.from_lane), node_cy(e.from_row));
    let (x2, y2) = (node_cx(e.to_lane), node_cy(e.to_row));
    if x1 == x2 {
        format!("M {x1} {y1} L {x2} {y2}")
    } else {
        let ym = (y1 + y2) / 2;
        format!("M {x1} {y1} C {x1} {ym}, {x2} {ym}, {x2} {y2}")
    }
}

/// Centre y of a branch stub's tip node. Stubs sharing a commit cascade upward:
/// `depth` 0 sits half a row above the commit (a short fork just above it), and
/// each deeper stub steps another half-row higher, so a stack of branches at one
/// commit reads as a staircase of hollow dots rather than a pile on the commit.
pub fn stub_node_cy(anchor_row: usize, depth: usize) -> i32 {
    node_cy(anchor_row) - (depth as i32 + 1) * (ROW_HEIGHT / 2)
}

/// Extra vertical margin kept above the highest stub ring at the home view, so
/// the ring doesn't kiss the canvas edge.
const STUB_TOP_MARGIN: i32 = 6;

/// How far (world px) the home camera must shift the graph down so every stub
/// ring is fully visible. Stubs cascade *upward* off their anchor commit, so a
/// branch created on the newest commit (row 0) tips above `y = 0` — born
/// half-clipped at the default view, invisible until the user pans. Returns the
/// overshoot of the highest ring past the top edge (plus a small margin), or
/// zero when nothing reaches above the canvas.
///
/// M1.10 (#63): the paged views place stubs from
/// [`ResolvedStub`](crate::features::graph::core::ResolvedStub), which is not a `BranchStub`,
/// so they call [`stub_headroom_for`] with the same `(anchor_row, depth)` pairs.
/// This form stays as the whole-`Graph` reference its tests below pin.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn stub_headroom(stubs: &[BranchStub]) -> f64 {
    stub_headroom_for(stubs.iter().map(|s| (s.anchor_row, s.depth)))
}

/// [`stub_headroom`] over bare `(anchor_row, depth)` pairs, so a caller holding
/// resolved paged stubs doesn't have to fabricate `BranchStub`s to ask how far
/// down the home camera must sit.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn stub_headroom_for(stubs: impl IntoIterator<Item = (usize, usize)>) -> f64 {
    stubs
        .into_iter()
        .map(|(anchor_row, depth)| stub_node_cy(anchor_row, depth) - NODE_RADIUS - STUB_TOP_MARGIN)
        .min()
        .map_or(0.0, |top| (-top).max(0) as f64)
}

/// SVG path up-and-out to a branch stub's tip node — a smooth S-curve, like a
/// branch edge, so the stub flows out of its source rather than cutting to it. The
/// source is the anchor commit for the first stub in a cascade (`depth` 0), or the
/// previous stub's tip (one lane left, one half-row down) for a deeper stub — so
/// the cascade visibly forks each new branch off the one before it.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn stub_path(anchor_lane: usize, anchor_row: usize, stub_lane: usize, depth: usize) -> String {
    let (sx, sy) = (node_cx(stub_lane), stub_node_cy(anchor_row, depth));
    let (ax, ay) = if depth == 0 {
        // Fork straight off the commit dot.
        (node_cx(anchor_lane), node_cy(anchor_row))
    } else {
        // Fork off the previous stub's tip (the lane immediately to the left).
        (node_cx(stub_lane - 1), stub_node_cy(anchor_row, depth - 1))
    };
    let ym = (ay + sy) / 2;
    format!("M {ax} {ay} C {ax} {ym}, {sx} {ym}, {sx} {sy}")
}

// Context-menu placement (iPad fix: the menu used to open at the raw tap point
// and run off the bottom of the screen, its clipped items unreachable). The
// menu's CSS caps its width at MENU_MAX_WIDTH, so the left clamp below only
// needs that one number; EDGE_PAD keeps a sliver of backdrop visible past the
// menu so tap-outside-to-close always has somewhere to land.
const MENU_MAX_WIDTH: f64 = 320.0;
const EDGE_PAD: f64 = 8.0;
// Never squeeze the menu shorter than this, even on an absurdly small
// viewport — a few rows plus scrolling beats a sliver.
const MENU_MIN_HEIGHT: f64 = 120.0;

/// Which vertical edge of the context menu sits at the tap point: `Top` pins
/// its top there (menu grows downward), `Bottom` pins its bottom (grows up).
/// Values are the CSS `top:`/`bottom:` px for a `position: fixed` element.
#[derive(Debug, PartialEq)]
pub enum VAnchor {
    Top(f64),
    Bottom(f64),
}

/// Where the context menu goes: clamped `left`, vertical anchor, `max-height`.
pub struct MenuPlacement {
    pub left: f64,
    pub anchor: VAnchor,
    pub max_height: f64,
}

/// Place the context menu for a tap at `(x, y)` in a `vw`×`vh` viewport (CSS
/// px). Like a native context menu, a tap in the lower half flips the menu
/// ABOVE the finger — the anchored edge stays at the tap point and the menu
/// grows toward the farther screen edge, so it always has at least half the
/// viewport of room. `max_height` is the space actually available on that
/// side; whatever doesn't fit scrolls (`.ctx-menu`'s `overflow-y`) instead of
/// clipping. That also covers the undo section arriving async after placement:
/// a menu that grows late starts scrolling, it never pushes items offscreen.
pub fn menu_placement(x: f64, y: f64, vw: f64, vh: f64) -> MenuPlacement {
    let left = x.min((vw - MENU_MAX_WIDTH - EDGE_PAD).max(EDGE_PAD));
    let (anchor, room) = if y > vh / 2.0 {
        (VAnchor::Bottom(vh - y), y - EDGE_PAD)
    } else {
        (VAnchor::Top(y), vh - y - EDGE_PAD)
    };
    MenuPlacement {
        left,
        anchor,
        max_height: room.max(MENU_MIN_HEIGHT),
    }
}

impl MenuPlacement {
    /// The inline `style` for the `.ctx-menu` div. Emits `top:` or `bottom:`
    /// per the anchor; the per-open `max-height` overrides the stylesheet's
    /// static viewport-sized backstop.
    pub fn style(&self) -> String {
        let v = match self.anchor {
            VAnchor::Top(t) => format!("top: {t}px;"),
            VAnchor::Bottom(b) => format!("bottom: {b}px;"),
        };
        format!(
            "left: {}px; {v} max-height: {}px;",
            self.left, self.max_height
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_touch_tap_tolerates_finger_wobble_a_mouse_click_stays_precise() {
        // A natural finger tap wobbles 5-10 CSS px on an iPad; a mouse doesn't.
        // Touch must tolerate more travel before a press becomes a pan, or node
        // taps are silently eaten (issue #115).
        assert!(drag_threshold("touch") >= 10.0);
        assert_eq!(drag_threshold("mouse"), 4.0);
        assert_eq!(drag_threshold("pen"), 4.0);
        // Unknown/empty pointer types keep the strict default.
        assert_eq!(drag_threshold(""), 4.0);
    }

    #[test]
    fn a_top_half_tap_opens_downward_with_room_to_the_bottom_edge() {
        let p = menu_placement(100.0, 200.0, 1024.0, 800.0);
        assert_eq!(p.anchor, VAnchor::Top(200.0));
        assert_eq!(p.max_height, 800.0 - 200.0 - 8.0);
        assert_eq!(p.left, 100.0);
        assert_eq!(p.style(), "left: 100px; top: 200px; max-height: 592px;");
    }

    #[test]
    fn a_bottom_half_tap_flips_the_menu_above_the_finger() {
        // Tap at y=700 of 800: bottom edge pinned 100px up from the viewport
        // bottom, growing upward, capped by the space above the tap.
        let p = menu_placement(100.0, 700.0, 1024.0, 800.0);
        assert_eq!(p.anchor, VAnchor::Bottom(100.0));
        assert_eq!(p.max_height, 700.0 - 8.0);
        assert!(p.style().contains("bottom: 100px;"), "{}", p.style());
    }

    #[test]
    fn a_tap_near_the_right_edge_pulls_the_menu_back_on_screen() {
        let p = menu_placement(1000.0, 200.0, 1024.0, 800.0);
        assert_eq!(p.left, 1024.0 - 320.0 - 8.0);
    }

    #[test]
    fn a_tiny_viewport_still_leaves_a_usable_scrollable_menu() {
        // Tap at the very top of a 100px-tall viewport: available room (92px)
        // is under the floor, so the menu keeps its minimum and scrolls.
        let p = menu_placement(5.0, 0.0, 200.0, 100.0);
        assert_eq!(p.max_height, 120.0);
        // And the left clamp never goes negative on a narrow screen.
        assert_eq!(menu_placement(150.0, 0.0, 200.0, 100.0).left, 8.0);
    }

    #[test]
    fn node_centres_step_by_the_configured_gaps() {
        assert_eq!(node_cx(0), PAD_X);
        assert_eq!(node_cx(2), PAD_X + 2 * LANE_WIDTH);
        assert_eq!(node_cy(0), PAD_Y);
        assert_eq!(node_cy(3), PAD_Y + 3 * ROW_HEIGHT);
    }

    #[test]
    fn label_column_sits_past_the_widest_lane_and_rows_straddle_nodes() {
        // One lane → column just right of lane 0; three lanes → right of lane 2.
        assert_eq!(label_x(1), node_cx(0) + LABEL_GAP);
        assert_eq!(label_x(3), node_cx(2) + LABEL_GAP);
        // The two text baselines bracket the node centre.
        assert!(label_top_y(2) < node_cy(2));
        assert!(label_bottom_y(2) > node_cy(2));
    }

    #[test]
    fn per_row_labels_hug_the_graph() {
        use git_vista_core::model::{CommitSummary, GraphRow, Oid};
        let commit = |id: &str| CommitSummary {
            id: Oid(id.into()),
            parents: vec![],
            summary: id.into(),
            author: "t".into(),
            time: 0,
        };
        let row = |r: usize, lane: usize| GraphRow {
            commit: commit(&r.to_string()),
            row: r,
            lane,
            refs: vec![],
            color: 0,
            on_remote: false,
        };
        let mut g = Graph {
            rows: vec![row(0, 0), row(1, 2), row(2, 0), row(3, 0)],
            lane_count: 8, // inflated by stub lanes — must NOT matter per-row
            ..Default::default()
        };
        let xs = label_x_per_row(&g);
        // A bare row hugs its own dot, not the global widest lane.
        assert_eq!(xs[3], node_cx(0) + LABEL_GAP);
        assert_eq!(xs[1], node_cx(2) + LABEL_GAP);

        // An edge fanning from lane 2 (row 1) to lane 0 (row 3) pushes the rows
        // it passes through: the middle row takes the outer lane, the endpoint
        // rows stay within a lane of their own end.
        g.edges = vec![Edge {
            from_row: 1,
            from_lane: 2,
            to_row: 3,
            to_lane: 0,
        }];
        let xs = label_x_per_row(&g);
        assert_eq!(xs[2], node_cx(2) + LABEL_GAP, "middle row clears the curve");
        assert_eq!(
            xs[3],
            node_cx(1) + LABEL_GAP,
            "endpoint allows one lane of bulge"
        );
        assert_eq!(
            xs[0],
            node_cx(0) + LABEL_GAP,
            "rows off the edge are untouched"
        );

        // A stub ring hovering over rows 0..=1 pushes them past its lane.
        g.stubs = vec![BranchStub {
            name: "s".into(),
            anchor_row: 1,
            anchor_lane: 2,
            lane: 5,
            color: 3,
            depth: 0,
        }];
        let xs = label_x_per_row(&g);
        assert_eq!(xs[1], node_cx(5) + LABEL_GAP);
        assert_eq!(
            xs[0],
            node_cx(5) + LABEL_GAP,
            "ring tips over the row above"
        );
        assert_eq!(
            xs[2],
            node_cx(2) + LABEL_GAP,
            "rows below the anchor are untouched"
        );

        // Empty graph: no rows, no panic.
        assert!(label_x_per_row(&Graph::default()).is_empty());
    }

    #[test]
    fn badge_width_grows_with_text_and_has_padding() {
        // Empty badge is just the two-sided padding; each char adds a fixed width.
        assert_eq!(badge_width(""), 2 * BADGE_PAD_X);
        assert_eq!(badge_width("ab"), 2 * BADGE_CHAR_W + 2 * BADGE_PAD_X);
        assert!(badge_width("main") > badge_width("v1"));
        // Pills sit on the top label line, above the hash·author line.
        assert!(badge_top_y(2) < label_top_y(2));
        assert!(badge_text_y(2) < label_bottom_y(2));
    }

    #[test]
    fn headroom_covers_only_stubs_that_overshoot_the_top() {
        let stub = |anchor_row: usize, depth: usize| BranchStub {
            name: String::new(),
            anchor_row,
            anchor_lane: 0,
            lane: 3,
            color: 3,
            depth,
        };
        // No stubs — no headroom.
        assert_eq!(stub_headroom(&[]), 0.0);
        // A stub deep in the graph stays fully on canvas: still none.
        assert_eq!(stub_headroom(&[stub(10, 0)]), 0.0);
        // A depth-1 stub on row 0 tips at y = PAD_Y - ROW_HEIGHT = -28; the home
        // view must shift down past its ring top (tip - radius - margin = -41).
        let need = stub_headroom(&[stub(0, 0), stub(0, 1)]);
        assert_eq!(need, (ROW_HEIGHT - PAD_Y + NODE_RADIUS + 6) as f64);
        // The deepest overshoot wins when cascades mix.
        assert_eq!(stub_headroom(&[stub(10, 0), stub(0, 1)]), need);
    }

    #[test]
    fn stub_cascade_steps_up_and_forks_off_the_previous_tip() {
        // Each deeper stub sits a further half-row above the commit.
        assert_eq!(stub_node_cy(4, 0), node_cy(4) - ROW_HEIGHT / 2);
        assert_eq!(stub_node_cy(4, 1), node_cy(4) - ROW_HEIGHT);
        assert!(
            stub_node_cy(4, 1) < stub_node_cy(4, 0),
            "deeper is higher up"
        );

        // The first stub in a cascade forks off the commit dot itself.
        let d0 = stub_path(0, 4, 3, 0);
        assert!(
            d0.starts_with(&format!("M {} {}", node_cx(0), node_cy(4))),
            "depth-0 stub starts at the commit dot: {d0}"
        );
        // A deeper stub forks off the previous stub's tip (one lane left, one
        // half-row down), NOT off the commit — that's the visible fork-from-a-dot.
        let d1 = stub_path(0, 4, 4, 1);
        assert!(
            d1.starts_with(&format!("M {} {}", node_cx(3), stub_node_cy(4, 0))),
            "depth-1 stub starts at the previous stub's tip: {d1}"
        );
        assert!(d1.contains(" C "), "still a smooth curve");
    }

    #[test]
    fn same_lane_edges_are_straight_others_curve() {
        let straight = Edge {
            from_row: 0,
            from_lane: 0,
            to_row: 1,
            to_lane: 0,
        };
        assert_eq!(
            edge_path(&straight),
            format!(
                "M {} {} L {} {}",
                node_cx(0),
                node_cy(0),
                node_cx(0),
                node_cy(1)
            )
        );

        let curved = Edge {
            from_row: 0,
            from_lane: 0,
            to_row: 1,
            to_lane: 1,
        };
        let d = edge_path(&curved);
        assert!(d.starts_with('M'), "starts with a move");
        assert!(d.contains(" C "), "lane-changing edge is a cubic curve");
    }

    // ---- paged-history geometry (M1.10, #63) --------------------------------
    //
    // [`label_x_per_row`] above computes label geometry from a whole `Graph`, which
    // only exists when the entire history has arrived. Paged history never has
    // that: rows appear a page at a time, so geometry is *grown* by
    // [`crate::features::graph::core::LoadedHistory`] instead — each append widens rows, never
    // narrows them. These tests pin that monotonicity and the resolved-stub
    // placement, reading geometry only through the aggregate's `label_occupancy()`
    // / `text_x()` accessors, because the backing vectors are private precisely so
    // nothing outside that module can shrink them.

    use crate::features::graph::core::{LoadedHistory, Page, ResolvedStub};
    use git_vista_core::model::{CommitSummary, FrameStub, GraphRow, Oid};
    use git_vista_protocol::GenerationToken;

    /// Every fixture page below belongs to one repository generation.
    const GEN: &str = "g1";

    fn hrow(index: usize, lane: usize, id: &str) -> GraphRow {
        GraphRow {
            commit: CommitSummary {
                id: Oid(id.into()),
                parents: vec![],
                summary: format!("commit {id}"),
                author: "tester".into(),
                time: 0,
            },
            row: index,
            lane,
            refs: vec![],
            color: 0,
            on_remote: false,
        }
    }

    fn hedge(from_row: usize, from_lane: usize, to_row: usize, to_lane: usize) -> Edge {
        Edge {
            from_row,
            from_lane,
            to_row,
            to_lane,
        }
    }

    fn hstub(name: &str, anchor: &str, lane_offset: usize) -> FrameStub {
        FrameStub {
            name: name.into(),
            anchor_commit: Oid(anchor.into()),
            lane_offset,
            color: 3,
            depth: 0,
        }
    }

    fn hpage(
        rows: Vec<GraphRow>,
        edges: Vec<Edge>,
        stubs: Vec<FrameStub>,
        lane_count: usize,
        cursor: Option<&str>,
    ) -> Page {
        Page {
            rows,
            edges,
            stubs,
            lane_count,
            cursor: cursor.map(str::to_owned),
            generation: GenerationToken::new(GEN).expect("test generation token"),
        }
    }

    /// Page 1 for these tests: rows 0..2 in lane 0, joined by the straight edge.
    fn hseeded() -> LoadedHistory {
        LoadedHistory::from_first_page(hpage(
            vec![hrow(0, 0, "aaa0"), hrow(1, 0, "bbb1")],
            vec![hedge(0, 0, 1, 0)],
            vec![],
            1,
            Some("c1"),
        ))
        .expect("page 1 is valid")
    }

    /// Nothing an append does may pull a row's label back toward the graph: a
    /// label that jumps left under the reader's eye is the failure mode this
    /// whole incremental path exists to prevent.
    fn assert_no_shrink(before: &[usize], after: &[usize]) {
        for (r, b) in before.iter().enumerate() {
            assert!(
                after[r] >= *b,
                "row {r} occupancy shrank from {b} to {}",
                after[r]
            );
        }
    }

    #[test]
    fn cross_page_edge_monotonically_widens_old_label() {
        let mut history = hseeded();
        let occupancy_before = history.label_occupancy().to_vec();
        let text_x_before = history.text_x().to_vec();

        // Page 2 lands a commit in lane 1 whose edge starts back in page 1: the
        // curve now sweeps across rows the prefix already drew, so their labels
        // must move right to clear it.
        let delta = history
            .append_page(
                "c1",
                hpage(
                    vec![hrow(2, 1, "ccc2")],
                    vec![hedge(0, 0, 2, 1)],
                    vec![],
                    2,
                    None,
                ),
            )
            .expect("a cross-page source edge is valid on the destination's page");

        assert!(delta.prefix_geometry_changed);
        let occupancy_after = history.label_occupancy();
        assert_no_shrink(&occupancy_before, occupancy_after);
        // Endpoint row 0 allows one lane of bulge (capped at the outer lane);
        // middle row 1 takes the outer lane outright.
        assert_eq!(occupancy_after, [1, 1, 1]);
        assert!(
            history.text_x()[0] > text_x_before[0],
            "the old label moved right, from {} to {}",
            text_x_before[0],
            history.text_x()[0]
        );
        assert_eq!(history.text_x()[1], node_cx(1) + LABEL_GAP);
    }

    #[test]
    fn straight_append_does_not_rekey_prefix() {
        let mut history = hseeded();
        let occupancy_before = history.label_occupancy().to_vec();
        let text_x_before = history.text_x().to_vec();

        // A commit straight below the tip in the same lane touches nothing above
        // it, so every prefix label must keep the exact x it already had —
        // otherwise every page would repaint the whole graph.
        let delta = history
            .append_page(
                "c1",
                hpage(
                    vec![hrow(2, 0, "ccc2")],
                    vec![hedge(1, 0, 2, 0)],
                    vec![],
                    1,
                    None,
                ),
            )
            .expect("a straight same-lane page 2 is valid");

        assert!(!delta.prefix_geometry_changed);
        assert_eq!(&history.label_occupancy()[..2], &occupancy_before[..]);
        assert_eq!(&history.text_x()[..2], &text_x_before[..]);
        assert_eq!(history.text_x()[2], node_cx(0) + LABEL_GAP);
    }

    #[test]
    fn lane_high_water_shift_rekeys_all_resolved_stubs() {
        // Page 1 carries a stub on its own anchor, so it is already resolved and
        // already widening the row its ring hangs over.
        let mut history = LoadedHistory::from_first_page(hpage(
            vec![hrow(0, 0, "aaa0"), hrow(1, 0, "bbb1")],
            vec![hedge(0, 0, 1, 0)],
            vec![hstub("wip", "aaa0", 0)],
            1,
            Some("c1"),
        ))
        .expect("page 1 is valid");
        assert_eq!(
            history.resolved_stubs()[0].lane,
            1,
            "stub columns start past the commit lanes"
        );
        let occupancy_before = history.label_occupancy().to_vec();

        // Page 2 raises the commit-lane high-water. Stub lanes are *relative* to
        // it, so a stub that resolved pages ago slides right even though this page
        // delivered no stub of its own.
        let delta = history
            .append_page(
                "c1",
                hpage(
                    vec![hrow(2, 1, "ccc2")],
                    vec![hedge(1, 0, 2, 1)],
                    vec![],
                    3,
                    None,
                ),
            )
            .expect("a page raising the lane high-water is valid");

        assert!(delta.stub_geometry_changed);
        assert_eq!(
            history.resolved_stubs()[0].lane,
            3,
            "high-water 3 + offset 0"
        );
        assert_no_shrink(&occupancy_before, history.label_occupancy());
        assert_eq!(
            history.label_occupancy()[0],
            3,
            "the shifted ring drags its anchor row's label with it"
        );
        assert_eq!(history.text_x()[0], node_cx(3) + LABEL_GAP);
    }

    #[test]
    fn same_page_stub_resolves_by_oid() {
        let mut history = hseeded();

        // The page delivering a stub also delivers its anchor commit, so the
        // aggregate can place it the moment it arrives.
        history
            .append_page(
                "c1",
                hpage(
                    vec![hrow(2, 1, "ccc2")],
                    vec![hedge(1, 0, 2, 1)],
                    vec![hstub("wip", "ccc2", 1)],
                    2,
                    None,
                ),
            )
            .expect("a page carrying its stub's anchor is valid");

        assert_eq!(
            history.resolved_stubs(),
            vec![ResolvedStub {
                stub: hstub("wip", "ccc2", 1),
                anchor_row: 2,
                anchor_lane: 1,
                lane: 3,
            }],
            "anchor row/lane come from the indexed commit; lane is high-water + offset"
        );
    }

    #[test]
    fn unresolved_stub_is_skipped_defensively() {
        let mut history = hseeded();

        // A stub whose anchor_commit matches no loaded row. This is malformed
        // input, not a sequence the server is allowed to produce; the client keeps
        // it rather than indexing it so a bad payload degrades to "not drawn"
        // instead of a panic in the middle of a render.
        history
            .append_page(
                "c1",
                hpage(
                    vec![hrow(2, 0, "ccc2")],
                    vec![hedge(1, 0, 2, 0)],
                    vec![hstub("orphan", "zzz9", 0)],
                    1,
                    None,
                ),
            )
            .expect("the page's own rows and edges are still valid");

        assert!(
            history.resolved_stubs().is_empty(),
            "an unknown anchor yields no placement"
        );
        assert_eq!(history.stubs.len(), 1, "but the stub is retained");
        // Geometry is complete and untouched by the orphan.
        assert_eq!(history.text_x().len(), history.rows.len());
        assert_eq!(history.label_occupancy(), [0, 0, 0]);
    }
}
