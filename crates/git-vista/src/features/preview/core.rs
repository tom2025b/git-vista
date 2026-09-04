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
/// Deliberately **not** `OperationKind`. Not because that type is unreachable
/// — this doc used to say it "lives in `crate::state`, which is
/// `#[cfg(target_arch = "wasm32")]`", and that was wrong: `crate::state`
/// re-exports it under its old name `PendingOp`, but the definition is in
/// `features::operations::kind`, which is framework-free and host-tested. The
/// re-export path is wasm-only; the type is not.
///
/// The real reason is narrowness. This vocabulary carries only what a preview
/// needs — a branch name, a commit id — so the preview core never has to know
/// the operations vocabulary, and a new `OperationKind` variant cannot change
/// what this file decides. `features::dialogs::core::preview_subject` does the
/// translation, and is host-tested for it; the rule lives here.
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

/// What the confirm dialog's preview slot should do, for one state of that
/// dialog.
///
/// Two arms, not three: "no dialog is open" and "this dialog has no picture"
/// are the same instruction to the slot, and collapsing them **here** rather
/// than in the caller is the whole point of this type existing. See
/// [`preview_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewAction {
    /// Ask the engine for this operation's picture.
    Start(GitOperation),
    /// Show no picture, and invalidate any request already on the wire.
    Clear,
}

/// What the preview slot should do for the confirm dialog's current subject.
///
/// # Why this is a function and not four lines in the effect
///
/// It *was* four lines in the effect — a `match` in `dialogs/confirm.rs`, which
/// is `#[cfg(target_arch = "wasm32")]`. [`previewable`] and
/// `features::dialogs::core::preview_subject` were both already here in core
/// and both host-tested, and #594's mutation proof duly reported "both caught"
/// — while only ever reaching those two. The line that *composed* them was
/// invisible to every runner, so swapping `preview.clear()` for `preview.start`
/// on the wrong arm, or dropping the `None` arm entirely, would have compiled,
/// shipped and stayed green. That is #612's premise, and this is the instance
/// #612's own body names. Moving the composition here is what makes the
/// original proof honest in retrospect.
///
/// # Both "no dialog" and "not previewable" clear
///
/// A close is what invalidates an in-flight request: clearing bumps the
/// preview generation, so a reply already on the wire cannot paint the *next*
/// dialog with the last one's picture. An unpreviewable dialog needs the same
/// treatment for the same reason — it may well be the dialog that opened over
/// a previewable one. The two paths must not diverge, so they are one arm.
///
/// # It never gates
///
/// There is deliberately no third arm meaning "refuse". Every operation
/// reaching a confirm dialog was confirmable before previews existed and stays
/// confirmable when there is no picture — the rule
/// [`PreviewView::advisory_only`] states, kept unrepresentable here.
pub fn preview_action(subject: Option<DialogSubject<'_>>) -> PreviewAction {
    match subject.and_then(previewable) {
        Some(operation) => PreviewAction::Start(operation),
        None => PreviewAction::Clear,
    }
}

#[cfg(test)]
mod preview_action_tests {
    use super::*;

    use crate::features::dialogs::core::preview_subject;
    use crate::features::operations::kind::{HeadBranch, OperationKind};
    use git_vista_core::activity::{UndoAction, Undoable};

    /// The wasm-only confirm modal, read as text. `dialogs/confirm.rs` is
    /// `#[cfg(target_arch = "wasm32")]`, so this is the only way a host test
    /// can see what it does with the answers below — the same thing
    /// `features::a11y::audit` does for markup it cannot mount.
    const CONFIRM: &str = include_str!("../../dialogs/confirm.rs");

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    fn undoable(action: UndoAction) -> Undoable {
        Undoable {
            action,
            label: "undo".to_string(),
            warn_pushed: false,
        }
    }

    /// The effect body in `confirm.rs` that drives the preview slot.
    fn preview_effect_body() -> String {
        let after = CONFIRM
            .split_once("let action = match &shell.confirm_op() {")
            .expect("dialogs/confirm.rs no longer contains the preview effect")
            .1;
        let end = after
            .find("    });")
            .expect("the preview effect is no longer a closed block");
        after[..end].to_string()
    }

