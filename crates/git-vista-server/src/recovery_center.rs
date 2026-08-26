//! The Recovery Center (M3.25, #78) — a browsable history of what *this app*
//! did, and, where a live check still says so, a button that puts it back.
//! Design: `docs/superpowers/specs/2026-08-18-m3-recovery-center.md`.
//!
//! Two surfaces, both loopback-only (ADR 0005 — they describe write outcomes):
//!
//!  * `GET /api/operations/history` — a non-mutating, keyset-paginated view
//!    over [`crate::durable`]'s `operations` table, one [`RecoveryClass`] per
//!    row, computed **live** on every load.
//!  * `POST /api/operations/{id}/recover` — re-derive that classification
//!    server-side, refuse unless it still says `Offered` *and* matches what
//!    the client claims to be running, then execute it through the ordinary
//!    write pipeline.
//!
//! ## The one rule the whole module exists to enforce
//!
//! **Never offer an undo you cannot stand behind.** A classification is never
//! served from the row's stored `recovery_json` (which records what the plan
//! decided at admission time, possibly months ago) — it is re-established
//! against the repository on every read. And a live check that could not *run*
//! ([`RecoveryClass::CheckFailed`]) is a distinct outcome from one that ran and
//! answered "no" ([`RecoveryClass::Expired`]); neither may ever be read as
//! "safe to offer". That three-way shape is not invented here: it mirrors
//! [`crate::activity::revert_offer_established`], whose own comment says
//! *"None of these is 'no conflict' — they're 'no fact', and a fact we don't
//! have is never grounds to offer"*, and
//! [`crate::activity::revert_would_conflict`]'s *"'couldn't tell' must never
//! read as 'safe to offer'"*.
//!
//! Only [`RecoveryClass::Offered`] carries an [`UndoAction`], and it carries it
//! *inside the variant* rather than as a shared field — so an exhaustive match
//! cannot construct or forward an undo for any other arm. That is a
//! compile-time property, enforced by the type checker at every construction
//! site, not a test that could rot.
//!
//! ## Two naming echoes, both deliberate
//!
//! `git_vista_protocol::history` already exports `HistoryFrame`/`HistoryPage`
//! for the unrelated M1.10 paged-commit-graph feature (#63), and
//! [`crate::history`] already has its own signed [`crate::history::CursorCodec`]
//! for that same walk. This module's `HistoryPage`/`HistoryCursor` are a
//! different domain in a different module and never share a scope with either.
//!
//! [`HistoryCursor`] is deliberately *not* [`crate::history::CursorCodec`]. That
//! codec's HMAC signing and generation pinning defend a **mutating**,
//! potentially cross-repository graph walk, where an unsigned cursor could
//! claim a scope it never held. Neither risk exists here: every query
//! re-applies `repository = ?1` itself regardless of what a cursor claims, and
//! a terminal `operations` row never changes after it is written, so there is
//! no live generation to pin against. What *is* borrowed is the discipline — a
//! length guard before any decode allocates, and a decode failure that always
//! reads as "this link is broken" (400), never as "you have reached the end"
//! (a dead end indistinguishable from real exhaustion) and never as a silent
//! restart at page 1 (an infinite scroll a browser-only user cannot escape).

use std::path::Path;
use std::str::FromStr;

use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use git_vista_core::activity::UndoAction;
use git_vista_core::identity::WorktreeId;
use git_vista_git::{read_commit, RepoError};
use git_vista_protocol::{
    BranchName, CommitOid, GitOperation, OperationId, OperationState, OperationStatus,
    RecoveryStrategy, RefName, RepositoryToken, TagName, UnixSeconds, WorktreeToken,
};

// ---------------------------------------------------------------------------
// What can be done about one past operation, right now
// ---------------------------------------------------------------------------

