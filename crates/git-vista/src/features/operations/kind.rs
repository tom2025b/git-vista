//! The closed set of git operations the frontend can ask the server to perform.
//!
//! Moved verbatim from `state.rs`'s `PendingOp` (M1.11, #64, decision D4). The variants
//! already map 1:1 onto `api.rs` functions — `dialogs/confirm.rs` has one match arm each —
//! so this is a re-home and a rename, not a redesign.
//!
//! Framework-free by construction: the payloads are `String`, `Option<String>`, `bool`,
//! `git_vista_core::activity::Undoable`, [`HeadBranch`], [`CheckoutElsewhere`], and
//! (since #233's `ForceWithLease`) `git_vista_protocol`'s own validated
//! `CommitOid`/`RiskLevel`/`Serviceable` — none of them tied to a UI framework, so the
//! operations core stays host-testable.
//!
//! `Debug`/`PartialEq`/`Eq` are new (the old `PendingOp` derived only `Clone`). The core
//! needs equality to enforce ADR 0020's rule that one idempotency key may not be rebound to
//! a *different* operation, and needs `Debug` for the assertions that prove it.

use git_vista_core::activity::Undoable;
use git_vista_protocol::plan::Advisory;
use git_vista_protocol::{
    branch_holder, BranchHolder, BranchName, CommitOid, Explanation, MergeStrategy, Plan,
    RiskLevel, Serviceable, WorktreeCensus,
};

use crate::features::freshness::core::PlanOnScreen;
use crate::features::graph::core::short_oid;

