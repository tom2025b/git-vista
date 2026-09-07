//! One repository-pinned status read for the chip and commit review (#141).
use super::detail::core::current_reading;
use crate::api::fetch_status_for;
use crate::features::activity::signals::Activity;
use crate::features::graph::core::GraphCore;
use git_vista_core::status::RepoStatus;
use leptos::*;

type StatusRead = Resource<(bool, u64, Option<String>), (u64, Option<String>, Option<RepoStatus>)>;

#[derive(Clone, Copy)]
pub struct StatusResource {
    resource: StatusRead,
    graph: RwSignal<GraphCore>,
    repo: Signal<Option<String>>,
}

impl StatusResource {
    pub fn refetch(self) {
        self.resource.refetch();
    }
}

/// The accepted frame's opaque repository id this resource is scoped to —
/// what a second caller (`menu.rs`'s own staged-file count) must pin its own
/// fetch to, the same way [`create`]'s resource does.
pub fn repo(status: StatusResource) -> Option<String> {
    status.repo.get()
}

/// Opening Activity or changing the accepted frame refreshes the shared read.
/// Until that frame arrives, no unscoped request can describe the previous repo.
pub fn create(
    graph: RwSignal<GraphCore>,
    activity: Activity,
    repo: Signal<Option<String>>,
) -> StatusResource {
    let resource = create_local_resource(
        move || (activity.is_open(), graph.get().epoch(), repo.get()),
        |(_, epoch, repo)| async move {
            let result = match repo.as_deref() {
                Some(id) => fetch_status_for(id).await.ok(),
                None => None,
            };
            (epoch, repo, result)
        },
    );
    StatusResource {
        resource,
        graph,
        repo,
    }
}

/// Failed, loading, old-epoch and old-repository readings are all unknown.
/// This is a signal adapter only: every decision lives in `current_reading`,
/// which is host-tested. The retained reply carries its own requested scope,
/// so nothing here can mismatch requested against current.
pub fn read(status: StatusResource) -> Option<RepoStatus> {
    current_reading(
        status.resource.loading().get(),
        status.resource.get(),
        status.graph.get().epoch(),
        status.repo.get().as_deref(),
    )
}
