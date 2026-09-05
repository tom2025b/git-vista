//! Animating the before→after picture as one scene, rather than two (#591).
//!
//! Framework-free and host-tested, exactly like [`super::scene`] beside it —
//! this module owns the whole of what changes *while the animation plays*:
//! which pixel a commit dot sits at when the clock reads `t`, when a ref
//! badge starts sliding, when an outcome label is allowed to appear. Nothing
//! here knows about `requestAnimationFrame`, a signal, or the DOM; the wasm
//! side only reads [`Frame`] and draws it.
//!
//! # The honesty rule, and how this module keeps it
//!
//! #591's issue text is explicit: *"the animation must only draw states that
//! are the two real endpoints. No invented intermediate git states."*
//! [`tween_of`] is the seam that could violate this, so it is built to make
//! violating it structurally awkward rather than merely undesirable:
//!
//! * A commit dot's [`NodeLifecycle`] is derived from **membership**, not
//!   imagination — [`Half`] and its `commit_id`s speak entirely in facts
//!   [`super::scene::half_scene`] already draws for the static picture.
//!   [`tween_of`] never invents a row, a lane, or a commit that is not in one
//!   of the two real [`Picture`] halves.
//! * A [`NodeLifecycle::Persistent`] commit slides between two **real**
//!   positions — where the static picture draws it *before*, and where the
//!   static picture draws it *after* — reusing the exact same
//!   `scene::lane_cx`/`scene::row_cy` arithmetic via [`layout_params`], so
//!   "the animation's endpoint" and "the static picture's dot" are the same
//!   pixel by construction, not by a second calculation that could drift
//!   from it.
//! * A commit only [`NodeLifecycle::Entering`] (the hypothetical commit the
//!   operation would create) is never given a starting position — there is
//!   no real "before" position for a commit that does not yet exist, so it
//!   fades in at rest rather than sliding from an invented origin.
//! * Edges are the after graph's real parent/child structure — never a
//!   fabricated intermediate topology — with only each endpoint's *screen
//!   position* interpolated. A tween is a rendering property of a real edge,
//!   never a claim about which edges exist.
//! * A ref only moves between the two commits [`RefMove`] names — `from` and
//!   `to`, both taken verbatim from the server's own change list rather than
//!   re-derived by scanning for a name. See [`RefMove`]'s own doc for why
//!   that matters.
//!
//! # Outcome labels reveal late, on purpose
//!
//! A `→main` pill, a `new` pill, a `lane 0→2` pill: each is a sentence about
//! the *after* state ("this operation would land a ref here"). Showing one
//! while its commit dot is still visibly mid-flight from its `before`
//! position reads as a fact that is already true before the picture agrees
//! it is. [`REVEAL_AFTER`] is the constant that fixes this — every
//! [`SceneTag`] built for a *mark* ([`SceneTag::is_mark`]) stays hidden until
//! the transition is almost entirely settled. A ref that already pointed
//! here and is simply carried through (`is_mark == false`) is not an outcome
//! of this operation and is visible throughout, exactly as the static
//! picture always drew it.
//!
//! # Windowing asymmetry is dropped, not disguised
//!
//! [`super::scene::window_for_before`]'s own doc already accepts that the
//! before window can include commits absent from the after window, and vice
//! versa, as a documented consequence of a fixed row budget. A commit like
//! that is not "removed by the operation" — it still exists, on both sides,
//! and would draw fine at a wider window. Fading such a commit out would
//! read as "this operation deleted this commit", which is false and exactly
//! the failure the honesty rule exists to prevent. So [`tween_of`] only ever
//! marks a commit [`NodeLifecycle::Leaving`] when it is missing from the
//! *entire* after half (`after.rows`), not merely outside the after
//! *window* — a case that cannot happen for any of the three operations the
//! preview engine supports (merge, revert, cherry-pick never destroy a
//! reachable commit), and is kept only so a future, more destructive preview
//! cannot silently degrade into a lie by omission.

use std::collections::HashMap;

use git_vista_core::color::branch_color;

use super::core::{Picture, RefMove};
use super::scene::{half_scene, layout_params, HalfScene, SceneNode, TAG_H};