/// A branch operation awaiting confirmation in the modal (Issue #33 follow-up).
/// Merge and delete change history/refs and push reaches the network, so each is
/// confirmed before it runs — reusing the same in-app modal the commit dialog uses
/// (a native `confirm()` gets blocked/flashed by the webview, same as `prompt()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationKind {
    /// Merge `branch` into the checked-out branch (`git merge <branch>`). `into` is
    /// the live HEAD answer, fetched when the item is clicked, so the confirmation
    /// names the true target. [`HeadBranch::Detached`] and [`HeadBranch::Unknown`]
    /// both disable the confirm button, with copy that says which one happened —
    /// a failed read is not evidence of a detached HEAD, and the dialog must not
    /// claim it is.
    Merge { branch: String, into: HeadBranch },
    /// Push `branch` to origin (`git push origin <branch>`), optionally
    /// recording it as the upstream and/or forcing it with a reviewed lease
    /// (M2.20g, #233 — widened from the bare `{ branch }` M1 shape).
    ///
    /// A plain push (`force: None`) keeps the single-tap confirmation this
    /// operation has always had. `force: Some(_)` is reached only through
    /// the menu's separate force-push entry point — never from the normal
    /// one-tap Push button, per #233's acceptance criterion — after that
    /// entry point has already reviewed a `POST /api/plan` response for the
    /// lease; see [`ForceWithLease`]'s doc comment for why the values it
    /// carries are read off that response rather than assumed here.
    Push {
        branch: String,
        /// `--set-upstream`, offered — never auto-applied — when
        /// `/api/rebase-status` reported the checked-out branch has none.
        /// Always `false` when `branch` isn't the branch currently checked
        /// out: there is no live upstream read for an arbitrary branch yet
        /// (`RebaseStatus::has_upstream` only ever answers for HEAD).
        set_upstream: bool,
        /// `Some` for a force-with-lease publish under review.
        force: Option<ForceWithLease>,
    },
    /// Delete `branch` (`git branch -d <branch>`). `current` is the live HEAD answer,
    /// fetched on click: [`HeadBranch::Known`] equal to `branch` disables the confirm
    /// button (git refuses to delete the checked-out branch), [`HeadBranch::Detached`]
    /// leaves it deletable — and [`HeadBranch::Unknown`] disables it too, because a
    /// delete offered on a failed read might be a delete of the branch you're on.
    /// "Couldn't tell" is never "safe to offer" (the same rule the server states in
    /// `activity.rs`).
    Delete { branch: String, current: HeadBranch },
    /// Check out `branch` (`git checkout <branch>`), moving HEAD and the working
    /// tree to it. `current` is the live HEAD branch, fetched on click; when it
    /// equals `branch` the confirm button is disabled (nothing to switch to).
    /// `None` => detached HEAD — checkout is *allowed* there, it re-attaches HEAD.
    ///
    /// `elsewhere` is the worktree census's answer for this branch, fetched on
    /// the same click (M11.02, #547). Git refuses a branch that is live in
    /// another linked worktree, so a checkout offered without asking is one
    /// offered on a check that was never made — and the dialog needs the
    /// answer to name the worktree and offer to open it instead.
    Checkout {
        branch: String,
        current: Option<String>,
        elsewhere: CheckoutElsewhere,
    },
    /// Force-delete `branch` (`git branch -D <branch>`), discarding unmerged commits.
    /// Only reached after the safe [`OperationKind::Delete`] is refused with "not fully
    /// merged": the modal re-opens as this so the user can override rather than hit a
    /// dead-end error.
    ForceDelete { branch: String },
    /// Rebase the checked-out branch onto main (`git rebase main`, or `origin/main`
    /// when that remote-tracking ref exists — resolved server-side). `current` is the
    /// live HEAD branch, fetched on click, purely to name it in the dialog; `None` =>
    /// detached HEAD (the confirm button is disabled — there's no branch to rebase).
    /// `base` names the server's actual rebase target (from `/api/rebase-status`),
    /// so the dialog says exactly what the branch will be replayed onto.
    Rebase {
        current: Option<String>,
        base: String,
    },
    /// Fetch from `remote` (`git fetch <remote>`, M2.20f/#232) — updates the
    /// remote-tracking refs; moves nothing local. Repo-scoped like
    /// [`Self::Rebase`] above, not per-branch like [`Self::Push`].
    Fetch { remote: String },
    /// Pull `branch` from `remote` into the checked-out branch, integrating
    /// with `strategy` (`git fetch` + `git merge`/`git rebase`, M2.20f/#232).
    ///
    /// `strategy` has no default anywhere in this vocabulary (ADR 0044): the
    /// picker that builds this variant cannot construct it before the user
    /// has chosen Merge or Rebase, so there is no "unset" value this field
    /// could ever silently carry — the type itself rules it out, the same
    /// discipline the wire `PullRequest` already enforces one layer further
    /// out (an omitted `strategy` field there is a deserialize error, never
    /// a fallback).
    Pull {
        remote: String,
        branch: String,
        strategy: MergeStrategy,
    },
    /// Execute one undo action (Activity/Undo step 5, `POST /api/undo`). Carries the
    /// whole [`Undoable`] — the action plus its server-built label and `warn_pushed`
    /// flag — so the dialog can name exactly what it's about to do and warn when the
    /// discarded state is already on the remote. Offered from the graph menu's undo
    /// section (`/api/undoables`) and straight from Activity feed rows.
    Undo(Undoable),
    /// `git checkout -- <paths>` on named working-tree paths
    /// (`POST /api/discard-tracked-paths`, M2.18a/#219 backend, M2.18b/#220
    /// UI). `paths` are worktree-relative and must every one be *tracked and
    /// dirty* — the server re-derives that classification from a fresh
    /// `git status` and refuses the whole batch if any path disagrees, so
    /// the frontend builds this list with
    /// `features::status::core::discardable_tracked_paths` rather than by
    /// hand.
    ///
    /// Named for the server's own `GitOperation::DiscardTrackedPaths` rather
    /// than #220's shorter suggestion, so the two halves of one operation
    /// are greppable as one thing.
    DiscardTrackedPaths { paths: Vec<String> },
    /// `git clean -f -- <paths>` (`POST /api/delete-untracked-paths`).
    ///
    /// **The one operation in this vocabulary with no way back.** The content
    /// was never in git's object database, so nothing in the repository, the
    /// reflog or this app's journal can produce it again. That is why
    /// `dialogs/core.rs`'s confirmation for this demands a second deliberate
    /// tap, and why nothing in its user-facing copy — or in `describe`
    /// below — says "undo", "restore" or "recover".
    DeleteUntrackedPaths { paths: Vec<String> },
    /// Cherry-pick `commit` onto the checked-out branch (`git cherry-pick
    /// <commit>`, M10.09/#596, `POST /api/cherry-pick`).
    ///
    /// `onto` is the live HEAD answer, fetched when the menu item is clicked,
    /// exactly as [`Self::Merge`]'s `into` is — a pick lands on whatever branch
    /// is checked out, never on the row that was tapped, so the confirmation
    /// has to name the real destination rather than the graph's idea of it.
    /// [`HeadBranch::Detached`] and [`HeadBranch::Unknown`] both disable the
    /// confirm button with copy that says which one happened; a detached HEAD
    /// would leave the picked commit unreferenced, and a failed read is not
    /// evidence of anything.
    ///
    /// `commit` is the full hex id of the tapped row. Ordinary commits only —
    /// `GitOperation::CherryPickMerge` is a separate operation needing a
    /// mainline, and has no route yet.
    CherryPick { commit: String, onto: HeadBranch },
    /// Delete the local tag `tag` (`git tag -d <tag>`, M2.21d/#238, `POST
    /// /api/delete-tag`). Local only — deleting a tag already pushed to a
    /// remote is a separate operation with its own route still to come
    /// (#74), because it opens a socket with credentials on it. Named for
    /// the server's own `GitOperation::DeleteLocalTag`, the same naming
    /// rule `DiscardTrackedPaths`/`DeleteUntrackedPaths` above follow.
    DeleteLocalTag { tag: String },
    /// `git worktree remove <path>` on a linked sibling (`POST
    /// /api/remove-worktree`, M11.05/#550). `id` is the opaque census id and
    /// the mutation authority — the server resolves it to a real path via a
    /// fresh census immediately before acting, so this carries no path at
    /// all. `name` is the census's own short display label, carried only for
    /// the confirmation's wording and the status strip; it plays no part in
    /// which desk actually closes.
    ///
    /// **The one operation in this vocabulary with no way back of any kind**
    /// — an uncommitted, never-staged edit in the removed worktree was never
    /// written to git's object database, so nothing here or on the server can
    /// ever reconstruct it. Same class as [`Self::DeleteUntrackedPaths`]
    /// above, and the two-tap ceremony `dialogs/core.rs`'s
    /// `remove_worktree_confirm` gives it is that operation's, not the
    /// single-tap ceremony the rest of this enum gets.
    RemoveWorktree { id: String, name: String },
}

