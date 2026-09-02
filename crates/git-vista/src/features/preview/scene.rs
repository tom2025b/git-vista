//! The static before/after picture, as pure data (M10.08 A6, #594).
//!
//! Framework-free and host-tested, like every other `core.rs` in this tree —
//! this module is the whole of the preview *renderer*, and the Leptos view
//! beside it only turns the shapes below into elements. Nothing here knows
//! about the DOM, a camera, a viewport or a gesture.
//!
//! # Why a second renderer, and not the canvas
//!
//! The page renderer cannot serve a modal, and the reasons are structural
//! rather than stylistic (checked against the source on 2026-09-01, #594's own
//! correction):
//!
//! * `render::build_node` (`render/nodes.rs:41`) takes a `StoredValue<RenderCtx>`,
//!   a `StoredValue<DisplayProjection>`, a `Shell`, an `RwSignal<GraphFocus>`,
//!   an `RwSignal<Camera>`, a viewport height and an `on_expand` callback.
//!   `Camera`, viewport height and focus exist for M1.13's roving tabindex and
//!   scroll-into-view (#65) — they are *page* concepts, and a modal has none
//!   of them.
//! * `render::visible_edges` (`render/edges.rs:26`) is viewport-range culling.
//! * `app::canvas::graph_canvas` owns the gesture signals, the window
//!   listeners, the overlay stack and — since M1.10 (#63) — the paged append
//!   loop that mutates the aggregate in place as history arrives.
//! * The builders read a `DisplayProjection` (`features/graph/collapse.rs:170`),
//!   which is the WIP-collapse projection: folding, expanded runs, display-row
//!   mapping. A preview graph has none of that and never will.
//!
//! Bending any of that to serve a modal would drag camera, focus and collapse
//! state into a dialog. A preview is a handful of rows at a fixed size, so it
//! costs a few hundred lines of arithmetic instead — and the arithmetic is
//! testable on the host, which nothing in `render/` is.
//!
//! # Windowing is the one genuinely hard decision here
//!
//! The server walks up to `PREVIEW_HISTORY_LIMIT` (500) commits into each half
//! (`git-vista-server/src/preview.rs:143`). A confirmation modal can show ten.
//! So this module picks a window — and picks it around **what changed**, never
//! blindly off the top: a preview whose one added commit was cropped out of
//! the picture is worse than no picture, because it looks like an answer.
//!
//! The two halves are windowed *together*: the after half is windowed around
//! its marks, and the before half is then windowed onto the same **commits**,
//! by id. Row indices cannot be shared — prepending one hypothetical commit
//! shifts every row beneath it — so anything that matched the two halves by
//! row number would silently compare unrelated commits.

use std::collections::{HashMap, HashSet};

use git_vista_core::color::{branch_color, BADGE_DARK, HEAD_BADGE, MERGE_FILL, TAG_BADGE};
use git_vista_core::model::RefKind;

use super::core::{Half, Picture, RowMark};
use crate::text::truncate;

// ---------------------------------------------------------------------------
// Geometry. Compact, fixed, and this module's own.
// ---------------------------------------------------------------------------

/// Vertical gap between rows, in SVG user units.
///
/// Less than half `geometry::ROW_HEIGHT` (56): the canvas gives each row two
/// text lines, and a preview row carries one.
pub const ROW_H: i32 = 26;
/// Horizontal gap between lanes.
pub const LANE_W: i32 = 16;
/// Commit-dot radius.
pub const DOT_R: i32 = 5;
/// Left inset of lane 0.
pub const PAD_X: i32 = 14;
/// Top inset of row 0 — deeper than `PAD_X` so a stub ring hanging half a row
/// above the topmost commit still fits inside the viewBox.
pub const PAD_Y: i32 = 20;
/// Bottom inset below the last row.
pub const PAD_BOTTOM: i32 = 12;
/// Gap between the rightmost lane and the start of the label column.
pub const LABEL_GAP: i32 = 12;
/// Fixed width of the label column. Fixed rather than measured so both halves
/// are the same width and their rows line up across the gap between them.
pub const LABEL_W: i32 = 208;
/// Per-character advance of the label text (11px monospace), for fitting.
pub const LABEL_CHAR_W: i32 = 6;

