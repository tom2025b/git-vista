//! The before/after picture inside a confirm dialog (M10.08 A6, #594).
//!
//! The last surface of the preview feature, and the thinnest: every word comes
//! from `features::preview::core` and every coordinate from
//! `features::preview::scene`, both pure and host-tested. This file arranges
//! what they return into elements, and decides nothing.
//!
//! Keeping the split that strict is what makes A6 checkable at all — `cargo
//! test` never compiles this file, exactly as it never compiles `render/`,
//! which is the reason a second static renderer exists rather than a reuse of
//! the first.
//!
//! # A computed preview informs; a failed request makes no promise
//!
//! Nothing in here touches the confirm button. Every *computed* preview arm
//! that has no picture prints [`reassurance`]'s sentence saying the operation
//! is still available, wired to `PreviewView::advisory_only` so it stops being
//! printed if the rule it promises ever stops being true. A transport failure
//! is different: a 405 from `/api/plan` establishes that the operation is not
//! available on this listener, so pending/failed request copy comes from the
//! host-tested listener policy and makes no availability claim.

use leptos::*;

use crate::features::freshness::core::{verdict_framing, verdict_headline, PlanVerdict};
use crate::features::freshness::signals::Freshness;
use crate::features::preview::core::{reassurance, PreviewView};
use crate::features::preview::scene::{
    scene_of, tag_width, HalfScene, LegendEntry, PreviewScene, SceneNode, MARK_ADDED, MARK_REF,
    TAG_H, TAG_R,
};
use crate::features::preview::signals::{prefers_reduced_motion, Playback, Preview, PreviewSlot};
use crate::features::preview::tween::{
    sample, tween_of, FrameBadge, FrameEdge, FrameNode, TweenScene,
};
use crate::listener_policy::{preview_failure_message, preview_pending_message};
use git_vista_core::color::BADGE_DARK;

/// Panel chrome, matching `explanation_panel_view`'s box in `confirm.rs` so
/// the two read as siblings under one confirmation.
const PANEL: &str = "margin-bottom:14px; border:1px solid #30363d; \
                     border-radius:8px; overflow:hidden;";
const PANEL_HEAD: &str = "padding:8px 12px; background:#0d1117; color:var(--muted); \
                          font-size:12px; letter-spacing:0.04em; text-transform:uppercase;";
const PANEL_BODY: &str = "padding:10px 12px; line-height:1.4;";
/// Muted body text, for a reason or a caption.
const MUTED: &str = "color:var(--muted); line-height:1.4;";

/// The freshness notice's own chrome — amber rather than the panel grey, and
/// above the picture rather than below it. A staleness warning printed *under*
/// the thing it invalidates is one a reader meets after they have already
/// believed the picture.
const STALE: &str = "margin-bottom:14px; padding:10px 12px; border-radius:8px;                      border:1px solid #9e6a03; background:#2b2007;                      color:#f2cc60; line-height:1.4;";

/// Is the plan on screen still current, and what does the panel say about it?
///
/// Renders nothing while the plan is current, and nothing at all when there is
/// no plan on screen — most confirmations in this modal have no preview, and
/// this feature makes no claim about them (M12.05, #555).
///
/// Every word comes from `features::freshness::core`, host-tested, for the same
/// reason every word of the panel below comes from `features::preview::core`.
pub fn freshness_notice_view(preview: Preview, freshness: Freshness) -> impl IntoView {
    move || {
        let verdict = freshness.of(&preview.plan());
        if matches!(verdict, PlanVerdict::NoPlan) {
            return None;
        }
        let headline = verdict_headline(&verdict)?;
        let framing = verdict_framing(&verdict);
        Some(view! {
            <div class="plan-stale" style=STALE role="status">
                <div style="font-weight:600;">{headline}</div>
                {framing.map(|text| view! {
                    <div style="margin-top:6px; opacity:0.9;">{text}</div>
                })}
            </div>
        })
    }
}

