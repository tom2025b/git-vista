//! Terminal projection of git-vista-core's already-computed commit lanes.

use git_vista_core::color::{branch_color, HEAD_BADGE, TAG_BADGE};
use git_vista_core::model::{BranchStub, Edge, FrameStub, Graph, GraphRow, RefKind};

/// The colour vocabulary a caller has established for its terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    Basic,
    Ansi256,
}

/// A foreground-only style. The absence of a background is deliberate: the
/// renderer cannot manufacture an unreadable foreground/background pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Foreground {
    Default,
    Indexed(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emphasis {
    Normal,
    Bold,
}

/// One independently styled run in a terminal row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSpan {
    pub text: String,
    pub foreground: Foreground,
    pub emphasis: Emphasis,
}

/// One physical terminal row. Commit rows alternate with connector rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphLine {
    pub spans: Vec<GraphSpan>,
}

impl GraphLine {
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

/// The common renderer-facing shape shared by whole-graph and paged layout.
/// All fields are borrowed directly from core's output; no lane is assigned or
/// inferred here.
#[derive(Clone, Copy, Debug)]
pub struct LayoutData<'a, S> {
    pub rows: &'a [GraphRow],
    pub edges: &'a [Edge],
    pub stubs: &'a [S],
    pub lane_count: usize,
}

impl<'a> From<&'a Graph> for LayoutData<'a, BranchStub> {
    fn from(graph: &'a Graph) -> Self {
        Self {
            rows: &graph.rows,
            edges: &graph.edges,
            stubs: &graph.stubs,
            lane_count: graph.lane_count,
        }
    }
}

/// A core-computed local-branch stub, expressed only as the badge facts the
/// terminal projection needs. The two implementations preserve each layout
/// format's own anchor identity rather than translating it into new lanes.
pub trait BranchAnchor {
    fn name(&self) -> &str;
    fn color(&self) -> usize;
    fn points_to(&self, row: &GraphRow) -> bool;
}

impl BranchAnchor for BranchStub {
    fn name(&self) -> &str {
        &self.name
    }

    fn color(&self) -> usize {
        self.color
    }

    fn points_to(&self, row: &GraphRow) -> bool {
        self.anchor_row == row.row
    }
}

impl BranchAnchor for FrameStub {
    fn name(&self) -> &str {
        &self.name
    }

    fn color(&self) -> usize {
        self.color
    }

    fn points_to(&self, row: &GraphRow) -> bool {
        self.anchor_commit == row.commit.id
    }
}

/// Pure graph-pane state: borrowed core layout plus an established terminal
/// capability. A draw asks this state for only its physical-row window.
#[derive(Clone, Copy, Debug)]
pub struct GraphPane<'a, S> {
    layout: LayoutData<'a, S>,
    colors: ColorDepth,
}

impl<'a, S: BranchAnchor> GraphPane<'a, S> {
    pub fn new(layout: LayoutData<'a, S>, colors: ColorDepth) -> Self {
        Self { layout, colors }
    }

    pub fn row_count(&self) -> usize {
        if self.layout.rows.is_empty() {
            0
        } else {
            self.layout.rows.len().saturating_mul(2).saturating_sub(1)
        }
    }

    pub fn window(&self, line_offset: usize, height: usize) -> Vec<GraphLine> {
        render_window(&self.layout, line_offset, height, self.colors)
    }
}

/// Project only `[line_offset, line_offset + height)` from the laid-out graph.
pub fn render_window<S: BranchAnchor>(
    layout: &LayoutData<'_, S>,
    line_offset: usize,
    height: usize,
    colors: ColorDepth,
) -> Vec<GraphLine> {
    if height == 0 || layout.rows.is_empty() {
        return Vec::new();
    }

    let total_lines = layout.rows.len().saturating_mul(2).saturating_sub(1);
    let end = line_offset.saturating_add(height).min(total_lines);
    if line_offset >= end {
        return Vec::new();
    }

    // An edge is relevant when any part of its vertical span crosses the
    // requested terminal window. Like the SVG culler, this retains a long
    // edge that merely passes through the viewport.
    let first_layout_row = line_offset / 2;
    let last_layout_row = (end - 1) / 2;
    let visible_edges: Vec<&Edge> = layout
        .edges
        .iter()
        .filter(|edge| edge.from_row <= last_layout_row && edge.to_row >= first_layout_row)
        .collect();

    (line_offset..end)
        .filter_map(|physical_row| {
            let layout_row = physical_row / 2;
            if physical_row % 2 == 0 {
                render_commit_line(layout, &visible_edges, layout_row, colors)
            } else {
                Some(render_connector_line(
                    layout,
                    &visible_edges,
                    layout_row,
                    colors,
                ))
            }
        })
        .collect()
}