/// The live answer to "can this operation be undone, and how" — recomputed on
/// every read, never stored.
///
/// `undo` lives inside [`RecoveryClass::Offered`]'s own fields, not on a
/// struct-level shared field. Hoisting it out so another arm could carry one is
/// a shape change the compiler rejects at every construction site and every
/// match; that is why the spec files this as a compile-time invariant rather
/// than a mutation-provable one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "recovery_class", rename_all = "snake_case")]
pub(crate) enum RecoveryClass {
    /// `RecoveryStrategy::NotNeeded` — the operation destroyed nothing.
    NoneNeeded,
    /// The strategy's target is already in the state recovery would produce:
    /// somebody already reset the branch back, already deleted the branch this
    /// would delete, or ran the undo from another tab. Needed recovery once;
    /// nothing left to do.
    AlreadyCurrent,
    /// Live-established: the recovery pin resolves, the target ref is in the
    /// state the strategy expects, and — for `RevertCommit` — reverting would
    /// not conflict. `undo` is exactly what
    /// `POST /api/operations/{id}/recover` must receive back, unchanged, for
    /// its equality gate to pass.
    Offered {
        undo: UndoAction,
        label: String,
        warn_pushed: bool,
    },
    /// A live check ran and returned a definite negative. A fact, not a guess
    /// — this is "no", never "couldn't tell".
    Expired { reason: RecoveryExpiredReason },
    /// The live check itself could not run. "No fact", never "no".
    CheckFailed { detail: CheckFailedReason },
    /// A real recovery still exists — the live check established that — but no
    /// [`UndoAction`] variant can express it yet, so there is text to show and
    /// no button to bind. Reporting by the *live fact* rather than by "has a
    /// button" is the point of keeping this and `Expired` separate: a strategy
    /// whose live check finds the fact no longer holds is `Expired`, button or
    /// no button.
    KnownNotWired { strategy: RecoveryStrategy },
    /// No git-vista-driven undo is possible, and nothing here is "live" in the
    /// sense the other arms are — a property of the strategy (and, for
    /// `Irrecoverable`, of the specific operation), not of the current
    /// repository state.
    Unsupported { reason: UnsupportedReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryExpiredReason {
    /// `refs/git-vista/recovery/<id>` no longer resolves — gc (or something
    /// else) reclaimed it. The one case the design's environment-verified
    /// gc-expiry numbers bear on directly: `gc.reflogExpireUnreachable`
    /// defaults to 30 days, and the pin is the only thing standing between a
    /// `ResetRef`/`RecreateTag` offer and permanent loss.
    PinMissing,
    /// The pin resolved but the object it names could not be read back. Should
    /// be impossible — a resolving ref names a reachable object by definition
    /// — kept distinct rather than folded into `PinMissing` so a future reader
    /// investigating *this* reason knows the ref itself was fine.
    ObjectUnreachable,
    /// `ResetRef`/`CheckoutPrevious`'s named ref no longer exists — there is
    /// nothing for "move it back" to act on. Recreating it is a different (and
    /// for `CheckoutPrevious`, unwired) recovery, not this one.
    TargetRefGone,
    /// `RecreateBranch`/`RecreateTag`'s name now points at something else —
    /// recreating would clobber whatever is there now, and
    /// `UndoAction::RestoreBranch` is documented as "creates, never destroys".
    NameReused,
    /// `RevertCommit`'s target would produce a real merge conflict against the
    /// current `HEAD`, established exactly the way
    /// [`crate::activity::revert_would_conflict`] establishes it for the
    /// undoables menu. A definite negative, which is why it is `Expired` and
    /// not `CheckFailed`.
    WouldConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckFailedReason {
    /// A sandboxed git spawn — or the direct gix object read
    /// `crate::activity::undoables` already makes for the identical purpose —
    /// did not produce an answer.
    GitSpawnFailed,
    /// The row's `WorktreeToken` no longer resolves via
    /// [`crate::state::resolve_worktree`], which is `Catalog::resolve`
    /// fail-closed for any id it does not hold. A history row can outlive the
    /// registration of the repository it names — moved, deregistered, or
    /// simply not rescanned this session — and there is then no path to run
    /// git against at all. This arm is the reason such a row reads as "we
    /// could not check", never as "expired".
    RepositoryNotRegistered,
    /// A `ResetRef` row named a ref outside `refs/heads/`. Every producer in
    /// the planner builds `ref_name` from the checked-out branch, never a bare
    /// `HEAD` or a remote-tracking ref, so this should be unreachable; kept as
    /// a named, fail-closed arm rather than a `panic!`.
    UnrecognizedRefShape,
    /// The row carries no `RecoveryStrategy` at all — a row written before a
    /// plan existed, or one whose strategy did not decode. Nothing was
    /// established either way, so it is "no fact", not "no".
    NoStrategyRecorded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnsupportedReason {
    /// The effect left this machine: a branch or tag push, ordinary or
    /// lease-forced, or a remote tag delete. An undo never force-pushes, so
    /// there is nothing local left to move back.
    EffectLeftTheRepository,
    /// The discarded state was never journaled — `GitOperation::ResetTestRepo`
    /// wipes the journal as part of what it does.
    NeverJournaled,
    /// The discarded content was never in git's object database to begin with
    /// — `GitOperation::DeleteUntrackedPaths` (#219), the one case where
    /// "irrecoverable" is a fact about the repository rather than about what
    /// git-vista offers.
    NeverInObjectDatabase,
    /// `RecoverableIfStaged`, or a `RevertCommit` row naming a merge or root
    /// commit: there is no ref or commit to name even in principle (the staged
    /// case), or no single parent to diff a revert against (the merge/root
    /// case — the same exclusion `crate::activity::undoables` already applies).
    NoRecoverableHandle,
    /// `ConflictRecreatableWhileInProgress` (M4.31, #84): git *can* rebuild the
    /// conflict, but only while the operation that produced it is still in
    /// progress, and the recovery centre reads journal rows after the fact.
    ///
    /// Not `NoRecoverableHandle` — that means no ref or commit exists to name
    /// even in principle, which is a different and more final thing. Here a
    /// mechanism exists and this surface simply cannot confirm the window is
    /// still open: a journal row cannot say whether the same merge is still
    /// running. Offering the undo belongs to the live conflict view, which can
    /// check.
    OnlyWhileOperationInProgress,
}

// ---------------------------------------------------------------------------
// Live classification
// ---------------------------------------------------------------------------

/// Resolve a durable history row's tokens to a filesystem path through the
/// catalog, then classify — the full path from a stored row to a
/// [`RecoveryClass`], including the one gap neither source design named: the
/// catalog may no longer hold the id at all.
///
/// `repository` is accepted but not used to resolve the path:
/// [`crate::state::resolve_worktree`] is keyed by `WorktreeId` alone, the same
/// key `Catalog::resolve` takes. It is threaded through so a future
/// cross-repository scope has it in hand without a signature change; today it
/// is redundant with `worktree` because M3.25 is scoped to the currently
/// selected repository.
pub(crate) async fn classify_recovery_for_row(
    _repository: &RepositoryToken,
    worktree: &WorktreeToken,
    operation_id: &OperationId,
    operation: &GitOperation,
    strategy: Option<&RecoveryStrategy>,
) -> RecoveryClass {
    let Ok(worktree_id) = WorktreeId::from_str(worktree.as_str()) else {
        return RecoveryClass::CheckFailed {
            detail: CheckFailedReason::RepositoryNotRegistered,
        };
    };
    let Some((repo, _read_only, _handle)) = crate::state::resolve_worktree(worktree_id) else {
        return RecoveryClass::CheckFailed {
            detail: CheckFailedReason::RepositoryNotRegistered,
        };
    };
    classify_recovery(&repo, operation_id, operation, strategy).await
}

/// Live-verified against `repo` — `refs/git-vista/recovery/<id>` and, for
/// every strategy that names a target ref, that ref itself. Never reflog.
/// Called once per row on a page: bounded by the clamped page size, the same
/// cost shape as `undoables`'s one-precheck-per-menu-open.
///
/// Takes `operation` in addition to the design's own sketch (`strategy` alone)
/// because `RecoveryStrategy::Irrecoverable` is one variant shared by
/// structurally different reasons — a push already left the machine, a
/// test-repo reset wiped the journal, deleted untracked paths were never in
/// the object database — and [`UnsupportedReason`] has to say which. The bare
/// strategy cannot.
pub(crate) async fn classify_recovery(
    repo: &Path,
    operation_id: &OperationId,
    operation: &GitOperation,
    strategy: Option<&RecoveryStrategy>,
) -> RecoveryClass {
    let Some(strategy) = strategy else {
        return RecoveryClass::CheckFailed {
            detail: CheckFailedReason::NoStrategyRecorded,
        };
    };
    match strategy {
        RecoveryStrategy::NotNeeded => RecoveryClass::NoneNeeded,

        RecoveryStrategy::RecoverableIfStaged => RecoveryClass::Unsupported {
            reason: UnsupportedReason::NoRecoverableHandle,
        },

        RecoveryStrategy::ConflictRecreatableWhileInProgress => RecoveryClass::Unsupported {
            reason: UnsupportedReason::OnlyWhileOperationInProgress,
        },

        RecoveryStrategy::Irrecoverable => RecoveryClass::Unsupported {
            reason: irrecoverable_reason(operation),
        },

        // No `recovery_oid` is ever pinned for these three
        // (`durable::recovery_oid` returns `None`), so the only live fact
        // available is whether the named ref still exists — which is enough to
        // tell `AlreadyCurrent`/`Expired` (a definite negative) apart from
        // `KnownNotWired` (still real, still no button).
        RecoveryStrategy::DeleteCreatedBranch { name } => {
            match resolve_ref_exact(repo, &heads(name.as_str())).await {
                Err(_) => check_failed_spawn(),
                // Already gone — the delete this strategy would perform has
                // already happened, one way or another.
                Ok(None) => RecoveryClass::AlreadyCurrent,
                Ok(Some(_)) => RecoveryClass::KnownNotWired {
                    strategy: strategy.clone(),
                },
            }
        }

        // M3.24 (#77). The pin (`durable::recovery_oid`) keeps the stash
        // commit reachable, so the object is available — but a stash entry is
        // a REFLOG LINE, not a ref, and there is no ref to resolve to ask
        // "is it back?". `git stash store` would append a NEW entry at
        // stash@{0} rather than restoring the original position, so the
        // question "has this been recovered already?" has no honest live
        // answer: two identical entries are indistinguishable from one
        // recovered one.
        //
        // KnownNotWired is therefore the truthful class, not a placeholder —
        // the strategy is real, recorded and pinned, and the button is
        // deliberately absent until the wiring can say what it restores.
        RecoveryStrategy::RecreateStashEntry { .. } => RecoveryClass::KnownNotWired {
            strategy: strategy.clone(),
        },

        RecoveryStrategy::DeleteCreatedTag { name } => {
            match resolve_ref_exact(repo, &tags(name.as_str())).await {
                Err(_) => check_failed_spawn(),
                Ok(None) => RecoveryClass::AlreadyCurrent,
                Ok(Some(_)) => RecoveryClass::KnownNotWired {
                    strategy: strategy.clone(),
                },
            }
        }

        RecoveryStrategy::CheckoutPrevious { branch } => {
            match resolve_ref_exact(repo, &heads(branch.as_str())).await {
                Err(_) => check_failed_spawn(),
                Ok(None) => RecoveryClass::Expired {
                    reason: RecoveryExpiredReason::TargetRefGone,
                },
                Ok(Some(_)) => RecoveryClass::KnownNotWired {
                    strategy: strategy.clone(),
                },
            }
        }

        RecoveryStrategy::ResetRef { ref_name, to } => {
            classify_reset_ref(repo, operation_id, ref_name, to).await
        }
        RecoveryStrategy::RecreateBranch { name, at } => {
            classify_recreate_branch(repo, operation_id, name, at).await
        }
        RecoveryStrategy::RecreateTag { name, at } => {
            classify_recreate_tag(repo, operation_id, name, at).await
        }
        RecoveryStrategy::RevertCommit { commit } => {
            classify_revert_commit(repo, operation_id, commit).await
        }
    }
}

/// Which [`UnsupportedReason`] an `Irrecoverable` row means, disambiguated by
/// the operation that produced it (see [`classify_recovery`]'s doc).
///
/// Must stay in sync with the planner's own `Irrecoverable` arms — all five
/// were read and are listed explicitly here: `PushBranch`, `ResetTestRepo`,
/// `DeleteUntrackedPaths`, `DeleteRemoteTag`, `PushTag`.
fn irrecoverable_reason(operation: &GitOperation) -> UnsupportedReason {
    match operation {
        GitOperation::PushBranch { .. }
        | GitOperation::PushTag { .. }
        | GitOperation::DeleteRemoteTag { .. } => UnsupportedReason::EffectLeftTheRepository,
        GitOperation::ResetTestRepo => UnsupportedReason::NeverJournaled,
        GitOperation::DeleteUntrackedPaths { .. } => UnsupportedReason::NeverInObjectDatabase,
        // Reaching this arm means a future operation started returning
        // `Irrecoverable` without being added above. Fail toward the reading
        // that is true unconditionally whenever `Irrecoverable` is returned at
        // all (nothing here can be recovered by this app), rather than
        // guessing at one of the two narrower, sometimes-wrong reasons.
        _ => UnsupportedReason::EffectLeftTheRepository,
    }
}

async fn classify_reset_ref(
    repo: &Path,
    operation_id: &OperationId,
    ref_name: &RefName,
    to: &CommitOid,
) -> RecoveryClass {
    if let Some(refused) = pin_refusal(repo, operation_id).await {
        return refused;
    }
    let Some(branch) = ref_name.as_str().strip_prefix("refs/heads/") else {
        return RecoveryClass::CheckFailed {
            detail: CheckFailedReason::UnrecognizedRefShape,
        };
    };
    match resolve_ref_exact(repo, ref_name.as_str()).await {
        Err(_) => check_failed_spawn(),
        Ok(None) => RecoveryClass::Expired {
            reason: RecoveryExpiredReason::TargetRefGone,
        },
        Ok(Some(current)) if current == to.as_str() => RecoveryClass::AlreadyCurrent,
        Ok(Some(current)) => {
            let warn_pushed = tip_is_pushed(repo, &current).await;
            RecoveryClass::Offered {
                undo: UndoAction::ResetBranch {
                    branch: branch.to_string(),
                    to: to.as_str().to_string(),
                    // Compare-and-swap against what the branch points at *now*
                    // — the same contract `/api/undo` enforces, so a page that
                    // went stale between render and click is refused by the
                    // planner even if it somehow got past the equality gate.
                    expected_tip: current,
                },
                label: format!("Undo — reset ‘{branch}’ to {}", short(to.as_str())),
                warn_pushed,
            }
        }
    }
}

async fn classify_recreate_branch(
    repo: &Path,
    operation_id: &OperationId,
    name: &BranchName,
    at: &CommitOid,
) -> RecoveryClass {
    if let Some(refused) = pin_refusal(repo, operation_id).await {
        return refused;
    }
    match resolve_ref_exact(repo, &heads(name.as_str())).await {
        Err(_) => check_failed_spawn(),
        Ok(Some(current)) if current == at.as_str() => RecoveryClass::AlreadyCurrent,
        // The name is occupied by something other than what recreation would
        // produce. `UndoAction::RestoreBranch` is a plain, non-forcing
        // `git branch <name> <tip>` — "creates, never destroys" — so offering
        // it here would either fail outright or, worse, break that promise. A
        // live fact, so `Expired`, not `KnownNotWired`.
        Ok(Some(_)) => RecoveryClass::Expired {
            reason: RecoveryExpiredReason::NameReused,
        },
        Ok(None) => RecoveryClass::Offered {
            undo: UndoAction::RestoreBranch {
                name: name.as_str().to_string(),
                tip: at.as_str().to_string(),
            },
            label: format!(
                "Restore branch ‘{}’ at {}",
                name.as_str(),
                short(at.as_str())
            ),
            // Matches `undo_hint`'s own `BranchDeleted` arm: restoring a branch
            // name is a create, so there is nothing here that could already be
            // on the remote in a way this action would discard.
            warn_pushed: false,
        },
    }
}

async fn classify_recreate_tag(
    repo: &Path,
    operation_id: &OperationId,
    name: &TagName,
    at: &CommitOid,
) -> RecoveryClass {
    if let Some(refused) = pin_refusal(repo, operation_id).await {
        return refused;
    }
    match resolve_ref_exact(repo, &tags(name.as_str())).await {
        Err(_) => check_failed_spawn(),
        Ok(Some(current)) if current == at.as_str() => RecoveryClass::AlreadyCurrent,
        Ok(Some(_)) => RecoveryClass::Expired {
            reason: RecoveryExpiredReason::NameReused,
        },
        // The live check found the recovery genuinely still possible — the pin
        // resolves, the name is free — but no `UndoAction` can byte-for-byte
        // restore an annotated tag yet. One of the four `KnownNotWired` gaps
        // the design leaves as Tom's call.
        Ok(None) => RecoveryClass::KnownNotWired {
            strategy: RecoveryStrategy::RecreateTag {
                name: name.clone(),
                at: at.clone(),
            },
        },
    }
}

async fn classify_revert_commit(
    repo: &Path,
    operation_id: &OperationId,
    commit: &CommitOid,
) -> RecoveryClass {
    if let Some(refused) = pin_refusal(repo, operation_id).await {
        return refused;
    }
    // The pin resolving proves `commit` is reachable; reading it back (gix,
    // straight from the object database — the same call `undoables` makes for
    // the identical purpose) is what finds its parent, which the conflict
    // check needs.
    let detail = match read_commit(repo, commit.as_str()) {
        Ok(detail) => detail,
        Err(RepoError::CommitNotFound(_)) => {
            return RecoveryClass::Expired {
                reason: RecoveryExpiredReason::ObjectUnreachable,
            }
        }
        Err(_) => return check_failed_spawn(),
    };
    // A merge commit, or the repository's very first commit, has no single
    // parent to diff the revert against — `undoables` excludes both for the
    // identical reason (a merge needs a `-m` parent choice this UI does not
    // collect; a root commit has nothing for `merge-tree` to use as `theirs`).
    // A structural fact about the operation, not a live repository state, so
    // `Unsupported` rather than `Expired`.
    let [parent] = detail.parents.as_slice() else {
        return RecoveryClass::Unsupported {
            reason: UnsupportedReason::NoRecoverableHandle,
        };
    };
    let head = match crate::git_cmd::rev_parse(repo, "HEAD").await {
        Ok(Some(head)) => head,
        // Mirrors `revert_offer_established`'s three-way collapse exactly: an
        // unborn or unresolvable `HEAD` is "no fact", the same bucket as the
        // spawn failing — never a definite negative.
        Ok(None) | Err(_) => return check_failed_spawn(),
    };
    match crate::activity::revert_would_conflict(repo, commit.as_str(), parent.0.as_str(), &head)
        .await
    {
        Ok(true) => RecoveryClass::Expired {
            reason: RecoveryExpiredReason::WouldConflict,
        },
        Ok(false) => RecoveryClass::Offered {
            undo: UndoAction::RevertCommit {
                commit: commit.as_str().to_string(),
            },
            label: format!("Revert {} (adds an inverse commit)", short(commit.as_str())),
            warn_pushed: tip_is_pushed(repo, commit.as_str()).await,
        },
        Err(_) => check_failed_spawn(),
    }
}

fn check_failed_spawn() -> RecoveryClass {
    RecoveryClass::CheckFailed {
        detail: CheckFailedReason::GitSpawnFailed,
    }
}

fn heads(name: &str) -> String {
    format!("refs/heads/{name}")
}

fn tags(name: &str) -> String {
    format!("refs/tags/{name}")
}

/// `Some(refusal)` when the recovery pin for `operation_id` does not resolve
/// (or the check could not run), `None` when it does and classification may
/// proceed.
///
/// The pin — `refs/git-vista/recovery/<id>`, written once by
/// `durable::write_recovery_ref` from inside the mutation guard, immediately
/// before the destructive command ran — is what keeps the pre-operation object
/// reachable against `git gc`. Every strategy that names an oid is gated on it
/// first: without the pin, the oid in the row is a number, not a restorable
/// object.
async fn pin_refusal(repo: &Path, operation_id: &OperationId) -> Option<RecoveryClass> {
    let ref_name = crate::durable::recovery_ref_name(operation_id);
    match resolve_ref_exact(repo, &ref_name).await {
        Ok(Some(_)) => None,
        Ok(None) => Some(RecoveryClass::Expired {
            reason: RecoveryExpiredReason::PinMissing,
        }),
        Err(_) => Some(check_failed_spawn()),
    }
}

/// `git rev-parse --verify --quiet <ref_name>` — the ref's literal target, with
/// **no `^{commit}` peel**.
///
/// [`crate::git_cmd::rev_parse`] always appends `^{commit}`, which is right for
/// the commit-ish arguments it exists to resolve and wrong here: for
/// `RecreateTag` the pinned value is an annotated *tag object's* own oid, and
/// peeling it to the commit it tags would silently compare the wrong value
/// everywhere that oid has to match `at` exactly. `Err` means git did not run
/// — a distinct answer from `Ok(None)`, "git ran and the ref does not resolve",
/// which is the whole `CheckFailed`/`Expired` split at the smallest scale.
async fn resolve_ref_exact(repo: &Path, ref_name: &str) -> Result<Option<String>, String> {
    let output = crate::git_cmd::git_output(repo, &["rev-parse", "--verify", "--quiet", ref_name])
        .await
        .map_err(|e| format!("couldn't run git rev-parse: {e}"))?;
    if !output.status.success() {
        // ONLY git's documented missing-ref exit is "the ref does not
        // resolve". `rev-parse --verify --quiet` exits 1 for an unresolvable
        // ref and says nothing; a FATAL failure (unreadable ref store, exit
        // 128) is a check that did not run to completion, and the doc comment
        // above already promised that distinction — the body then collapsed
        // it, so a fatal probe read as "name is free" and could turn into an
        // Offered recovery. Found by codex (a different model) in pre-merge
        // review, 2026-08-18: "the check never returned a positive absence."
        let code = output.status.code();
        if code == Some(1) && output.stderr.is_empty() {
            return Ok(None);
        }
        return Err(format!(
            "git rev-parse --verify failed (exit {:?}): {}",
            code,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!id.is_empty()).then_some(id))
}

/// Whether `tip` is reachable from a remote-tracking ref right now — the live
/// version of `undo_hint`'s `remote.contains(new)`, recomputed here because
/// classification has no caller-supplied remote set to reuse.
///
/// One `git rev-list -n 1 <tip> --not --remotes` answers exactly this
/// question, uncapped. The previous version reused `read_remote_commits`
/// with the display cap `HISTORY_LIMIT` — safe for the newest-window history
/// view it was built for (its doc proves that bound), and WRONG here, where
/// pages reach arbitrarily old operations: a pushed commit older than the
/// newest `HISTORY_LIMIT` remote commits fell outside the set and read as
/// not-pushed, so a reset offer shipped without its "already pushed"
/// warning. Its inner walk also skipped unreadable refs and still returned
/// `Ok`, slipping an incomplete set past the over-warn default. Found by
/// codex in pre-merge review, 2026-08-18.
///
/// A failure still defaults to `true`: this feeds a confirm-dialog warning
/// only, never the offer decision, so the safe default is to over-warn. That
/// asymmetry with this module's fail-closed rule is deliberate — the rule is
/// about whether to show a button at all, not about this one piece of copy.
/// With `rev-list`, git enumerates the remote refs itself, so an unreadable
/// ref store is a non-zero exit here rather than a silently smaller set.
async fn tip_is_pushed(repo: &Path, tip: &str) -> bool {
    match crate::git_cmd::git_output(repo, &["rev-list", "-n", "1", tip, "--not", "--remotes"])
        .await
    {
        // Empty output: every path from `tip` is cut off by a remote ref —
        // the tip is on a remote. Non-empty: rev-list emitted the tip, so no
        // remote ref reaches it.
        Ok(out) if out.status.success() => out.stdout.iter().all(|b| b.is_ascii_whitespace()),
        // Fatal or spawn failure: we could not tell. Over-warn.
        _ => true,
    }
}

/// The conventional 7-character short id, for labels.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

// ---------------------------------------------------------------------------
// The history read path
// ---------------------------------------------------------------------------

/// `GET /api/operations/history` query parameters. Every field is optional at
/// the wire level (missing is not malformed); [`operation_history`] applies the
/// real defaults and validation.
#[derive(Debug, Deserialize)]
pub(crate) struct HistoryParams {
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub state: Option<HistoryStateFilter>,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Which terminal states the history returns.
///
/// `Any` means "any *terminal* state", never "any row": an `Accepted`/`Running`
/// row has no settled outcome to show and no `ended_at` [`HistoryEntry`]'s
/// shape could carry. Implemented as an explicit two-element list rather than
/// "no predicate" precisely so that stays true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HistoryStateFilter {
    Succeeded,
    Failed,
    Any,
}

impl HistoryStateFilter {
    fn terminal_states(self) -> &'static [OperationState] {
        match self {
            HistoryStateFilter::Succeeded => &[OperationState::Succeeded],
            HistoryStateFilter::Failed => &[OperationState::Failed],
            HistoryStateFilter::Any => &[OperationState::Succeeded, OperationState::Failed],
        }
    }
}

/// The `(accepted_at, id)` keyset position a page ended at. Clients only ever
/// see the opaque string [`HistoryCursor::encode`] produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryCursor {
    accepted_at: UnixSeconds,
    id: OperationId,
}

/// Longer than any real [`HistoryCursor`] encodes to — a length guard before
/// the base64 decode allocates, so an oversized `?before=` costs a comparison
/// rather than an allocation. Same discipline as
/// [`crate::history::CursorCodec`]'s own guard.
const MAX_ENCODED_HISTORY_CURSOR_LEN: usize = 512;

impl HistoryCursor {
    fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("a HistoryCursor always serializes");
        URL_SAFE_NO_PAD.encode(json)
    }

    /// Fails closed on anything unusable — too long, bad base64, or a shape
    /// `serde_json` does not recognise. A malformed cursor must never read as
    /// "nothing more to show" or restart silently at page 1; it reads as
    /// exactly what it is, a broken link, so the caller refetches page 1 on
    /// purpose.
    fn decode(raw: &str) -> Result<Self, HistoryCursorError> {
        if raw.len() > MAX_ENCODED_HISTORY_CURSOR_LEN {
            return Err(HistoryCursorError);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|_| HistoryCursorError)?;
        serde_json::from_slice(&bytes).map_err(|_| HistoryCursorError)
    }
}

/// Any cursor-codec failure, as one opaque unit — too long, bad base64 and a
/// bad shape are indistinguishable to the caller, and there is nothing useful
/// a probing client could learn from telling them apart.
#[derive(Debug)]
pub(crate) struct HistoryCursorError;

impl HistoryCursorError {
    fn response(self) -> (StatusCode, String) {
        (
            StatusCode::BAD_REQUEST,
            "That history link is no longer valid — reload the list.".to_string(),
        )
    }
}

/// One row of the Recovery Center's list: the operation's stored facts, plus a
/// `recovery` that was computed **live, on this request** — never the row's
/// stored `recovery_json`, which records what the plan decided at admission
/// time and says nothing about whether it is still possible.
#[derive(Debug, Serialize)]
pub(crate) struct HistoryEntry {
    pub id: OperationId,
    pub operation: GitOperation,
    /// `Succeeded` or `Failed` only — the query admits no other state.
    pub state: OperationState,
    pub accepted_at: UnixSeconds,
    /// Always present: the query filters `ended_at IS NOT NULL`, so this is a
    /// plain value rather than an `Option` that every consumer would have to
    /// invent a meaning for.
    pub ended_at: UnixSeconds,
    pub status: Option<u16>,
    pub message: Option<String>,
    pub repository: RepositoryToken,
    pub worktree: WorktreeToken,
    /// Set only when this row is *itself* the executed recovery of an earlier
    /// operation. "Was X recovered" is the reverse lookup, not a flag on X.
    pub recovers: Option<OperationId>,
    pub recovery: RecoveryClass,
}

#[derive(Debug, Serialize)]
pub(crate) struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    /// The opaque `?before=` value a client sends to fetch the next page;
    /// `None` means this was the last page.
    pub next_cursor: Option<String>,
}

/// How many rows the history returns by default, and at most.
///
/// Deliberately far below `crate::activity`'s `MAX_LIMIT = 500`: every row on a
/// page costs at least one sandboxed `git rev-parse` (and up to three, plus a
/// `merge-tree`, for a revert), because classification is live. That per-row
/// cost has to be bounded by something well under an audit-log page size.
/// Chosen and justified here rather than carried over from anywhere else — the
/// design's Corrections section records that an earlier draft cited a wrong
/// line number and an invented figure for exactly this constant.
const DEFAULT_LIMIT: u32 = 25;
const MAX_LIMIT: u32 = 100;

/// `limit=0` clamps to [`DEFAULT_LIMIT`] rather than to a page that could never
/// advance; anything larger clamps to [`MAX_LIMIT`].
fn clamp_limit(requested: Option<u32>) -> u32 {
    match requested {
        None | Some(0) => DEFAULT_LIMIT,
        Some(n) => n.min(MAX_LIMIT),
    }
}

/// Split a `limit + 1` lookahead scan into the page to return and the cursor
/// for the next one.
///
/// Pure, and separated from the query for exactly that reason: the ways to get
/// this wrong are all silent in production and all trivially testable here.
///
/// It operates on **scanned keys**, not decoded rows — that distinction is
/// load-bearing twice:
///
/// * `next_cursor` is `Some` **only** when a `(limit + 1)`th row was actually
///   *scanned* — the existence of a next page is a fact the query observed,
///   never an inference from "we asked for a full page" (which would hand back
///   a working-looking cursor on the final page too, turning "no more rows"
///   into an endless scroll), and never a count of the rows that happened to
///   decode. Counting decoded survivors ended the list early: one undecodable
///   row inside the window shrank the lookahead to `limit`, read as "last
///   page", and permanently hid every older operation — the exact "one bad
///   row must not make other operations unrecoverable" failure the durable
///   layer promises against.
/// * The cursor is built from the **last scanned key inside the page window**,
///   after the lookahead row is dropped — whether or not that row's payload
///   decoded. The next page resumes past the full window, so an undecodable
///   row costs exactly itself, never its neighbours.
///
/// The page returns the window's decodable rows; a `status: None` entry is
/// already logged by the durable layer and appears nowhere else.
fn split_page(
    mut scanned: Vec<crate::durable::ScannedOperation>,
    limit: u32,
) -> (Vec<OperationStatus>, Option<HistoryCursor>) {
    let cursor = if scanned.len() as u32 > limit {
        scanned.truncate(limit as usize);
        scanned.last().map(|s| HistoryCursor {
            accepted_at: s.accepted_at,
            id: s.id.clone(),
        })
    } else {
        None
    };
    let rows = scanned.into_iter().filter_map(|s| s.status).collect();
    (rows, cursor)
}

/// `GET /api/operations/history` — the Recovery Center's browsable list of this
/// app's own past operations.
///
/// `no-store`, matching `/api/undoables/{id}`'s posture: a live-recomputed view
/// must never be cached across a mutation.
pub async fn operation_history(
    Query(params): Query<HistoryParams>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repository = resolve_repository(params.repository.as_deref())?;
    let filter = params.state.unwrap_or(HistoryStateFilter::Any);
    let cursor = match params.before.as_deref() {
        Some(raw) => Some(HistoryCursor::decode(raw).map_err(HistoryCursorError::response)?),
        None => None,
    };
    let limit = clamp_limit(params.limit);

    let before = cursor.map(|c| (c.accepted_at, c.id));
    let rows = crate::durable::list_operations(
        repository,
        filter.terminal_states(),
        before,
        limit.saturating_add(1),
    )
    .await
    .map_err(|e| {
        eprintln!("git-vista: /api/operations/history failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Couldn't read the operation history.".to_string(),
        )
    })?;

    let (rows, next) = split_page(rows, limit);
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(ended_at) = row.ended_at else {
            // Unreachable while the query filters `ended_at IS NOT NULL`;
            // skipped rather than defaulted, on the same "a row that doesn't
            // decode is dropped, never guessed at" rule `row_to_status` uses.
            eprintln!("git-vista: a terminal history row had no end time; skipped");
            continue;
        };
        let recovery = classify_recovery_for_row(
            &row.repository,
            &row.worktree,
            &row.id,
            &row.operation,
            row.recovery.as_ref(),
        )
        .await;
        entries.push(HistoryEntry {
            id: row.id,
            operation: row.operation,
            state: row.state,
            accepted_at: row.accepted_at,
            ended_at,
            status: row.status,
            message: row.message,
            repository: row.repository,
            worktree: row.worktree,
            recovers: row.recovers,
            recovery,
        });
    }

    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((
        no_store,
        Json(HistoryPage {
            entries,
            next_cursor: next.map(|c| c.encode()),
        }),
    ))
}