/// The graph-preview panel, under the confirmation it belongs to.
///
/// Renders nothing at all while [`PreviewSlot::Idle`] — most confirmations in
/// this modal have no preview, and an empty box under them would be noise.
pub fn preview_panel_view(preview: Preview) -> impl IntoView {
    move || match preview.slot() {
        PreviewSlot::Idle => None,
        PreviewSlot::Pending => Some(
            view! {
                <div style=PANEL>
                    <div style=PANEL_HEAD>"What this would do"</div>
                    <div style=format!("{PANEL_BODY}{MUTED}")>
                        {preview_pending_message()}
                    </div>
                </div>
            }
            .into_view(),
        ),
        // A failed round trip may be a capability refusal from `/api/plan`, not
        // merely a connection failure. Report the answer; never promise through it.
        PreviewSlot::Failed(why) => Some(
            view! {
                <div style=PANEL>
                    <div style=PANEL_HEAD>"No preview"</div>
                    <div style=PANEL_BODY>
                        <div style=MUTED>{preview_failure_message(&why)}</div>
                    </div>
                </div>
            }
            .into_view(),
        ),
        PreviewSlot::Ready(view) => Some(ready_view(view)),
    }
}

/// One of the four answers.
fn ready_view(view: PreviewView) -> View {
    let note = reassurance(&view).map(|text| {
        view! { <div style=format!("{MUTED} margin-top:8px;")>{text}</div> }
    });
    let (head, body) = match view {
        PreviewView::Picture(ref picture) => (
            "What this would do",
            picture_body(scene_of(picture), tween_of(picture)).into_view(),
        ),
        // A conflict is a live established fact — real git ran the real
        // three-way merge and it does not apply. Named paths, not a spinner
        // and not a generic error.
        PreviewView::Conflict { ref paths } => (
            "This would conflict",
            view! {
                <div>
                    <div>
                        {format!(
                            "Git tried the merge and {} would need your decision:",
                            if paths.len() == 1 { "one file".to_string() }
                            else { format!("{} files", paths.len()) },
                        )}
                    </div>
                    <ul style="margin:8px 0 0 0; padding-left:20px; \
                               max-height:24vh; overflow-y:auto;">
                        {paths
                            .iter()
                            .map(|p| view! { <li style="font-family:monospace;">{p.clone()}</li> })
                            .collect_view()}
                    </ul>
                </div>
            }
            .into_view(),
        ),
        // A permanent fact about the operation, for every host. Not a fault,
        // and not something to report anywhere.
        PreviewView::Unsupported { ref operation } => (
            "No picture for this one",
            view! {
                <div>
                    {format!(
                        "git-vista cannot draw a preview of {operation}. Nothing is \
                         wrong — this operation has no preview on any host."
                    )}
                </div>
            }
            .into_view(),
        ),
        // Previewable in principle; not here, or not now. The reason is always
        // named, and the remedy appears only where there is one.
        PreviewView::Unavailable {
            ref headline,
            ref detail,
            ref remedy,
        } => (
            "No preview here",
            view! {
                <div>
                    <div style="font-weight:600;">{headline.clone()}</div>
                    {detail.clone().map(|d| view! {
                        <div style=format!("{MUTED} margin-top:6px;")>{d}</div>
                    })}
                    {remedy.clone().map(|r| view! {
                        <div style="margin-top:6px;">{r}</div>
                    })}
                </div>
            }
            .into_view(),
        ),
    };
    view! {
        <div style=PANEL>
            <div style=PANEL_HEAD>{head}</div>
            <div style=PANEL_BODY>{body}{note}</div>
        </div>
    }
    .into_view()
}

/// The animated transition, the two static halves, the sentence, and the
/// legend.
///
/// The animation is additive (#591): a fresh [`Playback`] starts the moment
/// this body is built (once per preview becoming [`PreviewSlot::Ready`]), but
/// the two static halves below it are ordinary markup with no dependency on
/// it whatsoever — a host with `prefers-reduced-motion: reduce`, a screen
/// reader, or simply a slow frame never has less to look at than this panel
/// already offered before #591 existed.
fn picture_body(scene: PreviewScene, tween: TweenScene) -> impl IntoView {
    let PreviewScene {
        before,
        after,
        summary,
        legend,
    } = scene;
    let playback = Playback::new();
    playback.start(prefers_reduced_motion());
    view! {
        <div>
            <div style="margin-bottom:8px;">{summary}</div>
            {animated_scene_view(tween, playback)}
            <div style="display:flex; gap:14px; flex-wrap:wrap; align-items:flex-start; \
                        max-height:44vh; overflow:auto;">
                {half_view(before)}
                {half_view(after)}
            </div>
            {(!legend.is_empty()).then(|| view! {
                <div style="display:flex; gap:12px; flex-wrap:wrap; margin-top:8px;">
                    {legend.into_iter().map(legend_view).collect_view()}
                </div>
            })}
        </div>
    }
}

