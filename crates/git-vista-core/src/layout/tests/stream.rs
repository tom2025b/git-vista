//! Stream tests: the checkpointable topology core behind both public layout
//! entry points.
//!
//! These exercise [`StreamLayout`](crate::layout::stream::StreamLayout) directly
//! — absolute row numbering across checkpoints, lane state carried in
//! [`LayoutCheckpoint`](crate::layout::stream::LayoutCheckpoint), pending/resolved
//! edge sidecars, per-page delivery order vs. full-aggregate canonical order,
//! exact parent membership, and terminal cleanup — plus the serialized
//! equivalence that pins the stream to the legacy one-shot layout.

use std::collections::HashSet;

use super::*;
use crate::layout::stream::{
    canonicalize_edges, sort_resolved_edges, strip_resolved_edges, LayoutChunk, StreamLayout,
};
use crate::layout::topology::{stable_topo_order, trunk_reserve_tip};
use crate::layout::{decorate, layout, layout_with_refs};
use crate::model::GraphRow;

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

/// [`commit`] with an explicit commit time — the shared fixture hardcodes 0, and
/// several of these tests turn on time order (equal-second tips, a parent newer
/// than its child).
fn at(id: &str, parents: &[&str], time: i64) -> CommitSummary {
    let mut c = commit(id, parents);
    c.time = time;
    c
}

/// Every row number every chunk emitted, in chunk order — the absolute row
/// numbering has to run straight through the checkpoints.
fn row_numbers(chunks: &[LayoutChunk]) -> Vec<usize> {
    chunks
        .iter()
        .flat_map(|c| c.rows.iter().map(|r| r.row))
        .collect()
}

/// `id -> parent id` for wire edges, so assertions can talk about commits
/// instead of row numbers. `all_rows` must be the *full* aggregate.
fn wire_names(all_rows: &[GraphRow], edges: &[Edge]) -> Vec<(String, String)> {
    edges
        .iter()
        .map(|e| {
            (
                all_rows[e.from_row].commit.id.0.clone(),
                all_rows[e.to_row].commit.id.0.clone(),
            )
        })
        .collect()
}

/// The same, for the resolved-edge sidecars carried by a run of chunks.
fn edge_names(chunks: &[LayoutChunk], all_rows: &[GraphRow]) -> Vec<(String, String)> {
    let edges: Vec<Edge> = chunks
        .iter()
        .flat_map(|c| c.resolved_edges.iter().map(|r| r.edge.clone()))
        .collect();
    wire_names(all_rows, &edges)
}

/// Feed an already-normalised commit vector through [`StreamLayout`] in windows
/// of `window` commits: checkpoint between windows, `finish` the last one.
fn stream_chunks(
    normalized: &[CommitSummary],
    trunk_tip: Option<Oid>,
    present: &HashSet<Oid>,
    window: usize,
) -> Vec<LayoutChunk> {
    assert!(window > 0, "a window has to hold at least one commit");
    let mut chunks = Vec::new();
    let mut stream = StreamLayout::new(trunk_tip);
    let mut rest = normalized;
    loop {
        let take = window.min(rest.len());
        for c in &rest[..take] {
            stream.push(c.clone(), |oid| present.contains(oid));
        }
        rest = &rest[take..];
        if rest.is_empty() {
            chunks.push(stream.finish());
            break;
        }
        let (chunk, checkpoint) = stream.checkpoint();
        chunks.push(chunk);
        stream = StreamLayout::resume(checkpoint);
    }
    chunks
}

/// Concatenate page chunks into one full aggregate: per-page delivery order
/// first, then the full-aggregate canonical order.
fn aggregate(chunks: Vec<LayoutChunk>) -> Graph {
    let mut rows = Vec::new();
    let mut edges = Vec::new();
    let mut lane_count = 0;
    for mut chunk in chunks {
        sort_resolved_edges(&mut chunk.resolved_edges);
        lane_count = lane_count.max(chunk.lane_count);
        rows.append(&mut chunk.rows);
        edges.extend(strip_resolved_edges(chunk.resolved_edges));
    }
    canonicalize_edges(&rows, &mut edges);
    Graph {
        rows,
        edges,
        lane_count,
        ..Default::default()
    }
}

