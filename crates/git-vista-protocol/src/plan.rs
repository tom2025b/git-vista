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
//! | `POST /api/fetch` | `git fetch --progress <remote>` | [`GitOperation::FetchRemote`] |
//! | `POST /api/pull` | `git fetch <remote>` + `git merge`\|`git rebase <remote>/<branch>` | [`GitOperation::PullBranch`] |
//! | `POST /api/delete-branch` | `git branch -d <branch>` | [`GitOperation::DeleteBranch`] |
//! | `POST /api/force-delete-branch` | `git branch -D <branch>` | [`GitOperation::ForceDeleteBranch`] |
//! | `POST /api/rebase` | `git rebase <base>` (abort on failure) | [`GitOperation::RebaseOntoBase`] |
//! | `POST /api/undo` (restore) | `git branch <name> <tip>` | [`GitOperation::RestoreBranch`] |
//! | `POST /api/undo` (reset) | `git reset --hard` / `git branch -f` (CAS) | [`GitOperation::ResetBranch`] |
//! | `POST /api/undo` (revert) | `git revert --no-edit <commit>` | [`GitOperation::RevertCommit`] |
//! | `POST /api/reset-test-repo` | seeded composite restore | [`GitOperation::ResetTestRepo`] |
//! | `POST /api/discard-tracked-paths` | `git checkout -- <paths>` | [`GitOperation::DiscardTrackedPaths`] |
//! | `POST /api/delete-untracked-paths` | `git clean -f -- <paths>` | [`GitOperation::DeleteUntrackedPaths`] |
//! | `POST /api/amend-commit` | `git commit --amend [--allow-empty] -m` | [`GitOperation::AmendCommit`] |
//! | `POST /api/tag` | `git tag [-a -m <msg>] <name> <target>` | [`GitOperation::CreateTag`] |
//! | `POST /api/delete-tag` | `git tag -d <name>` | [`GitOperation::DeleteLocalTag`] |
//! | `POST /api/delete-remote-tag` | `git push <remote> --delete refs/tags/<name>` | [`GitOperation::DeleteRemoteTag`] |
//! | `POST /api/push-tag` | `git push <remote> refs/tags/<name>` | [`GitOperation::PushTag`] |
//!
//! Some rows above were staged as *(planned)* before any handler could reach
//! them: M2.19a (#222) and M2.20a (#227) land a typed contract — the
//! variant, its plan-building wiring in `git-vista-server`'s
//! `planner::shape`, its `sandbox` network classification, and the golden
//! fixture — ahead of any handler that could build one, so the dangerous
//! part of each (rewriting history; opening a socket with credentials on it)
//! is reviewed as its own slice. See each variant's own doc comment for the
//! full reasoning. M2.21f (#240) graduated the last two rows still marked
//! that way (`DeleteRemoteTag`, `PushTag`), so the table above carries no
//! `*(planned)*` row today — every variant now has a live route.
//! [`GitOperation::AmendCommit`] went through exactly that staging and
//! graduated: #222 landed the contract, #223 (ADR 0040) the execution.
//! The four tag rows (M2.21a, #235, ADR 0041) were staged the same way: the
//! typed contract — variants, `shape` wiring, network classification, golden
//! fixture — landed and was reviewed before any handler could build one, with
//! `planner::execute` refusing all four with `501`. M2.21d (#238, ADR 0048)
//! graduated the two **local** ones: `CreateTag` and `DeleteLocalTag` now have
//! routes and real execution. M2.21f (#240) graduated the remaining two,
//! `DeleteRemoteTag` and `PushTag` — the first tag code that opens a socket
//! with credentials on it, which is why they waited for a slice of their own
//! rather than landing with the other two.
//!
//! [`GitOperation::PushBranch`] is the one row that already had a handler and
//! was **widened** anyway (M2.20a, #227: `set_upstream` and `force`). Its
//! pre-existing combination still executes exactly as before; the new ones
//! are refused with `501` until M2.20g (#231).
//!
//! `POST /api/clone`, `/api/delete-clone`, `/api/select` and `/api/rescan` are
//! deliberately **not** operations: they manage the catalog / app session (which
//! repository is served, whether a clone exists on disk) and never mutate a
//! served repository's refs, index, or working tree — the state a plan's
//! generation and preconditions are defined over. ADR 0015 records this scope
//! decision.

use serde::{Deserialize, Serialize};

use crate::newtype::{
    require_git_safe, require_hex, require_non_empty, require_non_empty_bounded,
    require_remote_name, require_worktree_relative_path,
};

/// Why a plan field failed validation — see
/// [`newtype::PlanFieldError`](crate::newtype::PlanFieldError), re-exported
/// here because this is the module it was introduced with and the path every
/// caller already uses.
pub use crate::newtype::PlanFieldError;

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
    ///
    /// Almost always a commit; the one deliberate exception is
    /// [`RecoveryStrategy::RecreateTag`], whose `at` is *whatever object the
    /// deleted tag ref pointed at* — a *tag object* when the tag was
    /// annotated. The hex shape is identical and git addresses both the same
    /// way, so a sibling `TagObjectOid` newtype would duplicate the validator
    /// to encode a distinction no consumer switches on (see that variant's
    /// doc for why carrying the unpeeled oid is the whole point).
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

impl From<&BranchName> for RefName {
    /// Every local branch name *is* a ref name git can resolve, so this
    /// conversion is total — and it is total in the type system, not merely
    /// in practice: both newtypes validate with the identical
    /// [`require_git_safe`](crate::newtype) gate (non-empty, not
    /// option-shaped), so there is no `BranchName` whose contents `RefName`
    /// would refuse.
    ///
    /// It exists because the executors that take a ref
    /// (`git merge <ref>`, `git rebase <ref>`) are reached both from a
    /// branch-named operation ([`GitOperation::MergeBranch`]) and, since
    /// M2.20d (#230), from a pull integrating a *remote-tracking* name
    /// (`origin/main`) that is not a local branch at all. Widening those
    /// executors to [`RefName`] keeps the second case honestly typed; this
    /// impl is what keeps the first case from needing an `expect` on a
    /// constructor that cannot fail.
    fn from(branch: &BranchName) -> Self {
        Self(branch.0.clone())
    }
}

