//! Operation identity, lifecycle, and progress (M1.08, #61).
//!
//! M1.06 gave every mutation a typed [`GitOperation`](crate::plan::GitOperation)
//! and a reviewable [`Plan`](crate::plan::Plan); M1.07 gave it a per-repository
//! guard. What was still missing is **identity over time**: a request had no
//! name, no recorded state, and no existence outside the connection carrying
//! it. So a dropped tunnel cancelled the git command, a retry looked exactly
//! like a fresh intent, and a lost response meant the outcome was unknowable.
//!
//! This module is the wire vocabulary that fixes that:
//!
//! - [`IdempotencyKey`] — the **client's** name for one user action, sent in
//!   the [`IDEMPOTENCY_HEADER`](crate::version::IDEMPOTENCY_HEADER) on every
//!   write. Two requests carrying the same key are the same intent: the second
//!   replays the first's result and runs no git at all.
//! - [`OperationId`] — the **server's** name for the accepted operation,
//!   returned in the [`OPERATION_HEADER`](crate::version::OPERATION_HEADER) and
//!   the handle for [`OperationStatus`] lookups and the progress stream.
//! - [`OperationState`] / [`OperationStage`] — where the operation has got to.
//! - [`OperationStatus`] — the record: the replayable result plus the
//!   post-execution generation and typed recovery a client needs to reconcile.
//! - [`ProgressEvent`] — one server-sent event on the operation's stream.
//! - [`TransferProgress`] / [`TransferPhase`] (M2.20c, #229) — *inside* the
//!   `Executing` stage, which phase of an object transfer git is in and how
//!   far through it is. A long fetch is otherwise one opaque "running".
//!
//! Everything here is transport only. The registry that holds records, decides
//! duplicates, and evicts, lives in the server; issue #62 makes it durable, and
//! [`OperationStatus`] is deliberately shaped to be the thing it persists.

use serde::{Deserialize, Serialize};

use crate::newtype::require_token;
use crate::plan::{
    GenerationToken, GitOperation, OperationHash, RecoveryStrategy, RepositoryToken, UnixSeconds,
    WorktreeToken,
};

/// Longest [`IdempotencyKey`] the server accepts. The client chooses this value
/// and it becomes a map key server-side, so it is bounded — generously enough
/// for a UUID or any sane opaque id, and far short of anything that could be
/// used to grow the registry's memory per request.
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;

/// Longest [`OperationId`]. Server-minted, so this is a sanity bound rather
/// than a defence.
pub const MAX_OPERATION_ID_LEN: usize = 64;

validated_string!(
    /// The client's name for **one user action** — minted per intent (per tap
    /// of Commit), *not* per HTTP attempt, and repeated verbatim on every retry
    /// of that attempt.
    ///
    /// That distinction is the whole design. A retry of a request whose
    /// response was lost carries the same key, so the server recognises it and
    /// replays the recorded outcome without running git a second time. A
    /// genuine second tap is a *new* key, a new operation, and is refused (or
    /// not) by the staleness gate exactly as it is today.
    ///
    /// Bounded and token-shaped (`[A-Za-z0-9_-]`, at most
    /// [`MAX_IDEMPOTENCY_KEY_LEN`]) because it arrives in an HTTP header and is
    /// echoed into logs: nothing here needs escaping anywhere it travels.
    IdempotencyKey,
    |v| require_token(v, "idempotency key", MAX_IDEMPOTENCY_KEY_LEN)
);

validated_string!(
    /// The server's opaque handle for one accepted operation, minted from the
    /// OS CSPRNG when the operation is admitted.
    ///
    /// Unguessable rather than sequential: a session-authenticated client can
    /// fetch any id it knows, so ids are not a capability to hand out by
    /// counting. Opaque to clients — round-tripped, never parsed.
    OperationId,
    |v| require_token(v, "operation id", MAX_OPERATION_ID_LEN)
);

/// Where an operation has got to. The lifecycle is
/// `Accepted → Running → (Succeeded | Failed)`, and the two terminal states are
/// terminal for good: a record never leaves one.
///
/// Refusals that happen *before* admission — read-only mode, a malformed body,
/// a missing key, protocol or CSRF failures — never become operations at all.
/// Nothing was attempted, so there is nothing to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    /// Admitted to the registry; the pipeline has not started work yet.
    Accepted,
    /// The pipeline is running — planning, waiting for the guard, or executing.
    Running,
    /// Finished with a success response (2xx), replayable from the record.
    Succeeded,
    /// Finished with a failure response (4xx/5xx), replayable from the record.
    /// A refusal *is* an outcome: "your commit was refused as stale" is an
    /// answer, where a lost connection is not.
    Failed,
}