const NORTH: u8 = 1;
const EAST: u8 = 2;
const SOUTH: u8 = 4;
const WEST: u8 = 8;

#[derive(Clone, Copy)]
struct Cell {
    directions: u8,
    foreground: Foreground,
    emphasis: Emphasis,
    node: Option<char>,
    painted: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            directions: 0,
            foreground: Foreground::Default,
            emphasis: Emphasis::Normal,
            node: None,
            painted: false,
        }
    }
}

fn render_commit_line<S: BranchAnchor>(
    layout: &LayoutData<'_, S>,
    edges: &[&Edge],
    layout_row: usize,
    colors: ColorDepth,
) -> Option<GraphLine> {
    let row = layout.rows.get(layout_row)?;
    let mut cells = gutter(layout.lane_count);

    // Once an edge has left its child connector it occupies its parent's
    // lane until it reaches the parent node. This is what keeps long-running
    // and octopus lanes visible through intervening commit rows.
    for edge in edges {
        if edge.from_row < layout_row && layout_row <= edge.to_row {
            paint(
                &mut cells,
                lane_x(edge.to_lane),
                NORTH | SOUTH,
                edge_foreground(layout.rows, edge, colors),
            );
        }
    }

    let foreground = slot_foreground(row.color, colors);
    if let Some(cell) = cells.get_mut(lane_x(row.lane)) {
        cell.node = Some(if row.commit.is_merge() { '○' } else { '●' });
        cell.foreground = foreground;
        cell.emphasis = Emphasis::Bold;
        cell.painted = true;
    }

    let mut spans = cells_to_spans(cells);
    for git_ref in &row.refs {
        let (label, foreground) = match git_ref.kind {
            RefKind::Head => ("[HEAD]".to_string(), fixed_foreground(HEAD_BADGE, colors)),
            RefKind::Branch => (
                format!("[branch {}]", git_ref.name),
                slot_foreground(row.color, colors),
            ),
            RefKind::Tag => (
                format!("[tag {}]", git_ref.name),
                fixed_foreground(TAG_BADGE, colors),
            ),
            RefKind::RemoteBranch => continue,
        };
        spans.push(span(" ", Foreground::Default, Emphasis::Normal));
        spans.push(span(label, foreground, Emphasis::Bold));
    }
    for stub in layout.stubs.iter().filter(|stub| stub.points_to(row)) {
        spans.push(span(" ", Foreground::Default, Emphasis::Normal));
        spans.push(span(
            format!("[branch {}]", stub.name()),
            slot_foreground(stub.color(), colors),
            Emphasis::Bold,
        ));
    }
    spans.push(span(" ", Foreground::Default, Emphasis::Normal));
    spans.push(span(
        row.commit.summary.clone(),
        foreground,
        Emphasis::Normal,
    ));

    Some(GraphLine { spans })
}