/// A force-with-lease push under review (#233): what the danger-styled
/// confirmation names and what the request carries once confirmed.
///
/// Both fields come from a `POST /api/plan` response the menu's force-push
/// entry point already fetched before opening the modal — `expected_remote_tip`
/// is `Plan::expected_ref_changes[0]`'s `before` oid (the value
/// `--force-with-lease=<branch>:<oid>` pins the remote to, and the same oid
/// the confirmation names as "what this will overwrite"), and `risk` is
/// `Plan::risk` for that exact plan. Neither is derived here or in
/// `dialogs/confirm.rs` by mirroring `planner.rs`'s
/// `ForcePublish::WithLease => RiskLevel::Destructive` match — carrying
/// `risk` as a field, read off the server's own answer, is what keeps the
/// confirmation from drifting out of sync with the planner's actual
/// classification if that match is ever widened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForceWithLease {
    pub expected_remote_tip: CommitOid,
    pub risk: RiskLevel,
    /// The planner's own advisories for this exact lease plan (M4.32, #85).
    ///
    /// Carried rather than recomputed: `advisories_for` reads the remote's
    /// default branch off the repository, which this client cannot see. The
    /// server already answered the question; re-deriving it here could only
    /// disagree.
    pub advisories: Vec<Advisory>,
    /// Explain Mode's typed explanation of **this exact plan** (M6.39b, #545).
    ///
    /// Carried for the same reason `advisories` is, and it is the same plan:
    /// the menu already fetches a leased preview to read `risk`, so the rest
    /// of that plan was being read and thrown away. Taking the explanation
    /// off it costs no extra round trip and — more to the point — guarantees
    /// the panel explains the plan whose risk the button is coloured by,
    /// rather than a second plan built a moment later that could differ.
    ///
    /// Stored as the protocol's typed [`Explanation`], not as rendered text:
    /// the words are `features::explain::core`'s job and live in the view,
    /// which is what keeps every sentence in one replaceable place.
    pub explanation: Explanation,
    /// The generation **that same plan** was built against, and the refs it
    /// says it will move (M12.05, #555; #664 review, finding 7).
    ///
    /// Carried for the third time for the same reason `risk`, `advisories` and
    /// `explanation` are: this confirmation displays a server-built plan, and
    /// everything the dialog says about it must come off *that* plan rather
    /// than a second one. Without it the force-with-lease confirmation had no
    /// plan the freshness check could see — `preview_subject(Push)` is
    /// `NotPreviewable`, so `preview.plan()` is `None` here — and the button
    /// stayed enabled with no notice after the repository moved. That is
    /// precisely the plan-backed force-push case #555 exists for.
    pub plan: PlanOnScreen,
}

impl ForceWithLease {
    /// Everything this confirmation shows about a force-with-lease push, taken
    /// off the leased plan the server just built.
    ///
    /// One constructor, called by the menu that opens the confirmation and by
    /// the Rebuild that replaces it (#664 review, defect 2). Five values must
    /// come off the *same* plan — the risk that colours the button, the
    /// advisories, the explanation, the lease oid the button will send, and the
    /// generation the freshness check compares — and the way they stop coming
    /// off the same plan is two call sites assembling them separately.
    ///
    /// `expected_remote_tip` is passed in rather than re-read from `plan`
    /// because it is the oid the *plain* probe established and the leased plan
    /// was then built against: taking it from the leased plan's own
    /// `expected_ref_changes` would be reading back the value we just sent.
    pub fn from_leased_plan(plan: &Plan, expected_remote_tip: CommitOid) -> Self {
        Self {
            expected_remote_tip,
            risk: plan.risk,
            advisories: plan.advisories.clone(),
            explanation: git_vista_protocol::explain(plan),
            plan: PlanOnScreen::of(plan),
        }
    }
}

/// The live "which branch is checked out?" answer a menu pre-check hands the
/// confirm dialog — with the read's own failure kept as its own state.
///
/// `api::fetch_head_branch` distinguishes three outcomes: `Ok(Some(_))` (HEAD
/// is on a branch), `Ok(None)` (detached HEAD), and `Err(_)` (the read itself
/// failed — transport or JSON, per its doc comment). The menu pre-checks used
/// to fold that `Err` into detached with `unwrap_or(None)`, and for a delete
/// "detached" is the *enabling* answer — so a dead server made the confirm
/// dialog offer a branch delete with a confident caption when the honest
/// answer was "couldn't tell". This enum keeps the third state distinct all
/// the way into `dialogs/confirm.rs`, bringing the frontend to the standard
/// the server states in `activity.rs`: "couldn't tell" must never read as
/// "safe to offer".
///
/// Framework-free like everything else in this module, so the classification
/// and the prompts built from it are host-testable facts, not wasm-only ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadBranch {
    /// HEAD is checked out on this branch.
    Known(String),
    /// The read succeeded and said detached HEAD — a real answer, not a guess.
    Detached,
    /// The read failed. Carries the transport/JSON error text so the dialog
    /// can say *why* it refuses, instead of a bare "no".
    Unknown(String),
}

