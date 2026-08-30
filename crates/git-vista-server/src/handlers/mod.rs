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
//!   * [`fetch`] — fetch from a configured remote (M2.20c, #229): the first
//!     long-running network write, with streamed progress and cancellation.
//!   * [`pull`] — fetch and then integrate (M2.20d, #230), with the
//!     merge-or-rebase choice required on the wire and never defaulted.
//!   * [`operations`] — one write's recorded lifecycle, its progress stream,
//!     and (since #229) cancelling one that is still running.
//!   * [`tags`]    — `GET /api/tags`, every tag with its kind, target, tagger
//!     and message (M2.21b, #236).
//!   * [`plan`] — `POST /api/plan` (#248): build a reviewable `Plan` and hand
//!     it back unexecuted — the only endpoint that mints a plan without
//!     running it, and the one the MCP `plan_*` tools sit on.
//!   * [`preview`] — `POST /api/preview` (#576, ADR 0099): the graph that
//!     `Plan` would produce, computed by real git against the real objects in
//!     a throwaway store and written nowhere. The picture half of the same
//!     review roundtrip `plan` opens in words.
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
// M4.31a (#428): inspect a conflict — the metadata listing, a stage blob by
// oid, and the read-only result-pane worktree read.
pub(crate) mod conflicts;
// #219 (M2.18a): discard tracked-path changes / delete untracked paths.
pub(crate) mod discard;
// M2.20c (#229): fetch from a configured remote.
pub(crate) mod fetch;
// M2.20d (#230): fetch and integrate, with a mandatory merge/rebase strategy.
pub(crate) mod pull;
// M1.08 (#61): what happened to an operation, and watching one happen.
pub(crate) mod operations;
// M2.23d (#248, ADR 0046): build one reviewable Plan and return it, run nothing.
pub(crate) mod plan;
// M10.08 (#576, ADR 0099): the graph one Plan would produce, run nothing.
pub(crate) mod preview;
pub(crate) mod protocol;
pub(crate) mod read;
pub(crate) mod rebase;
pub(crate) mod reset;
pub(crate) mod select;
pub(crate) mod staging;
pub(crate) mod stash;
// M2.21b (#236): `GET /api/tags`, the tag listing with type/target/tagger/message.
pub(crate) mod tags;
// M1.04 (#57): establish / check / revoke a loopback session.
pub(crate) mod session;

/// One entry of a batched app journal write: exactly the five fields
/// [`journal_app_event`] takes, minus the repository they are written to.
///
/// A named struct rather than a tuple because two of the three `Option`
/// fields hold object ids and the third holds a ref name; a five-tuple at the
/// call site would let the two oids swap silently.
pub(crate) struct AppEntry {
    pub kind: ActivityKind,
    pub ref_name: Option<String>,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub summary: String,
}

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
    journal_app_events(
        repo,
        vec![AppEntry {
            kind,
            ref_name,
            old_oid,
            new_oid,
            summary,
        }],
    );
}

/// Record one operation that moved several refs — every entry written
/// together, under one ref capture and at one moment (#485, ADR 0080).
///
/// # One moment, not one per entry
///
/// Every entry is stamped with the same [`activity::now_secs`] reading,
/// because they describe one action. That is not a convenience: the feed
/// attributes a journal entry to git's own reflog line for the same movement
/// only when the two are within `JOURNAL_MATCH_SLACK` of each other, and while
/// each entry took its own reading *after* its own full ref read, entry *i*
/// drifted further and further from the reflog line git wrote for it. Past
/// roughly 170 refs the later entries stopped matching, their reflog lines
/// survived attribution, and the fold counted both copies — 500 refs reported
/// as 891 (`docs/investigations/2026-08-25-issue-329-fetch-feed-volume.md`,
/// F1). Removing the per-entry ref read removes most of that drift; taking one
/// reading for the batch removes the rest of it at this level.
///
/// It does **not** repair the fold, which over-counts whenever entries drift
/// past the window for any reason — that defect is still pinned in
/// `git_vista_core::activity`.
///
/// # Still one entry per ref
///
/// The batching is in the *capture*, never in the entries. `0a7ba777` reverted
/// an attempt to replace them with a single summary entry: the per-ref
/// entries, each carrying a `new_oid`, are what suppresses git's own per-ref
/// reflog lines, and a summary entry suppresses none of them.
pub(crate) fn journal_app_events(repo: &Path, entries: Vec<AppEntry>) {
    let time = activity::now_secs();
    let events: Vec<ActivityEvent> = entries
        .into_iter()
        .map(|entry| ActivityEvent {
            time,
            kind: entry.kind,
            ref_name: entry.ref_name,
            summary: entry.summary,
            old_oid: entry.old_oid,
            new_oid: entry.new_oid,
            source: ActivitySource::App,
            undo: None,
            // Left None deliberately: journal::append_all captures the
            // branch-tip map itself (#131), so no write endpoint can forget
            // to — once for the whole batch since #485.
            refs: None,
        })
        .collect();
    journal::append_all(repo, &events);
}
