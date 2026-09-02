//! What a `/api/preview` answer means for the confirm dialog (M10.08 A6, #594).
//!
//! Framework-free, like every other feature core. This module owns the one
//! genuine decision in the preview panel — **what the four arms of
//! [`PreviewOutcome`] should say to a person** — and the derivation of the
//! per-row marks the canvas draws. Rendering lives next door; nothing here
//! knows about Leptos, SVG or a viewport.
//!
//! # Why the four arms get four presentations, and never three
//!
//! The engine's entire argument is that it refuses rather than models: a
//! conflict is a *live established fact*, not an error; `Unsupported` is a
//! permanent fact about an operation; `Unavailable` is a fact about this host
//! or this repository. Collapsing any of those into "preview failed" would
//! throw away the distinction the server spent #576 establishing, and would do
//! it at the last possible moment — in front of the user.
//!
//! So [`PreviewView`] has one variant per arm and
//! `every_outcome_arm_gets_its_own_view` pins that a new arm cannot silently
//! land in an existing bucket.
//!
//! # The preview INFORMS. It must never gate.
//!
//! Every operation previewed here was executable before previews existed, and
//! stays executable when a preview cannot be produced. A host with git 2.37
//! gets `Unavailable { GitTooOld }` and **still merges** — the dialog says what
//! it could not show, and the confirm button stays live. That is why this core
//! exposes no "can proceed" flag: there is no question for it to answer.
//! [`PreviewView::advisory_only`] states the rule where a reader will find it.

use std::collections::HashMap;

use git_vista_core::model::{BranchStub, Edge, GraphRow};
use git_vista_core::preview::PreviewChange;
use git_vista_protocol::preview::{PreviewGraph, PreviewOutcome, PreviewUnavailable};
use git_vista_protocol::{BranchName, CommitOid, GitOperation};

/// The concrete answer `/api/preview` returns, spelled with this crate's own
/// model types.
///
/// The server has the identical alias (`server/src/preview.rs:129`) but it is
/// `pub(crate)` there, so this is a second spelling of one shape rather than a
/// second shape. The wire goldens
/// (`protocol/tests/fixtures/preview_v1.json`) are what actually keep the two
/// honest — a drift between them fails there, not here.
pub type PreviewResponse = PreviewOutcome<GraphRow, Edge, BranchStub, PreviewChange>;

/// A graph half, as it arrives.
pub type Half = PreviewGraph<GraphRow, Edge, BranchStub>;

/// What one row of the **after** graph should be marked with.
///
/// Composed of three independent facts rather than an enum, because a single
/// commit routinely carries more than one: the hypothetical commit is `added`
/// *and* is where the branch ref lands. An enum would force a precedence order
/// nobody asked for, and would quietly drop whichever fact lost.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RowMark {
    /// This commit does not exist yet — the operation would create it.
    pub added: bool,
    /// Refs that end up pointing here, in the order the server listed them.
    pub refs_landed: Vec<String>,
    /// `(from, to)` when this commit sits in a different lane than it did.
    ///
    /// Only ever `Some` for a commit present in **both** halves, which is what
    /// makes the before half load-bearing rather than decorative: a lane shift
    /// is defined by comparing the two layouts, and a caller holding only
    /// `after` cannot check a single one of these numbers.
    pub lane_shift: Option<(usize, usize)>,
}

impl RowMark {
    /// Whether this row is marked at all — `false` means draw it plainly.
    pub fn is_marked(&self) -> bool {
        self.added || !self.refs_landed.is_empty() || self.lane_shift.is_some()
    }
}

/// A before/after picture, with the after half marked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub before: Half,
    pub after: Half,
    /// Commit id -> what to mark it with. Only marked rows appear.
    pub marks: HashMap<String, RowMark>,
    /// One plain sentence describing the change, for readers who will not read
    /// a graph — and for a screen reader, which cannot.
    pub summary: String,
}

/// What the dialog shows beside its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewView {
    /// The operation applies; here is the repository as it would be.
    Picture(Picture),
    /// Real git ran the real three-way merge and it does not apply. A fact,
    /// not a failure — `paths` is never empty in this arm (the server turns a
    /// pathless conflict into `Unavailable { CheckFailed }` precisely so this
    /// cannot read as "conflicted, nothing conflicted").
    Conflict { paths: Vec<String> },
    /// The plumbing cannot express this operation, so no picture exists —
    /// permanently, for every host. Not a fault to report.
    Unsupported { operation: String },
    /// Previewable in principle; not here, or not now. Every case carries a
    /// named reason, and where anything can be done about it, `remedy` says so.
    Unavailable {
        headline: String,
        detail: Option<String>,
        remedy: Option<String>,
    },
}

impl PreviewView {
    /// Always `true`, and it is a function rather than a comment so that the
    /// rule is something a test can hold on to.
    ///
    /// A preview never decides whether an operation may proceed. It could not
    /// be otherwise without changing what these operations mean: they were all
    /// executable before #576 existed. A future reader tempted to gate the
    /// confirm button on `matches!(view, Picture(_))` should find this first.
    pub fn advisory_only(&self) -> bool {
        true
    }