fn render_connector_line<S>(
    layout: &LayoutData<'_, S>,
    edges: &[&Edge],
    layout_row: usize,
    colors: ColorDepth,
) -> GraphLine {
    let mut cells = gutter(layout.lane_count);
    for edge in edges {
        if !(edge.from_row <= layout_row && layout_row < edge.to_row) {
            continue;
        }
        let foreground = edge_foreground(layout.rows, edge, colors);
        if edge.from_row < layout_row || edge.from_lane == edge.to_lane {
            let lane = if edge.from_row == layout_row {
                edge.from_lane
            } else {
                edge.to_lane
            };
            paint(&mut cells, lane_x(lane), NORTH | SOUTH, foreground);
            continue;
        }

        let from = lane_x(edge.from_lane);
        let to = lane_x(edge.to_lane);
        if from < to {
            paint(&mut cells, from, NORTH | EAST, foreground);
            for x in from + 1..to {
                paint(&mut cells, x, EAST | WEST, foreground);
            }
            paint(&mut cells, to, SOUTH | WEST, foreground);
        } else {
            paint(&mut cells, from, NORTH | WEST, foreground);
            for x in to + 1..from {
                paint(&mut cells, x, EAST | WEST, foreground);
            }
            paint(&mut cells, to, SOUTH | EAST, foreground);
        }
    }
    GraphLine {
        spans: cells_to_spans(cells),
    }
}

fn gutter(lane_count: usize) -> Vec<Cell> {
    let lanes = lane_count.max(1);
    vec![Cell::default(); lanes.saturating_mul(2).saturating_sub(1)]
}

fn lane_x(lane: usize) -> usize {
    lane.saturating_mul(2)
}

fn paint(cells: &mut [Cell], x: usize, directions: u8, foreground: Foreground) {
    let Some(cell) = cells.get_mut(x) else {
        return;
    };
    cell.directions |= directions;
    if !cell.painted {
        cell.foreground = foreground;
    }
    cell.painted = true;
}

fn cells_to_spans(cells: Vec<Cell>) -> Vec<GraphSpan> {
    cells
        .into_iter()
        .map(|cell| {
            let glyph = cell.node.unwrap_or_else(|| box_glyph(cell.directions));
            span(glyph.to_string(), cell.foreground, cell.emphasis)
        })
        .collect()
}

fn box_glyph(directions: u8) -> char {
    const NS: u8 = NORTH | SOUTH;
    const EW: u8 = EAST | WEST;
    const ES: u8 = EAST | SOUTH;
    const SW: u8 = SOUTH | WEST;
    const NE: u8 = NORTH | EAST;
    const NW: u8 = NORTH | WEST;
    const NES: u8 = NORTH | EAST | SOUTH;
    const NSW: u8 = NORTH | SOUTH | WEST;
    const ESW: u8 = EAST | SOUTH | WEST;
    const NEW: u8 = NORTH | EAST | WEST;
    const NESW: u8 = NORTH | EAST | SOUTH | WEST;
    match directions {
        0 => ' ',
        NS => '│',
        EW => '─',
        ES => '┌',
        SW => '┐',
        NE => '└',
        NW => '┘',
        NES => '├',
        NSW => '┤',
        ESW => '┬',
        NEW => '┴',
        NESW => '┼',
        // A single direction is possible only for malformed layout input;
        // showing its axis is more honest than silently dropping the line.
        _ if directions & (NORTH | SOUTH) != 0 => '│',
        _ => '─',
    }
}

fn edge_foreground(rows: &[GraphRow], edge: &Edge, colors: ColorDepth) -> Foreground {
    let Some(from) = rows.get(edge.from_row) else {
        return Foreground::Default;
    };
    let Some(to) = rows.get(edge.to_row) else {
        return Foreground::Default;
    };
    let slot = if from.commit.parents.first() == Some(&to.commit.id) {
        from.color
    } else {
        to.color
    };
    slot_foreground(slot, colors)
}

fn slot_foreground(slot: usize, colors: ColorDepth) -> Foreground {
    fixed_foreground(branch_color(slot), colors)
}

fn fixed_foreground(hex: &str, colors: ColorDepth) -> Foreground {
    match colors {
        ColorDepth::Basic => Foreground::Default,
        ColorDepth::Ansi256 => Foreground::Indexed(hex_to_ansi256(hex).unwrap_or(7)),
    }
}

fn hex_to_ansi256(hex: &str) -> Option<u8> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    let cube = |channel: u8| ((u16::from(channel) * 5 + 127) / 255) as u8;
    Some(16 + 36 * cube(red) + 6 * cube(green) + cube(blue))
}

