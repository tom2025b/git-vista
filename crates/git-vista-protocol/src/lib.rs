//! `git-vista-protocol` — the versioned HTTP **transport contract** for git-vista.
//!
//! This crate is the boundary between the wire and the internals. It owns the
//! shapes that cross the HTTP/JSON edge and the rules for whether two peers may
//! talk at all, and nothing else:
//!
//! - [`version`] — the wire [`PROTOCOL_VERSION`], the `[min, max]` client window,
//!   the [`ProtocolInfo`] negotiation payload, the [`PROTOCOL_HEADER`] contract,
//!   and the pure [`check_compatibility`] verdict.
//! - [`error`]   — the [`ApiError`] envelope every endpoint returns on failure,
//!   its machine-readable [`ErrorCode`], and the [`RequestId`] correlation token.
//! - [`dto`]     — the shared request/response DTOs (branch/commit/clone bodies,
//!   rebase-status, the stash drawer's listing and write bodies) the server and
//!   frontend exchange.
//! - [`plan`]    — the closed, typed [`GitOperation`] vocabulary (every mutation
//!   the server can perform) and the reviewable [`Plan`] previewing one before
//!   execution (M1.06a, #142; enforcement lands with #145).
//! - [`operation`] — operation identity and lifecycle (M1.08, #61): the client's
//!   [`IdempotencyKey`], the server's [`OperationId`], the [`OperationState`]
//!   machine, the replayable [`OperationStatus`] record, and [`ProgressEvent`].
//! - [`status`]  — the generation-tagged [`WorktreeStatus`] DTO (M2.15, #68a):
//!   staged/unstaged/untracked/ignored/conflicted/renamed/submodule/binary
//!   states as a closed [`StatusEntry`] vocabulary, reusing [`GenerationToken`]
//!   (ADR 0001) rather than a new staleness mechanism.
//! - [`diff`]    — [`ParsedPatch`] (M2.16, #69a): unified-diff text parsed
//!   into files → hunks → lines with old/new line numbers, as a closed
//!   [`FileDiff`] vocabulary (ordinary edit, mode-change-only, binary, pure
//!   rename/copy, combined merge diff). [`DiffSpec`] (M2.16, #69b): the four
//!   diff modes (worktree-vs-index, index-vs-commit, commit-vs-commit,
//!   ref-vs-ref) as an explicit, closed source/target vocabulary, plus the
//!   pure [`diff_spec_argv`] mapping to `git diff`'s argv.
//! - [`newtype`] — the validating-newtype machinery the three above share, so
//!   every string-shaped wire value is checked in exactly one place.
//! - [`worktree`] — the read-only worktree census (M11.01, #546):
//!   [`WorktreeSibling`], [`Serviceable`] (the app's fence, kept separate from
//!   git's own `locked`/`prunable` flags), the fallible [`WorktreeCensus`],
//!   and [`parse_worktree_porcelain`] — the pure `git worktree list
//!   --porcelain` parser, sitting beside its own wire types the same way
//!   [`parse_porcelain_v2_z`] and [`parse_unified_diff`] do.
//!
//! ## Why a separate crate
//!
//! Transport is not domain. `git-vista-core` owns the repository, graph, and
//! identity model; letting those types double as the wire format would couple
//! contract compatibility to internal evolution — the exact trap V2_ARCHITECTURE
//! flags. So the dependency direction is deliberate and one-way:
//!
//! ```text
//!   git-vista-server  ─┐
//!                      ├─►  git-vista-protocol   (this crate)
//!   git-vista (wasm)  ─┘
//!
//!   git-vista-core  ──►  (nothing here) — core never depends on transport
//! ```
//!
//! Both the native server and the wasm frontend depend on this crate; it depends
//! on neither, and on nothing platform-specific. It is **pure and wasm-safe** —
//! no Axum, Leptos, tokio, gix, or filesystem — so the same types compile for and
//! (de)serialize identically on both sides. `git-vista-core` does *not* depend on
//! it, keeping the domain model free of transport concerns.

// `newtype` first, and `#[macro_use]`: it defines `validated_string!`, and a
// `macro_rules!` macro is only in scope for items declared *after* it.
#[macro_use]
pub mod newtype;

