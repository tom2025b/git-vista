//! The closed set of git operations the frontend can ask the server to perform.
//!
//! Moved verbatim from `state.rs`'s `PendingOp` (M1.11, #64, decision D4). The variants
//! already map 1:1 onto `api.rs` functions — `dialogs/confirm.rs` has one match arm each —
//! so this is a re-home and a rename, not a redesign.
//!
//! Framework-free by construction: the only payloads are `String`, `Option<String>` and
//! `git_vista_core::activity::Undoable`, so the operations core is host-testable.
//!
//! `Debug`/`PartialEq`/`Eq` are new (the old `PendingOp` derived only `Clone`). The core
//! needs equality to enforce ADR 0020's rule that one idempotency key may not be rebound to
//! a *different* operation, and needs `Debug` for the assertions that prove it.

use git_vista_core::activity::Undoable;
use git_vista_protocol::MergeStrategy;

/// A branch operation awaiting confirmation in the modal (Issue #33 follow-up).
/// Merge and delete change history/refs and push reaches the network, so each is
/// confirmed before it runs — reusing the same in-app modal the commit dialog uses
/// (a native `confirm()` gets blocked/flashed by the webview, same as `prompt()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationKind {
    /// Merge `branch` into the checked-out branch (`git merge <branch>`). `into` is
    /// the live HEAD branch, fetched when the item is clicked, so the confirmation
    /// names the true target; `None` => detached HEAD (the confirm button is disabled).
    Merge {
        branch: String,
        into: Option<String>,
    },
    /// Push `branch` to origin (`git push origin <branch>`).
    Push { branch: String },
    /// Delete `branch` (`git branch -d <branch>`). `current` is the live HEAD branch,
    /// fetched on click; when it equals `branch` the confirm button is disabled (git
    /// refuses to delete the checked-out branch). `None` => detached HEAD (deletable).
    Delete {
        branch: String,
        current: Option<String>,
    },
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
}

impl OperationKind {
    /// A short human phrase naming what this operation does, for the status strip that
    /// reports it. Lives beside the vocabulary so a new variant cannot be added without
    /// deciding how it reads.
    pub fn describe(&self) -> String {
        match self {
            Self::Merge { branch, .. } => format!("Merging \u{2018}{branch}\u{2019}"),
            Self::Push { branch } => format!("Pushing \u{2018}{branch}\u{2019}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_describes_itself_without_naming_its_enum() {
        // A status strip reading "ForceDelete" would be leaking the type name at the user.
        let kinds = [
            OperationKind::Merge {
                branch: "feature".into(),
                into: Some("main".into()),
            },
            OperationKind::Push {
                branch: "feature".into(),
            },
            OperationKind::Delete {
                branch: "feature".into(),
                current: None,
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
                into: Some("main".into()),
            },
            // Server-honoured (`honours_cancellation` says `true` for
            // `PushBranch`), client-narrower-by-choice — see the doc
            // comment on `is_cancellable` above.
            OperationKind::Push {
                branch: "feature".into(),
            },
            OperationKind::Delete {
                branch: "feature".into(),
                current: None,
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