impl OperationState {
    /// Whether this state is final — the record will not change again.
    pub fn is_terminal(self) -> bool {
        matches!(self, OperationState::Succeeded | OperationState::Failed)
    }
}

/// The step of the pipeline an operation is in — the progress detail behind
/// [`OperationState::Running`], and what the SSE stream reports as it moves.
///
/// These are the planner's real stages (M1.06/M1.07), not invented UI steps, so
/// a stuck operation names the thing it is stuck on: `Waiting` means another
/// mutation of this repository holds the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStage {
    /// Admitted, nothing done yet.
    Queued,
    /// Observing the repository and building the reviewable plan.
    Planning,
    /// Waiting for this repository's mutation guard (ADR 0019).
    Waiting,
    /// Re-checking the plan against the live repository — the staleness gate.
    Checking,
    /// Running the git command.
    Executing,
    /// Done; see the record's terminal state.
    Finished,
}

/// Which phase of an object transfer git reported (M2.20c, #229).
///
/// These are git's own `--progress` phases, in the order a fetch goes through
/// them, **not** invented UI steps — the same posture [`OperationStage`] takes
/// towards the planner's stages. They live beside [`OperationStage`] rather
/// than inside it because they are *not* pipeline stages: a fetch is in
/// `OperationStage::Executing` for the whole of this sequence, and folding
/// them into that enum would have made every existing exhaustive match over
/// `OperationStage` (the frontend has one) wrong the day a fetch ran.
///
/// Wire values are `snake_case`. The first three are reported *by the remote*
/// (git prefixes them `remote:`), the last two by the local process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    /// `remote: Enumerating objects: N, done.` — the remote is deciding what
    /// to send. Reports no percentage, only a running count.
    Enumerating,
    /// `remote: Counting objects: N% (a/b)`.
    Counting,
    /// `remote: Compressing objects: N% (a/b)`.
    Compressing,
    /// `Receiving objects: N% (a/b)` — bytes are arriving locally. The phase
    /// a user waits in, and the reason this vocabulary exists at all.
    Receiving,
    /// `Resolving deltas: N% (a/b)` — the local index is being built. Nothing
    /// has touched a ref yet at this point.
    Resolving,
}

/// How far through a [`TransferPhase`] git has got (M2.20c, #229).
///
/// Every field past `phase` is optional because git's own reporting is: the
/// `Enumerating` line carries a count and no percentage, and a phase's
/// closing `, done.` line may repeat the last numbers or not. A client that
/// wants a progress bar uses `percent` when present and falls back to naming
/// the phase; nothing here is ever synthesised to make the shape rectangular,
/// because a fabricated percentage is worse than an honest absent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferProgress {
    /// The phase this report is about.
    pub phase: TransferPhase,
    /// Percentage complete, 0-100, when git printed one.
    pub percent: Option<u8>,
    /// Objects done so far in this phase, when git printed a `(a/b)` pair —
    /// or the running count on the `Enumerating` line, which has no total.
    pub objects: Option<u64>,
    /// Objects in this phase in total, when git printed a `(a/b)` pair.
    pub total_objects: Option<u64>,
}