/// The full stream-backed pipeline, driven straight from the public pieces:
/// exact membership set, filtered trunk tip, windowed feed, sidecar stripping,
/// canonical edge order, then the shared [`decorate`] pass — the same one both
/// legacy entry points run, so a windowed feed and a one-shot one differ in
/// nothing but the window size.
fn stream_graph(
    normalized: &[CommitSummary],
    refs: &[GitRef],
    head_branch: Option<&str>,
    window: usize,
) -> Graph {
    let present: HashSet<Oid> = normalized.iter().map(|c| c.id.clone()).collect();
    let trunk_tip = trunk_reserve_tip(refs, head_branch).filter(|t| present.contains(t));
    let mut graph = aggregate(stream_chunks(normalized, trunk_tip, &present, window));
    decorate(&mut graph, refs.to_vec(), head_branch);
    graph
}

/// A 12-commit DAG with a trunk (`a06..a12`), two merged side lines (`s01/s02`
/// merged at `a11`, `u01`/`v01` octopus-merged at `a09`), an octopus parent
/// vector whose order differs from arrival order, a tag, a local branch `aaa`
/// on an interior trunk commit (which must render as a stub), two equal-second
/// tips (`a12`/`b01`), and a parent (`a07`) whose timestamp is newer than every
/// one of its children. Deliberately handed over in scrambled input order.
fn equivalence_fixture() -> (Vec<CommitSummary>, Vec<GitRef>) {
    let commits = vec![
        at("a08", &["a07"], 992),
        at("a12", &["a11"], 1000),
        at("v01", &["a07"], 993),
        at("a09", &["a08", "u01", "v01"], 995),
        at("s01", &["a08"], 997),
        at("a06", &[], 991),
        at("b01", &["a09"], 1000),
        at("a11", &["a10", "s02"], 999),
        at("u01", &["a07"], 994),
        at("a10", &["a09"], 996),
        at("s02", &["s01"], 998),
        at("a07", &["a06"], 1001),
    ];
    let refs = vec![
        gitref("main", RefKind::Branch, "a12"),
        gitref("side", RefKind::Branch, "s02"),
        gitref("aaa", RefKind::Branch, "a10"),
        gitref("v1.0", RefKind::Tag, "a08"),
        gitref("origin/main", RefKind::RemoteBranch, "a12"),
    ];
    (commits, refs)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn one_shot_windows_7_and_1_are_byte_identical() {
    let (commits, refs) = equivalence_fixture();
    // Normalise once; every window size then sees the exact same vector.
    let normalized = stable_topo_order(commits);
    assert_eq!(normalized.len(), 12);

    let one_shot = stream_graph(&normalized, &refs, Some("main"), normalized.len());
    let windows_7 = stream_graph(&normalized, &refs, Some("main"), 7);
    let windows_1 = stream_graph(&normalized, &refs, Some("main"), 1);

    assert_eq!(
        serde_json::to_vec(&one_shot.rows).unwrap(),
        serde_json::to_vec(&windows_7.rows).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&one_shot.edges).unwrap(),
        serde_json::to_vec(&windows_7.edges).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&one_shot.rows).unwrap(),
        serde_json::to_vec(&windows_1.rows).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&one_shot.edges).unwrap(),
        serde_json::to_vec(&windows_1.edges).unwrap()
    );
    assert_eq!(one_shot.stubs, windows_7.stubs);
    assert_eq!(one_shot.stubs, windows_1.stubs);
    assert!(one_shot.stubs.iter().any(|stub| stub.name == "aaa"));

    // ...and the point of the whole extraction: the stream reproduces the
    // legacy one-shot wrapper byte for byte, geometry, colours and stubs alike.
    let legacy = layout_with_refs(normalized.clone(), refs.clone(), Some("main"));
    assert_eq!(
        serde_json::to_vec(&legacy.rows).unwrap(),
        serde_json::to_vec(&one_shot.rows).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&legacy.edges).unwrap(),
        serde_json::to_vec(&one_shot.edges).unwrap()
    );
    assert_eq!(legacy.stubs, one_shot.stubs);
    assert_eq!(legacy.lane_count, one_shot.lane_count);
    assert_well_formed(&one_shot);
}