/// How long the transition plays, start to rest.
///
/// Tom is the primary user and a visual learner; the point of this feature is
/// comprehension, not polish. Under ~600ms and a lane re-flow reads as a jump
/// cut — the eye cannot track which dot went where. Over ~1200ms it stops
/// feeling like a picture and starts feeling like a wait. 900ms is the
/// midpoint of that range: long enough to watch a ref travel across a
/// gutter's width of lanes, short enough that replaying it is not a chore.
pub const DURATION_MS: f64 = 900.0;

/// The point in `[0, 1]` progress after which an outcome-only label (a
/// [`SceneTag`] with [`SceneTag::is_mark`] set) is allowed to render.
///
/// Not `1.0`: a label popping in on the exact final frame reads as a glitch
/// rather than an arrival. 0.92 leaves the last ~8% of the duration (well
/// under 100ms at [`DURATION_MS`]) for the label to appear alongside the dot
/// settling, which is fast enough to read as "arrived" rather than "still
/// moving".
pub const REVEAL_AFTER: f64 = 0.92;

/// Cubic ease-in-out, for position. Slow start, fast middle, slow finish —
/// the standard shape for "something is moving to a specific place", as
/// opposed to linear motion, which reads as mechanical rather than settling.
pub fn ease_in_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Linear interpolation.
fn lerp(from: f64, to: f64, t: f64) -> f64 {
    from + (to - from) * t
}

/// `elapsed_ms` since the animation started, turned into a `[0, 1]` progress
/// value against [`DURATION_MS`].
///
/// A pure function of elapsed time rather than a running clock, so "what
/// progress is this frame at" is exactly as testable as everything else in
/// this module — the wasm side's only job is reading `performance.now()` and
/// handing the difference here.
pub fn progress_at(elapsed_ms: f64) -> f64 {
    // `NaN` has no honest reading as "how much time has passed" — treat it
    // as "none yet" rather than propagating a `NaN` frame. `+INFINITY`
    // (unlike `NaN`) has an honest reading: the animation is long since
    // over, and `.clamp` already sends it to `1.0` below.
    if elapsed_ms.is_nan() {
        return 0.0;
    }
    (elapsed_ms / DURATION_MS).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// The static model: what does not change once a `Picture` is known.
// ---------------------------------------------------------------------------

/// How one commit dot behaves across the transition. See the module doc's
/// "honesty rule" section for why there are exactly three arms and what each
/// one is and is not allowed to draw.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeLifecycle {
    /// Drawn in both halves — slides from its real `before` position to its
    /// real `after` position (`look.cx`/`look.cy`, see [`TweenNode::look`]).
    Persistent { from: (i32, i32) },
    /// Drawn only in `after` — the hypothetical commit, or a commit newly
    /// reachable at all. Fixed at `look.cx`/`look.cy`; opacity ramps in.
    Entering,
    /// Drawn only in `before`, and genuinely absent from `after.rows`
    /// entirely (not merely outside the after window — see the module doc).
    /// Fixed at `look.cx`/`look.cy` (here, `before`'s own draw position);
    /// opacity ramps out.
    Leaving,
}

/// One animated commit dot.
#[derive(Debug, Clone, PartialEq)]
pub struct TweenNode {
    pub commit_id: String,
    pub lifecycle: NodeLifecycle,
    /// Everything about the dot that does not need a second calculation:
    /// colour, hollow-ness, tags, label. For [`NodeLifecycle::Persistent`]
    /// and [`NodeLifecycle::Entering`] this is the **after** half's node —
    /// the destination look is the one that is ever fully true. For
    /// [`NodeLifecycle::Leaving`] it is the **before** half's node, since no
    /// `after` counterpart exists to borrow a look from.
    pub look: SceneNode,
}

/// One animated edge — the after graph's real topology, endpoints resolved
/// by commit id rather than by row number, so they can be repositioned every
/// frame from wherever their node currently is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TweenEdge {
    pub from_id: String,
    pub to_id: String,
    pub color: &'static str,
    /// Mirrors [`super::scene::SceneEdge::clipped`]: true when either
    /// endpoint's row falls outside the window it is drawn in.
    pub clipped: bool,
}