validated_string!(
    /// A commit message — non-empty (the same rejection `/api/commit` gives an
    /// empty trimmed message).
    CommitMessage,
    |v| require_non_empty(v, "commit message")
);

/// The most bytes a [`RemoteName`] may carry.
///
/// A cap for the reason [`MAX_TAG_MESSAGE_LEN`] is one: the value is
/// client-chosen and rides into a [`Plan`] that is hashed, journaled and
/// persisted. 100 bytes is far past any real nickname (`origin` is six) while
/// keeping a hostile megabyte-long "name" a wire-boundary 400.
pub const MAX_REMOTE_NAME_LEN: usize = 100;

validated_string!(
    /// The name of a remote **configured in the repository** — `origin`,
    /// `upstream`, `fork-2`. Never a URL and never a path.
    ///
    /// # This is a security boundary (ADR 0047)
    ///
    /// It was [`require_git_safe`] until #229's follow-up, which is
    /// non-empty and not option-shaped — and therefore accepted
    /// `https://attacker.example/r.git`. `git fetch` does not refuse an
    /// argument it cannot find in the config; it falls through to treating it
    /// as a transport target, so that value made the `remote` request field
    /// choose which host this server opens a socket to, with whatever
    /// credential helper or SSH agent the host offers it. The validator now
    /// refuses every URL and path shape — see
    /// [`newtype::require_remote_name`](crate::newtype::require_remote_name)
    /// for the exact rule, the table of shapes it refuses, and why it is
    /// *necessary but not sufficient* (a well-formed name can still be one the
    /// repository never configured, which the server's `RemoteConfigured`
    /// precondition is what actually catches).
    ///
    /// Running the validator from `Deserialize` — which the macro does — is
    /// what makes this reach `PullBranch`/`PushBranch`/`PushTag`/
    /// `DeleteRemoteTag` too: their `remote` fields are this type, so a
    /// submitted plan carrying a URL is a hard wire error before any handler
    /// sees it.
    RemoteName,
    |v| require_remote_name(v, "remote name", MAX_REMOTE_NAME_LEN)
);

validated_string!(
    /// A tag's short name (`v1.0.0`, `release/2026-08`) — non-empty and not
    /// option-shaped, exactly the [`require_git_safe`] gate [`BranchName`] and
    /// [`RefName`] apply before a name reaches a git argv (M2.21a, #235).
    TagName,
    |v| require_git_safe(v, "tag name")
);

/// The most bytes a [`TagMessage`] may carry (16 KiB).
///
/// A cap because — unlike [`CommitMessage`], whose contents the *server's own
/// handlers* already gate — a tag message rides inside a [`GitOperation`] that
/// is hashed, journaled, and (for an annotated tag) written verbatim into the
/// repository's object database. Unbounded client-chosen bytes in all three
/// places is exactly the "client input grows server-side state" concern
/// `require_token`'s length cap exists for. 16 KiB is generous for real
/// release notes (the kernel's longest tag messages are ~2 KiB) while keeping
/// a hostile 100 MB "message" a wire-boundary 400 instead of a stored blob.
pub const MAX_TAG_MESSAGE_LEN: usize = 16 * 1024;

validated_string!(
    /// An annotated tag's message body — non-empty after trimming (the same
    /// rejection [`CommitMessage`] gives) and at most
    /// [`MAX_TAG_MESSAGE_LEN`] bytes (M2.21a, #235; see the constant for why
    /// this one is bounded when `CommitMessage` is not).
    TagMessage,
    |v| require_non_empty_bounded(v, "tag message", MAX_TAG_MESSAGE_LEN)
);

validated_string!(
    /// A path relative to the worktree root, naming one file a discard/delete
    /// operation targets (#219, M2.18a): non-empty, not option-shaped (the
    /// same argv-injection defense every other name in this file gets), never
    /// absolute, never carrying a `..` component, never embedding a NUL byte.
    /// See [`newtype::require_worktree_relative_path`](crate::newtype::require_worktree_relative_path)
    /// for the exact rule and — this matters — for why it is *necessary but
    /// not sufficient*: a symlinked path component or final entry can still
    /// resolve outside the worktree with no `..` anywhere in the string, which
    /// is why the executor re-checks the live filesystem immediately before
    /// running (`git-vista-server`'s `planner::symlink_containment_guard`).
    WorktreePath,
    |v| require_worktree_relative_path(v, "path")
);

/// A moment as Unix seconds (UTC) — the clock [`Plan::issued_at`] and
/// [`Plan::expires_at`] are read on, matching the activity journal's `time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixSeconds(pub i64);

// ---------------------------------------------------------------------------
// The closed operation vocabulary
// ---------------------------------------------------------------------------

/// How a [`GitOperation::PullBranch`] integrates the fetched commits into the
/// checked-out branch (M2.20a, #227).
///
/// # There is deliberately no third variant, and no `Default`
///
/// `git pull` picks between merge and rebase from `pull.rebase` /
/// `branch.<name>.rebase` config when the caller says nothing — a *silent*
/// choice whose answer lives in a file this app never shows the user. Two
/// people running "Pull" on the same branch can therefore get two different
/// histories, and neither reviewed which. This enum has no `Auto`/`Default`
/// variant and derives no [`Default`] impl, and the field that holds it
/// carries no `#[serde(default)]`, so:
///
///   * a request body that omits `strategy` is a **deserialize error** (a 400
///     at the wire boundary), never a value some config file chose, and
///   * nothing in Rust can construct a `PullBranch` without naming one —
///     it is a compile error, not a lint.
///
/// The plan a user approves therefore always *says* which integration it is,
/// which is the entire point of putting it in the reviewed vocabulary rather
/// than resolving it inside the executor. M2.20d (#230, ADR 0044) honours that
/// at execution: `planner::pull` dispatches on this value and has no arm to
/// fall back to, and `handlers::pull` turns an absent field into a `400`
/// naming both legal values rather than a `422` about serde.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// `git pull --no-rebase` — a merge commit when the histories diverged.
    /// The checked-out branch's old tip stays reachable as a parent.
    Merge,
    /// `git pull --rebase` — replay the local-only commits on top of the
    /// fetched tip. The pre-pull commits are rewritten, so the old tip
    /// survives only in the reflog.
    Rebase,
}

