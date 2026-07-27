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
        let live: Vec<_> = core.with(|c| {
            c.in_flight()
                .map(|e| (e.kind.describe(), stage_text(e.stage)))
                .collect()
        });
        let settled: Vec<_> = core.with(|c| {
            c.recent()
                .map(|s| {
                    (
                        s.id.clone(),
                        s.kind.describe(),
                        s.outcome.state,
                        s.outcome.message.clone().unwrap_or_default(),
                    )
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
                        .map(|(what, stage)| {
                            view! {
                                <div style="padding:8px 14px; background:#161b22; \
                                            border:1px solid #30363d; border-radius:8px; \
                                            color:var(--fg); font-size:0.9em;">
                                    {format!("{what} — {stage}…")}
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