/// Height of a tag pill (`new`, a ref badge, `lane 2→1`).
pub const TAG_H: i32 = 14;
/// Corner radius of a tag pill.
pub const TAG_R: i32 = 3;
/// Gap between adjacent tag pills, and after the last one.
pub const TAG_GAP: i32 = 4;
/// Per-character advance inside a tag pill (10px monospace).
pub const TAG_CHAR_W: i32 = 6;
/// Inner horizontal padding of a tag pill.
pub const TAG_PAD_X: i32 = 4;

/// The most commit rows either half draws.
///
/// Ten, not five hundred. The cap is what makes the picture fit a modal at
/// all; [`window_for_after`] is what makes the ten it keeps the *right* ten.
pub const MAX_ROWS: usize = 10;

/// The most lanes either half draws before lanes are clamped.
///
/// A repository with forty branch stubs would otherwise make the gutter wider
/// than the label column. Lanes past this are drawn *at* this lane, which is a
/// visible squash rather than a silent crop — and [`HalfScene::lanes_clamped`]
/// says so on the face of the picture.
pub const MAX_LANES: usize = 8;

// ---------------------------------------------------------------------------
// Mark colours.
// ---------------------------------------------------------------------------
//
// These are ANNOTATION colours, not branch colours. `git_vista_core::color` is
// the single source of truth for what colour a *branch* is, and this module
// asks it for every dot, line and ref badge below. What it has no vocabulary
// for is "this thing is new", "this ref moved here", "this commit changed
// lane" — three marks that exist only inside a preview. Naming them here keeps
// the Color God's rule intact ("nothing else defines a palette") rather than
// widening a shared palette for one modal.

/// The added (hypothetical) commit: the halo, and its `new` pill.
pub const MARK_ADDED: &str = "#3fb950";
/// A ref that lands on this commit: its badge outline and the arrow glyph.
pub const MARK_REF: &str = "#a371f7";
/// A commit whose lane number differs between the halves.
pub const MARK_LANE: &str = "#8b949e";

// ---------------------------------------------------------------------------
// Windowing.
// ---------------------------------------------------------------------------

/// An inclusive span of **row values** — `GraphRow::row`, not a position in
/// the `rows` vector.
///
/// Row values are what `Edge::from_row`/`to_row` speak in, so windowing in
/// that space is what lets an edge be clipped without a lookup table. It also
/// survives a half whose `rows` vector is not in row order, which nothing
/// promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowWindow {
    pub first: usize,
    pub last: usize,
}

impl RowWindow {
    /// Whether `row` is drawn.
    pub fn holds(&self, row: usize) -> bool {
        row >= self.first && row <= self.last
    }

    /// How many rows the window spans.
    pub fn len(&self) -> usize {
        self.last + 1 - self.first
    }
}

/// The window of `after` a modal should draw: centred on the marked rows,
/// padded with context, capped at `budget`.
///
/// With no marks at all — the "nothing would change" preview — this falls back
/// to the newest `budget` rows, which is the top of the graph.
pub fn window_for_after(
    after: &Half,
    marks: &HashMap<String, RowMark>,
    budget: usize,
) -> RowWindow {
    let Some((min_row, max_row)) = row_bounds(after) else {
        return RowWindow { first: 0, last: 0 };
    };
    let marked: Vec<usize> = after
        .rows
        .iter()
        .filter(|r| marks.get(&r.commit.id.0).is_some_and(|m| m.is_marked()))
        .map(|r| r.row)
        .collect();
    let (lo, hi) = match (marked.iter().min(), marked.iter().max()) {
        (Some(lo), Some(hi)) => (*lo, *hi),
        // No marks: anchor on the newest row and let the padding run downward.
        _ => (min_row, min_row),
    };
    window_around(min_row, max_row, lo, hi, budget)
}

