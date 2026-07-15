//! The `/api/*` route handlers, split out of `main.rs` by concern.
//!
//! Each submodule owns one cluster of endpoints; `main.rs` keeps the router and
//! `use`s the handler fns so the `Router::new().route(...)` table reads exactly as
//! before. The handlers are `pub(crate)` (the router in the crate root is their
//! only caller); helpers used by only one file stay private to it.
//!
//!   * [`read`]   — the read endpoints: history graph, one commit's detail/diff,
//!     the live head-branch / working-tree reads.
//!   * [`clone`]  — clone a public URL into a throwaway dir, view it read-only.
//!   * [`commit`] — create a commit (on HEAD, or an empty one on a named branch).
//!   * [`branch`] — create a branch and the branch operations (checkout / merge /
//!     push / delete / force-delete) that share one runner.
//!   * [`rebase`] — rebase the checked-out branch onto main, and its live gate.
//!   * [`reset`]  — restore a seeded test repo to its recorded state.
//!
//! [`journal_app_event`] lives here because the write handlers across several of
//! those submodules all record their successful operation the same way; the undo
//! handler in [`crate::activity`] records through it too, so it's `pub(crate)`.

use std::path::Path;

use git_vista_core::activity::{ActivityEvent, ActivityKind, ActivitySource};

use crate::{activity, journal};

pub(crate) mod branch;
pub(crate) mod clone;
pub(crate) mod commit;
pub(crate) mod read;
pub(crate) mod rebase;
pub(crate) mod reset;

/// Record one successful app operation in the journal (source: App). The
/// activity feed matches the operation's own reflog echo against this entry
/// and shows a single event labelled "via git-vista". Best-effort by design:
/// the git operation already succeeded, so journal trouble is only logged.
pub(crate) fn journal_app_event(
    repo: &Path,
    kind: ActivityKind,
    ref_name: Option<String>,
    old_oid: Option<String>,
    new_oid: Option<String>,
    summary: String,
) {
    journal::append(
        repo,
        &ActivityEvent {
            time: activity::now_secs(),
            kind,
            ref_name,
            summary,
            old_oid,
            new_oid,
            source: ActivitySource::App,
            undo: None,
        },
    );
}
