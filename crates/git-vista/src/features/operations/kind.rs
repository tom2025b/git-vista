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
        }
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
        ];
        for k in kinds {
            let text = k.describe();
            assert!(!text.is_empty());
            assert!(
                !text.contains("OperationKind") && !text.contains("ForceDelete"),
                "leaked a type name: {text}"
            );
        }
    }
}