/// The single animated scene: a "Watch it happen" caption, a Replay button,
/// and the SVG that plays the transition once and settles.
///
/// The scene's geometry ([`TweenScene`]) is built exactly once, when the
/// picture becomes ready — it does not change while the animation plays, so
/// it is stored rather than recomputed on every tick. Only [`sample`]'s
/// output (a [`Frame`]) is recomputed per frame, from [`playback`]'s
/// progress signal.
fn animated_scene_view(tween: TweenScene, playback: Playback) -> impl IntoView {
    let width = tween.width;
    let height = tween.height;
    let scene = store_value(tween);
    let frame = create_memo(move |_| scene.with_value(|s| sample(s, playback.progress())));
    view! {
        <div style="margin-bottom:10px;">
            <div style="display:flex; align-items:center; justify-content:space-between; \
                        margin-bottom:4px;">
                <div style="color:var(--muted); font-size:11px; letter-spacing:0.04em; \
                            text-transform:uppercase;">
                    "Watch it happen"
                </div>
                <button
                    type="button"
                    on:click=move |_| playback.start(prefers_reduced_motion())
                    style="font-size:11px; padding:2px 8px; border-radius:4px; \
                           border:1px solid #30363d; background:#161b22; \
                           color:var(--muted); cursor:pointer;"
                >
                    "Replay"
                </button>
            </div>
            <svg
                width=width
                height=height
                viewBox=format!("0 0 {width} {height}")
                role="img"
                aria-label="An animation from the repository as it is now to the \
                             state this operation would produce"
                style="display:block; max-width:100%; height:auto; \
                       background:#0d1117; border-radius:6px;"
            >
                {move || frame.get().edges.into_iter().map(tween_edge_view).collect_view()}
                {move || frame.get().nodes.into_iter().map(tween_node_view).collect_view()}
                {move || frame.get().badges.into_iter().map(tween_badge_view).collect_view()}
            </svg>
        </div>
    }
}

/// One edge at the current instant.
fn tween_edge_view(edge: FrameEdge) -> impl IntoView {
    view! {
        <path
            d=edge.d
            fill="none"
            stroke=edge.color
            stroke-width="2"
            stroke-linecap="round"
            opacity=edge.opacity.to_string()
        />
    }
}

/// One commit dot at the current instant. Same shape as [`node_view`], with
/// `cx`/`cy`/tag-`y`/label-`y` carrying fractional pixels mid-flight instead
/// of the static picture's integers.
fn tween_node_view(node: FrameNode) -> impl IntoView {
    let FrameNode {
        cx,
        cy,
        opacity,
        color,
        hollow,
        r,
        halo,
        tags,
        label,
        label_x,
        label_y,
        alt,
        ..
    } = node;
    view! {
        <g opacity=opacity.to_string()>
            <title>{alt}</title>
            {halo.map(|hr| view! {
                <circle cx=cx cy=cy r=hr fill="none" stroke=MARK_ADDED stroke-width="1.5"
                        stroke-dasharray="3 2" />
            })}
            <circle
                cx=cx
                cy=cy
                r=r
                fill=if hollow { "#0d1117" } else { color }
                stroke=color
                stroke-width="2"
            />
            {tags
                .into_iter()
                .map(|t| view! {
                    <rect x=t.x y=t.y width=t.w height=t.h rx="3" ry="3"
                          fill=t.fill stroke=t.stroke stroke-width="1" />
                    <text x=t.x + 4 y=t.y + 10.0 font-family="monospace" font-size="10"
                          fill=t.fg>
                        {t.text}
                    </text>
                })
                .collect_view()}
            <text x=label_x y=label_y font-family="monospace" font-size="11"
                  fill="#c9d1d9">
                {label}
            </text>
        </g>
    }
}

/// One ref badge in flight, drawn as a small pill centred on its current
/// point rather than anchored to a row's label column — it is not attached
/// to either commit's tag stack, it is travelling between them.
fn tween_badge_view(badge: FrameBadge) -> impl IntoView {
    let FrameBadge {
        text,
        cx,
        cy,
        opacity,
    } = badge;
    let w = tag_width(&text);
    let x = cx - (w as f64) / 2.0;
    let y = cy - (TAG_H as f64) / 2.0;
    view! {
        <g opacity=opacity.to_string()>
            <rect x=x y=y width=w height=TAG_H rx=TAG_R ry=TAG_R
                  fill=MARK_REF stroke=MARK_REF stroke-width="1" />
            <text x=x + 4.0 y=y + 10.0 font-family="monospace" font-size="10" fill=BADGE_DARK>
                {text}
            </text>
        </g>
    }
}