/// Whether a [`GitOperation::PushBranch`] may overwrite what the remote
/// already has, and under what guard (M2.20a, #227).
///
/// # There is no bare-force variant, on purpose
///
/// A plain `git push --force` overwrites the remote branch with no regard for
/// what arrived there since the pusher last looked — it is how a teammate's
/// commits get silently destroyed. This enum makes that **structurally
/// unrepresentable**: there is no variant that means "force, unconditionally",
/// so no handler, no future refactor and no deserializable request body can
/// ask for one. The only force available is
/// [`ForcePublish::WithLease`], which carries the remote tip the user
/// reviewed and turns the push into a compare-and-swap.
///
/// This is the same posture as the rest of the file — see the module docs'
/// "no catch-all variant" note. A capability that must never exist is best
/// expressed as a type that cannot name it, rather than as a `bool` plus a
/// convention that everyone remembers to check.
///
/// Wire form: internally tagged on `"mode"` (`{"mode": "none"}` /
/// `{"mode": "with_lease", "expected_remote_tip": "<oid>"}`), the same
/// internally-tagged shape [`Precondition`] and [`RecoveryStrategy`] use.
///
/// `deny_unknown_fields` is on, but note serde only enforces it for the
/// **struct** variant of an internally-tagged enum: a stray key beside
/// `{"mode": "with_lease", …}` is a hard error (so a misspelled
/// `expected_remote_tip` cannot become a lease that pins nothing), while one
/// beside `{"mode": "none"}` is ignored. That asymmetry lands on the safe
/// side — the ignored case still yields [`ForcePublish::None`] — and
/// `plan_golden.rs`'s `no_wire_body_can_request_an_unguarded_force_push`
/// pins both halves so neither can drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ForcePublish {
    /// A fast-forward-only push: `git push <remote> <branch>`, exactly what
    /// git-vista has always done. Git itself refuses a non-fast-forward.
    None,
    /// `git push --force-with-lease=<branch>:<expected_remote_tip>` — force,
    /// but only while the remote branch still points where the reviewer saw
    /// it. If anything landed there in between, git refuses and nothing is
    /// lost.
    ///
    /// The oid is carried in the operation (and therefore bound by the plan's
    /// [`OperationHash`]) rather than re-read at execution time on purpose: a
    /// lease re-derived from a fresh `git ls-remote` would leave the race
    /// wide open — it would assert "the remote is where it was a millisecond
    /// ago", which is always true and protects nobody. The value that makes
    /// the lease mean anything is the one the *user reviewed*.
    WithLease {
        /// The remote-tracking tip the plan was reviewed against — the
        /// `<expect>` half of `--force-with-lease=<ref>:<expect>`, and the
        /// oid `shape` turns into a [`Precondition::RefAt`] on
        /// `refs/remotes/<remote>/<branch>`.
        expected_remote_tip: CommitOid,
    },
}

