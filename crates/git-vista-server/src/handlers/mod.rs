//! The `/api/*` route handlers, split out of `main.rs` by concern.
//!
//! Each submodule owns one cluster of endpoints; `main.rs` keeps the router and
//! `use`s the handler fns so the `Router::new().route(...)` table reads exactly as
//! before. The handlers are `pub(crate)` (the router in the crate root is their
//! only caller); helpers used by only one file stay private to it.
//!
//!   * [`protocol`] — `GET /api/protocol`, the unversioned negotiation endpoint.
//!   * [`read`]   — the read endpoints: history graph, one commit's detail/diff,
//!     the live head-branch / working-tree reads.
//!   * [`clone`]  — clone a public URL into a throwaway dir, view it read-only.
//!   * [`commit`] — create a commit (on HEAD, or an empty one on a named branch).
//!   * [`branch`] — create a branch and the branch operations (checkout / merge /
//!     push / delete / force-delete).
//!   * [`rebase`] — rebase the checked-out branch onto main, and its live gate.
//!   * [`reset`]  — restore a seeded test repo to its recorded state.
//!   * [`discard`] — discard uncommitted changes to tracked paths, or delete
//!     untracked paths outright (#219).
//!   * [`operations`] — one write's recorded lifecycle, and its progress stream.
//!   * [`tags`]    — `GET /api/tags`, every tag with its kind, target, tagger
//!     and message (M2.21b, #236).
//!
//! Since M1.06b (#143) the write handlers don't run git themselves: each
//! validates its request, builds one typed `GitOperation` (#142), and hands it
//! to [`crate::planner`], which builds/validates/executes the reviewable Plan.
//!
//! [`journal_app_event`] lives here because the write handlers across several of
//! those submodules all record their successful operation the same way; the undo
//! handler in [`crate::activity`] records through it too, so it's `pub(crate)`.

use std::path::Path;

use git_vista_core::activity::{ActivityEvent, ActivityKind, ActivitySource};

use crate::{activity, journal};

pub(crate) mod branch;
pub(crate) mod catalog;
pub(crate) mod clone;
pub(crate) mod commit;
// #219 (M2.18a): discard tracked-path changes / delete untracked paths.
pub(crate) mod discard;
// M1.08 (#61): what happened to an operation, and watching one happen.
pub(crate) mod operations;
pub(crate) mod protocol;
pub(crate) mod read;
pub(crate) mod rebase;
pub(crate) mod reset;
pub(crate) mod select;
pub(crate) mod staging;
// M2.21b (#236): `GET /api/tags`, the tag listing with type/target/tagger/message.
pub(crate) mod tags;
// M1.04 (#57): establish / check / revoke a loopback session.
pub(crate) mod session;

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