/// One legend row: a swatch and what it means.
fn legend_view(entry: LegendEntry) -> impl IntoView {
    view! {
        <span style=format!("{MUTED} display:inline-flex; align-items:center; gap:5px; \
                             font-size:11px;")>
            <span style=format!(
                "width:9px; height:9px; border-radius:50%; border:2px solid {}; \
                 display:inline-block;",
                entry.color,
            )></span>
            {entry.text}
        </span>
    }
}

/// One half, as a static SVG with its captions.
///
/// `role="img"` plus a label, because the picture is the content here and a
/// bare `<svg>` is announced as nothing. Every mark also appears as real text
/// inside the drawing (`new`, `→main`, `lane 0→1`), so the marks are readable
/// without seeing colour at all.
fn half_view(half: HalfScene) -> impl IntoView {
    let HalfScene {
        title,
        width,
        height,
        edges,
        nodes,
        stubs,
        elided_above,
        elided_below,
        lanes_clamped,
        alt,
    } = half;
    let caption = |text: Option<String>| {
        text.map(|t| view! { <div style=format!("{MUTED} font-size:11px;")>{t}</div> })
    };
    view! {
        <div style="flex:1 1 260px; min-width:0;">
            <div style="color:var(--muted); font-size:11px; letter-spacing:0.04em; \
                        text-transform:uppercase; margin-bottom:4px;">
                {title}
            </div>
            {caption(elided_above)}
            <svg
                width=width
                height=height
                viewBox=format!("0 0 {width} {height}")
                role="img"
                aria-label=alt
                style="display:block; max-width:100%; height:auto; \
                       background:#0d1117; border-radius:6px;"
            >
                {edges
                    .into_iter()
                    .map(|e| view! {
                        <path
                            d=e.d
                            fill="none"
                            stroke=e.color
                            stroke-width="2"
                            stroke-linecap="round"
                            opacity=if e.clipped { "0.45" } else { "1" }
                        />
                    })
                    .collect_view()}
                {stubs
                    .into_iter()
                    .map(|s| view! {
                        <path d=s.d fill="none" stroke=s.color stroke-width="2"
                              stroke-linecap="round" opacity="0.8" />
                        <circle cx=s.cx cy=s.cy r=s.r fill="#0d1117" stroke=s.color
                                stroke-width="2" />
                        <title>{s.name}</title>
                    })
                    .collect_view()}
                {nodes.into_iter().map(node_view).collect_view()}
            </svg>
            {caption(elided_below)}
            {lanes_clamped.then(|| view! {
                <div style=format!("{MUTED} font-size:11px;")>
                    "columns beyond the eighth are drawn together"
                </div>
            })}
        </div>
    }
}

/// One commit: the halo, the dot, the pills, the summary.
fn node_view(node: SceneNode) -> impl IntoView {
    let SceneNode {
        cx,
        cy,
        r,
        color,
        hollow,
        halo,
        tags,
        label_x,
        label_y,
        label,
        marked,
        alt,
        // Only `tween` reads a node's identity across the two halves.
        commit_id: _,
    } = node;
    // Unmarked rows are dimmed so the eye lands on what changed. Dimmed, not
    // hidden: they are the context that makes the change legible.
    let strength = if marked { "1" } else { "0.62" };
    view! {
        <g opacity=strength>
            <title>{alt}</title>
            {halo.map(|hr| view! {
                <circle cx=cx cy=cy r=hr fill="none" stroke=MARK_ADDED stroke-width="1.5"
                        stroke-dasharray="3 2" />
            })}
            <circle
                cx=cx
                cy=cy
                r=r
                fill=if hollow { "#0d1117" } else { color }
                stroke=color
                stroke-width="2"
            />
            {tags
                .into_iter()
                .map(|t| view! {
                    <rect x=t.x y=t.y width=t.w height=t.h rx="3" ry="3"
                          fill=t.fill stroke=t.stroke stroke-width="1" />
                    <text x=t.x + 4 y=t.y + 10 font-family="monospace" font-size="10"
                          fill=t.fg>
                        {t.text}
                    </text>
                })
                .collect_view()}
            <text x=label_x y=label_y font-family="monospace" font-size="11"
                  fill="#c9d1d9">
                {label}
            </text>
        </g>
    }
}