/// The window of `before` that shows the same commits as `after`'s window.
///
/// Matched by commit id, never by row number: the whole point of the before
/// half is that its row numbering need not agree with the after half's, which
/// is exactly why `PreviewChange::LaneShifted` has to be computed by the
/// server and cannot be re-derived by a client holding one half.
///
/// A window whose commits are all absent from `before` — every row in it is
/// new — falls back to `before`'s own newest rows, so the left-hand picture is
/// still the repository as it stands rather than an empty box.
pub fn window_for_before(
    before: &Half,
    after: &Half,
    after_window: RowWindow,
    budget: usize,
) -> RowWindow {
    let Some((min_row, max_row)) = row_bounds(before) else {
        return RowWindow { first: 0, last: 0 };
    };
    let shown: HashSet<&str> = after
        .rows
        .iter()
        .filter(|r| after_window.holds(r.row))
        .map(|r| r.commit.id.0.as_str())
        .collect();
    let matched: Vec<usize> = before
        .rows
        .iter()
        .filter(|r| shown.contains(r.commit.id.0.as_str()))
        .map(|r| r.row)
        .collect();
    let (lo, hi) = match (matched.iter().min(), matched.iter().max()) {
        (Some(lo), Some(hi)) => (*lo, *hi),
        _ => (min_row, min_row),
    };
    window_around(min_row, max_row, lo, hi, budget)
}

/// The lowest and highest `row` value present, or `None` for an empty half.
fn row_bounds(half: &Half) -> Option<(usize, usize)> {
    let min = half.rows.iter().map(|r| r.row).min()?;
    let max = half.rows.iter().map(|r| r.row).max()?;
    Some((min, max))
}

/// A window of at most `budget` rows covering `lo..=hi`, padded with context
/// and clamped inside `min_row..=max_row`.
///
/// When the marked span alone exceeds the budget the window starts at `lo` and
/// is cut short: the *first* thing that changed is the one a reader most needs
/// to see, and the caption says how much was left out.
fn window_around(min_row: usize, max_row: usize, lo: usize, hi: usize, budget: usize) -> RowWindow {
    let budget = budget.max(1);
    let lo = lo.clamp(min_row, max_row);
    let hi = hi.clamp(lo, max_row);
    if hi + 1 - lo >= budget {
        return RowWindow {
            first: lo,
            last: (lo + budget - 1).min(max_row),
        };
    }
    let pad = budget - (hi + 1 - lo);
    let mut first = lo.saturating_sub(pad / 2).max(min_row);
    let mut last = first + budget - 1;
    if last > max_row {
        last = max_row;
        first = (last + 1).saturating_sub(budget).max(min_row);
    }
    RowWindow { first, last }
}

// ---------------------------------------------------------------------------
// The scene.
// ---------------------------------------------------------------------------

/// One line of the SVG's edge layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneEdge {
    /// SVG path data — a straight line in-lane, a vertical S-curve across lanes.
    pub d: String,
    pub color: &'static str,
    /// True when the edge runs off the top or bottom of the window, so the
    /// view can fade it rather than let it stop dead at the frame.
    pub clipped: bool,
}

/// A branch stub — a branch owning no commits of its own — as a ring hanging
/// off its anchor commit.
///
/// Drawn rather than dropped for the reason `PreviewGraph`'s own doc gives: an
/// after graph with no stubs, beside a before graph with them, reads as "this
/// operation deleted my branches".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneStub {
    pub name: String,
    /// Connector from the anchor commit to the ring.
    pub d: String,
    pub cx: i32,
    pub cy: i32,
    pub r: i32,
    pub color: &'static str,
}

/// A pill drawn in the label column, before the commit summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneTag {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Pill fill.
    pub fill: &'static str,
    /// Pill outline. Equal to `fill` for an unmarked badge; a mark colour when
    /// this tag is one of the changes.
    pub stroke: &'static str,
    /// Text colour.
    pub fg: &'static str,
}

/// One commit dot, with its label and marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneNode {
    pub cx: i32,
    pub cy: i32,
    pub r: i32,
    pub color: &'static str,
    /// Merge commits are drawn hollow, exactly as on the canvas.
    pub hollow: bool,
    /// Radius of the halo drawn around an added commit; `None` for every
    /// commit that already exists.
    pub halo: Option<i32>,
    pub tags: Vec<SceneTag>,
    /// Baseline of the summary text, after the tags.
    pub label_x: i32,
    pub label_y: i32,
    pub label: String,
    /// Whether this row carries any mark at all — the view draws a marked row
    /// at full strength and an unmarked one dimmed, so the eye lands on what
    /// changed.
    pub marked: bool,
    /// What a screen reader is told about this row, marks included.
    pub alt: String,
}