impl HeadBranch {
    /// Classify `api::fetch_head_branch`'s exact return shape — the one place
    /// its `Result` is allowed to disappear. `Err` becomes [`Self::Unknown`],
    /// never [`Self::Detached`]: collapsing the two is the defect this type
    /// exists to make unrepresentable.
    pub fn classify(fetched: Result<Option<String>, String>) -> Self {
        match fetched {
            Ok(Some(branch)) => Self::Known(branch),
            Ok(None) => Self::Detached,
            Err(err) => Self::Unknown(err),
        }
    }
}

/// One linked worktree that has a branch checked out (M11.02, #547).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldingWorktree {
    /// The opaque worktree id — what `POST /api/select` takes, and the reason
    /// "open that one instead" can be a button rather than an instruction.
    pub id: String,
    /// The short, non-path display label the census carries.
    pub name: String,
    /// Whether this application may open it. Kept as the census's own
    /// three-state answer rather than flattened to a `bool`, because the two
    /// refusals read differently to a user: a worktree outside the allowed
    /// roots is somewhere they can go in a terminal, and a missing one is
    /// somewhere nobody can go until `git worktree prune` runs.
    pub serviceable: Serviceable,
}

impl HoldingWorktree {
    /// Whether this application may open this worktree — the one place the
    /// three-state answer becomes the yes/no the button needs.
    pub fn is_openable(&self) -> bool {
        matches!(self.serviceable, Serviceable::Yes)
    }
}

/// What the worktree census said about a branch when the user clicked
/// Checkout (M11.02, #547) — the frontend's half of
/// `Precondition::BranchFreeInEveryOtherWorktree`.
///
/// Three states, and the third is the one that matters: a census that could
/// not be read establishes nothing, and folding it into [`Self::Free`] would
/// offer a checkout on a check that never ran. Exactly [`HeadBranch`]'s
/// discipline one type over, for exactly its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutElsewhere {
    /// The census was read and no other worktree has this branch.
    Free,
    /// Another worktree has it. Git will refuse the checkout.
    HeldBy(HoldingWorktree),
    /// The census could not be read — the request failed, or the server
    /// answered `CensusFailed`. Carries the reason so the dialog can say
    /// *why* it declines rather than a bare "no".
    Unknown(String),
}

impl CheckoutElsewhere {
    /// Classify `api::fetch_worktree_census`'s exact return shape for
    /// `branch` — the one place its `Result` is allowed to disappear.
    ///
    /// Two different failures collapse into [`Self::Unknown`] and neither may
    /// become [`Self::Free`]: a transport/JSON `Err` (this client could not
    /// reach the server) and a `WorktreeCensus::CensusFailed` (the server
    /// could not read the list). A third would-be failure is ruled out by
    /// construction: an `Observed` census with an empty sibling list cannot
    /// occur — the server refuses to report one, since every repository has
    /// at least its own worktree — but even if it did, it would correctly
    /// mean "nobody else holds this branch".
    pub fn classify(fetched: Result<WorktreeCensus, String>, branch: &str) -> Self {
        let census = match fetched {
            Ok(census) => census,
            Err(err) => return Self::Unknown(err),
        };
        let Ok(name) = BranchName::new(branch) else {
            // A branch name the protocol will not accept cannot be looked up,
            // and "could not look it up" is this variant's whole meaning.
            return Self::Unknown(format!("‘{branch}’ is not a name git-vista can look up"));
        };
        match branch_holder(&census, &name) {
            BranchHolder::Free => Self::Free,
            BranchHolder::HeldBy(sibling) => Self::HeldBy(HoldingWorktree {
                id: sibling.id.clone(),
                name: sibling.name.clone(),
                serviceable: sibling.serviceable.clone(),
            }),
            BranchHolder::Unknown(reason) => Self::Unknown(reason.to_string()),
        }
    }
}

impl OperationKind {
    /// A short human phrase naming what this operation does, for the status strip that
    /// reports it. Lives beside the vocabulary so a new variant cannot be added without
    /// deciding how it reads.
    pub fn describe(&self) -> String {
        match self {
            Self::Merge { branch, .. } => format!("Merging \u{2018}{branch}\u{2019}"),
            Self::Push {
                branch,
                force: Some(_),
                ..
            } => format!("Force-pushing \u{2018}{branch}\u{2019}"),
            Self::Push { branch, .. } => format!("Pushing \u{2018}{branch}\u{2019}"),
            Self::Delete { branch, .. } => format!("Deleting \u{2018}{branch}\u{2019}"),
            Self::ForceDelete { branch } => format!("Force-deleting \u{2018}{branch}\u{2019}"),
            Self::Checkout { branch, .. } => format!("Checking out \u{2018}{branch}\u{2019}"),
            Self::Rebase { base, .. } => format!("Rebasing onto {base}"),
            Self::Fetch { remote } => format!("Fetching from \u{2018}{remote}\u{2019}"),
            Self::Pull {
                remote,
                branch,
                strategy,
            } => {
                let verb = match strategy {
                    MergeStrategy::Merge => "merge",
                    MergeStrategy::Rebase => "rebase",
                };
                format!(
                    "Pulling \u{2018}{branch}\u{2019} from \u{2018}{remote}\u{2019} ({verb} strategy)"
                )
            }
            // Names the commit, not the destination: the destination is
            // wherever HEAD is, which the confirmation already stated and the
            // strip has no room to repeat. Short id — the strip is one line.
            Self::CherryPick { commit, .. } => {
                format!("Cherry-picking {}", short_oid(commit))
            }
            Self::Undo(u) => format!("Undoing: {}", u.label),
            Self::DiscardTrackedPaths { paths } => {
                format!("Discarding changes to {}", file_count(paths.len()))
            }
            // "permanently" carries the same load here as it does in the
            // server's own journal line: this strip is the only trace of the
            // operation the user sees once the modal closes.
            Self::DeleteUntrackedPaths { paths } => {
                format!("Deleting {} permanently", file_count(paths.len()))
            }
            // Mirrors the branch `Delete`/`ForceDelete` arms' wording —
            // named subject, no type-name leak.
            Self::DeleteLocalTag { tag } => format!("Deleting tag \u{2018}{tag}\u{2019}"),
            // Names the desk by its display label, never its id — the
            // strip is for a person, and an id is not a name.
            Self::RemoveWorktree { name, .. } => {
                format!("Removing worktree \u{2018}{name}\u{2019}")
            }
        }
    }

