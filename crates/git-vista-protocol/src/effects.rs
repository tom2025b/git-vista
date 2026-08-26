//! What an operation *does* to the working tree, the index and the network —
//! derived from the closed [`GitOperation`] vocabulary, never from argv.
//!
//! M6.39 (#92) needed two dimensions the [`Plan`](crate::Plan) does not carry
//! as fields: index/worktree effects and remote effects. Neither becomes a new
//! `Plan` field. Both are **derived accessors over the operation itself**, so
//! there is no wire-format change, no version gate, and none of the
//! `#[serde(default)]` hazard `Plan`'s own doc comment warns about.
//!
//! The precedent this follows is already in the tree:
//! [`network_need_for_operation`] classified the **typed operation**, while
//! its sibling `network_need(args: &[&str])` classifies **argv**. The argv
//! form is the "endpoint string" shape #92's acceptance criterion 1 forbids,
//! and it stays in the server where it belongs — it is a sandbox concern.
//!
//! ## Why every classifier here is an exhaustive match
//!
//! No catch-all `_ =>`, anywhere. A wildcard is exactly how a newly added
//! operation acquires a wrong explanation silently; an inexhaustive match is a
//! compile error, which is the whole benefit of a closed vocabulary. Adding an
//! operation should stop the build until someone decides what it does.

use crate::plan::GitOperation;
use serde::{Deserialize, Serialize};

/// What an operation does to the **working tree**.
///
/// Deliberately coarse. This answers "should I expect my files to change, and
/// how", not "which bytes" — the diff already answers that, and a taxonomy
/// fine enough to be interesting is fine enough to be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeEffect {
    /// Nothing in the working tree changes. Ref surgery, remote bookkeeping,
    /// and every read.
    Untouched,
    /// Tracked files are rewritten in place — a checkout, a merge that lands,
    /// a stash apply.
    FilesRewritten,
    /// Files are removed from the working tree.
    FilesRemoved,
    /// Files are rewritten and the operation may stop part-way with conflict
    /// markers on disk. Distinct from [`Self::FilesRewritten`] because the
    /// user may be left with work to finish, which is a different sentence.
    MayConflict,
}

/// What an operation does to the **index**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexEffect {
    /// The index is not touched.
    Untouched,
    /// Paths move from unstaged to staged.
    EntriesStaged,
    /// Paths move from staged to unstaged.
    EntriesUnstaged,
    /// Conflict stages collapse to a resolved entry.
    StagesResolved,
    /// The index is rebuilt wholesale against a new tree — checkout, reset,
    /// a landed merge.
    Rebuilt,
}

/// Whether a git invocation needs to reach the network. This is the axis
/// `policy_for` dispatches on (Task 8 / D3): the tier is a property of *what
/// the operation does*, not of the repository — with the single exception of
/// operator trust, which is a property of the repository and overrides both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkNeed {
    /// The subcommand talks to a remote (`push`/`fetch`/`clone`/`ls-remote`).
    /// Only the `Network` tier can serve it (a namespace breaks push, F3).
    Remote,
    /// Everything else — reads, commits, branch and ref manipulation, merges.
    /// These get the fuller-isolation `Strict` tier once it is available.
    Local,
}