#[test]
fn checkpoint_keeps_absolute_rows_and_open_lanes() {
    let c = commit("c", &["b"]);
    let b = commit("b", &["a"]);
    let a = commit("a", &[]);
    let present = HashSet::from([c.id.clone(), b.id.clone(), a.id.clone()]);

    let mut stream = StreamLayout::new(None);
    stream.push(c, |oid| present.contains(oid));
    let (first, checkpoint) = stream.checkpoint();
    let mut stream = StreamLayout::resume(checkpoint);
    stream.push(b, |oid| present.contains(oid));
    let (second, checkpoint) = stream.checkpoint();
    let mut stream = StreamLayout::resume(checkpoint);
    stream.push(a, |oid| present.contains(oid));
    let third = stream.finish();

    let chunks = [first, second, third];
    assert_eq!(row_numbers(&chunks), vec![0, 1, 2]);
    for chunk in &chunks {
        assert_eq!(chunk.rows.len(), 1);
        assert_eq!(
            chunk.rows[0].lane, 0,
            "a linear history never leaves lane 0"
        );
        assert_eq!(
            chunk.lane_count, 1,
            "the high-water is monotonic and stays at one lane"
        );
    }
    // (That the *reserved* lane, not just the leftmost free one, survives a
    // checkpoint is witnessed by `trunk_reservation_survives_checkpoint_before_tip`,
    // where the two differ.)

    // The high-water counts the highest *occupied* row lane, not the width of
    // the open-lane vector: M reserves lane 1 for P, but C's first-parent
    // continuation reserves P in lane 0 too, so P collapses left and no row ever
    // sits in lane 1. Carrying `open_lanes.len()` instead would widen the graph
    // by a phantom column — and break `lane < lane_count` for every consumer.
    let commits = stable_topo_order(vec![
        commit("M", &["A", "P"]),
        commit("A", &["C"]),
        commit("C", &["P"]),
        commit("P", &[]),
    ]);
    assert_eq!(
        commits.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(),
        vec!["M", "A", "C", "P"]
    );
    let present: HashSet<Oid> = commits.iter().map(|c| c.id.clone()).collect();
    let collapsed = stream_chunks(&commits, None, &present, 2);
    assert_eq!(collapsed.len(), 2);
    for chunk in &collapsed {
        assert!(chunk.rows.iter().all(|r| r.lane == 0));
        assert_eq!(
            chunk.lane_count, 1,
            "a reserved-then-freed lane is not an occupied one"
        );
    }
    assert_eq!(
        layout(commits).lane_count,
        1,
        "and that is what legacy says"
    );
}