/// One ref badge in flight between the commit it used to point at and the
/// commit the operation lands it on.
#[derive(Debug, Clone, PartialEq)]
pub struct TweenBadge {
    pub text: String,
    /// `None` when the ref's origin commit ([`RefMove::from`]) is not drawn
    /// in the before window — there is nowhere honest to slide it *from*, so
    /// it fades in at `to` instead, exactly like [`NodeLifecycle::Entering`].
    pub from: Option<(i32, i32)>,
    pub to: (i32, i32),
}

/// The whole animated scene: static geometry only, no notion of time.
/// [`sample`] is what turns this plus a progress value into pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct TweenScene {
    pub width: i32,
    pub height: i32,
    pub nodes: Vec<TweenNode>,
    pub edges: Vec<TweenEdge>,
    pub badges: Vec<TweenBadge>,
}

/// Build the animated scene from a [`Picture`].
///
/// Shares [`layout_params`] and [`half_scene`] with [`super::scene::scene_of`]
/// precisely so the animation's endpoints are pixel-identical to the static
/// picture's — see the module doc.
pub fn tween_of(picture: &Picture) -> TweenScene {
    let params = layout_params(picture);

    let before_scene = half_scene(
        "Before",
        &picture.before,
        params.before_window,
        params.lanes,
        params.lanes_clamped,
        &HashMap::new(),
    );
    let after_scene = half_scene(
        "After",
        &picture.after,
        params.after_window,
        params.lanes,
        params.lanes_clamped,
        &picture.marks,
    );

    let nodes = nodes_of(&before_scene, &after_scene, &picture.after);
    let edges = edges_of(&picture.after, params.after_window, params.lanes);
    let badges = badges_of(&picture.ref_moves, &before_scene, &after_scene);

    TweenScene {
        width: before_scene.width.max(after_scene.width),
        height: before_scene.height.max(after_scene.height),
        nodes,
        edges,
        badges,
    }
}

/// Match every drawn commit between the two halves by id, and classify it.
fn nodes_of(
    before_scene: &HalfScene,
    after_scene: &HalfScene,
    after_half: &super::core::Half,
) -> Vec<TweenNode> {
    let before_by_id: HashMap<&str, &SceneNode> = before_scene
        .nodes
        .iter()
        .map(|n| (n.commit_id.as_str(), n))
        .collect();
    let after_by_id: HashMap<&str, &SceneNode> = after_scene
        .nodes
        .iter()
        .map(|n| (n.commit_id.as_str(), n))
        .collect();
    // "Genuinely absent from after" — the whole half, not just its drawn
    // window. See `NodeLifecycle::Leaving`'s doc for why this distinction is
    // the one that keeps the animation honest.
    let after_exists: std::collections::HashSet<&str> = after_half
        .rows
        .iter()
        .map(|r| r.commit.id.0.as_str())
        .collect();

    let mut ids: Vec<&str> = before_by_id
        .keys()
        .chain(after_by_id.keys())
        .copied()
        .collect();
    ids.sort_unstable();
    ids.dedup();

    ids.into_iter()
        .filter_map(|id| match (before_by_id.get(id), after_by_id.get(id)) {
            (Some(before), Some(after)) => Some(TweenNode {
                commit_id: id.to_string(),
                lifecycle: NodeLifecycle::Persistent {
                    from: (before.cx, before.cy),
                },
                look: (*after).clone(),
            }),
            (None, Some(after)) => Some(TweenNode {
                commit_id: id.to_string(),
                lifecycle: NodeLifecycle::Entering,
                look: (*after).clone(),
            }),
            (Some(before), None) if !after_exists.contains(id) => Some(TweenNode {
                commit_id: id.to_string(),
                lifecycle: NodeLifecycle::Leaving,
                look: (*before).clone(),
            }),
            // Drawn in `before`'s window, absent from `after`'s window, but
            // still real in `after.rows` — a windowing artifact, not a
            // change to the repository. Dropping it here matches what the
            // static after picture already does: it simply is not drawn.
            (Some(_), None) => None,
            (None, None) => unreachable!("id came from one of the two maps"),
        })
        .collect()
}