/// **The authoritative classifier (D3).** The network need of a typed
/// [`GitOperation`] — what the server *decided to do*, not what its argv looks
/// like.
///
/// # Why this outranks `network_need`
///
/// `network_need`'s own doc comment already says a string match on argv cannot
/// be complete: aliases expand, plumbing reaches the network under names no
/// list will hold (`fetch-pack`, `send-pack`, transport helpers), and a partial
/// clone lazily fetches from otherwise-local commands. The C10 audit's
/// conclusion was that the dispatch must key on the *typed operation the server
/// chose*, because that value is known before any argv exists and is the only
/// thing that carries intent.
///
/// # Why a match with no wildcard arm
///
/// [`GitOperation`] is a closed enum, so this match is checked by the compiler.
/// Adding a variant **fails the build here** until somebody states what network
/// that operation needs — which is the whole reason this is a match and not a
/// lookup table or a `_ => Local` default. A default arm would silently
/// classify tomorrow's `FetchRemote` as `Local`, route it to the strict tier,
/// and break it at runtime instead of at compile time; worse, a default of
/// `Remote` would silently *widen* every new operation's sandbox. Neither
/// failure is acceptable, and the fix for both is to refuse to have a default.
///
/// That example stopped being hypothetical in M2.20a (#227), which added
/// `FetchRemote` and `PullBranch` for real. The build did fail here until
/// both arms existed — the guarantee this doc claims, observed working
/// rather than assumed (`network_need_for_operation` is the *only* thing in
/// the server that had to change for those two variants to be admitted to
/// the network tier).
///
/// If a variant's answer is ever unobvious, the tie-break is fail-closed:
/// `Local` routes to the stricter tier, so a misclassified network operation
/// breaks loudly rather than quietly gaining a socket.
pub fn network_need_for_operation(op: &GitOperation) -> NetworkNeed {
    match op {
        // The five operations in the enum that talk to a remote. `remote` is
        // part of each one's argv, but that is not why they are classified
        // here — they are classified here because reaching a remote is what
        // the server decided to do.
        //
        // M2.20a (#227) added the second and third before either executed,
        // and it would have been tempting to classify an operation that never
        // spawned anything as `Local` "for now". That would have been the
        // exact mistake this match exists to prevent: the declaration is what
        // picks the tier for the spawn, so a `Local` placeholder would be a
        // *wrong* answer sitting in the live data path waiting for execution
        // to arrive, and the arm that had to change would be in this file
        // rather than in the slice that wires the socket. Classify by what
        // the operation *is*, not by whether it runs today.
        //
        // That paid off exactly as intended in M2.20c (#229): wiring
        // `exec_fetch` needed **no change here at all** — the `Remote`
        // declaration this arm already carried is what routed the first real
        // fetch through the network tier and #228's askpass hardening.
        GitOperation::PushBranch { .. } => NetworkNeed::Remote,
        // `git fetch <remote>` — the whole operation is a network round trip;
        // `RiskLevel::Safe` says nothing local can be lost, which is an
        // independent axis from how far it reaches (see the variant's doc).
        GitOperation::FetchRemote { .. } => NetworkNeed::Remote,
        // `git pull` is a fetch plus an integration; the fetch half alone
        // settles this. Neither `MergeStrategy` changes the answer — merge
        // and rebase differ only in what happens after the objects arrive.
        GitOperation::PullBranch { .. } => NetworkNeed::Remote,
        // M2.21a (#235): both tag operations that reach a remote are pushes
        // under the hood — `git push <remote> refs/tags/<name>` and
        // `git push <remote> --delete refs/tags/<name>`. M2.21f (#240) wired
        // both for real execution, riding this same declaration unchanged —
        // exactly the reason the M2.20a comment above spells out: the
        // declaration is what picks the tier for the spawn, so classifying
        // it ahead of execution meant no change was needed here when the
        // execution arrived. That `DeleteRemoteTag` never says "push" in its
        // name changes nothing — this match keys on what the server decided
        // to do, and deleting a ref *on a remote* is a network round trip
        // with credentials on it.
        GitOperation::DeleteRemoteTag { .. } => NetworkNeed::Remote,
        GitOperation::PushTag { .. } => NetworkNeed::Remote,

        // Everything below manipulates refs, the index, the working tree or
        // the object database, all of it local. None of them opens a socket in
        // any configuration this server constructs: no `--recurse-submodules`,
        // no partial-clone promisor (a promisor fetch would make `CheckoutBranch`
        // and `RevertCommit` reach the network — see the note below), no
        // `git merge` of a remote-tracking ref this server does not create.
        // M3.24 (#77): the stash drawer is entirely local — refs/stash never
        // leaves the repository and no stash verb takes a remote.
        GitOperation::PushStash { .. } => NetworkNeed::Local,
        GitOperation::ApplyStash { .. } => NetworkNeed::Local,
        GitOperation::BranchFromStash { .. } => NetworkNeed::Local,
        GitOperation::DropStash { .. } => NetworkNeed::Local,
        // M4.31 (#84): `git checkout --ours|--theirs`, `git rm` and `git add`
        // read and write the local index and worktree only. The three versions
        // it chooses between are already in the object database — resolving a
        // conflict never needs to ask a remote anything.
        GitOperation::ResolveConflict { .. } => NetworkNeed::Local,
        // M4.31c (#432): a worktree write plus `git add` — the content itself
        // was composed client-side against sides already in the object
        // database, so this asks a remote nothing either.
        GitOperation::ResolveConflictContent { .. } => NetworkNeed::Local,
        GitOperation::CreateBranch { .. } => NetworkNeed::Local,
        GitOperation::CommitOnHead { .. } => NetworkNeed::Local,
        GitOperation::EmptyCommitOnBranch { .. } => NetworkNeed::Local,
        GitOperation::StageAll => NetworkNeed::Local,
        GitOperation::UnstageAll => NetworkNeed::Local,
        GitOperation::CheckoutBranch { .. } => NetworkNeed::Local,
        GitOperation::MergeBranch { .. } => NetworkNeed::Local,
        GitOperation::DeleteBranch { .. } => NetworkNeed::Local,
        GitOperation::ForceDeleteBranch { .. } => NetworkNeed::Local,
        GitOperation::RebaseOntoBase { .. } => NetworkNeed::Local,
        GitOperation::RestoreBranch { .. } => NetworkNeed::Local,
        GitOperation::ResetBranch { .. } => NetworkNeed::Local,
        GitOperation::RevertCommit { .. } => NetworkNeed::Local,
        GitOperation::RevertMerge { .. } => NetworkNeed::Local,
        // A cherry-pick reads one commit already in the object database and
        // writes a new one; nothing about it reaches a remote.
        // The sequencer is entirely local state under .git; driving it forward,
        // past, or backward never reaches a remote.
        GitOperation::SequenceContinue => NetworkNeed::Local,
        GitOperation::SequenceSkip => NetworkNeed::Local,
        GitOperation::SequenceAbort => NetworkNeed::Local,
        GitOperation::CherryPick { .. } => NetworkNeed::Local,
        GitOperation::CherryPickMerge { .. } => NetworkNeed::Local,
        // `git apply --cached` + pathspec add/reset: index-only, local.
        GitOperation::StageSelection { .. } => NetworkNeed::Local,
        GitOperation::ResetTestRepo => NetworkNeed::Local,
        // #219: `git checkout --`/`git clean -f` against named working-tree
        // paths — index/worktree only, never a remote.
        GitOperation::DiscardTrackedPaths { .. } => NetworkNeed::Local,
        GitOperation::DeleteUntrackedPaths { .. } => NetworkNeed::Local,
        // M2.19a (#222): `git commit --amend` rewrites the checked-out
        // branch's tip in place — index/object-database/ref work, never a
        // socket. Whether the *amended-away* commit had already been pushed
        // (and so now diverges from a remote-tracking ref) is a fact about
        // history, not about what this operation asks git to do over the
        // wire — #223's execution answers it with a walk of the local
        // `refs/remotes/*` cache (`planner::amended_commit_is_published`),
        // which opens no socket either, so `Local` stayed the truthful
        // declaration when execution landed (M2.19b, ADR 0040).
        GitOperation::AmendCommit { .. } => NetworkNeed::Local,
        // M2.21a (#235): `git tag [-a|-s]` writes a ref (and, annotated, one
        // tag object) into the local repository; `git tag -d` deletes a
        // local ref. Neither opens a socket in any configuration this server
        // constructs. Their remote-reaching siblings are classified
        // `Remote` above — the local/remote split across four variants
        // instead of a "where" flag on two is deliberate (see
        // `DeleteRemoteTag`'s doc in plan.rs).
        GitOperation::CreateTag { .. } => NetworkNeed::Local,
        GitOperation::DeleteLocalTag { .. } => NetworkNeed::Local,
    }
}
