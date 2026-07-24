//! The typed Git-operation vocabulary and the reviewable [`Plan`] (M1.06a, #142).
//!
//! [`GitOperation`] is the **closed** set of every mutation git-vista can
//! perform against the served repository today — one variant per real mutation
//! found by auditing the write handlers, no catch-all. [`Plan`] is the preview
//! of one such operation: which worktree it targets (opaque ids, never paths),
//! the [`GenerationToken`] of the state the user reviewed (ADR 0001), a binding
//! [`OperationHash`], an expiry window, a [`RiskLevel`], machine-checkable
//! [`Precondition`]s, the [`RefChange`]s the operation is expected to make, and
//! a typed [`RecoveryStrategy`].
//!
//! Every field is typed, none is free text: the string-shaped values are
//! validating newtypes whose `Deserialize` rejects malformed input at the wire
//! boundary (a 400, not a value a handler might act on). #145 builds on this:
//! at execution time the server re-checks generation equality, recomputes the
//! operation hash, and refuses an expired plan — all three checks are plain
//! comparisons over these fields.
//!
//! ## The audited vocabulary (what maps to what)
//!
//! | Write endpoint | git effect | variant |
//! |---|---|---|
//! | `POST /api/branch` | `git branch <name> <commit>` | [`GitOperation::CreateBranch`] |
//! | `POST /api/commit` (HEAD path) | `git commit [--allow-empty] -m` | [`GitOperation::CommitOnHead`] |
//! | `POST /api/commit` (stub path) | `git commit-tree` + CAS `git update-ref` | [`GitOperation::EmptyCommitOnBranch`] |
//! | `POST /api/stage` | `git add -A` | [`GitOperation::StageAll`] |
//! | `POST /api/unstage` | `git reset -q HEAD` | [`GitOperation::UnstageAll`] |
//! | `POST /api/checkout` | `git checkout <branch>` | [`GitOperation::CheckoutBranch`] |
//! | `POST /api/merge` | `git merge --no-edit <branch>` | [`GitOperation::MergeBranch`] |
//! | `POST /api/push` | `git push origin <branch>` | [`GitOperation::PushBranch`] |
//! | `POST /api/delete-branch` | `git branch -d <branch>` | [`GitOperation::DeleteBranch`] |
//! | `POST /api/force-delete-branch` | `git branch -D <branch>` | [`GitOperation::ForceDeleteBranch`] |
//! | `POST /api/rebase` | `git rebase <base>` (abort on failure) | [`GitOperation::RebaseOntoBase`] |
//! | `POST /api/undo` (restore) | `git branch <name> <tip>` | [`GitOperation::RestoreBranch`] |
//! | `POST /api/undo` (reset) | `git reset --hard` / `git branch -f` (CAS) | [`GitOperation::ResetBranch`] |
//! | `POST /api/undo` (revert) | `git revert --no-edit <commit>` | [`GitOperation::RevertCommit`] |
//! | `POST /api/reset-test-repo` | seeded composite restore | [`GitOperation::ResetTestRepo`] |
//!
//! `POST /api/clone`, `/api/delete-clone`, `/api/select` and `/api/rescan` are
//! deliberately **not** operations: they manage the catalog / app session (which
//! repository is served, whether a clone exists on disk) and never mutate a
//! served repository's refs, index, or working tree — the state a plan's
//! generation and preconditions are defined over. ADR 0015 records this scope
//! decision.

use std::fmt;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Field validation
// ---------------------------------------------------------------------------

/// Why a plan field failed validation, typed (used as the serde error message
/// when a malformed value arrives on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFieldError {
    /// The field is empty (or whitespace-only) where a value is required.
    Empty(&'static str),
    /// The value starts with `-`, so git could read it as an option.
    OptionShaped(&'static str),
    /// The value is not the required lowercase-hex shape.
    NotHex {
        field: &'static str,
        expected: &'static str,
    },
}

impl fmt::Display for PlanFieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanFieldError::Empty(field) => write!(f, "{field} can't be empty"),
            PlanFieldError::OptionShaped(field) => write!(f, "{field} can't start with '-'"),
            PlanFieldError::NotHex { field, expected } => {
                write!(f, "{field} must be {expected} lowercase hex characters")
            }
        }
    }
}