/// One half of the picture, laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalfScene {
    pub title: &'static str,
    pub width: i32,
    pub height: i32,
    pub edges: Vec<SceneEdge>,
    pub nodes: Vec<SceneNode>,
    pub stubs: Vec<SceneStub>,
    /// Rows newer than the window, when any were left out.
    pub elided_above: Option<String>,
    /// Rows older than the window, when any were left out.
    pub elided_below: Option<String>,
    /// True when this half used more lanes than [`MAX_LANES`] and the surplus
    /// were squashed into the last drawn lane.
    pub lanes_clamped: bool,
    /// The `aria-label` of the whole `<svg>`.
    pub alt: String,
}

/// Which mark vocabulary the picture actually used, so the legend names only
/// what is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegendMark {
    Added,
    RefMoved,
    LaneShifted,
    Stub,
}

/// One legend row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegendEntry {
    pub mark: LegendMark,
    pub color: &'static str,
    pub text: &'static str,
}

/// The whole panel: two halves, the sentence, and the legend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewScene {
    pub before: HalfScene,
    pub after: HalfScene,
    /// [`Picture::summary`], carried through unchanged.
    pub summary: String,
    pub legend: Vec<LegendEntry>,
}

/// Lay a [`Picture`] out as two static SVG scenes.
///
/// Both halves are laid out together, and share one lane count and one width,
/// so a commit that did not move sits at the same height and the same x in
/// both pictures. That is what makes a difference visible by comparison rather
/// than by reading two independent diagrams.
pub fn scene_of(picture: &Picture) -> PreviewScene {
    let after_window = window_for_after(&picture.after, &picture.marks, MAX_ROWS);
    let before_window = window_for_before(&picture.before, &picture.after, after_window, MAX_ROWS);

    // One shared gutter width. `lane_count` is `Graph::lane_count` verbatim —
    // stub columns included — so it is already the right number to size a
    // gutter from, and taking the max of the two halves keeps the pictures
    // aligned when an operation adds or removes a lane.
    let raw_lanes = picture
        .before
        .lane_count
        .max(picture.after.lane_count)
        .max(1);
    let lanes = raw_lanes.min(MAX_LANES);
    let clamped = raw_lanes > MAX_LANES;

    let before = half_scene(
        "Before",
        &picture.before,
        before_window,
        lanes,
        clamped,
        &HashMap::new(),
    );
    let after = half_scene(
        "After",
        &picture.after,
        after_window,
        lanes,
        clamped,
        &picture.marks,
    );

    PreviewScene {
        legend: legend_for(&picture.marks, &before, &after),
        before,
        after,
        summary: picture.summary.clone(),
    }
}

/// The legend, holding only the marks this picture actually drew.
fn legend_for(
    marks: &HashMap<String, RowMark>,
    before: &HalfScene,
    after: &HalfScene,
) -> Vec<LegendEntry> {
    let mut out = Vec::new();
    if marks.values().any(|m| m.added) {
        out.push(LegendEntry {
            mark: LegendMark::Added,
            color: MARK_ADDED,
            text: "a commit this operation would create",
        });
    }
    if marks.values().any(|m| !m.refs_landed.is_empty()) {
        out.push(LegendEntry {
            mark: LegendMark::RefMoved,
            color: MARK_REF,
            text: "a branch or HEAD that would end up here",
        });
    }
    if marks.values().any(|m| m.lane_shift.is_some()) {
        out.push(LegendEntry {
            mark: LegendMark::LaneShifted,
            color: MARK_LANE,
            text: "a commit drawn in a different column than before",
        });
    }
    if !before.stubs.is_empty() || !after.stubs.is_empty() {
        out.push(LegendEntry {
            mark: LegendMark::Stub,
            color: MARK_LANE,
            text: "a branch with no commits of its own",
        });
    }
    out
}

/// x of a lane's centre, with lanes past the gutter squashed onto the last one.
fn lane_cx(lane: usize, lanes: usize) -> i32 {
    PAD_X + (lane.min(lanes.saturating_sub(1))) as i32 * LANE_W
}

/// y of a row value, relative to the window's first row. Rows outside the
/// window get a y outside the drawn band, which is what lets an edge leaving
/// the window be clipped by arithmetic rather than by a special case.
fn row_cy(row: usize, window: RowWindow) -> i32 {
    PAD_Y + (row as i32 - window.first as i32) * ROW_H
}

/// Left edge of the label column.
fn label_left(lanes: usize) -> i32 {
    lane_cx(lanes.saturating_sub(1), lanes) + LABEL_GAP
}