/// The annotation of a [`GitOperation::CreateTag`] — present ⇒ an annotated
/// tag (a real tag *object* with message, tagger and date), absent ⇒ a
/// lightweight tag (a bare ref, no object) (M2.21a, #235, ADR 0041).
///
/// # Why `sign` lives *inside* the annotation, not beside it
///
/// Git cannot sign a lightweight tag: the signature is embedded in the tag
/// object, and a lightweight tag has no object to embed it in. A flat
/// `CreateTag { message: Option<TagMessage>, sign: bool }` — the shape #235
/// sketched — would make `sign: true, message: None` representable: a signed
/// tag with no object to sign, which no git argv can honour. This crate's
/// standing posture ([`ForcePublish`]'s "no bare-force variant" note) is that
/// a state which must never execute is best made *unrepresentable* rather
/// than caught by convention, so the signing flag exists only where a tag
/// object exists to carry a signature.
///
/// # `sign` has no `#[serde(default)]`, like `PushBranch`'s new fields
///
/// A body that supplies an annotation but omits `sign` is a 400, not an
/// unsigned tag by silent default — every annotated-tag request states
/// whether it asks for a signature, the same "make every caller state both
/// answers" reasoning M2.20a applied to `set_upstream`/`force`. Where the
/// signing *key and config* come from is deliberately **not** modelled here:
/// that is M2.21e's territory (#74). Until it lands, `planner::exec_create_tag`
/// answers `sign: true` with `501` before building any argv — M2.21d (#238)
/// wired the unsigned halves only, and ADR 0048 records why a refusal beats
/// silently dropping the flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagAnnotation {
    /// The tag object's message body (`git tag -a|-s -m <message>`).
    pub message: TagMessage,
    /// Ask git to GPG-sign the tag object (`git tag -s`). Carried in the
    /// reviewed contract now; execution and signing config are #239's.
    pub sign: bool,
}

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
    /// `git push [--set-upstream] [--force-with-lease=…] <remote> <branch>`
    /// (`/api/push`; the handler pushes to `origin`).
    ///
    /// # M2.20a (#227) widened this variant rather than adding a sibling
    ///
    /// `set_upstream` and `force` are new here. A second "publish" variant
    /// would have left two ways to spell a push in a vocabulary whose whole
    /// premise is one variant per mutation — and worse, the *plain* one would
    /// have stayed the path of least resistance, so the safety this adds
    /// would have been opt-in. Widening makes every caller state both
    /// answers.
    ///
    /// Only the pre-existing combination — `set_upstream: false`,
    /// [`ForcePublish::None`] — executes today; that is exactly the argv
    /// `/api/push` has always run. `planner::execute` refuses the other
    /// combinations with `501` until M2.20g (#231) wires them, so this slice
    /// changes the *vocabulary* without changing what any live endpoint does.
    ///
    /// # Recovery is [`RecoveryStrategy::Irrecoverable`] for both force modes
    ///
    /// Not "we did not build an undo button": the effect **left the
    /// machine**. Once the remote has the objects, no local command can
    /// recall them from whoever has already fetched. With
    /// [`ForcePublish::WithLease`] it is stronger still — commits that were
    /// on the remote branch are no longer referenced there, and this app has
    /// no copy of a remote's reflog to offer back.
    PushBranch {
        branch: BranchName,
        remote: RemoteName,
        /// `--set-upstream`: also record `<remote>/<branch>` as this
        /// branch's upstream. A config write, not a history change, and the
        /// only field here that does not affect [`RiskLevel`].
        set_upstream: bool,
        /// Whether this push may overwrite the remote branch, and under what
        /// guard. [`ForcePublish::WithLease`] raises the plan's risk from
        /// [`RiskLevel::Remote`] to [`RiskLevel::Destructive`] and adds the
        /// lease's compare-and-swap [`Precondition`] — see
        /// [`ForcePublish`] for why no third, unguarded option exists.
        force: ForcePublish,
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
    /// Apply a partial staging selection (M2.17b, #213; `/api/staging/apply`):
    /// `git apply --cached` of `patch` (`--reverse` when `direction` is
    /// unstage), then `git add -- <paths>` / `git reset -q HEAD -- <paths>`
    /// for `whole_files`. The fields are the *built* selection, not the
    /// [`crate::PatchPlan`] wire form — the operation hash binds the exact
    /// patch bytes and pathspecs that execute, so what was previewed is
    /// provably what applies. Both are constructed server-side from the same
    /// plan by `patch_build::build_selected_patch`.
    StageSelection {
        direction: crate::patch_plan::StageDirection,
        /// The `diff-v1:` token of the base diff the selection was verified
        /// against at the gate. The executor re-mints and re-compares this
        /// **inside the coordinator lock** before running `git apply`: the
        /// handler's gate runs outside the lock, and `git apply` alone is a
        /// soft backstop (it applies mid-file hunks at drifted offsets when
        /// the context still matches). Carried in the operation so the hash
        /// binds it.
        expected_diff_generation: GenerationToken,
        patch: String,
        whole_files: Vec<String>,
    },
    /// `git checkout -- <paths>` — discard uncommitted changes to
    /// already-tracked paths, restoring each to its checked-out (index, else
    /// HEAD) version (`POST /api/discard-tracked-paths`, #219/#71). A
    /// **separate** variant from [`GitOperation::DeleteUntrackedPaths`] below
    /// — never the same operation parameterised by a bool — so each carries
    /// its own risk and recovery story in this table and in the golden
    /// fixture.
    ///
    /// # Recovery is honest, not optimistic
    ///
    /// If a path's discarded content was staged (index differs from HEAD)
    /// *before* this ran, that content's blob is still reachable in the
    /// object database until the next `git gc` — but git-vista offers no
    /// built-in "undo" button for this operation: no ref moved, no reflog
    /// entry exists, and there is nothing in this app's own journal to
    /// replay. A worktree-only edit (never staged) has no fallback at all —
    /// its only copy was the file this operation just overwrote.
    /// [`RecoveryStrategy::Irrecoverable`] is therefore what the plan
    /// declares (the honest "git-vista itself offers no undo" reading); the
    /// executor's response/journal text spells out the staged-until-gc
    /// nuance in words, rather than letting the strategy tag imply either
    /// more or less recoverability than that.
    DiscardTrackedPaths { paths: Vec<WorktreePath> },
    /// `git clean -f -- <paths>` — delete untracked paths from the working
    /// tree outright (`POST /api/delete-untracked-paths`, #219/#71). **No
    /// journal-backed undo exists for this at all**: an untracked path was
    /// never written to git's object database in the first place, so there
    /// is nothing anywhere in the repository to reset back to.
    /// [`RecoveryStrategy::Irrecoverable`] here is a literal fact about the
    /// repository, not merely "git-vista has no button for it" the way it is
    /// for [`GitOperation::DiscardTrackedPaths`] above — this is the first
    /// genuinely irreversible operation in the vocabulary (plan.rs:328's
    /// `Irrecoverable`, previously used only by push and test-repo-reset,
    /// applies here for a stronger reason than either of those).
    DeleteUntrackedPaths { paths: Vec<WorktreePath> },
    /// `git commit --amend [--allow-empty] -m <message>` — rewrite the
    /// checked-out branch's tip commit in place (a new message, and,
    /// with `allow_empty` when nothing is staged, an otherwise-empty
    /// commit) rather than adding a new commit on top of it
    /// (`POST /api/amend-commit`, M2.19, #72).
    ///
    /// # Staged in two reviewed slices — see the module docs' table note
    ///
    /// M2.19a (#222) landed this variant contract-only: the typed
    /// vocabulary, `git-vista-server`'s `planner::shape` wiring, and the
    /// golden fixture, settled and reviewed *before* any git ran. M2.19b
    /// (#223, ADR 0040) then wired the actual `git commit --amend`
    /// execution (`planner::exec_amend_commit`, built from
    /// `handlers::commit::amend_commit`) — a history-rewriting operation
    /// that earned a review of its own rather than riding in on the
    /// vocabulary change.
    ///
    /// # No `branch` field
    ///
    /// Unlike [`GitOperation::EmptyCommitOnBranch`], amend always targets
    /// whatever the checked-out branch's tip already is. There is no "amend
    /// a commit on a branch that isn't checked out" primitive the way there
    /// is an "add an empty commit to a stub branch" one — amending
    /// presupposes an existing commit to rewrite, and this app never lets a
    /// user pick some other branch's commit to amend. A future milestone
    /// that needs that is a new variant, not a field bolted on here.
    ///
    /// # `expected_tip` is a live compare-and-swap, not a carried value
    ///
    /// `shape` turns `expected_tip` into a [`Precondition::RefAt`] on the
    /// checked-out branch — the same CAS pattern
    /// [`GitOperation::ResetBranch`] and [`GitOperation::EmptyCommitOnBranch`]
    /// already use for their own expected-tip fields. Amending when the tip
    /// moved under the reviewer between plan and execution is *the*
    /// dangerous case here: the reviewer approved rewriting one specific
    /// commit, and the `Precondition` machinery is what makes "the tip is
    /// still what was reviewed" a live, execution-time check rather than a
    /// value this field merely carries around unread.
    ///
    /// # Recovery is `ResetRef`, deliberately not a new tag
    ///
    /// Moving the checked-out branch back to `expected_tip` fully restores
    /// the pre-amend state — exactly the "reset the ref back" recovery
    /// every other commit-creating operation in this vocabulary already
    /// uses ([`GitOperation::CommitOnHead`], [`GitOperation::MergeBranch`],
    /// [`GitOperation::RebaseOntoBase`]) via `shape`'s shared `head_moves`
    /// helper. Two more exotic tags were considered and rejected rather
    /// than picked by reflex:
    ///
    ///   - [`RecoveryStrategy::RecoverableIfStaged`] is #219's tag for
    ///     **working-tree content**, whose recoverability depends on
    ///     whether it happened to have been staged before a discard. A
    ///     commit's content has no such conditional — it is always in the
    ///     object database by definition — so that question has no
    ///     analogue here.
    ///   - [`RecoveryStrategy::Irrecoverable`] would misstate a case that
    ///     is trivially restorable by moving one ref back.
    ///
    /// What *is* worth saying honestly: the amended-away commit sits on no
    /// ref afterward, so — like every commit `ResetRef` recovers — it
    /// survives only in the reflog until the next `git gc` (the default
    /// ~90-day reflog expiry). That caveat is not unique to amend; it
    /// already applies silently to `CommitOnHead`'s and `MergeBranch`'s
    /// `ResetRef` recovery too, so giving amend a different tag for the
    /// same property would single it out inconsistently rather than fix a
    /// real gap.
    ///
    /// # Divergence from an already-pushed tip: flagged, never blocked
    ///
    /// Amending a commit that has already been pushed makes the local and
    /// remote branch diverge — a real consequence this contract does not
    /// model in the *plan*: `shape`'s `Precondition::RefAt` only ever
    /// checks the **local** ref (matching every other CAS precondition in
    /// this file), and this variant carries no remote/tracking-ref field.
    /// #223 (ADR 0040) made the execution-time decision: the server checks
    /// whether the amended-away commit is reachable from any
    /// remote-tracking ref and reports it as the advisory
    /// `amended_published_commit` flag on the success response
    /// (`AmendCommitSuccess` in `dto.rs`) — it never refuses on it, since
    /// amending published history knowingly is legitimate and the
    /// pre-flight ceremony is the client's (M2.19d). Network need is
    /// unaffected: amending never itself talks to a remote (the
    /// reachability check walks local `refs/remotes/*`; see
    /// `sandbox::network_need_for_operation`).
    AmendCommit {
        message: CommitMessage,
        expected_tip: CommitOid,
        allow_empty: bool,
    },
    /// `git fetch --progress <remote>` — download the remote's objects and
    /// update its remote-tracking refs (`refs/remotes/<remote>/*`), touching
    /// no local branch, no index and no working tree (`POST /api/fetch`,
    /// M2.20, #73/#229).
    ///
    /// # Staged in two slices (M2.20a #227, then M2.20c #229)
    ///
    /// The vocabulary and the network classification landed first, with
    /// `planner::execute` refusing the variant — the same staging #222 used
    /// for `AmendCommit`, so both got reviewed before any code opened a
    /// socket. M2.20c (#229, ADR 0043) wired execution: streamed
    /// [`TransferProgress`](crate::operation::TransferProgress) on the
    /// operation's own record, a cancel that terminates the child process,
    /// and a typed failure taxonomy
    /// ([`FetchFailureKind`](crate::dto::FetchFailureKind)).
    ///
    /// # `RiskLevel::Safe`, and why that is not complacency
    ///
    /// Fetch is the only network operation in this vocabulary that risks
    /// nothing a user owns. It **adds** objects and rewrites refs under
    /// `refs/remotes/`, which are a cache of what the remote said — nothing
    /// under `refs/heads/`, nothing staged, nothing in the working tree. If
    /// the remote force-pushed, the old remote-tracking value is indeed
    /// replaced, but that value was never local work; it was this app's
    /// record of somebody else's branch, and it is re-derivable by fetching
    /// again.
    ///
    /// # Recovery is [`RecoveryStrategy::NotNeeded`], not `Irrecoverable`
    ///
    /// Reaching for `Irrecoverable` here because "it is a network
    /// operation" would be picking the closest-looking tag by reflex, and it
    /// would be wrong in the direction that matters: it would tell a UI to
    /// warn a user about an operation that cannot lose their work, training
    /// them to click through the warnings that *do* matter (push, pull).
    /// `Irrecoverable` is reserved for effects that left the machine or
    /// state that was never journaled — fetch is neither.
    ///
    /// # Why it still declares `NetworkNeed::Remote`
    ///
    /// Low *risk* and high *reach* are independent axes. Fetch opens a
    /// socket, so it needs the network tier — and, since #229 executes it,
    /// the credential handling that tier brings (#228's forced
    /// `core.askpass=` and output redaction). See
    /// `sandbox::network_need_for_operation`.
    FetchRemote { remote: RemoteName },
    /// `git pull --no-rebase|--rebase <remote> <branch>` — fetch, then
    /// integrate the fetched tip into the **checked-out** branch (planned
    /// `POST /api/pull`, M2.20, #73/#230).
    ///
    /// # Contract only (M2.20a, #227)
    ///
    /// Typed and classified by M2.20a (#227); executed by M2.20d (#230, ADR
    /// 0044) as `git fetch` (through the *same* executor `FetchRemote` uses)
    /// followed by `git merge --no-edit` or `git rebase` against
    /// `<remote>/<branch>`, dispatched on `strategy` alone.
    ///
    /// # `strategy` is mandatory, and that is the point
    ///
    /// See [`MergeStrategy`]: there is no `Auto` variant and no
    /// `#[serde(default)]` on this field, so a pull whose integration nobody
    /// chose cannot be constructed in Rust *or* deserialized off the wire.
    /// Leaving it optional would have let `pull.rebase` config decide — a
    /// value the reviewer never saw, in a file this app never shows.
    ///
    /// # `branch` is the remote's branch, not the local one
    ///
    /// It is the refspec argument (`git pull origin main` ⇒ `main` on
    /// `origin`). The *destination* is always whatever branch is checked
    /// out, exactly as for [`GitOperation::MergeBranch`] and
    /// [`GitOperation::RebaseOntoBase`] — this app never integrates into a
    /// branch that is not checked out.
    ///
    /// # `RiskLevel::Reversible` and `ResetRef` recovery
    ///
    /// A pull is a fetch (risk-free, above) plus an integration that moves
    /// one local ref. Moving that ref back to the tip the plan observed
    /// restores the pre-pull state completely, whichever strategy ran: after
    /// `Merge` the old tip is a parent of the merge commit, and after
    /// `Rebase` it is in the reflog. That is the same `ResetRef` story
    /// [`GitOperation::MergeBranch`] and [`GitOperation::RebaseOntoBase`]
    /// already have, and `shape` builds it with the same `head_moves`
    /// helper rather than a parallel copy.
    ///
    /// Deliberately *not* [`RecoveryStrategy::Irrecoverable`], which
    /// [`GitOperation::PushBranch`] has for a reason that does not apply
    /// here: a pull's effect never left this machine. The fetched objects
    /// are additive and the moved ref is local. The two operations sit on
    /// opposite sides of that line even though both talk to the same remote,
    /// which is precisely why collapsing them onto one tag would defeat the
    /// typed field.
    PullBranch {
        remote: RemoteName,
        branch: BranchName,
        strategy: MergeStrategy,
    },
    /// `git tag <name> <target>` (lightweight) or `git tag -a -m <message>
    /// <name> <target>` (annotated) — create a tag at a commit.
    ///
    /// # Staged in two slices (M2.21a #235 ADR 0041, then M2.21d #238 ADR 0048)
    ///
    /// M2.21a landed the vocabulary, risk ranking, plan shape and network
    /// classification with `planner::execute` refusing the operation — the
    /// same staging #222 used for `AmendCommit` and #227 for fetch/pull, so
    /// all of that was reviewed before any tag could be written. M2.21d then
    /// wired `POST /api/tag` and `planner::exec_create_tag`.
    ///
    /// `sign: true` is still refused with `501` (M2.21e, #74) — refused, not
    /// ignored: handing back an ordinary annotated tag for a request that
    /// asked for a signed one is a wrong outcome the user cannot see.
    ///
    /// # The annotation cannot be empty, and that is the no-editor guarantee
    ///
    /// [`TagAnnotation`] carries a [`TagMessage`], which cannot be empty. So
    /// "annotated" and "has a message" are the same fact in this type, and the
    /// executor's argv carries `-m <message>` whenever it carries `-a`. That
    /// matters more than it looks: `git tag -a` with no message launches
    /// `core.editor`, and a headless server has no editor and nobody to type
    /// into one — the request would hang forever or die on whatever `$EDITOR`
    /// happens to be. `git tag` has no `--no-edit` to close that after the
    /// fact, so making the empty-annotation state unrepresentable *is* the
    /// defence (ADR 0048).
    ///
    /// # One variant, kind chosen by `annotation` — not two variants
    ///
    /// Unlike [`GitOperation::DiscardTrackedPaths`] /
    /// [`GitOperation::DeleteUntrackedPaths`] — split because their risk and
    /// recovery stories differ — lightweight and annotated tag creation share
    /// one risk ([`RiskLevel::Reversible`]), one precondition shape
    /// (`RefAbsent` on `refs/tags/<name>`), and one recovery
    /// ([`RecoveryStrategy::DeleteCreatedTag`]). Two variants would be two
    /// spellings of one mutation. The states that must differ, differ in the
    /// type anyway: see [`TagAnnotation`] for why the signing flag lives
    /// inside the option, making a "signed lightweight tag"
    /// unrepresentable rather than refusable.
    ///
    /// # `target` is always the commit the tag speaks for
    ///
    /// For a lightweight tag the new ref points exactly at `target`; for an
    /// annotated tag the ref points at a *tag object* (created by the
    /// operation, so [`RefState::Computed`] in the plan) which itself points
    /// at `target`. Either way `target` is what the reviewer approves tagging.
    CreateTag {
        name: TagName,
        target: CommitOid,
        /// Present ⇒ annotated (and possibly signed); absent ⇒ lightweight.
        /// Absence is a *reviewed* value here, not a config-resolved default
        /// — the plan shows exactly which kind will be created — so an
        /// `Option` is honest where [`MergeStrategy`]'s missing-field-is-400
        /// posture guards against a choice some config file would otherwise
        /// make silently.
        annotation: Option<TagAnnotation>,
    },
    /// `git tag -d <name>` — delete a local tag. Contract M2.21a (#235),
    /// execution M2.21d (#238, ADR 0048) via `POST /api/delete-tag`.
    ///
    /// # This is `-D`-shaped, not `-d`-shaped, despite the flag
    ///
    /// `git branch -d` refuses to delete unmerged work, which is why
    /// [`GitOperation::DeleteBranch`] ranks [`RiskLevel::Reversible`]. `git
    /// tag -d` has **no such guard**: it deletes regardless of whether the
    /// tagged commit is reachable from anything else, so a tag that was the
    /// only ref keeping a commit alive takes that commit with it (reflogs
    /// don't cover tag refs). The shape therefore ranks it
    /// [`RiskLevel::Destructive`] with [`ForceDeleteBranch`]'s reasoning
    /// (`ForceDeleteBranch`), not `DeleteBranch`'s — and recovery is
    /// [`RecoveryStrategy::RecreateTag`] carrying the *exact* pre-delete ref
    /// value, which restores an annotated tag byte-identically (signature
    /// included) rather than minting a look-alike. See that variant's doc.
    ///
    /// [`ForceDeleteBranch`]: GitOperation::ForceDeleteBranch
    DeleteLocalTag { name: TagName },
    /// `git push <remote> --delete refs/tags/<name>` — delete a tag from a
    /// remote, via `POST /api/delete-remote-tag` (M2.21f, #240).
    ///
    /// # Classified M2.21a (#235); executed M2.21f (#240) — see [`GitOperation::CreateTag`]
    ///
    /// A **separate variant** from [`GitOperation::DeleteLocalTag`], never
    /// one operation parameterised by a "where" flag: one is a local ref
    /// edit ([`RiskLevel::Destructive`] locally recoverable via the object
    /// store), the other *leaves the machine* — it is a push under the hood,
    /// classified `NetworkNeed::Remote` and routed through the network
    /// tier's askpass hardening (ADR 0036) accordingly. Same split, same
    /// reasoning as discard-vs-delete in #219.
    ///
    /// # Recovery is [`RecoveryStrategy::Irrecoverable`], with one honest nuance
    ///
    /// The remote's ref is gone and no local command can restore *other
    /// clones'* view of it. If a same-named local tag still exists, a later
    /// [`GitOperation::PushTag`] can re-publish it — but the plan cannot
    /// promise the local tag survives until then, so the strategy tag stays
    /// the honest "git-vista offers no undo", exactly
    /// [`GitOperation::DiscardTrackedPaths`]'s posture of putting nuance in
    /// prose rather than optimism in the tag.
    DeleteRemoteTag { name: TagName, remote: RemoteName },
    /// `git push <remote> refs/tags/<name>` — publish one tag, via
    /// `POST /api/push-tag` (M2.21f, #240).
    ///
    /// # Classified M2.21a (#235); executed M2.21f (#240) — see [`GitOperation::CreateTag`]
    ///
    /// Pushes exactly the named tag — never `--tags` (publishing every local
    /// tag is not an operation this vocabulary can express, deliberately) and
    /// never `--force` (git refuses to move an existing remote tag, and no
    /// field here can ask it not to — the same
    /// structurally-unrepresentable posture as [`ForcePublish`]).
    /// [`RiskLevel::Remote`] and [`RecoveryStrategy::Irrecoverable`] for
    /// [`GitOperation::PushBranch`]'s reason: the effect leaves the machine,
    /// and whoever fetches the tag keeps it.
    PushTag { name: TagName, remote: RemoteName },
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
    /// checkout itself; fetch, which only adds objects and rewrites the
    /// remote-tracking cache under `refs/remotes/`).
    Safe,
    /// State moves but a journaled local undo exists (commit, merge, rebase,
    /// branch create/restore, safe delete of a merged branch).
    Reversible,
    /// Commits or working-tree state can become unreachable (force-delete,
    /// hard reset, revert conflicts aside, test-repo reset), **or** commits
    /// on a remote branch can (a [`ForcePublish::WithLease`] push, M2.20a).
    ///
    /// That last case is why a lease-force push is `Destructive` and not
    /// [`RiskLevel::Remote`]. The two tags describe different axes — how far
    /// the effect reaches vs. whether anything is destroyed — and this is
    /// one scalar, so the ranking has to pick. It picks the one that scales
    /// the UI's confirmation ceremony *up*: an ordinary push adds to the
    /// remote and can be followed by another commit, while a lease-force can
    /// leave a colleague's commits referenced by nothing.
    Destructive,
    /// The effect leaves this machine and adds to it (a fast-forward push,
    /// [`ForcePublish::None`]) — no local undo can recall what the remote
    /// and its other clients already have.
    ///
    /// A force push is *not* this tag: `git-vista` cannot express an
    /// unguarded force at all (see [`ForcePublish`]), and the one guarded
    /// form it can express is ranked [`RiskLevel::Destructive`] above.
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
    /// Point `refs/tags/<name>` back at `at` — the undo for a
    /// [`GitOperation::DeleteLocalTag`] (M2.21a, #235, ADR 0041), mirroring
    /// [`RecreateBranch`](Self::RecreateBranch) exactly as tag-delete mirrors
    /// branch-delete.
    ///
    /// # `at` is the *unpeeled* pre-delete ref value — that is the decision
    ///
    /// For a lightweight tag `at` is the tagged commit. For an **annotated**
    /// tag it is the **tag object's own oid** (what `git rev-parse
    /// refs/tags/<name>` returned before the delete, and what `git tag -d`
    /// prints as `(was <oid>)`) — *not* the peeled commit. `git tag -d`
    /// deletes only the ref; the tag object survives, dangling, until git gc
    /// prunes it, so `git update-ref refs/tags/<name> <at>` restores the tag
    /// **byte-identically**: same message, same tagger, same date, same GPG
    /// signature. #235's sketch (`{ name, target, message }`) would instead
    /// re-run `git tag -a` and mint a *look-alike* — new tagger, new date,
    /// signature gone forever, since no key this server will ever hold can
    /// re-sign as the original tagger. Carrying the one oid that makes exact
    /// recovery possible is the entire difference between an undo and a
    /// forgery.
    ///
    /// # "Until git gc" — and the pin that extends it
    ///
    /// Like every recovery that names a dangling object, this holds only
    /// until gc prunes it. But `durable`'s recovery pin
    /// (`refs/git-vista/recovery/<id>`, which `recovery_oid` feeds) points a
    /// real ref at `at`, keeping the tag object *reachable* — so taking this
    /// strategy's oid durable is also what protects it from gc. A
    /// message-carrying shape would have had nothing to pin.
    RecreateTag { name: TagName, at: CommitOid },
    /// Delete the tag the operation created (undo of a
    /// [`GitOperation::CreateTag`], which added `refs/tags/<name>` and —
    /// annotated — one now-unreferenced tag object, and nothing else). The
    /// tag sibling of [`DeleteCreatedBranch`](Self::DeleteCreatedBranch),
    /// separate because the ref namespace and the deleting command differ
    /// and a consumer switching on this type must be able to say "tag", not
    /// "branch".
    DeleteCreatedTag { name: TagName },
    /// Check the previous branch back out (undo of a checkout).
    CheckoutPrevious { branch: BranchName },
    /// Revert the commit the operation lands (history-preserving recovery for
    /// an already-shared result).
    RevertCommit { commit: CommitOid },
    /// No git-vista-driven undo, but the content may still exist as a
    /// dangling blob in git's object database until the next `git gc` —
    /// true exactly when the discarded content was `git add`ed at some point
    /// before this ran (`discard-tracked-paths`, #219). Distinct from
    /// [`Irrecoverable`](Self::Irrecoverable): whether recovery is even
    /// *possible* differs by the repository's actual history, not just by
    /// what git-vista chooses to offer — a caller that needs to distinguish
    /// "gone forever" from "maybe still in the object store" must not treat
    /// this the same as `Irrecoverable` (the review finding this variant
    /// exists to fix: both operations previously shared one tag, defeating
    /// the point of a typed field a future reader is expected to switch on
    /// rather than re-derive by also matching on [`GitOperation`]).
    RecoverableIfStaged,
    /// No recovery exists inside git-vista, and none is possible regardless:
    /// the effect left the machine (push — the remote is ahead and we never
    /// force-push), the discarded state was never journaled (test-repo reset
    /// wipes the journal), or the discarded state was never in git's object
    /// database to begin with (delete-untracked-paths, #219 — the one case
    /// where "irrecoverable" is a fact about the repository, not just about
    /// what git-vista offers).
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
        assert!(WorktreePath::new("src/lib.rs").is_ok());
        assert!(OperationHash::new("b".repeat(64)).is_ok());
        assert!(GenerationToken::new("1234567890").is_ok());
        assert!(RemoteName::new("origin").is_ok());
        assert!(CommitMessage::new("fix: a thing").is_ok());
        assert!(TagName::new("v1.0.0").is_ok());
        assert!(TagName::new("release/2026-08").is_ok());
        assert!(TagMessage::new("v1.0.0 — first stable release\n\nNotes here.").is_ok());
    }

    #[test]
    fn tag_newtypes_reject_bad_values() {
        // TagName: the same require_git_safe gate as BranchName — empty and
        // option-shaped are refused before a name could reach a git argv.
        assert_eq!(TagName::new(""), Err(PlanFieldError::Empty("tag name")));
        assert_eq!(
            TagName::new("-d"),
            Err(PlanFieldError::OptionShaped("tag name"))
        );
        // TagMessage: non-empty like CommitMessage…
        assert_eq!(
            TagMessage::new("  \n "),
            Err(PlanFieldError::Empty("tag message"))
        );
        // …and, unlike CommitMessage, bounded (see MAX_TAG_MESSAGE_LEN's doc).
        assert_eq!(
            TagMessage::new("x".repeat(MAX_TAG_MESSAGE_LEN + 1)),
            Err(PlanFieldError::TooLong {
                field: "tag message",
                max: MAX_TAG_MESSAGE_LEN
            })
        );
        // The paired positive at the exact boundary, so the cap is proven to
        // sit at MAX_TAG_MESSAGE_LEN and not one byte off.
        assert!(TagMessage::new("x".repeat(MAX_TAG_MESSAGE_LEN)).is_ok());
        // Deserialize runs the same validators (the wire is the boundary).
        assert!(serde_json::from_str::<TagName>(r#""-d""#).is_err());
        assert!(serde_json::from_str::<TagMessage>(&format!(
            "\"{}\"",
            "x".repeat(MAX_TAG_MESSAGE_LEN + 1)
        ))
        .is_err());
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
        // WorktreePath (#219): absolute and `..`-carrying paths are refused —
        // the exhaustive rule set lives in newtype.rs's own tests; this pins
        // that the newtype actually wires it in.
        assert!(WorktreePath::new("/etc/passwd").is_err());
        assert!(WorktreePath::new("../outside.txt").is_err());
        assert!(WorktreePath::new("-rf").is_err());
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
        let discard = GitOperation::DiscardTrackedPaths {
            paths: vec![WorktreePath::new("a.txt").unwrap()],
        };
        assert_eq!(
            serde_json::to_string(&discard).unwrap(),
            r#"{"op":"discard_tracked_paths","paths":["a.txt"]}"#
        );
        let delete = GitOperation::DeleteUntrackedPaths {
            paths: vec![WorktreePath::new("scratch/tmp.log").unwrap()],
        };
        assert_eq!(
            serde_json::to_string(&delete).unwrap(),
            r#"{"op":"delete_untracked_paths","paths":["scratch/tmp.log"]}"#
        );
        let amend = GitOperation::AmendCommit {
            message: CommitMessage::new("fix: typo").unwrap(),
            expected_tip: oid('a'),
            allow_empty: false,
        };
        assert_eq!(
            serde_json::to_string(&amend).unwrap(),
            format!(
                r#"{{"op":"amend_commit","message":"fix: typo","expected_tip":"{}","allow_empty":false}}"#,
                "a".repeat(40)
            )
        );
        // M2.21a (#235): the four tag operations' wire names and field order.
        let lightweight = GitOperation::CreateTag {
            name: TagName::new("v1.0.0").unwrap(),
            target: oid('a'),
            annotation: None,
        };
        assert_eq!(
            serde_json::to_string(&lightweight).unwrap(),
            format!(
                r#"{{"op":"create_tag","name":"v1.0.0","target":"{}","annotation":null}}"#,
                "a".repeat(40)
            )
        );
        let annotated = GitOperation::CreateTag {
            name: TagName::new("v1.0.0").unwrap(),
            target: oid('a'),
            annotation: Some(TagAnnotation {
                message: TagMessage::new("v1.0.0").unwrap(),
                sign: false,
            }),
        };
        assert_eq!(
            serde_json::to_string(&annotated).unwrap(),
            format!(
                r#"{{"op":"create_tag","name":"v1.0.0","target":"{}","annotation":{{"message":"v1.0.0","sign":false}}}}"#,
                "a".repeat(40)
            )
        );
        let delete_local = GitOperation::DeleteLocalTag {
            name: TagName::new("v1.0.0").unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&delete_local).unwrap(),
            r#"{"op":"delete_local_tag","name":"v1.0.0"}"#
        );
        let delete_remote = GitOperation::DeleteRemoteTag {
            name: TagName::new("v1.0.0").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&delete_remote).unwrap(),
            r#"{"op":"delete_remote_tag","name":"v1.0.0","remote":"origin"}"#
        );
        let push_tag = GitOperation::PushTag {
            name: TagName::new("v1.0.0").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&push_tag).unwrap(),
            r#"{"op":"push_tag","name":"v1.0.0","remote":"origin"}"#
        );
    }

    #[test]
    fn there_is_no_catch_all_operation() {
        // An unknown `op` tag must fail to deserialize — the vocabulary is
        // closed, with no generic escape hatch.
        assert!(serde_json::from_str::<GitOperation>(r#"{"op":"run_arbitrary_git"}"#).is_err());
        assert!(serde_json::from_str::<GitOperation>(r#"{"op":"generic","argv":["gc"]}"#).is_err());
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
        assert_eq!(
            json,
            format!(r#"{{"kind":"at","value":"{}"}}"#, "a".repeat(40))
        );
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