fn span(text: impl Into<String>, foreground: Foreground, emphasis: Emphasis) -> GraphSpan {
    GraphSpan {
        text: text.into(),
        foreground,
        emphasis,
    }
}

#[cfg(test)]
mod tests {
    use git_vista_core::layout::{layout, layout_with_refs};
    use git_vista_core::model::{CommitSummary, GitRef, Oid, RefKind};

    use super::*;

    fn commit(id: &str, parents: &[&str]) -> CommitSummary {
        CommitSummary {
            id: Oid(id.to_string()),
            parents: parents.iter().map(|id| Oid((*id).to_string())).collect(),
            summary: format!("commit {id}"),
            author: "Ada".to_string(),
            time: 0,
        }
    }

    fn git_ref(name: &str, kind: RefKind, target: &str) -> GitRef {
        GitRef {
            name: name.to_string(),
            kind,
            target: Oid(target.to_string()),
        }
    }

    fn whole_graph(graph: &Graph, colors: ColorDepth) -> Vec<GraphLine> {
        let pane = GraphPane::new(LayoutData::from(graph), colors);
        pane.window(0, pane.row_count())
    }

    #[test]
    fn merge_and_octopus_edges_fan_out_then_stay_vertical_to_their_parents() {
        let merge = layout(vec![
            commit("M", &["C", "D"]),
            commit("C", &["B"]),
            commit("D", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ]);
        let merge_lines = whole_graph(&merge, ColorDepth::Ansi256);
        assert!(
            merge_lines[1].plain_text().starts_with("├─┐"),
            "two parents must visibly split: {:?}",
            merge_lines[1]
        );
        assert!(
            merge_lines[2].plain_text().starts_with("● │"),
            "the second-parent lane must continue past the first parent: {:?}",
            merge_lines[2]
        );

        let octopus = layout(vec![
            commit("O", &["A", "B", "C"]),
            commit("A", &["R"]),
            commit("B", &["R"]),
            commit("C", &["R"]),
            commit("R", &[]),
        ]);
        let octopus_lines = whole_graph(&octopus, ColorDepth::Ansi256);
        assert!(
            octopus_lines[1].plain_text().starts_with("├─┬─┐"),
            "all three parent edges must leave the octopus node: {:?}",
            octopus_lines[1]
        );
        assert!(
            octopus_lines[2].plain_text().starts_with("● │ │"),
            "both outer parent lanes must remain continuous: {:?}",
            octopus_lines[2]
        );
        assert!(
            octopus_lines[4].plain_text().starts_with("│ ● │"),
            "the third-parent lane must survive while the second parent is drawn: {:?}",
            octopus_lines[4]
        );
    }

    #[test]
    fn head_local_branches_and_tags_are_badged_only_on_their_target_row() {
        let graph = layout_with_refs(
            vec![commit("tip", &["base"]), commit("base", &[])],
            vec![
                git_ref("HEAD", RefKind::Head, "tip"),
                git_ref("main", RefKind::Branch, "tip"),
                git_ref("fork", RefKind::Branch, "tip"),
                git_ref("v1.0", RefKind::Tag, "base"),
                git_ref("origin/main", RefKind::RemoteBranch, "tip"),
            ],
            Some("main"),
        );
        let lines = whole_graph(&graph, ColorDepth::Ansi256);
        let tip = lines[0].plain_text();
        let base = lines[2].plain_text();

        assert!(tip.contains("[HEAD]"), "{tip}");
        assert!(tip.contains("[branch main]"), "{tip}");
        assert!(
            tip.contains("[branch fork]"),
            "a core stub is still a local branch on this commit: {tip}"
        );
        assert!(!tip.contains("v1.0"), "{tip}");
        assert!(base.contains("[tag v1.0]"), "{base}");
        assert!(
            !base.contains("HEAD") && !base.contains("branch main"),
            "{base}"
        );
        assert!(
            !tip.contains("origin/main"),
            "remote refs are not local-branch badges: {tip}"
        );

        let page_stub = [FrameStub {
            name: "page-fork".to_string(),
            anchor_commit: Oid("base".to_string()),
            lane_offset: 0,
            color: 2,
            depth: 0,
        }];
        let page = GraphPane::new(
            LayoutData {
                rows: &graph.rows,
                edges: &graph.edges,
                stubs: &page_stub,
                lane_count: graph.lane_count,
            },
            ColorDepth::Ansi256,
        );
        let page_lines = page.window(0, page.row_count());
        assert!(!page_lines[0].plain_text().contains("page-fork"));
        assert!(page_lines[2].plain_text().contains("[branch page-fork]"));
    }

    #[test]
    fn a_large_graph_materializes_only_the_requested_terminal_window() {
        let commits = (0..5_000)
            .rev()
            .map(|n| {
                let id = n.to_string();
                let parents = (n > 0).then(|| (n - 1).to_string());
                CommitSummary {
                    id: Oid(id.clone()),
                    parents: parents.into_iter().map(Oid).collect(),
                    summary: format!("commit {id}"),
                    author: "Ada".to_string(),
                    time: 0,
                }
            })
            .collect();
        let graph = layout(commits);

        let pane = GraphPane::new(LayoutData::from(&graph), ColorDepth::Ansi256);
        let lines = pane.window(8_000, 7);
        assert_eq!(lines.len(), 7, "only viewport-height rows are materialized");
        let text = lines
            .iter()
            .map(GraphLine::plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("commit 999"), "{text}");
        assert!(text.contains("commit 996"), "{text}");
        assert!(
            !text.contains("commit 4999"),
            "off-screen head leaked in: {text}"
        );
        assert!(
            !text.contains("commit 0"),
            "off-screen root leaked in: {text}"
        );
        assert!(pane.window(0, 0).is_empty());
        assert!(pane.window(10_000, 7).is_empty());
    }

    #[test]
    fn basic_terminals_receive_only_their_default_foreground() {
        let graph = layout(vec![commit("tip", &["base"]), commit("base", &[])]);
        let basic = whole_graph(&graph, ColorDepth::Basic);
        assert!(
            basic
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.foreground == Foreground::Default),
            "a basic terminal must not be sent an indexed approximation: {basic:?}"
        );

        let rich = whole_graph(&graph, ColorDepth::Ansi256);
        assert!(
            rich.iter()
                .flat_map(|line| &line.spans)
                .any(|span| matches!(span.foreground, Foreground::Indexed(_))),
            "the established 256-colour capability should preserve lane colour"
        );
    }