    #[test]
    fn every_previewable_dialog_asks_for_the_operation_it_is_about() {
        assert_eq!(
            preview_action(Some(DialogSubject::Merge { branch: "feature" })),
            PreviewAction::Start(GitOperation::MergeBranch {
                branch: BranchName::new("feature").expect("a valid branch name"),
            }),
        );
        assert_eq!(
            preview_action(Some(DialogSubject::Revert { commit: OID })),
            PreviewAction::Start(GitOperation::RevertCommit {
                commit: CommitOid::new(OID).expect("a valid oid"),
            }),
        );
        assert_eq!(
            preview_action(Some(DialogSubject::CherryPick { commit: OID })),
            PreviewAction::Start(GitOperation::CherryPick {
                commit: CommitOid::new(OID).expect("a valid oid"),
            }),
        );
    }

    #[test]
    fn a_dialog_with_no_picture_and_no_dialog_at_all_give_the_same_instruction() {
        // The collapse this function exists for. `confirm.rs` used to make it
        // itself, in a nested match no runner could reach: an unpreviewable
        // dialog opening over a previewable one must invalidate the picture
        // already on the wire exactly as a close does, or the new dialog
        // inherits the old one's graph.
        assert_eq!(
            preview_action(Some(DialogSubject::NotPreviewable)),
            PreviewAction::Clear,
        );
        assert_eq!(preview_action(None), PreviewAction::Clear);
    }

    #[test]
    fn a_name_this_app_could_not_have_produced_draws_no_picture_and_does_not_panic() {
        // `BranchName`/`CommitOid` validate on construction, and the right
        // behaviour in a dialog is no picture — the operation itself is still
        // confirmable, and it is the server that must refuse a bad name.
        assert_eq!(
            preview_action(Some(DialogSubject::Merge { branch: "" })),
            PreviewAction::Clear,
        );
        assert_eq!(
            preview_action(Some(DialogSubject::Revert { commit: "nope" })),
            PreviewAction::Clear,
        );
    }

    #[test]
    fn every_dialog_subject_is_routed_and_only_the_engines_three_get_a_picture() {
        // Completeness in the shape #531 taught: one flag per variant, ticked
        // by an exhaustive match so a new `DialogSubject` is a *compile* error
        // here, then asserted by name so a missing entry is a named red
        // assertion rather than a stale count.
        #[derive(Default)]
        struct Census {
            merge: bool,
            revert: bool,
            cherry_pick: bool,
            not_previewable: bool,
        }
        let mut census = Census::default();
        let mut started = 0usize;
        for subject in [
            DialogSubject::Merge { branch: "b" },
            DialogSubject::Revert { commit: OID },
            DialogSubject::CherryPick { commit: OID },
            DialogSubject::NotPreviewable,
        ] {
            match subject {
                DialogSubject::Merge { .. } => census.merge = true,
                DialogSubject::Revert { .. } => census.revert = true,
                DialogSubject::CherryPick { .. } => census.cherry_pick = true,
                DialogSubject::NotPreviewable => census.not_previewable = true,
            }
            if matches!(preview_action(Some(subject)), PreviewAction::Start(_)) {
                started += 1;
            }
        }
        assert!(census.merge, "Merge is not in the list above");
        assert!(census.revert, "Revert is not in the list above");
        assert!(census.cherry_pick, "CherryPick is not in the list above");
        assert!(
            census.not_previewable,
            "NotPreviewable is not in the list above"
        );
        assert_eq!(
            started, 3,
            "the engine previews exactly three operations \
             (git-vista-server/src/preview.rs) and this side must ask for the \
             same three — no more, or the dialog spends two round trips to be \
             told Unsupported; no fewer, and #594 is back"
        );
    }