impl std::error::Error for PlanFieldError {}

/// Non-empty after trimming — the same test every write handler applies.
fn require_non_empty(value: &str, field: &'static str) -> Result<(), PlanFieldError> {
    if value.trim().is_empty() {
        return Err(PlanFieldError::Empty(field));
    }
    Ok(())
}

/// Non-empty and not option-shaped — the belt-and-braces check the handlers
/// run before a name goes anywhere near a git argv.
fn require_git_safe(value: &str, field: &'static str) -> Result<(), PlanFieldError> {
    require_non_empty(value, field)?;
    if value.starts_with('-') {
        return Err(PlanFieldError::OptionShaped(field));
    }
    Ok(())
}

fn require_hex(
    value: &str,
    lens: &[usize],
    field: &'static str,
    expected: &'static str,
) -> Result<(), PlanFieldError> {
    let hex_ok = lens.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex_ok {
        Ok(())
    } else {
        Err(PlanFieldError::NotHex { field, expected })
    }
}

/// Declare a validated, string-backed newtype: serializes as a bare JSON
/// string, and `Deserialize` runs the same validator as [`Self::new`] so a
/// malformed value is a hard wire error, never a smuggled payload.
macro_rules! validated_string {
    ($(#[$doc:meta])* $name:ident, $validate:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap a raw value.
            pub fn new(value: impl Into<String>) -> Result<Self, PlanFieldError> {
                let value = value.into();
                let validate: fn(&str) -> Result<(), PlanFieldError> = $validate;
                validate(&value)?;
                Ok(Self(value))
            }

            /// The raw wire value.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

validated_string!(
    /// Opaque id of the shared repository a plan targets — the string form of
    /// `git-vista-core`'s `RepositoryId`, kept opaque here exactly like
    /// [`RepositoryDescriptor::repository`](crate::RepositoryDescriptor::repository)
    /// (transport never learns paths; the catalog maps id → path, fail-closed).
    RepositoryToken,
    |v| require_non_empty(v, "repository id")
);

validated_string!(
    /// Opaque id of the specific worktree a plan targets — the string form of
    /// `git-vista-core`'s `WorktreeId`. A generation is per-worktree (ADR
    /// 0001), so this is the id [`Plan::generation`] is scoped to.
    WorktreeToken,
    |v| require_non_empty(v, "worktree id")
);

validated_string!(
    /// The repository-generation the plan was built against, as an **opaque
    /// token** compared only for equality (ADR 0001: no ordering, no "newer").
    /// Today it is the decimal form of the core `RepositoryGeneration` `u64`;
    /// carrying it as an opaque string lets a future algorithm version add a
    /// discriminator without a client-visible format break, exactly as ADR
    /// 0001's versioning note prescribes. #145 admits an execution only while
    /// the worktree's current token still equals this one.
    GenerationToken,
    |v| require_non_empty(v, "generation token")
);

validated_string!(
    /// SHA-256 of the plan's [`GitOperation`] in canonical JSON form (the
    /// exact bytes `serde_json::to_string` produces for it — field order is
    /// fixed by the struct definitions), as 64 lowercase hex characters.
    /// Binds an approval to one operation: #145 recomputes this from the
    /// operation it is about to execute and refuses on mismatch.
    OperationHash,
    |v| require_hex(v, &[64], "operation hash", "64")
);

validated_string!(
    /// A git object id — 40 (SHA-1) or 64 (SHA-256) lowercase hex characters,
    /// the same shape `git-vista-core`'s `ObjectId` enforces. Used wherever a
    /// plan pins an exact commit (CAS preconditions, recovery targets).
    CommitOid,
    |v| require_hex(v, &[40, 64], "commit id", "40 or 64")
);

validated_string!(
    /// A local branch's short name (`main`, `feature/x`) — non-empty and not
    /// option-shaped, the same gate every branch handler applies before the
    /// name reaches a git argv.
    BranchName,
    |v| require_git_safe(v, "branch name")
);

validated_string!(
    /// A ref as git resolves it — a full ref name (`refs/heads/main`), a
    /// remote-tracking name (`origin/main`), or the `HEAD` symref. Non-empty
    /// and not option-shaped.
    RefName,
    |v| require_git_safe(v, "ref name")
);

validated_string!(
    /// A commit message — non-empty (the same rejection `/api/commit` gives an
    /// empty trimmed message).
    CommitMessage,
    |v| require_non_empty(v, "commit message")
);

validated_string!(
    /// A configured remote's name (today always `origin` — the only remote the
    /// push handler addresses). Non-empty and not option-shaped.
    RemoteName,
    |v| require_git_safe(v, "remote name")
);

/// A moment as Unix seconds (UTC) — the clock [`Plan::issued_at`] and
/// [`Plan::expires_at`] are read on, matching the activity journal's `time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixSeconds(pub i64);

// ---------------------------------------------------------------------------
// The closed operation vocabulary
// ---------------------------------------------------------------------------

/// Every Git mutation git-vista can perform against the served repository —
/// the **closed** vocabulary (M1.06a, #142). One variant per real mutation the
/// write-handler audit found; there is deliberately no catch-all/"generic"
/// variant, so a new kind of mutation *must* extend this enum (and its wire
/// name is then pinned by the golden fixture).
///
/// Wire form: internally tagged on `"op"` with `snake_case` names — see the
/// module docs' table for the endpoint each variant is the mutation of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GitOperation {
    /// `git branch <name> <at>` — create a branch at a commit (`/api/branch`).
    CreateBranch { name: BranchName, at: CommitOid },
    /// `git commit [--allow-empty] -m <message>` on the checked-out branch
    /// (`/api/commit` with no branch, or with the checked-out branch, named).
    CommitOnHead {
        message: CommitMessage,
        allow_empty: bool,
    },
    /// An empty commit written onto a branch that is *not* checked out
    /// (`/api/commit` with another branch named): `git commit-tree` on the
    /// branch tip's own tree, then a compare-and-swap `git update-ref` from
    /// `expected_tip` — HEAD, index and working tree untouched.
    EmptyCommitOnBranch {
        branch: BranchName,
        message: CommitMessage,
        expected_tip: CommitOid,
    },
    /// `git add -A` — stage every working-tree change (`/api/stage`).
    StageAll,
    /// `git reset -q HEAD` — unstage everything, keeping every edit in the
    /// working tree (`/api/unstage`).
    UnstageAll,
    /// `git checkout <branch>` — move HEAD and the working tree
    /// (`/api/checkout`); git refuses if local changes would be overwritten.
    CheckoutBranch { branch: BranchName },
    /// `git merge --no-edit <branch>` into the checked-out branch
    /// (`/api/merge`).
    MergeBranch { branch: BranchName },
    /// `git push <remote> <branch>` (`/api/push`; the handler pushes to
    /// `origin`).
    PushBranch {
        branch: BranchName,
        remote: RemoteName,
    },
    /// `git branch -d <branch>` — the safe delete; git refuses an unmerged
    /// branch (`/api/delete-branch`).
    DeleteBranch { branch: BranchName },
    /// `git branch -D <branch>` — delete even when unmerged, discarding any
    /// commits only it holds (`/api/force-delete-branch`).
    ForceDeleteBranch { branch: BranchName },
    /// `git rebase <base>` of the checked-out branch (`/api/rebase`; base is
    /// `origin/main` when that remote-tracking ref exists, else `main`; a
    /// failed rebase is `--abort`ed to restore the pre-rebase state).
    RebaseOntoBase { base: RefName },
    /// `git branch <name> <tip>` — re-create a deleted branch at its journaled
    /// tip; the safe undo for a deletion (`/api/undo`).
    RestoreBranch { name: BranchName, tip: CommitOid },
    /// Move a branch back to `to`, undoing a merge/rebase/commit whose result
    /// still sits at the tip (`/api/undo`): `git reset --hard <to>` when the
    /// branch is checked out with a clean worktree, else `git branch -f`.
    /// `expected_tip` is compare-and-swap — refused if the branch moved.
    ResetBranch {
        branch: BranchName,
        to: CommitOid,
        expected_tip: CommitOid,
    },
    /// `git revert --no-edit <commit>` — the history-preserving undo for a
    /// commit that's already shared (`/api/undo`; `--abort`ed on conflict).
    RevertCommit { commit: CommitOid },
    /// Restore a seeded test repo to its recorded state
    /// (`/api/reset-test-repo`): unbundle seed objects, move every seeded
    /// branch back, forced checkout of the seeded HEAD, hard reset + clean,
    /// delete branches the seed doesn't know, wipe the app journal. Gated on
    /// a repo explicitly opted in with `gv --seed`; the whole composite is
    /// computed server-side from the seed, so the operation carries no fields.
    ResetTestRepo,
}

// ---------------------------------------------------------------------------
// The Plan
// ---------------------------------------------------------------------------

/// How much a reviewer is risking by approving the plan — a property of the
/// operation kind, surfaced so the UI can scale its confirmation ceremony.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Nothing can be lost: the operation only adds or re-arranges state that
    /// remains reachable (stage, unstage, checkout — git refuses a clobbering
    /// checkout itself).
    Safe,
    /// State moves but a journaled local undo exists (commit, merge, rebase,
    /// branch create/restore, safe delete of a merged branch).
    Reversible,
    /// Commits or working-tree state can become unreachable (force-delete,
    /// hard reset, revert conflicts aside, test-repo reset).
    Destructive,
    /// The effect leaves this machine (push) — no local undo can recall it,
    /// and git-vista never force-pushes.
    Remote,
}

/// One machine-checkable condition that must hold at execution time —
/// evaluated by the server against the live repository, not trusted from the
/// client. Internally tagged on `"check"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum Precondition {
    /// The ref resolves exactly to `oid` — the compare-and-swap guard (stale
    /// graph ⇒ refuse rather than clobber).
    RefAt { ref_name: RefName, oid: CommitOid },
    /// The ref exists (whatever it points at).
    RefExists { ref_name: RefName },
    /// No ref by this name exists (e.g. the branch a create would add).
    RefAbsent { ref_name: RefName },
    /// `branch` is the checked-out branch (HEAD's symbolic target).
    BranchCheckedOut { branch: BranchName },
    /// `branch` is *not* the checked-out branch (the stub-commit path).
    BranchNotCheckedOut { branch: BranchName },
    /// The working tree and index are clean (hard-reset path of a branch
    /// reset).
    CleanWorktree,
    /// A remote by this name is configured (push).
    RemoteConfigured { remote: RemoteName },
    /// The repository carries a recorded `gv --seed` (test-repo reset gate).
    SeedRecorded,
}