/// Width of a tag pill holding `text`.
pub fn tag_width(text: &str) -> i32 {
    text.chars().count() as i32 * TAG_CHAR_W + 2 * TAG_PAD_X
}

/// Lay one half out.
fn half_scene(
    title: &'static str,
    half: &Half,
    window: RowWindow,
    lanes: usize,
    lanes_clamped: bool,
    marks: &HashMap<String, RowMark>,
) -> HalfScene {
    let width = label_left(lanes) + LABEL_W;
    let drawn: Vec<_> = half.rows.iter().filter(|r| window.holds(r.row)).collect();
    let height = PAD_Y + (window.len() as i32 - 1).max(0) * ROW_H + PAD_BOTTOM;

    // The y band an edge may occupy: half a row of overhang past the first and
    // last drawn rows, so an edge continuing past the window visibly runs off
    // rather than stopping dead on a dot that is not there.
    let top = PAD_Y - ROW_H / 2;
    let bottom = PAD_Y + (window.len() as i32 - 1).max(0) * ROW_H + ROW_H / 2;

    let mut edges = Vec::new();
    for e in &half.edges {
        let inside_from = window.holds(e.from_row);
        let inside_to = window.holds(e.to_row);
        // Both endpoints off the same side: the edge never crosses the window.
        if !inside_from && !inside_to {
            let both_above = e.from_row < window.first && e.to_row < window.first;
            let both_below = e.from_row > window.last && e.to_row > window.last;
            if both_above || both_below {
                continue;
            }
        }
        let x1 = lane_cx(e.from_lane, lanes);
        let x2 = lane_cx(e.to_lane, lanes);
        let y1 = row_cy(e.from_row, window).clamp(top, bottom);
        let y2 = row_cy(e.to_row, window).clamp(top, bottom);
        let d = if x1 == x2 {
            format!("M {x1} {y1} L {x2} {y2}")
        } else {
            let ym = (y1 + y2) / 2;
            format!("M {x1} {y1} C {x1} {ym}, {x2} {ym}, {x2} {y2}")
        };
        // Colour the edge like the commit it descends from, matching the
        // canvas: an edge takes the child's line.
        let color = half
            .rows
            .iter()
            .find(|r| r.row == e.from_row)
            .map_or(MARK_LANE, |r| branch_color(r.color));
        edges.push(SceneEdge {
            d,
            color,
            clipped: !inside_from || !inside_to,
        });
    }

    let mut nodes = Vec::new();
    for r in &drawn {
        let mark = marks.get(&r.commit.id.0);
        let cx = lane_cx(r.lane, lanes);
        let cy = row_cy(r.row, window);
        let color = branch_color(r.color);
        let (tags, label_x, label) = row_label(r, mark, lanes, cy);
        nodes.push(SceneNode {
            cx,
            cy,
            r: DOT_R,
            color,
            hollow: r.commit.is_merge(),
            halo: mark.filter(|m| m.added).map(|_| DOT_R + 4),
            tags,
            label_x,
            label_y: cy + 4,
            label,
            marked: mark.is_some_and(|m| m.is_marked()),
            alt: row_alt(r, mark),
        });
    }

    let stubs = half
        .stubs
        .iter()
        .filter(|s| window.holds(s.anchor_row))
        .map(|s| {
            let ax = lane_cx(s.anchor_lane, lanes);
            let ay = row_cy(s.anchor_row, window);
            let cx = lane_cx(s.lane, lanes);
            // Stubs cascade upward off their anchor, half a row per depth —
            // the same staircase `geometry::stub_node_cy` draws on the canvas,
            // at this module's scale.
            let cy = ay - (s.depth as i32 + 1) * (ROW_H / 2);
            let ym = (ay + cy) / 2;
            SceneStub {
                name: s.name.clone(),
                d: format!("M {ax} {ay} C {ax} {ym}, {cx} {ym}, {cx} {cy}"),
                cx,
                cy,
                r: DOT_R - 1,
                color: branch_color(s.color),
            }
        })
        .collect::<Vec<_>>();

    let shown = drawn.len();
    let above = half.rows.iter().filter(|r| r.row < window.first).count();
    let below = half.rows.iter().filter(|r| r.row > window.last).count();

    HalfScene {
        title,
        width,
        height,
        edges,
        nodes,
        stubs,
        elided_above: (above > 0)
            .then(|| plural(above, "newer commit not shown", "newer commits not shown")),
        elided_below: (below > 0)
            .then(|| plural(below, "older commit not shown", "older commits not shown")),
        lanes_clamped,
        alt: format!(
            "{title}: {}",
            plural(shown, "commit drawn", "commits drawn")
        ),
    }
}