/// The full record of one operation — the response of
/// `GET /api/operations/{id}` and the payload of the stream's final event.
///
/// This is what a client that lost its response reads to reconcile. Beyond the
/// replayed status and message, it carries the two things that let the client
/// act without re-reading the whole repository: the **generation after
/// execution** (has the world moved on from what I have cached?) and the
/// **typed recovery strategy** (how would I undo this?).
///
/// Unknown fields are *not* denied on the way in: this is a response, and an
/// older client must keep parsing it when a later protocol adds a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationStatus {
    /// The server's handle for this operation.
    pub id: OperationId,
    /// Where it has got to.
    pub state: OperationState,
    /// The pipeline step behind that state.
    pub stage: OperationStage,
    /// What was requested — the closed, typed vocabulary, echoed back so a
    /// reconnecting client can say *what* it is reconciling.
    pub operation: GitOperation,
    /// SHA-256 of `operation`'s canonical JSON. Binds this record to one exact
    /// operation: a key reused with a different operation is refused, never
    /// answered with someone else's result.
    pub operation_hash: OperationHash,
    /// Opaque id of the repository the operation targets (never a path).
    pub repository: RepositoryToken,
    /// Opaque id of the worktree the operation targets.
    pub worktree: WorktreeToken,
    /// When the operation was admitted (Unix seconds, server clock).
    pub accepted_at: UnixSeconds,
    /// When it reached a terminal state; `None` while it is still running.
    pub ended_at: Option<UnixSeconds>,
    /// The HTTP status of the recorded response; `None` until terminal.
    pub status: Option<u16>,
    /// The recorded response body — git's own message, verbatim; `None` until
    /// terminal.
    pub message: Option<String>,
    /// The repository generation observed *after* execution (ADR 0001), so a
    /// client can tell whether its cached graph is stale. `None` until
    /// terminal, and also when the generation could not be read.
    pub generation: Option<GenerationToken>,
    /// How the pre-operation state can be recovered, from the plan that ran.
    /// `None` until a plan exists.
    pub recovery: Option<RecoveryStrategy>,
    /// The last object-transfer report this operation produced (M2.20c,
    /// #229). `None` for every operation that transfers nothing, and for a
    /// fetch that has not yet reached its first phase. On a *terminal*
    /// record it is the last report before the operation ended, which is
    /// exactly the useful thing after a cancel ("it stopped 62% through
    /// receiving").
    ///
    /// **Not persisted across a restart** (`durable.rs` rehydrates this as
    /// `None`): a terminal record's transfer is over, and a *running* one is
    /// not resumable across a process boundary anyway — the same reasoning
    /// that module already applies to running records generally.
    #[serde(default)]
    pub progress: Option<TransferProgress>,
}

impl OperationStatus {
    /// Whether this record is final — sugar for `state.is_terminal()`, so
    /// callers don't reach through two fields to ask the common question.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

/// One event on `GET /api/operations/{id}/events`.
///
/// The stream is a sequence of these, ending with one whose [`state`] is
/// terminal — at which point the server closes the stream rather than leaving
/// an idle connection open. A client that reconnects after the close gets the
/// same answer from `GET /api/operations/{id}`; the stream is an optimisation,
/// never the only way to learn an outcome.
///
/// [`state`]: ProgressEvent::state
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressEvent {
    /// Which operation this is about (streams are per-operation, but the id
    /// makes an event self-describing in a log).
    pub id: OperationId,
    /// The lifecycle state at this moment.
    pub state: OperationState,
    /// The pipeline step at this moment.
    pub stage: OperationStage,
    /// When the transition happened (Unix seconds, server clock).
    pub at: UnixSeconds,
    /// The object-transfer report at this moment, when the operation is one
    /// that transfers objects and has started (M2.20c, #229). This is what
    /// makes a long fetch legible: `stage` stays `Executing` throughout, and
    /// this field is the only thing that moves.
    #[serde(default)]
    pub progress: Option<TransferProgress>,
}

/// The SSE `event:` name carrying a [`ProgressEvent`].
pub const PROGRESS_EVENT: &str = "progress";

/// The SSE `event:` name carrying the terminal [`OperationStatus`]. The stream
/// closes immediately after sending one.
pub const RESULT_EVENT: &str = "result";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{BranchName, CommitOid};

    fn oid(c: char) -> CommitOid {
        CommitOid::new(c.to_string().repeat(40)).unwrap()
    }

    fn status() -> OperationStatus {
        OperationStatus {
            id: OperationId::new("op_0123456789abcdef").unwrap(),
            state: OperationState::Succeeded,
            stage: OperationStage::Finished,
            operation: GitOperation::CreateBranch {
                name: BranchName::new("feature/x").unwrap(),
                at: oid('a'),
            },
            operation_hash: OperationHash::new("b".repeat(64)).unwrap(),
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            accepted_at: UnixSeconds(1_753_400_000),
            ended_at: Some(UnixSeconds(1_753_400_002)),
            status: Some(200),
            message: Some("Created branch feature/x.".into()),
            generation: Some(GenerationToken::new("99").unwrap()),
            recovery: Some(RecoveryStrategy::DeleteCreatedBranch {
                name: BranchName::new("feature/x").unwrap(),
            }),
            progress: None,
        }
    }

    #[test]
    fn idempotency_keys_accept_client_shaped_ids() {
        // A UUID with its dashes, and the opaque ids a client might mint.
        assert!(IdempotencyKey::new("2f1c9e6a-4b7d-4a51-9c33-2b0f6a1d8e77").is_ok());
        assert!(IdempotencyKey::new("commit_17534000001").is_ok());
    }