    /// Whether the client offers a Cancel button for this operation.
    ///
    /// This is **not** a full mirror of the server's
    /// `planner::honours_cancellation` (`crates/git-vista-server/src/planner.rs`,
    /// checked at ~4294) — it used to claim to be, and that claim was false.
    /// The server watches the cancellation latch for `FetchRemote`,
    /// `PullBranch`, *and* `PushBranch`; the client here only offers Cancel
    /// for `Fetch` and `Pull`. That gap is a deliberate scope decision, not
    /// a bug the way the false doc comment implied: #232/M2.20f's whole
    /// surface — the progress strip, the resume-across-reload plumbing in
    /// `prefs.rs`, this menu's disabled-while-in-flight gate — was scoped to
    /// Fetch/Pull throughout, and offering Cancel for Push is real UI work
    /// (a new client-side button plus surfacing the server's own weaker
    /// promise for it — see `honours_cancellation`'s doc comment on what
    /// "cancelled" even means for a push whose effect already landed on the
    /// remote) that #232 never asked for. If a future issue extends
    /// cancellation to Push, add `Self::Push { .. }` here and update the
    /// test below to expect it — the two-crate cross-reference stays
    /// (there's still no shared type), but this comment now says what the
    /// client actually does rather than what it claims to.
    pub fn is_cancellable(&self) -> bool {
        matches!(self, Self::Fetch { .. } | Self::Pull { .. })
    }
}

