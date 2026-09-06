//! One repository-pinned status read for the chip and commit review (#141).
use super::detail::core::reading_is_current;
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
                Some(id) => fetch_status_for(Some(id)).await.ok(),
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
pub fn read(status: StatusResource) -> Option<RepoStatus> {
    let (epoch, repo, result) = status.resource.get()?;
    reading_is_current(
        status.resource.loading().get(),
        epoch,
        status.graph.get().epoch(),
        repo.as_deref(),
        status.repo.get().as_deref(),
    )
    .then_some(result)
    .flatten()
}
