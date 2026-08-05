//! The operations status strip — wasm only (M1.11, #64).
//!
//! Mounted in the App shell, **above** `graph_canvas`, so it keeps reporting through the
//! epoch bump a completed write triggers. This is the visible half of acceptance criterion
//! 2: before M1.11 a write in flight had no representation at all, and a failed one
//! reported itself through `window.alert()` — a modal outside the component tree that the
//! app could neither style nor dismiss, and which left nothing behind once acknowledged.
//!
//! ADR 0012 holds: this is a fixed-position strip with its own bounded height. It adds no
//! page-level scrolling, and its message column scrolls inside itself if git is verbose.

use leptos::*;

use git_vista_protocol::operation::{OperationStage, OperationState};

use crate::features::operations::core::{fetch_or_pull_summary, progress_line};
use crate::features::operations::signals::Operations;

/// What the pipeline is doing, in words the user did not have to learn.
///
/// The stages are the server's real ones (ADR 0020), which is the point: `Waiting` means
/// another mutation holds this repository's guard, and saying so beats a spinner.
fn stage_text(stage: OperationStage) -> &'static str {
    match stage {
        OperationStage::Queued => "queued",
        OperationStage::Planning => "planning",
        OperationStage::Waiting => "waiting for the repository",
        OperationStage::Checking => "checking",
        OperationStage::Executing => "running",
        OperationStage::Finished => "finishing",
    }
}

