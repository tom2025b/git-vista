//! The closed set of git operations the frontend can ask the server to perform.
//!
//! Moved verbatim from `state.rs`'s `PendingOp` (M1.11, #64, decision D4). The variants
//! already map 1:1 onto `api.rs` functions — `dialogs/confirm.rs` has one match arm each —
//! so this is a re-home and a rename, not a redesign.
//!
//! Framework-free by construction: the payloads are `String`, `Option<String>`, `bool`,
//! `git_vista_core::activity::Undoable`, [`HeadBranch`], and (since #233's
//! `ForceWithLease`) `git_vista_protocol`'s own validated `CommitOid`/`RiskLevel` — none
//! of them tied to a UI framework, so the operations core stays host-testable.
//!
//! `Debug`/`PartialEq`/`Eq` are new (the old `PendingOp` derived only `Clone`). The core
//! needs equality to enforce ADR 0020's rule that one idempotency key may not be rebound to
//! a *different* operation, and needs `Debug` for the assertions that prove it.

use git_vista_core::activity::Undoable;
use git_vista_protocol::plan::Advisory;
use git_vista_protocol::{CommitOid, MergeStrategy, RiskLevel};

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
    Checkout {
        branch: String,
        current: Option<String>,
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
    /// Delete the local tag `tag` (`git tag -d <tag>`, M2.21d/#238, `POST
    /// /api/delete-tag`). Local only — deleting a tag already pushed to a
    /// remote is a separate operation with its own route still to come
    /// (#74), because it opens a socket with credentials on it. Named for
    /// the server's own `GitOperation::DeleteLocalTag`, the same naming
    /// rule `DiscardTrackedPaths`/`DeleteUntrackedPaths` above follow.
    DeleteLocalTag { tag: String },
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
}