/// `1 one thing` / `4 many things`.
fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The tags and summary for one row, laid out left to right inside the label
/// column, with the summary truncated to whatever width the tags left it.
fn row_label(
    r: &git_vista_core::model::GraphRow,
    mark: Option<&RowMark>,
    lanes: usize,
    cy: i32,
) -> (Vec<SceneTag>, i32, String) {
    let mut x = label_left(lanes);
    // Pills straddle the row's centre line, so the summary baseline beside
    // them (`label_y`, `cy + 4`) sits inside the pill rather than under it.
    let y = cy - TAG_H / 2;
    let mut tags = Vec::new();
    let right = label_left(lanes) + LABEL_W;

    let push = |text: String,
                fill: &'static str,
                stroke: &'static str,
                fg: &'static str,
                x: &mut i32,
                tags: &mut Vec<SceneTag>| {
        let w = tag_width(&text);
        // A pill that would not fit is dropped rather than drawn off the edge.
        // Dropping is honest here in a way overflow is not: the summary that
        // follows is truncated for the same reason, and both are visible as
        // truncation.
        if *x + w > right {
            return;
        }
        tags.push(SceneTag {
            text,
            x: *x,
            y,
            w,
            h: TAG_H,
            fill,
            stroke,
            fg,
        });
        *x += w + TAG_GAP;
    };

    if mark.is_some_and(|m| m.added) {
        push(
            "new".to_string(),
            MARK_ADDED,
            MARK_ADDED,
            BADGE_DARK,
            &mut x,
            &mut tags,
        );
    }
    let landed: &[String] = mark.map_or(&[][..], |m| &m.refs_landed);
    for gref in &r.refs {
        let moved = landed.iter().any(|n| n == &gref.name);
        let fill = match gref.kind {
            RefKind::Head => HEAD_BADGE,
            RefKind::Tag => TAG_BADGE,
            RefKind::Branch | RefKind::RemoteBranch => branch_color(r.color),
        };
        let text = if moved {
            format!("→{}", gref.name)
        } else {
            gref.name.clone()
        };
        let stroke = if moved { MARK_REF } else { fill };
        push(text, fill, stroke, BADGE_DARK, &mut x, &mut tags);
    }
    // A ref the server says lands here that the layout did not attach to this
    // row is still reported. Silence would be the one wrong answer: the change
    // list is the authority on what moves, and a row missing its badge would
    // read as "nothing moved here".
    for name in landed {
        if !r.refs.iter().any(|g| &g.name == name) {
            push(
                format!("→{name}"),
                MARK_REF,
                MARK_REF,
                BADGE_DARK,
                &mut x,
                &mut tags,
            );
        }
    }
    if let Some((from, to)) = mark.and_then(|m| m.lane_shift) {
        push(
            format!("lane {from}→{to}"),
            MERGE_FILL,
            MARK_LANE,
            MARK_LANE,
            &mut x,
            &mut tags,
        );
    }

    let room = ((right - x) / LABEL_CHAR_W).max(0) as usize;
    let label = truncate(r.commit.summary.trim(), room);
    (tags, x, label)
}

/// What a screen reader is told about one row.
fn row_alt(r: &git_vista_core::model::GraphRow, mark: Option<&RowMark>) -> String {
    let short: String = r.commit.id.0.chars().take(7).collect();
    let mut out = format!("{short} {}", r.commit.summary.trim());
    if let Some(m) = mark {
        if m.added {
            out.push_str(" — new, this operation would create it");
        }
        if !m.refs_landed.is_empty() {
            out.push_str(&format!(
                " — {} would end up here",
                m.refs_landed.join(", ")
            ));
        }
        if let Some((from, to)) = m.lane_shift {
            out.push_str(&format!(" — moves from column {from} to column {to}"));
        }
    }
    out
}

#[cfg(test)]
#[path = "scene_suite.rs"]
mod scene_suite;
