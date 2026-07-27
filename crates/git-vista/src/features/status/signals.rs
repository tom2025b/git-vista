//! The app's one working-tree status read — wasm only (M1.11, #64).
//!
//! Deliberately thin, for the reason [`super`] gives: the behaviour belongs to M2.15 (#68),
//! and speculative state here would be worse than a shaped hole. What Task 7 does add is
//! the part that cannot wait — a single owner. Before this there were two independent
//! `Resource`s issuing the same `GET /api/status`: the topbar chip's, created in `App` and
//! keyed on the graph epoch, and the Activity panel's, created inside the panel and keyed
//! on `(open, epoch)`. They never shared a cache, and because the panel is rebuilt on every
//! epoch bump, the second one was also re-created far more often than "when the panel
//! opens" suggests.
//!
//! `menu.rs`'s `staged_count` is **not** folded in. It reads the same endpoint but reduces
//! to a single `usize` and gates on a third, unrelated condition (the menu being open on a
//! non-branch HEAD commit); collapsing it would couple the context menu's fetch timing to
//! the panel's for no gain.

use leptos::{create_local_resource, Resource, RwSignal, SignalGet};

use git_vista_core::status::RepoStatus;

use crate::api::fetch_status;
use crate::features::activity::signals::Activity;
use crate::features::graph::core::GraphCore;

/// The shared status read. Keyed on `(activity panel open, graph epoch)`; resolves to
/// `None` when the fetch failed, which simply hides the chip and the panel's section — a
/// broken status probe should not take either down.
pub type StatusResource = Resource<(bool, u64), Option<RepoStatus>>;

/// Create the one status `Resource`. Call this in `App`, above `graph_canvas`, so it
/// outlives the canvas that an epoch bump rebuilds.
///
/// The epoch half of the key is what makes Refresh — and every post-write reload —
/// re-read it. The `activity` half preserves a documented product requirement the panel's
/// private copy used to carry on its own: opening the panel always shows a fresh read,
/// even when the epoch has not moved since the last one (`activity.rs`'s issue-16 lesson).
///
/// Consequence worth knowing: *closing* the panel also flips the key, so it costs one
/// fetch the old private `Resource` avoided by short-circuiting to `None` while closed.
/// One extra local status read per close, in exchange for deleting a whole second read
/// path — and the topbar chip gets a refresh out of it.
pub fn create(graph: RwSignal<GraphCore>, activity: Activity) -> StatusResource {
    create_local_resource(
        move || (activity.is_open(), graph.get().epoch()),
        // Always fetch, whatever the bool says: the topbar chip wants a live status
        // whether or not the panel happens to be open. The bool is in the key to force a
        // re-read, not to gate one.
        |_| async { fetch_status().await.ok() },
    )
}

/// The current status, or `None` while the first fetch is in flight or after it failed.
/// A tracked read — both consumers re-render from it.
pub fn read(status: StatusResource) -> Option<RepoStatus> {
    status.get().flatten()
}
