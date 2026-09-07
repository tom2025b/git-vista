//! One repository-pinned status read for the chip and commit review (#141).
use super::detail::core::{actionable_staged_count, current_reading};
use crate::api::fetch_status_for;
use crate::features::activity::signals::Activity;
use crate::features::graph::core::GraphCore;
use git_vista_core::status::RepoStatus;
use leptos::*;

/// One `/api/status` reply, tagged with the epoch and repository id it was
/// *requested for* — the shape `detail::core::current_reading` resolves
/// against the live frame. The scope travels with the reply so no consumer
/// can pair one request's answer with another request's scope.
type StatusReply = (u64, Option<String>, Option<RepoStatus>);
type StatusRead = Resource<(bool, u64, Option<String>), StatusReply>;
/// `(loading, reply, live epoch, live repository)` — what every accessor
/// below needs, gathered once by [`StatusResource::reply_and_frame`].
type ReplyAndFrame = (bool, Option<StatusReply>, u64, Option<String>);

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

    /// The retained reply and the live frame, read together in one place.
    ///
    /// Every consumer below resolves the same four values, so they are
    /// gathered once rather than re-listed per accessor: a second accessor
    /// that assembled them by hand is exactly how a call site ends up
    /// comparing a frame against its own scope (`current_reading`'s doc
    /// comment). Tracked reads, so each consumer stays reactive.
    fn reply_and_frame(self) -> ReplyAndFrame {
        (
            self.resource.loading().get(),
            self.resource.get(),
            self.graph.get().epoch(),
            self.repo.get(),
        )
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
    let (loading, reply, epoch, repo) = status.reply_and_frame();
    current_reading(loading, reply, epoch, repo.as_deref())
}

/// The context menu's staged-file count, from a current reading only (#709).
///
/// `menu.rs` used to open its own unscoped `fetch_status()` for this, which
/// gave the staging items a reading that no frame vouched for. It reads this
/// instead: the same one owner, the same pinning, and the decision itself in
/// `actionable_staged_count`, which is host-tested — `menu.rs` is wasm-only,
/// so a count computed there could never go red in `cargo test`.
pub fn staged_count(status: StatusResource) -> usize {
    let (loading, reply, epoch, repo) = status.reply_and_frame();
    actionable_staged_count(loading, reply, epoch, repo.as_deref())
}