/// In-flight writes, and outcomes not yet acknowledged.
pub fn operations_status_view(operations: Operations) -> impl IntoView {
    let core = operations.core();
    move || {
        // The id, cancellability and cancel-requested flag ride along beside
        // the describe/stage text so the Cancel button (#232) can be built
        // per row without a second pass over `in_flight()`.
        let live: Vec<_> = core.with(|c| {
            c.in_flight()
                .map(|e| {
                    (
                        e.id.clone(),
                        e.kind.describe(),
                        stage_text(e.stage),
                        e.kind.is_cancellable(),
                        e.cancel_requested,
                        // `progress_line` is only ever called on the `Some` case (M2.20g,
                        // #232) — a progress-free operation (`InFlight::progress` is `None`
                        // for anything that transfers nothing, per its own doc comment in
                        // `core.rs`) renders no progress fragment at all, never a fabricated
                        // "0%".
                        e.progress.as_ref().map(progress_line),
                    )
                })
                .collect()
        });
        let settled: Vec<_> = core.with(|c| {
            c.recent()
                .map(|s| {
                    // Fetch/Pull settle with the operation record's raw JSON body in
                    // `message` (see `fetch_or_pull_summary`'s own doc comment for why);
                    // every other kind's `message` is already prose, and the function is
                    // a no-op pass-through for them.
                    let message = s
                        .outcome
                        .message
                        .as_deref()
                        .map(|m| fetch_or_pull_summary(&s.kind, m))
                        .unwrap_or_default();
                    (s.id.clone(), s.kind.describe(), s.outcome.state, message)
                })
                .collect()
        });
        (!live.is_empty() || !settled.is_empty()).then(|| {
            view! {
                <div style="position:fixed; left:50%; transform:translateX(-50%); \
                            bottom:12px; z-index:800; display:flex; flex-direction:column; \
                            gap:6px; max-width:min(560px, 92vw);">
                    {live
                        .into_iter()
                        .map(|(id, what, stage, cancellable, cancel_requested, progress)| {
                            // "Cancelling…" is a distinct rendered state, never a
                            // silent removal (#232's own acceptance criterion): the
                            // row stays in the in-flight list, showing that the ask
                            // landed, until the real terminal event arrives through
                            // the existing subscribe()/commit_settlement path — this
                            // view never terminalises anything on its own initiative.
                            let stage_line = if cancel_requested {
                                "cancelling…".to_string()
                            } else {
                                match &progress {
                                    // The detail #232's acceptance criterion asks for:
                                    // "show live progress ... rather than a spinner with
                                    // no detail". `progress_line` (core.rs) is the pure,
                                    // host-testable formatter; this view only places its
                                    // output beside the existing stage word, never
                                    // recomputes it.
                                    //
                                    // KNOWN HAZARD, not fixed here: this whole row sits
                                    // inside the `.map(...).collect_view()` that redraws
                                    // on every signal change, and on Leptos 0.6 that
                                    // rebuilds the strip's entire DOM subtree per tick —
                                    // there is no fine-grained `DynChild` isolating just
                                    // this text node. A fast-ticking fetch will steal
                                    // focus from the Cancel button below and re-announce
                                    // the whole row to a screen reader on every percent
                                    // change, not just when the number actually moves.
                                    // Fixing that means giving the progress text its own
                                    // reactive scope; out of scope for #232.
                                    Some(detail) => format!("{stage}… — {detail}"),
                                    None => format!("{stage}…"),
                                }
                            };
                            // Only `FetchRemote`/`PullBranch`/`PushBranch` honour
                            // cancellation server-side (`planner::honours_cancellation`),
                            // mirrored here by `OperationKind::is_cancellable()`; and
                            // there is nothing to cancel before the write response has
                            // bound a server id (`InFlight::id` is `None` in that
                            // window) — both gate the button so it is never offered
                            // where the server would refuse it.
                            let show_cancel = cancellable && !cancel_requested && id.is_some();
                            let cancel_id = id.clone();
                            let on_cancel = move |_| {
                                if let Some(id) = cancel_id.clone() {
                                    operations.cancel(&id);
                                }
                            };
                            view! {
                                <div
                                    role="status"
                                    aria-live="polite"
                                    style="padding:8px 14px; background:#161b22; \
                                            border:1px solid #30363d; border-radius:8px; \
                                            color:var(--fg); font-size:0.9em; display:flex; \
                                            gap:10px; align-items:center; \
                                            justify-content:space-between;">
                                    <span>{format!("{what} — {stage_line}")}</span>
                                    {show_cancel.then(|| view! {
                                        // Its own `.op-cancel-btn` class (styles.css),
                                        // not inline styles like the settled cards'
                                        // Dismiss button below — that is what makes
                                        // this control visible to
                                        // `features::a11y::audit`'s stylesheet census
                                        // and gets it a real 44x44 tap target via the
                                        // shared #65 rule, instead of the 32px this
                                        // button used to be stuck at (#232).
                                        <button
                                            class="op-cancel-btn"
                                            aria-label=format!("Cancel: {what}")
                                            on:click=on_cancel
                                        >
                                            "Cancel"
                                        </button>
                                    })}
                                </div>
                            }
                        })
                        .collect_view()}
                    {settled
                        .into_iter()
                        .map(|(id, what, state, message)| {
                            let failed = state == OperationState::Failed;
                            // Red only for a real failure; a success that says "Already up
                            // to date" is information, not a problem.
                            let border = if failed { "#f85149" } else { "#30363d" };
                            let dismiss = move |_| operations.dismiss(&id);
                            view! {
                                <div style=format!(
                                    "padding:8px 14px; background:#161b22; \
                                     border:1px solid {border}; border-radius:8px; \
                                     color:var(--fg); font-size:0.9em; display:flex; \
                                     gap:10px; align-items:flex-start;"
                                )>
                                    // The message column is the one that scrolls: git can
                                    // be verbose, and the strip must not grow the page.
                                    <div style="flex:1 1 auto; min-width:0; max-height:6em; \
                                                overflow-y:auto; \
                                                -webkit-overflow-scrolling:touch; \
                                                white-space:pre-wrap; overflow-wrap:anywhere;">
                                        <div style="font-weight:600;">
                                            {if failed {
                                                format!("{what} failed")
                                            } else {
                                                what.clone()
                                            }}
                                        </div>
                                        {(!message.is_empty()).then(|| view! {
                                            <div style="opacity:0.8;">{message}</div>
                                        })}
                                    </div>
                                    <button
                                        style="flex:0 0 auto; padding:2px 10px; font:inherit; \
                                               color:var(--fg); background:#21262d; \
                                               border:1px solid #30363d; border-radius:6px;"
                                        on:click=dismiss
                                    >
                                        "Dismiss"
                                    </button>
                                </div>
                            }
                        })
                        .collect_view()}
                </div>
            }
        })
    }
}
