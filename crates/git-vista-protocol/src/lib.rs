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
//!   rebase-status) the server and frontend exchange.
//! - [`plan`]    — the closed, typed [`GitOperation`] vocabulary (every mutation
//!   the server can perform) and the reviewable [`Plan`] previewing one before
//!   execution (M1.06a, #142; enforcement lands with #145).
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

pub mod dto;
pub mod error;
pub mod plan;
pub mod version;

pub use dto::{
    validate_clone_url, BranchRequest, CloneRequest, CreateBranchRequest, CreateCommitRequest,
    DeleteCloneRequest, RebaseStatus, RepoMode, RepositoryDescriptor, RepositoryKind,
    SelectRequest, SessionInfo, SessionRequest,
};
pub use error::{ApiError, ApiErrorBody, ErrorCode, RequestId};
pub use plan::{
    BranchName, CommitMessage, CommitOid, GenerationToken, GitOperation, OperationHash, Plan,
    PlanFieldError, Precondition, RecoveryStrategy, RefChange, RefName, RefState, RemoteName,
    RepositoryToken, RiskLevel, UnixSeconds, WorktreeToken,
};
pub use version::{
    check_compatibility, parse_protocol_header, Compatibility, ProtocolInfo, CSRF_HEADER,
    MAX_CLIENT_PROTOCOL, MIN_CLIENT_PROTOCOL, PROTOCOL_HEADER, PROTOCOL_VERSION, REQUEST_ID_HEADER,
};
