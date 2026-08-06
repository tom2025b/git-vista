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
}