/// `"1 file"` / `"3 files"` — the pluralisation both worktree arms need.
fn file_count(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// The per-commit menu's "Rebase" item label (Issue #328).
///
/// Every other item in that menu — Merge, Checkout, Undo — acts on the node
/// the user clicked. Rebase never has: it always replays the *checked-out*
/// branch onto `base` (see [`OperationKind::Rebase`]'s doc comment), which
/// the confirm dialog already states correctly ("Rebase 'main' onto
/// origin/main?", `dialogs/confirm.rs`). The bug wasn't the rebase itself —
/// it was that the menu item's own label, built before the confirm dialog
/// ever opens, read as plain "Rebase onto {base}" with no subject at all, so
/// a user who clicked a specific commit had no way to tell — before
/// clicking — that the item would ignore that commit entirely. This
/// restates the same subject the confirm dialog names, one step earlier, so
/// the scope is visible before the click rather than only after it.
///
/// `branch` is `None` for two situations the caller can't yet distinguish
/// at label-build time: the live `/api/rebase-status` fetch hasn't resolved
/// yet (menu.rs's `rebase_status` resource starts `None` and the item stays
/// enabled while loading), or HEAD is detached (a case menu.rs disables the
/// item for separately, once the fetch *has* resolved). Either way "the
/// current branch" is the honest word for the label to use: it doesn't
/// invent a name it doesn't have, and it still states scope up front instead
/// of implying "this commit" the way the bare `Some(base)`-only label did.
pub fn rebase_item_label(branch: Option<&str>, base: &str) -> String {
    match branch {
        Some(b) => format!("Rebase \u{2018}{b}\u{2019} onto {base}"),
        None => format!("Rebase current branch onto {base}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(c: char) -> CommitOid {
        CommitOid::new(c.to_string().repeat(40)).unwrap()
    }

    /// An explanation with no sections, for the tests in this module — which
    /// are about the operation's one-line **caption**, never about the panel.
    ///
    /// This is not a shape [`git_vista_protocol::explain`] produces: it always
    /// emits six sections, empty ones included. It exists so these assertions
    /// say plainly that the explanation is not what they are measuring, rather
    /// than carrying a plausible-looking one that a later reader might mistake
    /// for coverage. The panel's own behaviour is pinned in
    /// `features::explain::core` and in the protocol crate's parity test.
    /// A plan-on-screen for the tests in this module, which are not about
    /// freshness — the same posture `caption_only_explanation` takes below.
    fn no_particular_plan() -> PlanOnScreen {
        PlanOnScreen {
            generation: "1".to_string(),
            expects: Vec::new(),
            repository: "repo-1".to_string(),
            worktree: "wt-1".to_string(),
        }
    }

    fn caption_only_explanation() -> Explanation {
        Explanation {
            sections: Vec::new(),
        }
    }

    #[test]
    fn every_variant_describes_itself_without_naming_its_enum() {
        // A status strip reading "ForceDelete" would be leaking the type name at the user.
        let kinds = [
            OperationKind::Merge {
                branch: "feature".into(),
                into: HeadBranch::Known("main".into()),
            },
            OperationKind::Push {
                branch: "feature".into(),
                set_upstream: false,
                force: None,
            },
            OperationKind::Push {
                branch: "feature".into(),
                set_upstream: false,
                force: Some(ForceWithLease {
                    expected_remote_tip: oid('a'),
                    risk: RiskLevel::Destructive,
                    advisories: Vec::new(),
                    explanation: caption_only_explanation(),
                    plan: no_particular_plan(),
                }),
            },
            OperationKind::Delete {
                branch: "feature".into(),
                current: HeadBranch::Detached,
            },
            OperationKind::ForceDelete {
                branch: "feature".into(),
            },
            OperationKind::Checkout {
                branch: "feature".into(),
                current: None,
                elsewhere: CheckoutElsewhere::Free,
            },
            OperationKind::Rebase {
                current: None,
                base: "origin/main".into(),
            },
            OperationKind::Fetch {
                remote: "origin".into(),
            },
            OperationKind::Pull {
                remote: "origin".into(),
                branch: "main".into(),
                strategy: MergeStrategy::Merge,
            },
            OperationKind::DiscardTrackedPaths {
                paths: vec!["src/a.rs".into()],
            },
            OperationKind::DeleteUntrackedPaths {
                paths: vec!["scratch.txt".into(), "note.md".into()],
            },
            OperationKind::DeleteLocalTag { tag: "v1.0".into() },
            OperationKind::RemoveWorktree {
                id: "worktree-desk-two".into(),
                name: "desk-two".into(),
            },
        ];
        for k in kinds {
            let text = k.describe();
            assert!(!text.is_empty());
            assert!(
                !text.contains("OperationKind")
                    && !text.contains("ForceDelete")
                    && !text.contains("TrackedPaths"),
                "leaked a type name: {text}"
            );
        }
    }

    /// #233: the status strip must say which push actually ran — a
    /// force-with-lease and a plain fast-forward are different enough
    /// operations (one can discard a colleague's commits, the other never
    /// can) that collapsing them onto the same "Pushing '…'" line would
    /// hide the one distinction this strip exists to surface.
    #[test]
    fn describe_distinguishes_a_force_with_lease_push_from_a_plain_one() {
        let plain = OperationKind::Push {
            branch: "feature".into(),
            set_upstream: false,
            force: None,
        }
        .describe();
        let forced = OperationKind::Push {
            branch: "feature".into(),
            set_upstream: false,
            force: Some(ForceWithLease {
                expected_remote_tip: oid('a'),
                risk: RiskLevel::Destructive,
                advisories: Vec::new(),
                explanation: caption_only_explanation(),
                plan: no_particular_plan(),
            }),
        }
        .describe();
        assert_ne!(plain, forced);
        assert!(plain.contains("feature") && !plain.to_lowercase().contains("force"));
        assert!(forced.contains("feature") && forced.to_lowercase().contains("force"));
    }

    /// The status strip is the only place the delete is named once its modal
    /// has closed, so it holds the same line the confirmation and the
    /// server's own journal text do (M2.18b, #220).
    ///
    /// The paired assertion is the discard arm: its text is *allowed* those
    /// words and is checked to be free of them anyway for a different reason
    /// — proving the two arms produce genuinely different strings rather
    /// than one shared template that happens to avoid the words.
    #[test]
    fn the_delete_never_describes_itself_as_reversible() {
        let delete = OperationKind::DeleteUntrackedPaths {
            paths: vec!["scratch.txt".into()],
        }
        .describe();
        let lower = delete.to_lowercase();
        for word in ["undo", "restore", "recover"] {
            assert!(!lower.contains(word), "found {word:?} in: {delete}");
        }
        assert!(lower.contains("permanently"), "{delete}");

        let discard = OperationKind::DiscardTrackedPaths {
            paths: vec!["scratch.txt".into()],
        }
        .describe();
        assert_ne!(
            discard, delete,
            "the two operations must not describe themselves identically"
        );
        assert!(!discard.to_lowercase().contains("permanently"), "{discard}");
    }

    #[test]
    fn a_single_file_is_not_described_as_files() {
        let one = OperationKind::DeleteUntrackedPaths {
            paths: vec!["a.txt".into()],
        }
        .describe();
        assert!(one.contains("1 file") && !one.contains("1 files"), "{one}");
        let two = OperationKind::DeleteUntrackedPaths {
            paths: vec!["a.txt".into(), "b.txt".into()],
        }
        .describe();
        assert!(two.contains("2 files"), "{two}");
    }

    /// Issue #328: the menu item must name its subject (the checked-out
    /// branch) before the click, matching what the confirm dialog already
    /// says afterward. A mutation back to the old `format!("Rebase onto
    /// {base}")` — dropping the branch name entirely — would leave this
    /// failing on the exact-equality check, not just a substring probe, so
    /// it can't survive by accident.
    #[test]
    fn rebase_label_names_the_checked_out_branch_before_the_click() {
        assert_eq!(
            rebase_item_label(Some("feature"), "origin/main"),
            "Rebase \u{2018}feature\u{2019} onto origin/main"
        );
    }

    /// Detached HEAD, or the live status fetch simply hasn't resolved yet —
    /// either way the label must still say the item acts on a branch, not on
    /// whatever commit was clicked. This is the case the original bug shipped
    /// in: `rebase_status` starts `None` on every menu open, so the very
    /// first render of this item always takes this branch of the label.
    #[test]
    fn rebase_label_states_scope_even_without_a_resolved_branch_name() {
        let label = rebase_item_label(None, "main");
        assert_eq!(label, "Rebase current branch onto main");
        assert!(
            label.contains("current branch"),
            "label must name its subject even when the branch name isn't \
             known yet: {label}"
        );
    }

    /// The two arms must actually differ — proves the branch argument is
    /// read, not a shared template that ignores it.
    #[test]
    fn rebase_label_known_and_unknown_branch_are_not_the_same_string() {
        let known = rebase_item_label(Some("main"), "origin/main");
        let unknown = rebase_item_label(None, "origin/main");
        assert_ne!(known, unknown);
    }

    /// The tag-delete confirmation strip names the tag, the same way the
    /// branch delete/undo arms name their subject — never a bare "Deleting".
    #[test]
    fn delete_local_tag_names_the_tag() {
        let text = OperationKind::DeleteLocalTag { tag: "v1.0".into() }.describe();
        assert_eq!(text, "Deleting tag \u{2018}v1.0\u{2019}");
    }
}

#[cfg(test)]
mod fetch_pull_tests {
    use super::*;

    fn fetch() -> OperationKind {
        OperationKind::Fetch {
            remote: "origin".into(),
        }
    }

    fn pull(strategy: MergeStrategy) -> OperationKind {
        OperationKind::Pull {
            remote: "origin".into(),
            branch: "main".into(),
            strategy,
        }
    }

    /// Mutation this catches: dropping `| Self::Pull { .. }` (or the whole
    /// `matches!` arm) from `is_cancellable`, or flipping it to `true` for
    /// everything.
    ///
    /// `Push` is asserted `false` here **deliberately**, not because it
    /// happens to fall out of the current match. The server's
    /// `planner::honours_cancellation` (`crates/git-vista-server/src/planner.rs`)
    /// says `true` for `PushBranch` — this test pins the client's narrower,
    /// intentional #232/M2.20f scope (Fetch/Pull only) against that wider
    /// server behaviour, so a future change that widens the client's set
    /// must edit this assertion on purpose rather than trip over it by
    /// accident. See `is_cancellable`'s doc comment for why Push isn't
    /// included yet.
    #[test]
    fn is_cancellable_is_true_only_for_fetch_and_pull() {
        assert!(fetch().is_cancellable());
        assert!(pull(MergeStrategy::Merge).is_cancellable());
        assert!(pull(MergeStrategy::Rebase).is_cancellable());

        let not_cancellable = [
            OperationKind::Merge {
                branch: "feature".into(),
                into: HeadBranch::Known("main".into()),
            },
            // Server-honoured (`honours_cancellation` says `true` for
            // `PushBranch`), client-narrower-by-choice — see the doc
            // comment on `is_cancellable` above.
            OperationKind::Push {
                branch: "feature".into(),
                set_upstream: false,
                force: None,
            },
            OperationKind::Delete {
                branch: "feature".into(),
                current: HeadBranch::Detached,
            },
            OperationKind::ForceDelete {
                branch: "feature".into(),
            },
            OperationKind::Checkout {
                branch: "feature".into(),
                current: None,
                elsewhere: CheckoutElsewhere::Free,
            },
            OperationKind::Rebase {
                current: None,
                base: "origin/main".into(),
            },
            OperationKind::DiscardTrackedPaths {
                paths: vec!["src/a.rs".into()],
            },
            OperationKind::DeleteUntrackedPaths {
                paths: vec!["scratch.txt".into()],
            },
            OperationKind::DeleteLocalTag { tag: "v1.0".into() },
            OperationKind::RemoveWorktree {
                id: "worktree-desk-two".into(),
                name: "desk-two".into(),
            },
        ];
        for k in not_cancellable {
            assert!(!k.is_cancellable(), "{k:?} should not be cancellable");
        }
    }

    /// Mutation this catches: describe()'s Pull arm hard-coding one verb
    /// (e.g. always "merge") instead of reading `strategy` — ADR 0044's
    /// no-default rule would then be silently undone in the one place a
    /// user actually sees the choice confirmed.
    #[test]
    fn pull_describes_the_strategy_it_will_actually_run() {
        let merge = pull(MergeStrategy::Merge).describe();
        let rebase = pull(MergeStrategy::Rebase).describe();
        assert_ne!(merge, rebase);
        assert!(merge.to_lowercase().contains("merge"));
        assert!(rebase.to_lowercase().contains("rebase"));
    }

    /// Mutation this catches: describe() interpolating the wrong field (e.g.
    /// swapping `remote`/`branch`, or a copy-pasted literal "origin").
    #[test]
    fn fetch_and_pull_name_the_remote_they_describe() {
        assert!(fetch().describe().contains("origin"));
        assert!(pull(MergeStrategy::Merge).describe().contains("origin"));
        assert!(pull(MergeStrategy::Merge).describe().contains("main"));
    }
}

#[cfg(test)]
mod head_branch_tests {
    use super::*;
    use git_vista_protocol::WorktreeSibling;

    #[test]
    fn a_named_branch_classifies_as_known() {
        assert_eq!(
            HeadBranch::classify(Ok(Some("main".into()))),
            HeadBranch::Known("main".into())
        );
    }

    #[test]
    fn a_successful_none_classifies_as_detached() {
        assert_eq!(HeadBranch::classify(Ok(None)), HeadBranch::Detached);
    }

    /// The defect this type exists for: the menu used to run
    /// `fetch_head_branch().await.unwrap_or(None)`, which made a transport
    /// failure indistinguishable from a real detached HEAD — and for a branch
    /// delete, "detached" is the answer that *enables* the button. A mutation
    /// of `classify`'s `Err` arm back to `Detached` must fail here, not
    /// survive into the dialog.
    #[test]
    fn a_failed_read_classifies_as_unknown_never_detached() {
        let classified = HeadBranch::classify(Err("connection refused".into()));
        assert_eq!(classified, HeadBranch::Unknown("connection refused".into()));
        assert_ne!(classified, HeadBranch::Detached);
    }

    /// The error text rides along verbatim — the dialog shows *why* the read
    /// failed, so a classify that drops or rewrites it loses the one clue the
    /// user gets.
    #[test]
    fn unknown_carries_the_original_error_text() {
        let HeadBranch::Unknown(err) = HeadBranch::classify(Err("HTTP 502".into())) else {
            panic!("an Err must classify as Unknown");
        };
        assert_eq!(err, "HTTP 502");
    }

    // -----------------------------------------------------------------
    // `CheckoutElsewhere::classify` (M11.02, #547)
    // -----------------------------------------------------------------

    fn sibling_on(branch: &str, name: &str, serviceable: Serviceable) -> WorktreeSibling {
        WorktreeSibling {
            repository: "repo-1".to_string(),
            id: format!("worktree-{name}"),
            name: name.to_string(),
            path: None,
            branch: Some(BranchName::new(branch).unwrap()),
            head: None,
            is_current: false,
            locked: false,
            prunable: false,
            bare: false,
            serviceable,
        }
    }

    fn census_with(siblings: Vec<WorktreeSibling>) -> Result<WorktreeCensus, String> {
        Ok(WorktreeCensus::Observed { siblings })
    }

    #[test]
    fn classify_reports_free_when_no_other_worktree_holds_the_branch() {
        let census = census_with(vec![sibling_on("feature/y", "desk-two", Serviceable::Yes)]);
        assert_eq!(
            CheckoutElsewhere::classify(census, "feature/x"),
            CheckoutElsewhere::Free
        );
    }

    #[test]
    fn classify_names_the_worktree_that_holds_the_branch() {
        let census = census_with(vec![sibling_on("feature/x", "desk-two", Serviceable::Yes)]);
        match CheckoutElsewhere::classify(census, "feature/x") {
            CheckoutElsewhere::HeldBy(w) => {
                assert_eq!(w.name, "desk-two");
                assert_eq!(w.id, "worktree-desk-two");
                assert!(w.is_openable());
            }
            other => panic!("expected the holder to be named, got {other:?}"),
        }
    }

    /// A holder the app may not open is still a holder — git's refusal does
    /// not consult this application's fence — but it is not openable.
    #[test]
    fn a_holder_outside_the_allowed_roots_is_reported_and_is_not_openable() {
        let census = census_with(vec![sibling_on(
            "feature/x",
            "outside",
            Serviceable::OutsideAllowedRoots,
        )]);
        match CheckoutElsewhere::classify(census, "feature/x") {
            CheckoutElsewhere::HeldBy(w) => assert!(!w.is_openable(), "{w:?}"),
            other => panic!("a fenced-off worktree still holds the branch, got {other:?}"),
        }
    }

    /// The transport half of the fail-open this type exists to close: the
    /// request never reached the server, so nothing is known.
    #[test]
    fn a_failed_census_fetch_never_becomes_free() {
        let answer = CheckoutElsewhere::classify(Err("network error".to_string()), "feature/x");
        assert!(
            matches!(&answer, CheckoutElsewhere::Unknown(why) if why.contains("network error")),
            "expected the transport failure to survive, got {answer:?}"
        );
        assert_ne!(answer, CheckoutElsewhere::Free);
    }

    /// The server half: it answered, and its answer was "I could not read the
    /// list". An empty sibling list would have meant "nobody holds it"; this
    /// does not, and the two must not converge here.
    #[test]
    fn a_census_failed_response_never_becomes_free() {
        let answer = CheckoutElsewhere::classify(
            Ok(WorktreeCensus::CensusFailed {
                detail: None,
                reason: "git worktree list exited 128".to_string(),
            }),
            "feature/x",
        );
        assert!(
            matches!(&answer, CheckoutElsewhere::Unknown(why) if why.contains("exited 128")),
            "expected the server's reason to survive, got {answer:?}"
        );
    }

    /// A name the protocol will not accept cannot be looked up, and "could not
    /// look it up" is `Unknown`'s whole meaning — never `Free`.
    #[test]
    fn a_branch_name_the_protocol_rejects_is_unknown_not_free() {
        let census = census_with(vec![sibling_on("feature/x", "desk-two", Serviceable::Yes)]);
        assert!(matches!(
            CheckoutElsewhere::classify(census, ""),
            CheckoutElsewhere::Unknown(_)
        ));
    }
}