/// The after graph's real edges, endpoints named by commit id so they can be
/// repositioned from wherever their node currently sits.
fn edges_of(
    after_half: &super::core::Half,
    window: super::scene::RowWindow,
    lanes: usize,
) -> Vec<TweenEdge> {
    let by_row: HashMap<usize, &git_vista_core::model::GraphRow> =
        after_half.rows.iter().map(|r| (r.row, r)).collect();
    let _ = lanes; // kept for signature symmetry with `half_scene`; positions come from nodes, not recomputed here.

    after_half
        .edges
        .iter()
        .filter_map(|e| {
            let inside_from = window.holds(e.from_row);
            let inside_to = window.holds(e.to_row);
            if !inside_from && !inside_to {
                let both_above = e.from_row < window.first && e.to_row < window.first;
                let both_below = e.from_row > window.last && e.to_row > window.last;
                if both_above || both_below {
                    return None;
                }
            }
            let from_row = by_row.get(&e.from_row)?;
            let to_row = by_row.get(&e.to_row)?;
            Some(TweenEdge {
                from_id: from_row.commit.id.0.clone(),
                to_id: to_row.commit.id.0.clone(),
                // Matches `scene::half_scene`: an edge takes the child's line.
                color: branch_color(from_row.color),
                clipped: !inside_from || !inside_to,
            })
        })
        .collect()
}

/// A ref badge for every [`RefMove`], positioned at its real origin (when
/// drawn) and its real destination.
fn badges_of(
    ref_moves: &[RefMove],
    before_scene: &HalfScene,
    after_scene: &HalfScene,
) -> Vec<TweenBadge> {
    let before_by_id: HashMap<&str, &SceneNode> = before_scene
        .nodes
        .iter()
        .map(|n| (n.commit_id.as_str(), n))
        .collect();
    let after_by_id: HashMap<&str, &SceneNode> = after_scene
        .nodes
        .iter()
        .map(|n| (n.commit_id.as_str(), n))
        .collect();

    ref_moves
        .iter()
        .filter_map(|mv| {
            let to = after_by_id.get(mv.to.as_str())?;
            let from = before_by_id.get(mv.from.as_str()).map(|n| (n.cx, n.cy));
            Some(TweenBadge {
                text: mv.ref_name.clone(),
                from,
                to: (to.cx, to.cy),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The frame: what `sample` computes at one instant.
// ---------------------------------------------------------------------------

/// One tag pill at a specific instant, positioned for the node's *current*
/// (interpolated) height. Only the y coordinate is time-dependent — the x
/// stacking a tag occupies is fixed by [`super::scene::row_label`]'s label
/// column, which does not move when a commit changes lane.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameTag {
    pub text: String,
    pub x: i32,
    pub y: f64,
    pub w: i32,
    pub h: i32,
    pub fill: &'static str,
    pub stroke: &'static str,
    pub fg: &'static str,
}

/// One commit dot at a specific instant.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameNode {
    pub commit_id: String,
    pub cx: f64,
    pub cy: f64,
    pub opacity: f64,
    pub color: &'static str,
    pub hollow: bool,
    pub r: i32,
    pub halo: Option<i32>,
    pub tags: Vec<FrameTag>,
    pub label: String,
    pub label_x: i32,
    pub label_y: f64,
    pub alt: String,
}

/// One edge at a specific instant.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameEdge {
    pub d: String,
    pub color: &'static str,
    pub opacity: f64,
}

/// One ref badge at a specific instant.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameBadge {
    pub text: String,
    pub cx: f64,
    pub cy: f64,
    pub opacity: f64,
}

/// A complete drawable instant.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub nodes: Vec<FrameNode>,
    pub edges: Vec<FrameEdge>,
    pub badges: Vec<FrameBadge>,
}

/// A node's `(cx, cy, opacity)` at progress `t`, before the shared
/// [`FrameNode::opacity`] dimming for an unmarked row is applied.
fn node_position(node: &TweenNode, t: f64, eased: f64) -> (f64, f64, f64) {
    match node.lifecycle {
        NodeLifecycle::Persistent { from } => (
            lerp(from.0 as f64, node.look.cx as f64, eased),
            lerp(from.1 as f64, node.look.cy as f64, eased),
            1.0,
        ),
        // Opacity ramps linearly against raw progress, not the eased curve:
        // a fade is not a position, and does not need position's slow-start
        // slow-finish shape to read clearly.
        NodeLifecycle::Entering => (node.look.cx as f64, node.look.cy as f64, t),
        NodeLifecycle::Leaving => (node.look.cx as f64, node.look.cy as f64, 1.0 - t),
    }
}