#[test]
fn cross_page_edges_belong_to_destination_page_and_emit_once() {
    // M -> [A, B], A -> R, B -> R, R. Checkpoint after every single commit, so
    // every edge in this DAG has to cross at least one page boundary.
    let commits = stable_topo_order(vec![
        commit("M", &["A", "B"]),
        commit("A", &["R"]),
        commit("B", &["R"]),
        commit("R", &[]),
    ]);
    assert_eq!(
        commits.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(),
        vec!["M", "A", "B", "R"]
    );
    let present: HashSet<Oid> = commits.iter().map(|c| c.id.clone()).collect();
    let chunks = stream_chunks(&commits, None, &present, 1);
    assert_eq!(chunks.len(), 4, "one page per commit");

    let all_rows: Vec<GraphRow> = chunks.iter().flat_map(|c| c.rows.clone()).collect();
    assert_eq!(row_numbers(&chunks), vec![0, 1, 2, 3]);

    // Exactly the four in-window links, each emitted once, across all pages.
    let mut seen = edge_names(&chunks, &all_rows);
    let emitted = seen.len();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), emitted, "no edge is emitted twice");
    assert_eq!(
        seen,
        vec![
            ("A".to_string(), "R".to_string()),
            ("B".to_string(), "R".to_string()),
            ("M".to_string(), "A".to_string()),
            ("M".to_string(), "B".to_string()),
        ]
    );

    // Every page owns all and only the edges whose destination row lands in it;
    // `from_row` is free to sit below the page floor.
    let mut floor = 0;
    for chunk in &chunks {
        let ceiling = floor + chunk.rows.len();
        for resolved in &chunk.resolved_edges {
            assert!(
                floor <= resolved.edge.to_row && resolved.edge.to_row < ceiling,
                "edge {:?} does not belong to page [{floor}, {ceiling})",
                resolved.edge
            );
        }
        floor = ceiling;
    }

    // The first page holds M alone: both of its edges point at rows it has not
    // reached, so nothing unresolved is serialized.
    assert!(
        chunks[0].resolved_edges.is_empty(),
        "a future-parent edge never leaves the checkpoint"
    );
    assert_eq!(
        edge_names(&chunks[1..2], &all_rows),
        vec![("M".to_string(), "A".to_string())]
    );
    assert_eq!(
        edge_names(&chunks[2..3], &all_rows),
        vec![("M".to_string(), "B".to_string())]
    );
    assert_eq!(
        edge_names(&chunks[3..4], &all_rows),
        vec![
            ("A".to_string(), "R".to_string()),
            ("B".to_string(), "R".to_string())
        ]
    );
}

#[test]
fn canonical_edge_order_is_row_then_parent_vector_order() {
    // `m0`'s parent vector is [zzz, aab], but `aab` is the one that arrives
    // first (row 1) and `zzz` only lands at row 2 — so arrival order and
    // parent-vector order disagree for the merge.
    let commits = stable_topo_order(vec![
        commit("m0", &["zzz", "aab"]),
        commit("aab", &["r"]),
        commit("zzz", &["r"]),
        commit("r", &[]),
    ]);
    assert_eq!(
        commits.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(),
        vec!["m0", "aab", "zzz", "r"]
    );
    let present: HashSet<Oid> = commits.iter().map(|c| c.id.clone()).collect();
    let graph = aggregate(stream_chunks(&commits, None, &present, commits.len()));

    let canonical = vec![
        ("m0".to_string(), "zzz".to_string()),
        ("m0".to_string(), "aab".to_string()),
        ("aab".to_string(), "r".to_string()),
        ("zzz".to_string(), "r".to_string()),
    ];
    assert_eq!(wire_names(&graph.rows, &graph.edges), canonical);
    // Row-major first, then the child's own parent vector — *not* destination
    // row: `m0 -> zzz` (row 2) deliberately precedes `m0 -> aab` (row 1).
    assert!(graph.edges[0].to_row > graph.edges[1].to_row);

    // The legacy one-shot layout is the oracle for what canonical means.
    let oracle = layout(commits.clone());
    assert_eq!(graph.edges, oracle.edges);

    // And the helper recovers that order from any permutation of the aggregate.
    let mut scrambled = graph.edges.clone();
    scrambled.reverse();
    canonicalize_edges(&graph.rows, &mut scrambled);
    assert_eq!(scrambled, graph.edges);
}

