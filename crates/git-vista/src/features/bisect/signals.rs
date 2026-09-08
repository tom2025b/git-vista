//! Browser read and DOM wrapper; the display decision lives in `core`.
use super::core::indicator;
use crate::api::fetch_bisect_status_for;
use crate::features::graph::core::GraphCore;
use leptos::*;

/// Mounted in the shell: refresh on load, epoch/repository changes and reconnect.
/// Poll too: an external bisect can change its state without moving HEAD.
pub fn indicator_view(
    graph: RwSignal<GraphCore>,
    repo: Signal<Option<String>>,
    online: RwSignal<bool>,
) -> impl IntoView {
    let tick = create_rw_signal(0_u64);
    if let Ok(handle) = set_interval_with_handle(
        move || tick.update(|n| *n = n.wrapping_add(1)),
        std::time::Duration::from_secs(5),
    ) {
        on_cleanup(move || handle.clear());
    }
    let resource = create_local_resource(
        move || (graph.get().epoch(), repo.get(), online.get(), tick.get()),
        |(epoch, repo, online, _)| async move {
            let result = match (online, repo.as_deref()) {
                (true, Some(id)) => fetch_bisect_status_for(id).await.ok(),
                _ => None,
            };
            (epoch, repo, result)
        },
    );
    view! {
        <span role="status" class="bisect-status">
            {move || indicator(
                resource.loading().get() || !online.get(),
                resource.get(),
                graph.get().epoch(),
                repo.get().as_deref(),
            )}
        </span>
    }
}