/// Where one side of a [`RefChange`] stands: the shape a ref has before the
/// operation, or is expected to have after it. Adjacently tagged
/// (`{"kind": …, "value": …}`) so unit and payload variants share one shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum RefState {
    /// The ref does not exist (before a create; after a delete).
    Absent,
    /// The ref points exactly at this commit.
    At(CommitOid),
    /// The ref is symbolic, pointing at another ref (HEAD before/after a
    /// checkout).
    Symbolic(RefName),
    /// The value is produced *by* the operation and unknowable until it runs
    /// (the new commit of a commit/merge/rebase/revert).
    Computed,
}

/// One ref the operation is expected to move, with its state on both sides —
/// the reviewable diff of the plan, and (where `before`/`after` are exact)
/// the values an execution-time check can verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefChange {
    /// The ref that moves — a full ref name, or `HEAD` for the symref itself.
    pub ref_name: RefName,
    pub before: RefState,
    pub after: RefState,
}

/// How the pre-operation state can be recovered once the operation has run —
/// typed, so the UI can *say* it and a later milestone can *offer* it.
/// Internally tagged on `"strategy"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum RecoveryStrategy {
    /// Nothing is destroyed, so no recovery is needed (stage; unstage keeps
    /// every working-tree edit).
    NotNeeded,
    /// Move `ref_name` back to `to` — the reset-style undo for a
    /// commit/merge/rebase whose result sits at the tip.
    ResetRef { ref_name: RefName, to: CommitOid },
    /// Re-create branch `name` at `at` — the undo for a deletion (after a
    /// force-delete this holds only until git gc prunes the commits).
    RecreateBranch { name: BranchName, at: CommitOid },
    /// Delete the branch the operation created (undo of a create/restore that
    /// added a ref and nothing else).
    DeleteCreatedBranch { name: BranchName },
    /// Check the previous branch back out (undo of a checkout).
    CheckoutPrevious { branch: BranchName },
    /// Revert the commit the operation lands (history-preserving recovery for
    /// an already-shared result).
    RevertCommit { commit: CommitOid },
    /// No recovery exists inside git-vista: the effect left the machine
    /// (push — the remote is ahead and we never force-push) or the discarded
    /// state was never journaled (test-repo reset wipes the journal).
    Irrecoverable,
}