#[test]
fn cross_page_edge_order_uses_resolved_sidecar_not_page_rows() {
    // Exact topo order: M(0) -> [A(1), B(3)], A(1) -> R(2). Cutting after R
    // strands M's *second* parent link on a page that starts at row 3.
    let commits = stable_topo_order(vec![
        at("M", &["A", "B"], 10),
        at("A", &["R"], 5),
        at("R", &[], 3),
        at("B", &[], 1),
    ]);
    assert_eq!(
        commits.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(),
        vec!["M", "A", "R", "B"]
    );
    let present: HashSet<Oid> = commits.iter().map(|c| c.id.clone()).collect();
    let chunks = stream_chunks(&commits, None, &present, 3);
    assert_eq!(chunks.len(), 2, "cut after R");
    let all_rows: Vec<GraphRow> = chunks.iter().flat_map(|c| c.rows.clone()).collect();

    // Page 1 delivers the two edges whose destinations it holds.
    assert_eq!(
        edge_names(&chunks[0..1], &all_rows),
        vec![
            ("M".to_string(), "A".to_string()),
            ("A".to_string(), "R".to_string()),
        ]
    );
    // Page 2 owns M -> B, and orders itself purely through the sidecar: its
    // `from_row` is an absolute row three above anything in its own slice.
    let page2 = &chunks[1];
    assert_eq!(page2.rows.len(), 1);
    assert_eq!(page2.rows[0].row, 3);
    assert_eq!(page2.resolved_edges.len(), 1);
    let only = &page2.resolved_edges[0];
    assert_eq!(
        only.parent_ordinal, 1,
        "B is M's second parent; the page cannot rediscover that from its rows"
    );
    assert_eq!(only.edge.from_row, 0);
    assert!(
        only.edge.from_row < page2.rows[0].row,
        "an absolute row, never an index into this page's slice"
    );
    let mut sorted = page2.resolved_edges.clone();
    sort_resolved_edges(&mut sorted);
    assert_eq!(sorted, page2.resolved_edges, "sorting a page needs no rows");

    // Raw page-by-page concatenation is delivery order, not canonical order.
    let raw: Vec<Edge> = chunks
        .iter()
        .flat_map(|c| c.resolved_edges.iter().map(|r| r.edge.clone()))
        .collect();
    assert_eq!(
        wire_names(&all_rows, &raw),
        vec![
            ("M".to_string(), "A".to_string()),
            ("A".to_string(), "R".to_string()),
            ("M".to_string(), "B".to_string()),
        ]
    );

    // Canonicalizing the completed union recovers it, and matches the
    // uninterrupted one-shot oracle exactly.
    let mut union = raw.clone();
    canonicalize_edges(&all_rows, &mut union);
    assert_eq!(
        wire_names(&all_rows, &union),
        vec![
            ("M".to_string(), "A".to_string()),
            ("M".to_string(), "B".to_string()),
            ("A".to_string(), "R".to_string()),
        ]
    );
    let oracle = layout(commits.clone());
    assert_eq!(union, oracle.edges);
}