/// Resolve `?repository=` to the token this build is allowed to browse: absent
/// falls back to the current selection (the same `selection_tokens()` every
/// write already goes through); present must equal it exactly.
///
/// A mismatch is refused rather than silently substituting the current
/// selection for whatever the caller actually asked for — cross-repository
/// history is deferred by the design, not implemented, and a client that
/// believes it is reading repository B's history must never be shown
/// repository A's.
fn resolve_repository(requested: Option<&str>) -> Result<RepositoryToken, (StatusCode, String)> {
    let (current, _worktree) = crate::planner::selection_tokens();
    match requested {
        None => Ok(current),
        Some(raw) => {
            let Ok(token) = RepositoryToken::new(raw) else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "That isn't a repository id.".to_string(),
                ));
            };
            if token == current {
                Ok(token)
            } else {
                Err((
                    StatusCode::BAD_REQUEST,
                    "Operation history is only browsable for the currently selected \
                     repository."
                        .to_string(),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The write path
// ---------------------------------------------------------------------------

/// Whether a durable row's own `state` permits *attempting* a recovery at all
/// — step 1 of [`recover_operation`], before live classification (step 2)
/// runs at all.
///
/// Terminal only, via [`OperationState::is_terminal`] — but that means both
/// `Succeeded` *and* `Failed`, not `Succeeded` alone: [`classify_recovery`]
/// never consults `state`, a failed `ResetRef` still had its recovery pin
/// written before `execute` ran (`planner.rs`'s `pin_recovery` call sits
/// immediately before `execute`), and `OperationState::Failed`'s own doc says
/// a refusal *is* a settled outcome, not a lost answer. Gating this on
/// `Succeeded` alone let [`HistoryStateFilter::Any`] (whose
/// [`HistoryStateFilter::terminal_states`] already admits `Failed`) advertise
/// an `Offered` recovery this endpoint then refused with 400 — an undo the
/// list showed with no way to press it. `Accepted`/`Running` are still
/// refused: a non-terminal row has no settled outcome, and nothing durable
/// guarantees a pin is in place while it's still running.
fn state_permits_recovery_attempt(state: OperationState) -> bool {
    state.is_terminal()
}

/// `POST /api/operations/{id}/recover` — run the recovery this server itself
/// establishes for one past operation.
///
/// The rule, enforced in exactly one place (step 3): **a client may not hand in
/// an action to run; it may only ask to run the one the server independently
/// computes.** The enum shape keeps the server honest about what it *could*
/// construct; this equality check is what stops a stale or hand-crafted request
/// from mattering. Same seriousness as #145's plan-staleness gate, one layer up
/// — and the design's own risk list names a later refactor treating the request
/// body as authoritative as the single highest-risk failure of this feature.
///
/// Everything from step 5 on is inherited unchanged from every other write:
/// this handler builds an ordinary [`GitOperation`] and hands it to
/// [`crate::planner::plan_and_execute_recovery`], which applies the same
/// admission, guard, staleness gate and durable terminal record. Nothing here
/// runs git to mutate anything and nothing here writes to the journal directly.
pub async fn recover_operation(
    AxumPath(id): AxumPath<String>,
    Json(claimed): Json<UndoAction>,
) -> (StatusCode, String) {
    if let Some(rejected) = crate::state::reject_if_read_only() {
        return rejected;
    }
    // A malformed id and an id that never existed get the same answer, so this
    // route never reveals which shapes the server has ever minted.
    let Ok(id) = OperationId::new(id) else {
        return (
            StatusCode::NOT_FOUND,
            "No operation with that id.".to_string(),
        );
    };
    let repo = match crate::state::resolve_target() {
        Ok((repo, _entry)) => repo,
        Err(rejected) => return rejected,
    };

    // 1. The durable row — never `crate::operations`'s in-memory registry,
    //    whose bound and TTL are exactly why this table is what the Recovery
    //    Center reads.
    let Some(row) = crate::durable::load_operation(&id).await else {
        return (
            StatusCode::NOT_FOUND,
            "No operation with that id.".to_string(),
        );
    };
    if !state_permits_recovery_attempt(row.state) {
        return (
            StatusCode::BAD_REQUEST,
            "This operation hasn't finished yet — its recovery can't be checked.".to_string(),
        );
    }

    // 1b. `repo` above is the CURRENT selection, and `classify_recovery`
    //     below (and the execution after it) both run against exactly that
    //     path — there is no per-row resolution here, unlike the read path's
    //     `classify_recovery_for_row`, which resolves each row's own
    //     `worktree` (`state::resolve_worktree`). `select_operations_blocking`
    //     filters on `repository` alone, so one history page can legitimately
    //     mix rows from several worktrees of one clone (identity.rs: a main
    //     working tree and its linked worktrees share a `RepositoryId` but
    //     carry distinct `WorktreeId`s/HEADs). Without this check, a row from
    //     worktree B would be classified and, if `Offered`, executed against
    //     whatever worktree happens to be selected right now — silently the
    //     wrong HEAD. Refuse instead of resolving `row.worktree` here, so
    //     classification (step 2) and execution (step 5, which itself
    //     re-resolves the current selection via
    //     `plan_and_execute_recovery`/`resolve_target`) can never disagree
    //     about which worktree they mean.
    let (_current_repository, current_worktree) = crate::planner::selection_tokens();
    if row.worktree != current_worktree {
        return (
            StatusCode::CONFLICT,
            "This operation belongs to a different worktree than the one \
             currently selected — switch to it and try again."
                .to_string(),
        );
    }

    // 2. Live, right now — against the repository, not the row's stored
    //    (static) `recovery_json`, and not any class a client cached from an
    //    earlier page load.
    let class = classify_recovery(&repo, &id, &row.operation, row.recovery.as_ref()).await;

    // 3. THE gate.
    let RecoveryClass::Offered { undo, .. } = class else {
        return (
            StatusCode::CONFLICT,
            "This operation can no longer be recovered — its recovery point is \
             no longer available."
                .to_string(),
        );
    };
    if undo != claimed {
        return (
            StatusCode::CONFLICT,
            "The recovery offered for this operation has changed — refresh and \
             try again."
                .to_string(),
        );
    }

    // 4. Build from `undo` — the value THIS request just re-established — not
    //    from `claimed`. They compared equal above, but using the server's own
    //    copy keeps step 3 the only place trust crosses the client boundary,
    //    rather than a check whose result is then discarded in favour of the
    //    request body.
    let op = match crate::activity::undo_action_to_operation(&repo, undo).await {
        Ok(op) => op,
        Err(refused) => return refused,
    };

    // 5. Inherit admission, the staleness gate and the durable terminal record
    //    like every other mutation — with `recovers` threaded through, so the
    //    NEW row this produces (never `id`'s own row) is the one naming `id`.
    crate::planner::plan_and_execute_recovery(op, id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{
        CommitMessage, GenerationToken, IdempotencyKey, OperationHash, OperationStage,
        OperationState, RepoMode,
    };

    fn row(id: &str, accepted_at: i64) -> OperationStatus {
        OperationStatus {
            id: OperationId::new(id).unwrap(),
            state: OperationState::Succeeded,
            stage: OperationStage::Finished,
            operation: GitOperation::CommitOnHead {
                message: CommitMessage::new("m").unwrap(),
                allow_empty: false,
            },
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            repository: RepositoryToken::new("r").unwrap(),
            worktree: WorktreeToken::new("w").unwrap(),
            accepted_at: UnixSeconds(accepted_at),
            ended_at: Some(UnixSeconds(accepted_at + 1)),
            status: Some(200),
            message: None,
            generation: Some(GenerationToken::new("1").unwrap()),
            recovery: None,
            recovers: None,
            progress: None,
        }
    }

    /// A scanned row whose payload decoded — the ordinary shape.
    fn scanned(id: &str, accepted_at: i64) -> crate::durable::ScannedOperation {
        crate::durable::ScannedOperation {
            accepted_at: UnixSeconds(accepted_at),
            id: OperationId::new(id).unwrap(),
            status: Some(row(id, accepted_at)),
        }
    }

    /// A scanned key whose payload failed to decode — the `status: None` shape
    /// the durable layer hands back for a corrupt row.
    fn scanned_undecodable(id: &str, accepted_at: i64) -> crate::durable::ScannedOperation {
        crate::durable::ScannedOperation {
            accepted_at: UnixSeconds(accepted_at),
            id: OperationId::new(id).unwrap(),
            status: None,
        }
    }

    /// A full page plus the lookahead row: the lookahead is dropped, and the
    /// cursor names the last scanned key **inside the page window**.
    ///
    /// Goes red if `split_page` builds the cursor from the lookahead row
    /// instead (`.last()` before the truncate) — the next page would then
    /// skip a row, silently, forever.
    #[test]
    fn a_full_page_reports_the_cursor_of_its_last_returned_row() {
        let rows = vec![scanned("a", 300), scanned("b", 200), scanned("c", 100)];
        let (page, cursor) = split_page(rows, 2);
        assert_eq!(
            page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let cursor = cursor.expect("a lookahead row was scanned, so there is a next page");
        assert_eq!(cursor.id.as_str(), "b");
        assert_eq!(cursor.accepted_at, UnixSeconds(200));
    }

    /// The last page reports no cursor even when it is exactly `limit` long —
    /// the only honest signal that there is nothing more.
    ///
    /// Goes red if `split_page` decides on `scanned.len() == limit` (an
    /// off-by-one that hands back a cursor for a page with nothing after it):
    /// the client would fetch an empty page forever, which in a browser with
    /// no shell is an infinite scroll with no exit.
    #[test]
    fn an_exactly_full_last_page_reports_no_cursor() {
        let rows = vec![scanned("a", 300), scanned("b", 200)];
        let (page, cursor) = split_page(rows, 2);
        assert_eq!(page.len(), 2);
        assert!(
            cursor.is_none(),
            "no lookahead row was scanned, so this was the last page"
        );
    }

    #[test]
    fn an_empty_result_reports_no_cursor() {
        let (page, cursor) = split_page(Vec::new(), 25);
        assert!(page.is_empty());
        assert!(cursor.is_none());
    }

    /// **The one-bad-row case from pre-merge review (codex, 2026-08-18).**
    /// `limit + 1` rows were scanned but one inside the page window failed to
    /// decode: the page comes back one row short AND `next_cursor` is still
    /// `Some`, pointing past the full scanned window — the bad row costs
    /// exactly itself, never the older history behind it.
    ///
    /// Goes red two ways:
    /// * if the next-page decision reverts to counting decoded rows
    ///   (post-drop `rows.len() <= limit`) — the cursor comes back `None`,
    ///   and "c" plus every older row is permanently hidden behind one
    ///   corrupt neighbour;
    /// * if the cursor is built from the last *decoded* row instead of the
    ///   last *scanned* key — it would name ("a", 300), and the next page
    ///   would re-scan from the top of the window instead of resuming past
    ///   it.
    #[test]
    fn an_undecodable_row_inside_the_window_does_not_end_the_list() {
        let rows = vec![
            scanned("a", 300),
            scanned_undecodable("b", 200),
            scanned("c", 100),
        ];
        let (page, cursor) = split_page(rows, 2);
        assert_eq!(
            page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["a"],
            "the bad row is dropped from the page — and only the bad row"
        );
        let cursor = cursor.expect(
            "a (limit + 1)th row was scanned, so there IS a next page — None here \
             is exactly how one bad row used to hide all older history",
        );
        assert_eq!(cursor.id.as_str(), "b");
        assert_eq!(cursor.accepted_at, UnixSeconds(200));
    }

    /// The lookahead row's decodability is irrelevant to the next-page
    /// decision: a scanned `(limit + 1)`th row proves more history exists
    /// even when that particular row will never render.
    ///
    /// Goes red if the lookahead is judged on the rows that decoded rather
    /// than the rows that were scanned.
    #[test]
    fn an_undecodable_lookahead_row_still_proves_a_next_page() {
        let rows = vec![
            scanned("a", 300),
            scanned("b", 200),
            scanned_undecodable("c", 100),
        ];
        let (page, cursor) = split_page(rows, 2);
        assert_eq!(page.len(), 2);
        let cursor = cursor.expect("three rows scanned at limit 2 means a next page exists");
        assert_eq!(cursor.id.as_str(), "b");
        assert_eq!(cursor.accepted_at, UnixSeconds(200));
    }

    #[test]
    fn a_cursor_round_trips_through_its_wire_encoding() {
        let cursor = HistoryCursor {
            accepted_at: UnixSeconds(1_753_400_000),
            id: OperationId::new("op_0123456789abcdef").unwrap(),
        };
        let encoded = cursor.encode();
        assert!(
            !encoded.contains('=') && !encoded.contains('+') && !encoded.contains('/'),
            "the cursor travels in a query string, so it must be base64url with no padding"
        );
        assert_eq!(HistoryCursor::decode(&encoded).unwrap(), cursor);
    }

    /// Every unusable cursor is an error, never a silent restart at page 1 and
    /// never a silent "you have reached the end".
    ///
    /// Goes red if `decode` is changed to `unwrap_or_default()`/`ok()` at any
    /// of its three failure points — which is exactly the shape of fix someone
    /// reaches for when a malformed cursor 400s in a demo.
    #[test]
    fn an_unusable_cursor_fails_closed_rather_than_restarting_the_list() {
        for bad in [
            "",
            "not base64!!",
            // Valid base64url, but not a cursor.
            &URL_SAFE_NO_PAD.encode(b"{\"nope\":1}"),
            // Past the length guard, before any decode allocates.
            &"A".repeat(MAX_ENCODED_HISTORY_CURSOR_LEN + 1),
        ] {
            assert!(
                HistoryCursor::decode(bad).is_err(),
                "‘{bad}’ must refuse, not silently start over"
            );
        }
    }

    #[test]
    fn the_page_limit_is_clamped_at_both_ends() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(
            clamp_limit(Some(0)),
            DEFAULT_LIMIT,
            "never a zero-length page"
        );
        assert_eq!(clamp_limit(Some(10)), 10);
        assert_eq!(clamp_limit(Some(u32::MAX)), MAX_LIMIT);
    }

    /// `Any` is "any terminal state", never "no filter" — a non-terminal row
    /// has no `ended_at` for `HistoryEntry` to carry.
    ///
    /// Goes red if `terminal_states` grows an `Accepted`/`Running` entry, or if
    /// `Any` is ever reimplemented as an empty slice meaning "no predicate".
    #[test]
    fn every_state_filter_admits_only_terminal_states() {
        for filter in [
            HistoryStateFilter::Succeeded,
            HistoryStateFilter::Failed,
            HistoryStateFilter::Any,
        ] {
            let states = filter.terminal_states();
            assert!(
                !states.is_empty(),
                "an empty list would mean 'no predicate'"
            );
            assert!(
                states.iter().all(|s| s.is_terminal()),
                "{filter:?} admits a non-terminal state"
            );
        }
    }

    /// Every `Irrecoverable`-producing operation the planner has is named
    /// explicitly, and the three reasons stay distinct.
    ///
    /// Goes red if the match is collapsed to a single blanket arm — which
    /// would report a deleted untracked file as "the effect left the
    /// repository", telling the user to go look on the remote for something
    /// that was never in git at all.
    #[test]
    fn irrecoverable_names_which_kind_of_irrecoverable() {
        use git_vista_protocol::{ForcePublish, RemoteName, TagName, WorktreePath};
        assert_eq!(
            irrecoverable_reason(&GitOperation::PushBranch {
                branch: BranchName::new("main").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
                set_upstream: false,
                force: ForcePublish::None,
            }),
            UnsupportedReason::EffectLeftTheRepository
        );
        assert_eq!(
            irrecoverable_reason(&GitOperation::PushTag {
                name: TagName::new("v1.0.0").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
            }),
            UnsupportedReason::EffectLeftTheRepository
        );
        assert_eq!(
            irrecoverable_reason(&GitOperation::DeleteRemoteTag {
                name: TagName::new("v1.0.0").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
            }),
            UnsupportedReason::EffectLeftTheRepository
        );
        assert_eq!(
            irrecoverable_reason(&GitOperation::ResetTestRepo),
            UnsupportedReason::NeverJournaled
        );
        assert_eq!(
            irrecoverable_reason(&GitOperation::DeleteUntrackedPaths {
                paths: vec![WorktreePath::new("scratch.txt").unwrap()],
            }),
            UnsupportedReason::NeverInObjectDatabase,
            "content that was never in the object database is not 'gone to the remote'"
        );
    }

    /// `/api/operations/history` is a **static** path registered alongside the
    /// **parameterised** `/api/operations/{id}`, and it is registered *after*
    /// it. The router must still send `history` to the list handler rather than
    /// treating it as an operation id — otherwise the whole read surface is
    /// silently dead, answering "No operation with that id." to every request,
    /// with no error anywhere to notice.
    ///
    /// Asserted rather than assumed. The precedence is a property of axum's
    /// router, not of this crate, so it could change under a dependency bump
    /// with nothing else in the suite noticing. Two stand-in handlers so the
    /// check is about routing alone and needs no session, CSRF or repository
    /// selection.
    ///
    /// Goes red if the precedence ever inverts, and would also have gone red
    /// had the two routes been registered in a way the router rejects
    /// outright.
    #[tokio::test]
    async fn the_history_path_wins_over_the_operation_id_parameter() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt;

        let app = Router::new()
            .route("/api/operations/{id}", get(|| async { "by-id" }))
            .route("/api/operations/history", get(|| async { "history" }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/operations/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "history",
            "the static segment must win over the {{id}} parameter, or the \
             Recovery Center's list is unreachable"
        );
    }

    /// A row with no recorded strategy is "we could not check", never "no".
    ///
    /// Goes red if the `None` arm is changed to any `Expired` variant — which
    /// would tell the user a definite "this expired" about a check that never
    /// ran.
    #[tokio::test]
    async fn a_row_with_no_strategy_is_check_failed_not_expired() {
        let class = classify_recovery(
            Path::new("/nonexistent"),
            &OperationId::new("no-strategy").unwrap(),
            &GitOperation::StageAll,
            None,
        )
        .await;
        assert_eq!(
            class,
            RecoveryClass::CheckFailed {
                detail: CheckFailedReason::NoStrategyRecorded
            }
        );
    }

    // -----------------------------------------------------------------------
    // Live classification against a real repository
    // -----------------------------------------------------------------------

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        first: String,
        /// Name of an *annotated* tag `fixture()` creates at `HEAD` (the
        /// `second` commit), and the tag **object's own oid** — what
        /// `git rev-parse --verify --quiet refs/tags/<name>` returns with no
        /// `^{commit}` peel, i.e. exactly what [`resolve_ref_exact`] returns
        /// and exactly what `RecoveryStrategy::RecreateTag::at` carries
        /// (`durable::recovery_oid`'s own doc). A *peeled* `rev_parse` on
        /// this same ref would return `HEAD`'s commit oid instead — a
        /// different value — which is the whole reason a test needs a real
        /// annotated tag rather than a lightweight one: only an annotated
        /// tag makes the two reads diverge.
        annotated_tag: String,
        annotated_tag_oid: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "seed"]);
        let first = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        // A second commit, so refs/heads/main has moved off `first` — the case
        // a recovery pin exists to answer.
        std::fs::write(repo.join("a.txt"), "b\n").unwrap();
        run(&["commit", "-qam", "second"]);
        let annotated_tag = "annotated-tag".to_string();
        run(&["tag", "-a", &annotated_tag, "-m", "an annotated tag"]);
        let annotated_tag_oid = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/tags/{annotated_tag}"),
                ])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        Fixture {
            _dir: dir,
            repo,
            first,
            annotated_tag,
            annotated_tag_oid,
        }
    }

    /// `git <args>` in `repo`, asserting success — for the tests below that
    /// need commit history `fixture()` doesn't already provide (a conflicting
    /// revert needs a third commit with a real dependency on the one being
    /// reverted).
    fn run_git(repo: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap()
                .success(),
            "git {args:?} failed in {repo:?}"
        );
    }

    /// `git rev-parse <rev>` in `repo`, trimmed.
    fn rev_parse_plain(repo: &Path, rev: &str) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git rev-parse {rev} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn reset_ref_to(first: &str) -> RecoveryStrategy {
        RecoveryStrategy::ResetRef {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            to: CommitOid::new(first.to_string()).unwrap(),
        }
    }

    /// **The load-bearing test of this whole module.** With no recovery pin —
    /// the state every operation reaches once `git gc` has reclaimed it, and
    /// the state every operation that never wrote one is in — the answer is a
    /// definite `Expired { PinMissing }`, and there is no `UndoAction`
    /// anywhere in it.
    ///
    /// Goes red if the pin check is removed or made permissive: the branch has
    /// genuinely moved, so without the pin gate this row classifies as
    /// `Offered` and the UI grows a button whose target may have been pruned
    /// out of the object database.
    #[tokio::test]
    async fn a_reset_with_no_surviving_pin_is_expired_and_carries_no_undo() {
        let f = fixture();
        let class = classify_recovery(
            &f.repo,
            &OperationId::new("no-pin").unwrap(),
            &GitOperation::StageAll,
            Some(&reset_ref_to(&f.first)),
        )
        .await;
        assert_eq!(
            class,
            RecoveryClass::Expired {
                reason: RecoveryExpiredReason::PinMissing
            }
        );
        assert!(
            !matches!(class, RecoveryClass::Offered { .. }),
            "a missing pin can never produce an offer"
        );
    }

    /// With the pin written, the same row is offered — and the offer's
    /// compare-and-swap tip is the branch's *live* tip, not anything stored.
    ///
    /// Goes red if `expected_tip` is built from the strategy's `to` (the
    /// pre-operation value) instead of the current tip: the planner's own CAS
    /// would then always refuse, and every offered undo would 409.
    #[tokio::test]
    async fn a_reset_with_a_live_pin_is_offered_with_the_live_tip() {
        let f = fixture();
        let id = OperationId::new("with-pin").unwrap();
        let strategy = reset_ref_to(&f.first);
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let head = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        match class {
            RecoveryClass::Offered {
                undo,
                warn_pushed,
                label,
            } => {
                assert_eq!(
                    undo,
                    UndoAction::ResetBranch {
                        branch: "main".to_string(),
                        to: f.first.clone(),
                        expected_tip: head,
                    }
                );
                assert!(label.contains("main"));
                assert!(
                    !warn_pushed,
                    "this fixture has no remote-tracking refs at all"
                );
            }
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    /// The bug this pins: `operation_history`'s `Any` filter (the default
    /// when `?state=` is absent) already includes `Failed` rows — a failed
    /// `ResetRef` still had its recovery pin written before `execute` ran, so
    /// `classify_recovery` genuinely offers one. `recover_operation`'s own
    /// state gate must not disagree with that, or the list advertises a
    /// recovery the endpoint categorically refuses with 400 — an undo with no
    /// way to press it.
    ///
    /// Goes red if [`state_permits_recovery_attempt`] reverts to
    /// `state == OperationState::Succeeded` (the original bug): `Failed` is
    /// still in `HistoryStateFilter::Any::terminal_states()`, so the loop
    /// below would find a state the list offers that the gate refuses.
    #[tokio::test]
    async fn a_failed_row_the_list_would_offer_is_not_refused_by_the_endpoints_own_gate() {
        let f = fixture();
        let id = OperationId::new("failed-with-pin").unwrap();
        let strategy = reset_ref_to(&f.first);
        // The pin is written from inside the mutation guard immediately
        // *before* `execute` (`planner.rs`) — so it exists regardless of
        // whether the operation went on to succeed or fail. Nothing about
        // this fixture setup is state-specific; only the row's stored
        // `state` (asserted below) is.
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        assert!(
            matches!(class, RecoveryClass::Offered { .. }),
            "a row with a live pin and a moved branch must classify Offered — \
             same call the list uses per row — got {class:?}"
        );

        for state in HistoryStateFilter::Any.terminal_states() {
            assert!(
                state_permits_recovery_attempt(*state),
                "{state:?} is one of the states `operation_history`'s default \
                 `Any` filter returns, so `recover_operation`'s own gate must \
                 not refuse it"
            );
        }
    }

    /// The branch is already where recovery would put it: nothing to do, and
    /// — critically — no button.
    ///
    /// Goes red if the `current == to` arm is dropped: the row would offer a
    /// reset to the value it already holds, which the planner's CAS accepts,
    /// producing a no-op write and a journal entry for an operation that
    /// changed nothing.
    #[tokio::test]
    async fn a_reset_whose_target_is_already_current_is_not_offered() {
        let f = fixture();
        let id = OperationId::new("already-current").unwrap();
        let head = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        // A strategy that would reset `main` to where it already points.
        let strategy = reset_ref_to(&head);
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        assert_eq!(class, RecoveryClass::AlreadyCurrent);
    }

    /// A branch this strategy would recreate, whose name is now taken by
    /// something else, is a live-established `Expired { NameReused }` — never
    /// an offer that would either fail or clobber.
    #[tokio::test]
    async fn recreating_a_branch_whose_name_was_reused_is_expired() {
        let f = fixture();
        let id = OperationId::new("name-reused").unwrap();
        let strategy = RecoveryStrategy::RecreateBranch {
            name: BranchName::new("main").unwrap(),
            at: CommitOid::new(f.first.clone()).unwrap(),
        };
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        // `refs/heads/main` exists and points at the *second* commit, not at
        // the strategy's `at`.
        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        assert_eq!(
            class,
            RecoveryClass::Expired {
                reason: RecoveryExpiredReason::NameReused
            }
        );
    }

    /// A deleted branch, with its pin alive and its name free, is offered as a
    /// plain restore — the one recovery that creates and never destroys.
    #[tokio::test]
    async fn recreating_a_branch_whose_name_is_free_is_offered() {
        let f = fixture();
        let id = OperationId::new("restore-branch").unwrap();
        let strategy = RecoveryStrategy::RecreateBranch {
            name: BranchName::new("gone").unwrap(),
            at: CommitOid::new(f.first.clone()).unwrap(),
        };
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        assert_eq!(
            class,
            RecoveryClass::Offered {
                undo: UndoAction::RestoreBranch {
                    name: "gone".to_string(),
                    tip: f.first.clone(),
                },
                label: format!("Restore branch ‘gone’ at {}", &f.first[..7]),
                warn_pushed: false,
            }
        );
    }

    // -------------------------------------------------------------------
    // `classify_recreate_tag` — exercised nowhere else in this suite
    // -------------------------------------------------------------------

    /// The load-bearing reason `classify_recreate_tag` uses
    /// [`resolve_ref_exact`] (no peel) rather than
    /// [`crate::git_cmd::rev_parse`] (peels to `^{commit}`): an *annotated*
    /// tag's ref does not point at the commit at all, it points at the tag
    /// object, and `RecoveryStrategy::RecreateTag::at` pins that tag object's
    /// own oid (`durable::recovery_oid`'s doc). With the tag still present
    /// and unchanged, the live read must equal `at` and classify
    /// `AlreadyCurrent`.
    ///
    /// Goes red if `resolve_ref_exact(repo, &tags(name))` in
    /// `classify_recreate_tag` is swapped for the peeling
    /// `crate::git_cmd::rev_parse(repo, &tags(name))`: the peeled read would
    /// return the *commit* `HEAD` points at, not the tag object's oid, which
    /// no longer equals `at` — the match falls through to
    /// `Expired { NameReused }` instead, silently telling the user their own
    /// still-current tag needs recreating.
    #[tokio::test]
    async fn recreating_a_still_present_annotated_tag_uses_the_tag_objects_own_oid() {
        let f = fixture();
        let id = OperationId::new("recreate-tag-already-current").unwrap();
        let strategy = RecoveryStrategy::RecreateTag {
            name: TagName::new(f.annotated_tag.clone()).unwrap(),
            at: CommitOid::new(f.annotated_tag_oid.clone()).unwrap(),
        };
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        // Sanity on the fixture itself: an annotated tag's own oid really is
        // different from the commit it tags, or this test would pass for the
        // wrong reason (peeled and unpeeled reads happening to agree).
        let head = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            f.annotated_tag_oid, head,
            "fixture bug: an annotated tag object must not share its commit's oid"
        );

        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        assert_eq!(class, RecoveryClass::AlreadyCurrent);
    }

    // -------------------------------------------------------------------
    // `classify_revert_commit` — exercised nowhere else in this suite
    // -------------------------------------------------------------------

    /// A revert with nothing built on top of the commit being reverted must
    /// classify `Offered`, live-established via
    /// `crate::activity::revert_would_conflict` (`Ok(false)`) — the mirror
    /// case of the conflicting test below.
    ///
    /// Goes red if the `Ok(true)`/`Ok(false)` arms at
    /// `classify_revert_commit`'s `match … revert_would_conflict(..).await`
    /// are swapped: a clean revert would then classify
    /// `Expired { WouldConflict }` and carry no `UndoAction`.
    #[tokio::test]
    async fn reverting_the_current_tip_with_no_dependents_is_offered() {
        let f = fixture();
        let id = OperationId::new("revert-clean").unwrap();
        let head = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        let strategy = RecoveryStrategy::RevertCommit {
            commit: CommitOid::new(head.clone()).unwrap(),
        };
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        assert_eq!(
            class,
            RecoveryClass::Offered {
                undo: UndoAction::RevertCommit {
                    commit: head.clone(),
                },
                label: format!("Revert {} (adds an inverse commit)", &head[..7]),
                // This fixture has no remote-tracking refs at all, same
                // reasoning as the `ResetRef` offer test above.
                warn_pushed: false,
            }
        );
    }

    /// The conflicting mirror case, same repro shape as
    /// `crate::activity`'s own
    /// `a_commit_later_work_depends_on_is_reported_as_conflicting`: a commit
    /// that adds a line, then a later commit whose own content depends on
    /// that line staying present. Reverting the first must classify
    /// `Expired { WouldConflict }`, live-established, and carry no
    /// `UndoAction` — never `Offered` on the strength of "the pin still
    /// resolves" alone.
    ///
    /// Goes red under the same swapped-arm mutation as the clean-case test
    /// above, from the opposite direction: a genuinely conflicting revert
    /// would classify `Offered` and hand the UI a button whose execution
    /// `git revert` cannot actually complete cleanly.
    #[tokio::test]
    async fn reverting_a_commit_later_work_depends_on_is_expired_would_conflict() {
        let f = fixture();
        run_git(&f.repo, &["checkout", "-q", "main"]);
        std::fs::write(f.repo.join("c.txt"), "line1\n").unwrap();
        run_git(&f.repo, &["add", "c.txt"]);
        run_git(&f.repo, &["commit", "-q", "-m", "base c"]);

        std::fs::write(f.repo.join("c.txt"), "line1\nline2\n").unwrap();
        run_git(&f.repo, &["add", "c.txt"]);
        run_git(&f.repo, &["commit", "-q", "-m", "add line2"]);
        let to_revert = rev_parse_plain(&f.repo, "HEAD");

        std::fs::write(f.repo.join("c.txt"), "line1\nline2\nline3\n").unwrap();
        run_git(&f.repo, &["add", "c.txt"]);
        run_git(&f.repo, &["commit", "-q", "-m", "add line3, needs line2"]);

        let id = OperationId::new("revert-conflict").unwrap();
        let strategy = RecoveryStrategy::RevertCommit {
            commit: CommitOid::new(to_revert.clone()).unwrap(),
        };
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let class = classify_recovery(&f.repo, &id, &GitOperation::StageAll, Some(&strategy)).await;
        assert_eq!(
            class,
            RecoveryClass::Expired {
                reason: RecoveryExpiredReason::WouldConflict
            }
        );
        assert!(
            !matches!(class, RecoveryClass::Offered { .. }),
            "a commit later work depends on must never be offered as a clean revert"
        );
    }

    /// `NotNeeded` never touches git at all — no pin, no ref read, no offer.
    #[tokio::test]
    async fn a_strategy_that_needed_nothing_reports_none_needed() {
        let f = fixture();
        let class = classify_recovery(
            &f.repo,
            &OperationId::new("nothing").unwrap(),
            &GitOperation::StageAll,
            Some(&RecoveryStrategy::NotNeeded),
        )
        .await;
        assert_eq!(class, RecoveryClass::NoneNeeded);
    }

    /// A branch git-vista created and could delete again, still present: a
    /// real recovery with no `UndoAction` to express it — `KnownNotWired`, not
    /// `Offered` and not `Expired`.
    ///
    /// Goes red if `KnownNotWired` is ever "helpfully" mapped onto an existing
    /// `UndoAction`, which is how an unsupported recovery gets labelled undo.
    #[tokio::test]
    async fn a_recovery_with_no_undo_action_yet_is_known_not_wired() {
        let f = fixture();
        let strategy = RecoveryStrategy::DeleteCreatedBranch {
            name: BranchName::new("main").unwrap(),
        };
        let class = classify_recovery(
            &f.repo,
            &OperationId::new("not-wired").unwrap(),
            &GitOperation::StageAll,
            Some(&strategy),
        )
        .await;
        assert_eq!(class, RecoveryClass::KnownNotWired { strategy });
    }

    /// The same strategy once the branch is already gone: the delete this
    /// would perform has happened, so there is nothing left to do.
    #[tokio::test]
    async fn deleting_a_branch_that_is_already_gone_is_already_current() {
        let f = fixture();
        let class = classify_recovery(
            &f.repo,
            &OperationId::new("gone-already").unwrap(),
            &GitOperation::StageAll,
            Some(&RecoveryStrategy::DeleteCreatedBranch {
                name: BranchName::new("never-existed").unwrap(),
            }),
        )
        .await;
        assert_eq!(class, RecoveryClass::AlreadyCurrent);
    }

    // -------------------------------------------------------------------
    // `POST /api/operations/{id}/recover` — the equality gate itself
    // -------------------------------------------------------------------

    /// **The handler's own highest-risk point, driven end to end.** The design
    /// doc's Consequences #1 names step 3 (`if undo != claimed`) as the single
    /// place a stale or hand-crafted request could get treated as authoritative;
    /// this pins that step by calling [`recover_operation`] itself, not by
    /// grepping its source text.
    ///
    /// The branch moves a second time *after* the offer the client is holding
    /// was drawn, so the `undo` `recover_operation` re-derives right now (a
    /// reset to the newest tip) is a different [`UndoAction`] from the stale
    /// `claimed` one built against the older tip. The gate must refuse this
    /// with 409 and — the second half of the same fact — must never fall
    /// through to actually resetting `main`, whether to the stale value or to
    /// its own freshly-derived one.
    ///
    /// Goes red under the described neutering mutation of step 3 (keep the
    /// `if undo != claimed {` line, empty its block and drop the `return`):
    /// execution then falls through to step 4, which builds its operation
    /// from the server's own freshly re-derived `undo` — not from `claimed` —
    /// and reaches the planner, which really executes it. Manually verified
    /// (not via `failure-atlas`'s `mutation_check`, whose clone reflects git
    /// HEAD and so cannot see this test while it is uncommitted): applying
    /// exactly that mutation flips this test from green to a panic reporting
    /// `left: 200, right: 409` with the response body `"Reset 'main' to
    /// a699791."` — the branch actually moved, to the server's own
    /// re-derived tip, not to the stale value `claimed` carried. Both
    /// assertions below are live nets against that outcome. The call is
    /// wrapped in [`crate::operations::with_key`] so the planner is actually
    /// reachable under the mutation rather than refusing one step earlier at
    /// its own idempotency-header check, which would still catch the
    /// mutation but would prove less. A bare `assert!(recover_body.contains(
    /// ...))` substring check on the source text (`contract_suite.rs`) stays
    /// green through this exact mutation, since the text `undo != claimed`
    /// remains in the file — that is the gap this test closes.
    #[tokio::test]
    async fn a_stale_claimed_undo_is_refused_and_the_branch_is_left_alone() {
        crate::state::with_isolated_test_current(
            a_stale_claimed_undo_is_refused_and_the_branch_is_left_alone_in_scope(),
        )
        .await;
    }

    async fn a_stale_claimed_undo_is_refused_and_the_branch_is_left_alone_in_scope() {
        let f = fixture();
        crate::state::set_current(&f.repo, RepoMode::Active);

        let id = OperationId::new("recover-stale-claim").unwrap();
        let strategy = reset_ref_to(&f.first);
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let mut durable_row = row(id.as_str(), 1_000);
        durable_row.operation = GitOperation::StageAll;
        durable_row.recovery = Some(strategy.clone());
        // This test's own concern is the equality gate (step 3), not the
        // worktree-scope check (step 1b) — give it the current selection's
        // real tokens so it reaches step 3 rather than being refused one
        // step earlier by an unrelated (also correct) gate. `row()`'s
        // placeholder "w"/"r" tokens are deliberately foreign; see
        // `a_row_from_a_foreign_worktree_is_refused_not_executed_against_the_current_selection`
        // for the test that exercises that gate.
        let (repository, worktree) = crate::planner::selection_tokens();
        durable_row.repository = repository;
        durable_row.worktree = worktree;
        crate::durable::persist(
            IdempotencyKey::new("recover-stale-claim-key").unwrap(),
            durable_row,
        )
        .await;

        // What the client's page is holding: the offer as it looked before the
        // branch moved again.
        let stale_tip = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        let claimed = UndoAction::ResetBranch {
            branch: "main".to_string(),
            to: f.first.clone(),
            expected_tip: stale_tip,
        };

        // The branch moves again — a third commit — so `recover_operation`'s
        // own live re-classification now offers a different `expected_tip`
        // than the one `claimed` above carries.
        std::fs::write(f.repo.join("a.txt"), "c\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["commit", "-qam", "third"])
            .current_dir(&f.repo)
            .status()
            .unwrap()
            .success());
        let tip_before_call = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();

        // Scoped with a real idempotency key, the same task-local the
        // idempotency middleware sets up for a real request: without it, a
        // neutered gate would still be caught (the planner's own
        // `current_key()` check refuses with 400 before touching git), but
        // that would prove less than the real risk this pins — a genuine
        // client request always carries this header, so the branch-unmoved
        // assertion below must be exercised with the planner actually
        // reachable, not merely blocked one step earlier for an unrelated
        // reason.
        let (status, body) = crate::operations::with_key(
            IdempotencyKey::new("recover-stale-claim-req").unwrap(),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            recover_operation(AxumPath(id.as_str().to_string()), Json(claimed)),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a stale claim must be refused, not executed against a different \
             target — body was: {body}"
        );
        assert!(
            body.contains("changed"),
            "the refusal must say the offer changed, not something unrelated: {body}"
        );

        let tip_after_call = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tip_after_call, tip_before_call,
            "a refused recovery must never move the branch — not to the stale \
             value the client claimed, and not to the server's own freshly \
             re-derived one either"
        );
    }

    /// **The bug this pins:** `recover_operation` must classify and execute
    /// against the row's OWN worktree, never silently against whatever
    /// happens to be the current selection. `select_operations_blocking`
    /// filters on `repository` alone (`durable.rs`), so one history page can
    /// legitimately mix rows from several worktrees of one clone
    /// (`identity.rs`: a main working tree and every linked worktree share a
    /// `RepositoryId` but carry distinct `WorktreeId`s/HEADs). A row from a
    /// foreign worktree must never be classified — let alone executed —
    /// against the currently selected one.
    ///
    /// This row's `strategy`/`recovery` and the live pin are set up exactly
    /// as the "with a live pin" test, so that if the handler ever falls back
    /// to classifying against the current selection (`f.repo`), it finds a
    /// genuine `Offered` whose `undo` matches `claimed` exactly — the gate at
    /// step 3 would then have nothing to catch, and step 5 would really
    /// reset `main`. Only a worktree-scope check catches this.
    ///
    /// Goes red under the mutation this pins against: delete the `if
    /// row.worktree != current_worktree` block added at recovery_center.rs
    /// (the fix for this finding). Without it, this test's status assertion
    /// flips from 409 to 200 and the branch-unmoved assertion fails — `main`
    /// is actually reset to `f.first`, proved by `tip_after_call` differing
    /// from `tip_before_call`.
    #[tokio::test]
    async fn a_row_from_a_foreign_worktree_is_refused_not_executed_against_the_current_selection() {
        crate::state::with_isolated_test_current(
            a_row_from_a_foreign_worktree_is_refused_not_executed_against_the_current_selection_in_scope(),
        )
        .await;
    }

    async fn a_row_from_a_foreign_worktree_is_refused_not_executed_against_the_current_selection_in_scope(
    ) {
        let f = fixture();
        crate::state::set_current(&f.repo, RepoMode::Active);

        // The worktree this request is actually about to run against, per
        // the same resolution the planner itself uses.
        let (_repo_token, current_worktree) = crate::planner::selection_tokens();

        let id = OperationId::new("recover-foreign-worktree").unwrap();
        let strategy = reset_ref_to(&f.first);
        // The pin lives in `f.repo` — the CURRENT selection — precisely so a
        // fallback to the current selection would find it and offer it.
        crate::durable::write_recovery_ref(&f.repo, &id, &strategy).await;

        let mut durable_row = row(id.as_str(), 2_000);
        durable_row.operation = GitOperation::StageAll;
        durable_row.recovery = Some(strategy.clone());
        // The row belongs to a worktree the request never selected. Any
        // literal that isn't the real UUID `current_worktree` holds proves
        // the point; spelled out for clarity rather than relying on `row()`'s
        // own default ("w") happening to differ.
        durable_row.worktree = WorktreeToken::new("foreign-worktree-id").unwrap();
        assert_ne!(
            durable_row.worktree, current_worktree,
            "the fixture must actually be foreign to the current selection"
        );
        crate::durable::persist(
            IdempotencyKey::new("recover-foreign-worktree-key").unwrap(),
            durable_row,
        )
        .await;

        // What a correct live re-derivation against `f.repo` would offer —
        // built to match exactly, so the equality gate (step 3) cannot be
        // what refuses this request; only the worktree-scope check can.
        let head = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        let claimed = UndoAction::ResetBranch {
            branch: "main".to_string(),
            to: f.first.clone(),
            expected_tip: head.clone(),
        };

        let (status, body) = crate::operations::with_key(
            IdempotencyKey::new("recover-foreign-worktree-req").unwrap(),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            recover_operation(AxumPath(id.as_str().to_string()), Json(claimed)),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a row from a foreign worktree must be refused, not classified or \
             executed against the current selection — body was: {body}"
        );
        assert!(
            body.contains("worktree"),
            "the refusal must name the actual reason (worktree scope), not \
             something unrelated: {body}"
        );

        let tip_after_call = crate::git_cmd::rev_parse(&f.repo, "HEAD")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tip_after_call, head,
            "a foreign-worktree row must never move the current selection's \
             branch, even though its own live re-derivation would have been \
             a genuine, matching offer"
        );
    }
}