    #[test]
    fn layout_color_slots_drive_stable_terminal_colours() {
        let graph = layout_with_refs(
            vec![
                commit("M", &["main", "side2"]),
                commit("main", &["base"]),
                commit("side2", &["side1"]),
                commit("side1", &["base"]),
                commit("base", &[]),
            ],
            vec![
                git_ref("HEAD", RefKind::Head, "M"),
                git_ref("main", RefKind::Branch, "M"),
                git_ref("feature", RefKind::Branch, "side2"),
            ],
            Some("main"),
        );
        let lines = whole_graph(&graph, ColorDepth::Ansi256);
        let color_of_node = |row: usize| {
            lines[row * 2]
                .spans
                .iter()
                .find(|span| span.text.contains('●') || span.text.contains('○'))
                .expect("a node span")
                .foreground
        };
        let side2 = graph
            .rows
            .iter()
            .position(|row| row.commit.id.0 == "side2")
            .unwrap();
        let side1 = graph
            .rows
            .iter()
            .position(|row| row.commit.id.0 == "side1")
            .unwrap();
        let main = graph
            .rows
            .iter()
            .position(|row| row.commit.id.0 == "main")
            .unwrap();

        assert_eq!(graph.rows[side2].color, graph.rows[side1].color);
        assert_eq!(color_of_node(side2), color_of_node(side1));
        assert_ne!(color_of_node(main), color_of_node(side1));
        assert_eq!(
            lines[1].spans[0].foreground,
            color_of_node(0),
            "the first-parent edge leaves the merge in its child's colour"
        );
        assert_eq!(
            lines[1].spans[lane_x(graph.rows[side2].lane)].foreground,
            color_of_node(side2),
            "the non-first-parent edge arrives in the merged branch's colour"
        );
    }
}