#[test]
fn dangling_parent_does_not_hold_lane_or_emit_edge() {
    // Both cut sites at once: the first parent (which would otherwise continue
    // this commit's own lane) and an extra merge parent (which would otherwise
    // reserve a fresh lane to the right).
    let mut stream = StreamLayout::new(None);
    stream.push(commit("a", &["y", "z"]), |_| false);
    let (first, checkpoint) = stream.checkpoint();
    assert_eq!(first.rows.len(), 1);
    assert_eq!(first.rows[0].lane, 0);
    assert_eq!(first.lane_count, 1, "just \"a\" itself");
    assert!(
        first.resolved_edges.is_empty(),
        "a cut parent wires no edge"
    );

    // Nothing is held for the cut parents, so the next unrelated tip — even
    // after a checkpoint — reuses lane 0 rather than widening the graph.
    let mut stream = StreamLayout::resume(checkpoint);
    stream.push(commit("b", &[]), |_| false);
    let second = stream.finish();
    assert_eq!(second.rows[0].lane, 0, "a cut parent holds no lane");
    assert_eq!(second.lane_count, 1);
    assert!(second.resolved_edges.is_empty());

    // Sharper: a cut *merge* parent must not quietly occupy a column to the
    // right either. If it did, the next real merge parent would be shoved one
    // lane further out than the topology calls for — invisible in `lane_count`
    // (no row ever sits there) but plainly wrong on screen.
    let present: HashSet<Oid> = ["m", "n", "c", "d"]
        .iter()
        .map(|s| Oid((*s).into()))
        .collect();
    let mut stream = StreamLayout::new(None);
    for c in [
        commit("m", &["n", "cut"]),
        commit("n", &["c", "d"]),
        commit("c", &[]),
        commit("d", &[]),
    ] {
        stream.push(c, |oid| present.contains(oid));
    }
    let chunk = stream.finish();
    assert_eq!(chunk.rows[3].commit.id.0, "d");
    assert_eq!(
        chunk.rows[3].lane, 1,
        "the cut parent reserved no column for d to dodge"
    );
    assert_eq!(chunk.lane_count, 2);
    assert_eq!(
        chunk.resolved_edges.len(),
        3,
        "m->n, n->c, n->d — and no more"
    );

    // Nor does a cut parent leave a *pending* edge behind: even when that exact
    // id turns up later on some other line, the link the caller cut stays cut.
    let mut stream = StreamLayout::new(None);
    stream.push(commit("a", &["z"]), |_| false);
    stream.push(commit("z", &[]), |_| false);
    let chunk = stream.finish();
    assert_eq!(chunk.rows.len(), 2);
    assert!(
        chunk.resolved_edges.is_empty(),
        "a cut link is never wired, even once its parent id shows up"
    );
}

#[test]
fn terminal_finish_discards_unresolved_edge() {
    // The predicate accepts "ghost", so the walk reserves a lane for it and
    // holds a pending edge — but the history ends before it ever arrives.
    let mut stream = StreamLayout::new(None);
    stream.push(commit("c", &["ghost"]), |_| true);
    let chunk = stream.finish();
    assert_eq!(chunk.rows.len(), 1);
    assert_eq!(chunk.rows[0].lane, 0);
    assert_eq!(chunk.lane_count, 1, "a reservation is not an occupied lane");
    assert!(
        chunk.resolved_edges.is_empty(),
        "an edge with no destination row never reaches the wire"
    );

    // ...and that really was a live pending edge: had the walk gone on, the
    // very same link would have resolved on the page holding "ghost".
    let mut stream = StreamLayout::new(None);
    stream.push(commit("c", &["ghost"]), |_| true);
    let (_first, checkpoint) = stream.checkpoint();
    let mut stream = StreamLayout::resume(checkpoint);
    stream.push(commit("ghost", &[]), |_| true);
    let second = stream.finish();
    assert_eq!(second.resolved_edges.len(), 1);
    assert_eq!(
        second.resolved_edges[0].edge,
        Edge {
            from_row: 0,
            from_lane: 0,
            to_row: 1,
            to_lane: 0
        }
    );
}

#[test]
fn trunk_reservation_survives_checkpoint_before_tip() {
    // Lane 0 is held for the trunk tip T, but the newer side commit X is walked
    // first — and a checkpoint falls between them.
    let t = commit("T", &[]);
    let x = commit("X", &["T"]);
    let present = HashSet::from([t.id.clone(), x.id.clone()]);

    let mut stream = StreamLayout::new(Some(t.id.clone()));
    stream.push(x, |oid| present.contains(oid));
    let (first, checkpoint) = stream.checkpoint();
    let mut stream = StreamLayout::resume(checkpoint);
    stream.push(t, |oid| present.contains(oid));
    let second = stream.finish();

    assert_eq!(
        first.rows[0].lane, 1,
        "the side tip forks right of the trunk"
    );
    assert_eq!(
        second.rows[0].lane, 0,
        "the trunk's own column was still held"
    );
    assert_eq!(second.rows[0].row, 1, "absolute rows run through the cut");
    assert_eq!(second.lane_count, 2);
    assert_eq!(
        second.resolved_edges[0].edge,
        Edge {
            from_row: 0,
            from_lane: 1,
            to_row: 1,
            to_lane: 0
        }
    );
}