/// The reviewable preview of one [`GitOperation`] (M1.06a, #142): everything a
/// user approves *before* the mutation runs, and everything #145 needs to
/// refuse a stale, tampered, or expired approval — each check a plain typed
/// comparison, none of it free text.
///
/// `#[serde(deny_unknown_fields)]` like every request body: a stray key is a
/// hard 400, never a silently-ignored value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Opaque id of the shared repository (never a path; ADR 0003).
    pub repository: RepositoryToken,
    /// Opaque id of the worktree the operation targets — the scope
    /// [`Plan::generation`] is meaningful in (ADR 0001).
    pub worktree: WorktreeToken,
    /// The generation of the reviewed state. Execution is admitted only while
    /// the worktree's live generation still *equals* this token (#145).
    pub generation: GenerationToken,
    /// The operation being previewed — one variant of the closed vocabulary.
    pub operation: GitOperation,
    /// SHA-256 (lowercase hex) of `operation`'s canonical JSON — recomputed
    /// and compared at execution time, binding approval to this exact
    /// operation (#145).
    pub operation_hash: OperationHash,
    /// When the plan was issued (Unix seconds, server clock).
    pub issued_at: UnixSeconds,
    /// When the plan stops being executable (Unix seconds, same clock).
    /// #145 refuses execution once `now > expires_at`.
    pub expires_at: UnixSeconds,
    /// How much approving this plan risks.
    pub risk: RiskLevel,
    /// Conditions the server re-checks against the live repository at
    /// execution time; any failure refuses the plan.
    pub preconditions: Vec<Precondition>,
    /// The refs the operation is expected to move, with before/after states.
    pub expected_ref_changes: Vec<RefChange>,
    /// How the pre-operation state can be recovered afterwards.
    pub recovery: RecoveryStrategy,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: char) -> CommitOid {
        CommitOid::new(byte.to_string().repeat(40)).unwrap()
    }

    #[test]
    fn validated_newtypes_accept_good_values() {
        assert!(BranchName::new("feature/m1.06").is_ok());
        assert!(RefName::new("refs/heads/main").is_ok());
        assert!(RefName::new("HEAD").is_ok());
        assert!(CommitOid::new("a".repeat(40)).is_ok());
        assert!(CommitOid::new("0".repeat(64)).is_ok());
        assert!(OperationHash::new("b".repeat(64)).is_ok());
        assert!(GenerationToken::new("1234567890").is_ok());
        assert!(RemoteName::new("origin").is_ok());
        assert!(CommitMessage::new("fix: a thing").is_ok());
    }

    #[test]
    fn validated_newtypes_reject_bad_values() {
        // Empty / whitespace-only.
        assert_eq!(
            BranchName::new(""),
            Err(PlanFieldError::Empty("branch name"))
        );
        assert_eq!(
            CommitMessage::new("   "),
            Err(PlanFieldError::Empty("commit message"))
        );
        // Option-shaped (could be read by git as a flag).
        assert_eq!(
            BranchName::new("-D"),
            Err(PlanFieldError::OptionShaped("branch name"))
        );
        assert_eq!(
            RefName::new("--force"),
            Err(PlanFieldError::OptionShaped("ref name"))
        );
        // Hex-shaped fields: wrong length, uppercase, non-hex.
        assert!(CommitOid::new("abc123").is_err());
        assert!(CommitOid::new("A".repeat(40)).is_err());
        assert!(OperationHash::new("g".repeat(64)).is_err());
        assert!(OperationHash::new("c".repeat(63)).is_err());
    }

    #[test]
    fn deserialize_runs_the_same_validation() {
        // Malformed wire values are hard errors, not smuggled payloads.
        assert!(serde_json::from_str::<BranchName>(r#""-D""#).is_err());
        assert!(serde_json::from_str::<CommitOid>(r#""not-hex""#).is_err());
        assert!(serde_json::from_str::<OperationHash>(r#""""#).is_err());
        assert!(serde_json::from_str::<GenerationToken>(r#""""#).is_err());
        // And well-formed ones round-trip transparently (bare JSON strings).
        let b: BranchName = serde_json::from_str(r#""main""#).unwrap();
        assert_eq!(serde_json::to_string(&b).unwrap(), r#""main""#);
    }

    #[test]
    fn operation_wire_names_are_stable_snake_case() {
        // Wire names are contract (like RepoMode's): pin them so a rename is
        // a deliberate, visible protocol change rather than an accident.
        assert_eq!(
            serde_json::to_string(&GitOperation::StageAll).unwrap(),
            r#"{"op":"stage_all"}"#
        );
        assert_eq!(
            serde_json::to_string(&GitOperation::ResetTestRepo).unwrap(),
            r#"{"op":"reset_test_repo"}"#
        );
        let del = GitOperation::ForceDeleteBranch {
            branch: BranchName::new("wip").unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&del).unwrap(),
            r#"{"op":"force_delete_branch","branch":"wip"}"#
        );
    }

    #[test]
    fn there_is_no_catch_all_operation() {
        // An unknown `op` tag must fail to deserialize — the vocabulary is
        // closed, with no generic escape hatch.
        assert!(serde_json::from_str::<GitOperation>(r#"{"op":"run_arbitrary_git"}"#).is_err());
        assert!(
            serde_json::from_str::<GitOperation>(r#"{"op":"generic","argv":["gc"]}"#).is_err()
        );
    }

    #[test]
    fn ref_state_wire_shape_is_adjacently_tagged() {
        assert_eq!(
            serde_json::to_string(&RefState::Absent).unwrap(),
            r#"{"kind":"absent"}"#
        );
        assert_eq!(
            serde_json::to_string(&RefState::Computed).unwrap(),
            r#"{"kind":"computed"}"#
        );
        let at = RefState::At(oid('a'));
        let json = serde_json::to_string(&at).unwrap();
        assert_eq!(json, format!(r#"{{"kind":"at","value":"{}"}}"#, "a".repeat(40)));
        assert_eq!(serde_json::from_str::<RefState>(&json).unwrap(), at);
        let sym = RefState::Symbolic(RefName::new("refs/heads/main").unwrap());
        let json = serde_json::to_string(&sym).unwrap();
        assert_eq!(serde_json::from_str::<RefState>(&json).unwrap(), sym);
    }

    #[test]
    fn plan_rejects_unknown_fields() {
        // Same wire posture as every request body: a stray key (a path, say)
        // is a hard error the server never acts on.
        let json = format!(
            r#"{{
              "repository": "r", "worktree": "w", "generation": "1",
              "operation": {{"op":"stage_all"}},
              "operation_hash": "{}",
              "issued_at": 1, "expires_at": 2, "risk": "safe",
              "preconditions": [], "expected_ref_changes": [],
              "recovery": {{"strategy":"not_needed"}},
              "path": "/etc"
            }}"#,
            "a".repeat(64)
        );
        assert!(serde_json::from_str::<Plan>(&json).is_err());
    }

    #[test]
    fn plan_round_trips_through_json() {
        let plan = Plan {
            repository: RepositoryToken::new("11111111-1111-5111-8111-111111111111").unwrap(),
            worktree: WorktreeToken::new("22222222-2222-5222-8222-222222222222").unwrap(),
            generation: GenerationToken::new("12345678901234567890").unwrap(),
            operation: GitOperation::ResetBranch {
                branch: BranchName::new("main").unwrap(),
                to: oid('a'),
                expected_tip: oid('b'),
            },
            operation_hash: OperationHash::new("c".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_753_300_000),
            expires_at: UnixSeconds(1_753_300_300),
            risk: RiskLevel::Destructive,
            preconditions: vec![
                Precondition::RefAt {
                    ref_name: RefName::new("refs/heads/main").unwrap(),
                    oid: oid('b'),
                },
                Precondition::CleanWorktree,
            ],
            expected_ref_changes: vec![RefChange {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                before: RefState::At(oid('b')),
                after: RefState::At(oid('a')),
            }],
            recovery: RecoveryStrategy::ResetRef {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                to: oid('b'),
            },
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);
    }
}