    #[test]
    fn idempotency_keys_reject_anything_unsafe_in_a_header_or_log() {
        // Empty, over-long, and every shape that would need escaping somewhere.
        assert!(IdempotencyKey::new("").is_err());
        assert!(IdempotencyKey::new("a".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1)).is_err());
        for bad in ["with space", "new\nline", "sl/ash", "co:lon", "per%20cent"] {
            assert!(
                IdempotencyKey::new(bad).is_err(),
                "should have been refused: {bad:?}"
            );
        }
        // And the wire runs the same validation, so none of it is smuggled in.
        assert!(serde_json::from_str::<IdempotencyKey>(r#""a b""#).is_err());
        assert!(serde_json::from_str::<OperationId>(r#""""#).is_err());
    }

    #[test]
    fn only_succeeded_and_failed_are_terminal() {
        assert!(!OperationState::Accepted.is_terminal());
        assert!(!OperationState::Running.is_terminal());
        assert!(OperationState::Succeeded.is_terminal());
        assert!(OperationState::Failed.is_terminal());
    }

    #[test]
    fn state_and_stage_wire_names_are_stable_snake_case() {
        // Wire names are contract: pin them so a rename is a deliberate,
        // visible protocol change rather than an accident.
        assert_eq!(
            serde_json::to_string(&OperationState::Succeeded).unwrap(),
            r#""succeeded""#
        );
        assert_eq!(
            serde_json::to_string(&OperationStage::Waiting).unwrap(),
            r#""waiting""#
        );
        assert_eq!(
            serde_json::to_string(&OperationStage::Executing).unwrap(),
            r#""executing""#
        );
    }

    #[test]
    fn status_round_trips_through_json() {
        let s = status();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<OperationStatus>(&json).unwrap(), s);
        assert!(s.is_terminal());
    }

    #[test]
    fn a_running_record_carries_no_result_yet() {
        let mut s = status();
        s.state = OperationState::Running;
        s.stage = OperationStage::Executing;
        s.ended_at = None;
        s.status = None;
        s.message = None;
        s.generation = None;
        let json = serde_json::to_string(&s).unwrap();
        let back: OperationStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert!(!back.is_terminal());
    }

    #[test]
    fn a_response_tolerates_fields_a_newer_server_adds() {
        // Forward compatibility: a client on an older build must keep parsing
        // a record that grew a field, not fall over on it.
        let mut value = serde_json::to_value(status()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("something_new".into(), serde_json::json!(true));
        assert!(serde_json::from_value::<OperationStatus>(value).is_ok());
    }

    #[test]
    fn progress_events_round_trip() {
        let e = ProgressEvent {
            id: OperationId::new("op_dead_beef").unwrap(),
            state: OperationState::Running,
            stage: OperationStage::Planning,
            at: UnixSeconds(1_753_400_001),
            progress: None,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<ProgressEvent>(&json).unwrap(), e);
    }

    /// M2.20c (#229): the transfer vocabulary's wire spellings are contract —
    /// a client's progress bar branches on them.
    #[test]
    fn transfer_phase_wire_names_are_stable_snake_case() {
        for (phase, wire) in [
            (TransferPhase::Enumerating, r#""enumerating""#),
            (TransferPhase::Counting, r#""counting""#),
            (TransferPhase::Compressing, r#""compressing""#),
            (TransferPhase::Receiving, r#""receiving""#),
            (TransferPhase::Resolving, r#""resolving""#),
        ] {
            assert_eq!(serde_json::to_string(&phase).unwrap(), wire);
        }
    }

    /// A `progress`-carrying event round-trips, **and** an event minted by a
    /// server that predates the field still parses — the additive-field rule
    /// (M1.02) applied to the one field #229 adds to a live wire type.
    #[test]
    fn transfer_progress_round_trips_and_is_optional_on_the_wire() {
        let e = ProgressEvent {
            id: OperationId::new("op_dead_beef").unwrap(),
            state: OperationState::Running,
            stage: OperationStage::Executing,
            at: UnixSeconds(1_753_400_001),
            progress: Some(TransferProgress {
                phase: TransferPhase::Receiving,
                percent: Some(42),
                objects: Some(51),
                total_objects: Some(120),
            }),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<ProgressEvent>(&json).unwrap(), e);

        let older =
            r#"{"id":"op_dead_beef","state":"running","stage":"executing","at":1753400001}"#;
        assert_eq!(
            serde_json::from_str::<ProgressEvent>(older)
                .unwrap()
                .progress,
            None,
            "an event from a server without the field must parse, not fail"
        );
    }
}