/// Render one [`TweenScene`] at progress `t` (clamped to `[0, 1]`).
pub fn sample(scene: &TweenScene, t: f64) -> Frame {
    let t = t.clamp(0.0, 1.0);
    let eased = ease_in_out_cubic(t);

    let mut positions: HashMap<&str, (f64, f64, f64)> = HashMap::new();
    let mut frame_nodes = Vec::with_capacity(scene.nodes.len());
    for node in &scene.nodes {
        let (cx, cy, life_opacity) = node_position(node, t, eased);
        positions.insert(node.commit_id.as_str(), (cx, cy, life_opacity));

        // The "marked rows draw at full strength, everything else dims"
        // convention is an *after*-picture concept — `RowMark`s are only
        // ever computed against the after half, so a `Leaving` node's
        // `look` (borrowed from `before`, which is never marked) would
        // always read as unmarked and dim to 0.62 for a reason that has
        // nothing to do with it. Leaving nodes fade by their own lifecycle
        // opacity alone.
        let dim = match node.lifecycle {
            NodeLifecycle::Leaving => 1.0,
            NodeLifecycle::Persistent { .. } | NodeLifecycle::Entering => {
                if node.look.marked {
                    1.0
                } else {
                    0.62
                }
            }
        };
        let opacity = life_opacity * dim;

        let tags = node
            .look
            .tags
            .iter()
            .filter(|tag| !tag.is_mark || t >= REVEAL_AFTER)
            .map(|tag| FrameTag {
                text: tag.text.clone(),
                x: tag.x,
                y: cy - (TAG_H as f64) / 2.0,
                w: tag.w,
                h: tag.h,
                fill: tag.fill,
                stroke: tag.stroke,
                fg: tag.fg,
            })
            .collect();

        frame_nodes.push(FrameNode {
            commit_id: node.commit_id.clone(),
            cx,
            cy,
            opacity,
            color: node.look.color,
            hollow: node.look.hollow,
            r: node.look.r,
            halo: node.look.halo,
            tags,
            label: node.look.label.clone(),
            label_x: node.look.label_x,
            label_y: cy + 4.0,
            alt: node.look.alt.clone(),
        });
    }

    let frame_edges = scene
        .edges
        .iter()
        .filter_map(|edge| {
            let (x1, y1, from_life) = *positions.get(edge.from_id.as_str())?;
            let (x2, y2, to_life) = *positions.get(edge.to_id.as_str())?;
            let d = if (x1 - x2).abs() < 0.01 {
                format!("M {x1:.2} {y1:.2} L {x2:.2} {y2:.2}")
            } else {
                let ym = (y1 + y2) / 2.0;
                format!("M {x1:.2} {y1:.2} C {x1:.2} {ym:.2}, {x2:.2} {ym:.2}, {x2:.2} {y2:.2}")
            };
            let clip = if edge.clipped { 0.45 } else { 1.0 };
            Some(FrameEdge {
                d,
                color: edge.color,
                opacity: from_life.min(to_life) * clip,
            })
        })
        .collect();

    let frame_badges = scene
        .badges
        .iter()
        .map(|badge| match badge.from {
            Some(from) => FrameBadge {
                text: badge.text.clone(),
                cx: lerp(from.0 as f64, badge.to.0 as f64, eased),
                cy: lerp(from.1 as f64, badge.to.1 as f64, eased),
                opacity: 1.0,
            },
            None => FrameBadge {
                text: badge.text.clone(),
                cx: badge.to.0 as f64,
                cy: badge.to.1 as f64,
                opacity: t,
            },
        })
        .collect();

    Frame {
        nodes: frame_nodes,
        edges: frame_edges,
        badges: frame_badges,
    }
}

#[cfg(test)]
#[path = "tween_suite.rs"]
mod tween_suite;