pub mod effects;

pub mod conflict;
pub mod diff;
pub mod dto;
pub mod error;
pub mod explain;
pub mod history;
pub mod operation;
pub mod patch_build;
pub mod patch_plan;
pub mod plan;
pub mod status;
pub mod version;
pub mod worktree;

pub use conflict::{
    ConflictSource, ConflictedFile, ContentResolutionRefused, Continuation, NotTextResolvable,
    Stage,
};
pub use diff::{
    diff_spec_argv, parse_unified_diff, path_or_dev_null, ComparisonBasis, DiffLine, DiffSpec,
    FileDiff, Hunk, LineKind, ParsedPatch,
};
pub use dto::{
    validate_clone_url, AmendCommitError, AmendCommitRequest, AmendCommitSuccess, AmendFailureKind,
    BranchFromStashRequest, BranchRequest, CloneRequest, CommitError, CommitFailureKind,
    CreateBranchRequest, CreateCommitRequest, CreateTagRequest, DeleteCloneRequest,
    DeleteRemoteTagRequest, DeleteTagRequest, DropContext, DropStashRequest, FetchError,
    FetchFailureKind, FetchRequest, FetchSuccess, HookPolicy, PullError, PullFailureKind,
    PullRequest, PullSuccess, PushRequest, PushStashRequest, PushTagRequest, RebaseStatus,
    RemoteRefUpdate, RepoMode, RepositoryDescriptor, RepositoryKind, ResolveConflictContentRequest,
    ResolveConflictRequest, SelectRequest, SessionInfo, SessionRequest, SignTagError,
    SignTagFailureKind, SignatureStatus, StashEntry, StashTarget, TagDetail, TagKind,
    WorktreePathsRequest, CLONE_IN_PROGRESS_SENTINEL,
};
pub use effects::{network_need_for_operation, IndexEffect, NetworkNeed, WorktreeEffect};
pub use error::{ApiError, ApiErrorBody, ErrorCode, RequestId};
pub use explain::{explain, Explanation, ExplanationFact, Section, Topic};
pub use history::{HeadState, HistoryFrame, HistoryPage};
pub use operation::{
    IdempotencyKey, OperationByKeyResponse, OperationId, OperationStage, OperationState,
    OperationStatus, ProgressEvent, TransferPhase, TransferProgress, MAX_IDEMPOTENCY_KEY_LEN,
    MAX_OPERATION_ID_LEN, PROGRESS_EVENT, RESULT_EVENT,
};
pub use patch_build::{build_selected_patch, canonical_path, SelectedPatch, SelectionMismatch};
pub use patch_plan::{
    FileSelection, HunkLines, HunkRef, PatchPlan, PatchPlanError, PatchPreview, SelectionShape,
    StageDirection, StagingDiff,
};
pub use plan::{
    Advisory, BranchName, CommitMessage, CommitOid, ForcePublish, GenerationToken, GitOperation,
    MergeStrategy, OperationHash, Plan, PlanFieldError, Precondition, RecoveryStrategy, RefChange,
    RefName, RefState, RemoteName, RepositoryToken, RiskLevel, StashMessage, StashSelector,
    TagAnnotation, TagMessage, TagName, UnixSeconds, WorktreePath, WorktreeToken,
    MAX_REMOTE_NAME_LEN, MAX_TAG_MESSAGE_LEN,
};
pub use status::{
    parse_porcelain_v2_z, ChangeKind, ChangeSides, ConflictKind, ParsedStatus, StatusEntry,
    SubmoduleState, WorktreeStatus,
};
pub use version::{
    check_compatibility, parse_protocol_header, Compatibility, ProtocolInfo, CSRF_HEADER,
    IDEMPOTENCY_HEADER, MAX_CLIENT_PROTOCOL, MIN_CLIENT_PROTOCOL, OPERATION_HEADER,
    PROTOCOL_HEADER, PROTOCOL_QUERY, PROTOCOL_VERSION, REQUEST_ID_HEADER,
};
pub use worktree::{
    parse_worktree_porcelain, Serviceable, WorktreeCensus, WorktreeListRecord, WorktreeSibling,
};