    /// Whether there is a graph to draw. Not "whether the preview succeeded" —
    /// a `Conflict` is a successful preview with no picture in it.
    pub fn has_picture(&self) -> bool {
        matches!(self, PreviewView::Picture(_))
    }
}

/// Fold one `/api/preview` answer into what the dialog should show.
pub fn view_of(response: PreviewResponse) -> PreviewView {
    match response {
        PreviewOutcome::Graph {
            before,
            after,
            changes,
        } => {
            let marks = marks_from(&changes);
            let summary = summarize(&changes);
            PreviewView::Picture(Picture {
                before,
                after,
                marks,
                summary,
            })
        }
        PreviewOutcome::Conflict { paths } => PreviewView::Conflict { paths },
        PreviewOutcome::Unsupported { operation } => PreviewView::Unsupported { operation },
        PreviewOutcome::Unavailable { reason } => unavailable_view(reason),
    }
}

/// The named reasons, each rendered as headline + detail + remedy.
///
/// `remedy` is `Some` only where the user can actually do something. Inventing
/// advice for `CheckFailed` — whose whole meaning is "a git step ran and did
/// not produce an answer" — would be guessing in the one arm defined by not
/// knowing.
fn unavailable_view(reason: PreviewUnavailable) -> PreviewView {
    match reason {
        PreviewUnavailable::RepositoryReadOnly => PreviewView::Unavailable {
            headline: "No preview in Visualize mode".to_string(),
            detail: Some(
                "A preview needs somewhere to write the hypothetical commit, and a \
                 read-only repository grants it nowhere. Nothing is wrong with the \
                 repository."
                    .to_string(),
            ),
            remedy: Some("Reopen this repository in Active mode to see previews.".to_string()),
        },
        PreviewUnavailable::GitTooOld { found, minimum } => PreviewView::Unavailable {
            headline: format!("This host's git ({found}) is older than previews need"),
            detail: Some(format!(
                "Drawing the result without running it needs `merge-tree --write-tree`, \
                 added in git {minimum}. Everything else here works on this version."
            )),
            remedy: Some(format!("Upgrade git to {minimum} or newer for previews.")),
        },
        PreviewUnavailable::ScratchStore { detail } => PreviewView::Unavailable {
            headline: "The preview had nowhere to run".to_string(),
            detail: Some(detail),
            remedy: None,
        },
        PreviewUnavailable::CheckFailed { detail } => PreviewView::Unavailable {
            headline: "The preview could not be computed".to_string(),
            detail: Some(detail),
            remedy: None,
        },
    }
}

/// Turn the change list into per-commit marks.
///
/// A commit can appear in several changes — added, and the place a ref lands —
/// so entries accumulate onto one [`RowMark`] rather than replacing it.
fn marks_from(changes: &[PreviewChange]) -> HashMap<String, RowMark> {
    let mut marks: HashMap<String, RowMark> = HashMap::new();
    for change in changes {
        match change {
            PreviewChange::Added { commit } => {
                marks.entry(commit.0.clone()).or_default().added = true;
            }
            PreviewChange::RefMoved { ref_name, to, .. } => {
                marks
                    .entry(to.0.clone())
                    .or_default()
                    .refs_landed
                    .push(ref_name.clone());
            }
            PreviewChange::LaneShifted {
                commit,
                from_lane,
                to_lane,
            } => {
                marks.entry(commit.0.clone()).or_default().lane_shift =
                    Some((*from_lane, *to_lane));
            }
        }
    }
    marks
}

/// One plain sentence for the change list.
///
/// Deliberately plain: this is what a person reads when they will not read a
/// graph, and the only thing a screen reader gets. An empty `changes` is a
/// claim — "this operation changes nothing" — and is said out loud rather than
/// rendered as an absence.
fn summarize(changes: &[PreviewChange]) -> String {
    if changes.is_empty() {
        return "Nothing would change.".to_string();
    }
    let added = changes
        .iter()
        .filter(|c| matches!(c, PreviewChange::Added { .. }))
        .count();
    let moved: Vec<&str> = changes
        .iter()
        .filter_map(|c| match c {
            PreviewChange::RefMoved { ref_name, .. } => Some(ref_name.as_str()),
            _ => None,
        })
        .collect();
    let shifted = changes
        .iter()
        .filter(|c| matches!(c, PreviewChange::LaneShifted { .. }))
        .count();

    let mut parts: Vec<String> = Vec::new();
    if added == 1 {
        parts.push("one new commit".to_string());
    } else if added > 1 {
        parts.push(format!("{added} new commits"));
    }
    match moved.len() {
        0 => {}
        1 => parts.push(format!("{} moves", moved[0])),
        _ => parts.push(format!("{} refs move", moved.len())),
    }
    if shifted == 1 {
        parts.push("one commit changes lane".to_string());
    } else if shifted > 1 {
        parts.push(format!("{shifted} commits change lane"));
    }

    if parts.is_empty() {
        // Every arm above declined, which means `changes` holds a variant this
        // function does not know about. Say the count rather than "nothing":
        // silently reporting no change for a change we cannot name is the one
        // wrong answer here.
        return format!("{} change(s).", changes.len());
    }
    format!("{}.", sentence_list(&parts))
}