    /// The two host-tested halves, composed — which is the one thing neither
    /// half could prove on its own.
    ///
    /// `preview_subject`'s variant mapping is already pinned next door in
    /// `features::dialogs::core`, and `previewable`'s table is pinned above.
    /// What was never checked is that an `OperationKind` as the modal actually
    /// holds it comes out the far end as the *right instruction*, because the
    /// line joining them lived in `dialogs/confirm.rs`. So this asserts the
    /// composed answer, not the intermediate subject.
    #[test]
    fn an_operation_the_modal_holds_composes_all_the_way_to_its_picture() {
        let action = |kind: OperationKind| preview_action(Some(preview_subject(&kind)));

        assert_eq!(
            action(OperationKind::Merge {
                branch: "feature".into(),
                into: HeadBranch::Known("main".into()),
            }),
            PreviewAction::Start(GitOperation::MergeBranch {
                branch: BranchName::new("feature").expect("a valid branch name"),
            }),
        );
        assert_eq!(
            action(OperationKind::CherryPick {
                commit: OID.into(),
                onto: HeadBranch::Known("main".into()),
            }),
            PreviewAction::Start(GitOperation::CherryPick {
                commit: CommitOid::new(OID).expect("a valid oid"),
            }),
            "a cherry-pick must reach a CherryPick preview — Revert carries the \
             same single commit id and is the exact inverse"
        );
        assert_eq!(
            action(OperationKind::Undo(undoable(UndoAction::RevertCommit {
                commit: OID.into()
            }))),
            PreviewAction::Start(GitOperation::RevertCommit {
                commit: CommitOid::new(OID).expect("a valid oid"),
            }),
        );
        // And an undo that moves a ref reaches the same `Undo` arm as the one
        // that IS previewable, so it is the case most likely to leak a picture
        // of the wrong operation.
        assert_eq!(
            action(OperationKind::Undo(undoable(UndoAction::ResetBranch {
                branch: "main".into(),
                to: OID.into(),
                expected_tip: OID.into(),
            }))),
            PreviewAction::Clear,
        );
        assert_eq!(
            action(OperationKind::Checkout {
                branch: "main".into(),
                current: None,
                elsewhere: crate::features::operations::kind::CheckoutElsewhere::Free,
            }),
            PreviewAction::Clear,
        );
    }

    #[test]
    fn the_confirm_dialog_routes_its_preview_through_core() {
        let body = preview_effect_body();
        assert!(
            body.contains("preview_action("),
            "the confirm dialog no longer calls `preview_action`, so every test \
             above proves a rule nothing uses. Effect body was:\n{body}"
        );
        assert!(
            !body.contains("previewable("),
            "the confirm dialog calls `previewable` directly again. That is the \
             composition #594's mutation proof could not see, back in a wasm-only \
             file. Effect body was:\n{body}"
        );
        assert_eq!(
            CONFIRM.matches("preview.start(").count(),
            1,
            "`preview.start` is reachable from more than one place in confirm.rs; \
             only the `PreviewAction::Start` arm may ask for a picture"
        );
        assert_eq!(
            CONFIRM.matches("preview.clear(").count(),
            1,
            "`preview.clear` is reachable from more than one place in confirm.rs; \
             only the `PreviewAction::Clear` arm may invalidate one"
        );
    }

    #[test]
    fn the_confirm_dialog_does_not_have_the_two_arms_the_wrong_way_round() {
        // The mutation this whole slice exists to make catchable. Both method
        // names appear in the file either way, so counting them proves nothing
        // about which arm reaches which — pair each arm with the first
        // `preview.` call that follows it, in source order.
        let body = preview_effect_body();
        for (arm, expected) in [
            ("PreviewAction::Start", "preview.start("),
            ("PreviewAction::Clear", "preview.clear("),
        ] {
            let at = body
                .find(arm)
                .unwrap_or_else(|| panic!("confirm.rs no longer matches on `{arm}`:\n{body}"));
            let rest = &body[at + arm.len()..];
            let call = rest
                .find("preview.")
                .unwrap_or_else(|| panic!("the `{arm}` arm calls nothing on the preview slot"));
            assert!(
                rest[call..].starts_with(expected),
                "the `{arm}` arm reaches `{}` and not `{expected}` — the two arms \
                 are swapped, which draws the last dialog's picture over this one \
                 (or draws none at all for a dialog that has one)",
                &rest[call..rest[call..].find('(').map_or(rest.len(), |o| call + o + 1)],
            );
        }
    }
}

#[cfg(test)]
#[path = "core_suite.rs"]
mod core_suite;