/// `["a"] -> "a"`, `["a","b"] -> "a and b"`, `["a","b","c"] -> "a, b and c"`.
fn sentence_list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [only] => only.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// What a confirm dialog is asking about, reduced to the part a preview needs.
///
/// Deliberately **not** `PendingOp`. That type lives in `crate::state`, which
/// is `#[cfg(target_arch = "wasm32")]`, so a decision written against it could
/// never be host-tested — and "which dialogs get a preview" is exactly the
/// kind of decision that rots silently when nothing can check it. The wasm
/// dialog does the one-line translation; the rule lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogSubject<'a> {
    /// `git merge --no-edit <branch>`.
    Merge { branch: &'a str },
    /// `git revert --no-edit <commit>` — reached through `UndoAction::RevertCommit`.
    Revert { commit: &'a str },
    /// `git cherry-pick <commit>` — reached through `PendingOp::CherryPick`,
    /// the confirm dialog #599 built for the door it opened.
    ///
    /// Ordinary commits only, like [`Revert`]. A merge commit has no sole
    /// parent, so the engine answers `Unsupported` rather than guessing which
    /// side the change is measured against, and this panel renders that
    /// honestly — which is exactly what `menu::commit_items` defers to it for.
    ///
    /// The destination branch is deliberately **not** carried here.
    /// `PendingOp::CherryPick` knows its `onto`, but a pick lands on whatever
    /// HEAD is checked out when the server runs it, and the engine reads that
    /// itself. A second copy of the destination could only ever disagree with
    /// the operation it claims to picture.
    ///
    /// [`Revert`]: DialogSubject::Revert
    CherryPick { commit: &'a str },
    /// Every other confirmation this modal shows. Checkout, delete, fetch,
    /// pull, push, reset, discard: the engine previews none of them
    /// (`git-vista-server/src/preview.rs:753-766` maps exactly three
    /// operations), so asking would spend two round trips to be told
    /// `Unsupported`.
    NotPreviewable,
}

/// The operation to preview for this dialog, or `None` if it has none.
///
/// # All three of the engine's operations are mapped
///
/// This mapped two until #599 gave cherry-pick the confirm dialog it had no
/// route to (#596). The note that used to stand here said cherry-pick would
/// "inherit this panel for free the day it gets one, by adding a `CherryPick`
/// arm here and one line to the caller" — that day is this commit, and that
/// is exactly what it cost. The engine previews three operations
/// (`git-vista-server/src/preview.rs:753-766`) and this now maps all three,
/// so the two sides are no longer silently narrower than each other.
///
/// # An invalid name yields `None`, never a panic
///
/// `BranchName`/`CommitOid` validate on construction. A name this app could
/// not have produced is a bug somewhere upstream, and the right behaviour in a
/// dialog is to show no picture — the operation itself is still confirmable,
/// and it is the *server* that must refuse a bad name, not a preview panel.
pub fn previewable(subject: DialogSubject<'_>) -> Option<GitOperation> {
    match subject {
        DialogSubject::Merge { branch } => Some(GitOperation::MergeBranch {
            branch: BranchName::new(branch).ok()?,
        }),
        DialogSubject::Revert { commit } => Some(GitOperation::RevertCommit {
            commit: CommitOid::new(commit).ok()?,
        }),
        DialogSubject::CherryPick { commit } => Some(GitOperation::CherryPick {
            commit: CommitOid::new(commit).ok()?,
        }),
        DialogSubject::NotPreviewable => None,
    }
}

/// The sentence shown under a preview that has no picture in it, or `None`
/// when the picture speaks for itself.
///
/// Every arm but `Picture` shows a reader something that *looks* like a
/// refusal — "these files conflict", "this host's git is too old" — and the
/// one thing that must not follow from any of them is a belief that the
/// operation is now unavailable. It is not, and it never was: all of these
/// were executable before previews existed.
///
/// # Why this consults [`PreviewView::advisory_only`] instead of asserting it
///
/// The guard reads as redundant today, because `advisory_only` is
/// unconditional. That is the point. If some future change makes a preview
/// gate an operation, this sentence — which *promises* the operation is still
/// available — stops being printed, rather than becoming a lie printed under
/// a disabled button. A promise wired to the thing it promises is worth more
/// than an assertion beside it.
pub fn reassurance(view: &PreviewView) -> Option<&'static str> {
    if !view.advisory_only() {
        return None;
    }
    match view {
        PreviewView::Picture(_) => None,
        _ => Some(
            "This is only a picture of what would happen. The operation itself is \
             unchanged and still available — confirm below to run it.",
        ),
    }
}

#[cfg(test)]
#[path = "core_suite.rs"]
mod core_suite;
