//! The shared planner (M1.06b, #143): the one path every write action takes.
//!
//! Every write handler now does exactly three things: apply its own request
//! validation (unchanged wording, unchanged status codes), build one variant of
//! the closed [`GitOperation`] vocabulary (#142), and hand it to
//! [`plan_and_execute`]. From there this module:
//!
//!  1. **builds** the reviewable [`Plan`] — repository/worktree tokens, the
//!     live generation, the operation's SHA-256 hash, an expiry window, and the
//!     per-operation risk / preconditions / expected ref changes / recovery
//!     (the shapes ADR 0015 pinned in the golden fixture);
//!  2. **validates** it — today the structural checks (hash equality, expiry)
//!     that #145 extends into full staleness/generation/precondition
//!     enforcement for client-reviewed plans;
//!  3. **executes** it — the *only* place in the server where a mutating git
//!     argv is constructed. The per-operation execution is the write handlers'
//!     old code moved here verbatim: same git commands, same journaling, same
//!     success/failure texts and status codes (this migration is a refactor,
//!     not a behavior change).
//!
//! A plan is built and executed inside a single request for now — there is no
//! client review roundtrip yet — so the build/validate seam looks trivial.
//! It is deliberate: #144 closes the browser's ad-hoc-request escape hatch and
//! #145 makes the validation load-bearing, both on top of exactly this
//! pipeline.

use std::path::Path;
use std::process::Output;

use axum::http::StatusCode;
use sha2::{Digest, Sha256};

use git_vista_core::activity::ActivityKind;
use git_vista_core::identity::GenerationInputs;
use git_vista_core::seed::{parse_seed, reset_plan, Seed};
use git_vista_protocol::{
    BranchName, CommitMessage, CommitOid, GenerationToken, GitOperation, OperationHash, Plan,
    Precondition, RecoveryStrategy, RefChange, RefName, RefState, RemoteName, RepositoryToken,
    RiskLevel, UnixSeconds, WorktreeToken,
};

use crate::git_cmd::{git_ok, rev_parse};
use crate::handlers::journal_app_event;
use crate::journal;
use crate::state::{current, current_handle, reject_if_read_only};

/// How long a freshly issued plan stays executable. Irrelevant while plans are
/// executed in the same request they're built in; the moment a client-review
/// roundtrip exists (#145), this is the staleness window it enforces.
const PLAN_TTL_SECS: i64 = 300;

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// Build → validate → execute one operation against the current selection.
/// The single entry point every write handler calls; everything below it is
/// private to the planner.
pub(crate) async fn plan_and_execute(op: GitOperation) -> (StatusCode, String) {
    // The write gate, kept here as well as in the handlers (defense in depth —
    // no operation executes against a Visualize-mode selection).
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let repo = current().0;
    let (plan, observed) = build_plan(&repo, op).await;
    if let Err(refused) = validate(&plan) {
        return refused;
    }
    execute(&repo, plan, observed).await
}

/// Resolve arbitrary request input to an exact [`CommitOid`]. A full 40/64
/// lowercase-hex id — what the UI always sends — is taken as-is; anything else
/// (a hand-crafted symbolic or abbreviated start point) is resolved through
/// `git rev-parse`. Failure mirrors git's own "not a valid object name" text,
/// since git would previously have been the one to refuse it.
pub(crate) async fn resolve_commit_oid(
    repo: &Path,
    given: &str,
) -> Result<CommitOid, (StatusCode, String)> {
    if let Ok(oid) = CommitOid::new(given) {
        return Ok(oid);
    }
    match rev_parse(repo, given).await {
        Some(full) => CommitOid::new(full).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("git rev-parse returned an unusable id: {e}"),
            )
        }),
        None => Err((
            StatusCode::BAD_REQUEST,
            format!("fatal: not a valid object name: '{given}'"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Pre-execution observations of the live repository, captured while building
/// the plan and reused by the executor — exactly the values the old handlers
/// read before mutating (journal "before" oids, the compare-and-swap tip), so
/// nothing is read twice and nothing is read *after* the mutation that needs
/// the before-state.
struct Observed {
    /// The checked-out branch's short name (`read_head_branch`), if any.
    head_branch: Option<String>,
    /// What `HEAD` resolves to, if it resolves (unborn HEAD ⇒ `None`).
    head_tip: Option<String>,
    /// The tip of the branch the operation names, for the operations that need
    /// it before executing (delete's journaled restore point, reset's CAS).
    branch_tip: Option<String>,
}

/// Build the reviewable [`Plan`] for `operation` against the live repository.
///
/// Observation here is **best-effort by design**: a read that fails (unborn
/// HEAD, a branch that doesn't exist, no remote-tracking ref) simply thins the
/// plan's preconditions/ref-changes rather than refusing the operation —
/// execution then surfaces git's own error exactly as it always has. #145 is
/// where preconditions become load-bearing checks.
async fn build_plan(repo: &Path, operation: GitOperation) -> (Plan, Observed) {
    let head_branch = git_vista_git::read_head_branch(repo);
    let head_tip = rev_parse(repo, "HEAD").await;
    let branch_tip = match &operation {
        GitOperation::DeleteBranch { branch }
        | GitOperation::ForceDeleteBranch { branch }
        | GitOperation::ResetBranch { branch, .. } => rev_parse(repo, branch.as_str()).await,
        _ => None,
    };
    let observed = Observed {
        head_branch,
        head_tip,
        branch_tip,
    };

    let (risk, preconditions, expected_ref_changes, recovery) =
        shape(repo, &operation, &observed).await;

    let (repository, worktree) = selection_tokens();
    let operation_hash = operation_hash(&operation);
    let generation = generation_token(repo, &observed);
    let now = crate::activity::now_secs();

    let plan = Plan {
        repository,
        worktree,
        generation,
        operation,
        operation_hash,
        issued_at: UnixSeconds(now),
        expires_at: UnixSeconds(now + PLAN_TTL_SECS),
        risk,
        preconditions,
        expected_ref_changes,
        recovery,
    };
    (plan, observed)
}

/// The current selection's opaque id tokens. In degraded mode (the served path
/// wouldn't classify as a repository, so it has no catalog entry) a fixed
/// placeholder keeps the plan well-formed; execution then fails with git's own
/// error exactly as the un-migrated handlers did.
fn selection_tokens() -> (RepositoryToken, WorktreeToken) {
    match current_handle() {
        Some(handle) => (
            RepositoryToken::new(handle.repository.to_string())
                .expect("a RepositoryId displays as a non-empty uuid"),
            WorktreeToken::new(handle.worktree.to_string())
                .expect("a WorktreeId displays as a non-empty uuid"),
        ),
        None => (
            RepositoryToken::new("unregistered").expect("literal is non-empty"),
            WorktreeToken::new("unregistered").expect("literal is non-empty"),
        ),
    }
}

/// The live repository generation as the plan's opaque token (ADR 0001).
/// Computed from HEAD and every ref; the index/worktree digests join in #145
/// when generation equality becomes an enforced execution-time check — the
/// token is opaque and compared only for equality, so deepening its inputs
/// later is not a wire change.
fn generation_token(repo: &Path, observed: &Observed) -> GenerationToken {
    let mut inputs = GenerationInputs::new();
    inputs.field(
        "head",
        format!(
            "{}\u{0}{}",
            observed.head_branch.as_deref().unwrap_or(""),
            observed.head_tip.as_deref().unwrap_or("")
        ),
    );
    if let Ok(refs) = git_vista_git::read_refs(repo) {
        for r in &refs {
            inputs.field(format!("ref:{:?}:{}", r.kind, r.name), r.target.0.clone());
        }
    }
    GenerationToken::new(inputs.generation().to_string())
        .expect("a RepositoryGeneration displays as non-empty decimal")
}

/// SHA-256 (lowercase hex) of the operation's canonical JSON — the digest the
/// [`Plan::operation_hash`] contract pins (#142) and #145 recomputes before
/// executing a client-approved plan.
fn operation_hash(operation: &GitOperation) -> OperationHash {
    let canonical =
        serde_json::to_string(operation).expect("GitOperation serialization cannot fail");
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    OperationHash::new(hex).expect("a sha256 digest is 64 lowercase hex chars")
}

/// The full ref name of a local branch.
fn heads(branch: &BranchName) -> Option<RefName> {
    RefName::new(format!("refs/heads/{branch}")).ok()
}

/// Best-effort `CommitOid` from an observed string.
fn oid_of(observed: &Option<String>) -> Option<CommitOid> {
    observed.as_deref().and_then(|o| CommitOid::new(o).ok())
}

/// The per-operation review shapes — risk, preconditions, expected ref
/// changes, recovery — following exactly the per-variant patterns the golden
/// fixture pinned (ADR 0015). Purely descriptive today (#145 enforces).
async fn shape(
    repo: &Path,
    operation: &GitOperation,
    observed: &Observed,
) -> (
    RiskLevel,
    Vec<Precondition>,
    Vec<RefChange>,
    RecoveryStrategy,
) {
    // The checked-out branch, in the pieces most shapes want.
    let head_name = observed
        .head_branch
        .as_deref()
        .and_then(|b| BranchName::new(b).ok());
    let head_ref = head_name.as_ref().and_then(heads);
    let head_oid = oid_of(&observed.head_tip);

    // A "the checked-out branch moves to a computed commit" shape, shared by
    // commit-on-HEAD / merge / rebase / revert: precondition that the branch is
    // checked out at its observed tip, a computed after-state, and a reset-back
    // recovery — exactly the fixture's pattern for those operations.
    let head_moves = |extra: Option<Precondition>| {
        let mut preconditions = Vec::new();
        if let Some(name) = head_name.clone() {
            preconditions.push(Precondition::BranchCheckedOut { branch: name });
        }
        preconditions.extend(extra);
        let changes = match (&head_ref, &head_oid) {
            (Some(r), Some(o)) => vec![RefChange {
                ref_name: r.clone(),
                before: RefState::At(o.clone()),
                after: RefState::Computed,
            }],
            _ => Vec::new(),
        };
        let recovery = match (&head_ref, &head_oid) {
            (Some(r), Some(o)) => RecoveryStrategy::ResetRef {
                ref_name: r.clone(),
                to: o.clone(),
            },
            // An unborn or unreadable HEAD: nothing to reset back to; the
            // operation itself will say what (if anything) it did.
            _ => RecoveryStrategy::NotNeeded,
        };
        (preconditions, changes, recovery)
    };

    match operation {
        GitOperation::CreateBranch { name, at } => {
            let target = heads(name);
            let preconditions = target
                .iter()
                .map(|r| Precondition::RefAbsent {
                    ref_name: r.clone(),
                })
                .collect();
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::Absent,
                    after: RefState::At(at.clone()),
                })
                .collect();
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::DeleteCreatedBranch { name: name.clone() },
            )
        }
        GitOperation::CommitOnHead { .. } => {
            let (preconditions, changes, recovery) = head_moves(None);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::EmptyCommitOnBranch {
            branch,
            expected_tip,
            ..
        } => {
            let target = heads(branch);
            let mut preconditions = vec![Precondition::BranchNotCheckedOut {
                branch: branch.clone(),
            }];
            preconditions.extend(target.iter().map(|r| Precondition::RefAt {
                ref_name: r.clone(),
                oid: expected_tip.clone(),
            }));
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(expected_tip.clone()),
                    after: RefState::Computed,
                })
                .collect();
            let recovery = match target {
                Some(r) => RecoveryStrategy::ResetRef {
                    ref_name: r,
                    to: expected_tip.clone(),
                },
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::StageAll | GitOperation::UnstageAll => (
            RiskLevel::Safe,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::NotNeeded,
        ),
        GitOperation::CheckoutBranch { branch } => {
            let target = heads(branch);
            let preconditions = target
                .iter()
                .map(|r| Precondition::RefExists {
                    ref_name: r.clone(),
                })
                .collect();
            // HEAD's symbolic move: from the current branch (or, detached, the
            // exact commit it sits on) to the named branch.
            let before = match (&head_ref, &head_oid) {
                (Some(r), _) => Some(RefState::Symbolic(r.clone())),
                (None, Some(o)) => Some(RefState::At(o.clone())),
                (None, None) => None,
            };
            let changes = match (before, &target) {
                (Some(before), Some(t)) => vec![RefChange {
                    ref_name: RefName::new("HEAD").expect("literal is valid"),
                    before,
                    after: RefState::Symbolic(t.clone()),
                }],
                _ => Vec::new(),
            };
            let recovery = match head_name {
                Some(previous) => RecoveryStrategy::CheckoutPrevious { branch: previous },
                // Detached HEAD: there is no previous *branch* to return to.
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Safe, preconditions, changes, recovery)
        }
        GitOperation::MergeBranch { .. } => {
            let extra = match (&head_ref, &head_oid) {
                (Some(r), Some(o)) => Some(Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: o.clone(),
                }),
                _ => None,
            };
            let (preconditions, changes, recovery) = head_moves(extra);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::PushBranch { branch, remote } => {
            let mut preconditions = vec![Precondition::RemoteConfigured {
                remote: remote.clone(),
            }];
            let target = heads(branch);
            preconditions.extend(target.iter().map(|r| Precondition::RefExists {
                ref_name: r.clone(),
            }));
            // The remote-tracking ref this push is expected to move.
            let tracking = RefName::new(format!("refs/remotes/{remote}/{branch}")).ok();
            let remote_tip = rev_parse(repo, &format!("{remote}/{branch}")).await;
            let local_tip = rev_parse(repo, branch.as_str()).await;
            let changes = match (tracking, oid_of(&local_tip)) {
                (Some(r), Some(local)) => vec![RefChange {
                    ref_name: r,
                    before: match oid_of(&remote_tip) {
                        Some(o) => RefState::At(o),
                        None => RefState::Absent,
                    },
                    after: RefState::At(local),
                }],
                _ => Vec::new(),
            };
            (
                RiskLevel::Remote,
                preconditions,
                changes,
                RecoveryStrategy::Irrecoverable,
            )
        }
        GitOperation::DeleteBranch { branch } | GitOperation::ForceDeleteBranch { branch } => {
            let risk = if matches!(operation, GitOperation::DeleteBranch { .. }) {
                RiskLevel::Reversible
            } else {
                RiskLevel::Destructive
            };
            let target = heads(branch);
            let tip = oid_of(&observed.branch_tip);
            let preconditions = match (&target, &tip) {
                (Some(r), Some(o)) => vec![Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: o.clone(),
                }],
                (Some(r), None) => vec![Precondition::RefExists {
                    ref_name: r.clone(),
                }],
                _ => Vec::new(),
            };
            let changes = match (&target, &tip) {
                (Some(r), Some(o)) => vec![RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(o.clone()),
                    after: RefState::Absent,
                }],
                _ => Vec::new(),
            };
            let recovery = match tip {
                Some(at) => RecoveryStrategy::RecreateBranch {
                    name: branch.clone(),
                    at,
                },
                // No observed tip means no branch to delete — execution will
                // surface git's own refusal.
                None => RecoveryStrategy::NotNeeded,
            };
            (risk, preconditions, changes, recovery)
        }
        GitOperation::RebaseOntoBase { base } => {
            // The base as git resolves it: `origin/main` is a remote-tracking
            // ref, a bare `main` a local branch.
            let base_full = if base.as_str().contains('/') {
                RefName::new(format!("refs/remotes/{base}")).ok()
            } else {
                RefName::new(format!("refs/heads/{base}")).ok()
            };
            let extra = base_full.map(|r| Precondition::RefExists { ref_name: r });
            let (preconditions, changes, recovery) = head_moves(extra);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::RestoreBranch { name, tip } => {
            let target = heads(name);
            let preconditions = target
                .iter()
                .map(|r| Precondition::RefAbsent {
                    ref_name: r.clone(),
                })
                .collect();
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::Absent,
                    after: RefState::At(tip.clone()),
                })
                .collect();
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::DeleteCreatedBranch { name: name.clone() },
            )
        }
        GitOperation::ResetBranch {
            branch,
            to,
            expected_tip,
        } => {
            let target = heads(branch);
            let mut preconditions: Vec<Precondition> = target
                .iter()
                .map(|r| Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: expected_tip.clone(),
                })
                .collect();
            // The clean-worktree requirement holds exactly when the reset runs
            // as `git reset --hard` — i.e. the branch is the checked-out one.
            if observed.head_branch.as_deref() == Some(branch.as_str()) {
                preconditions.push(Precondition::CleanWorktree);
            }
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(expected_tip.clone()),
                    after: RefState::At(to.clone()),
                })
                .collect();
            let recovery = match target {
                Some(r) => RecoveryStrategy::ResetRef {
                    ref_name: r,
                    to: expected_tip.clone(),
                },
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Destructive, preconditions, changes, recovery)
        }
        GitOperation::RevertCommit { commit } => {
            let (preconditions, changes, _) = head_moves(None);
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::RevertCommit {
                    commit: commit.clone(),
                },
            )
        }
        GitOperation::ResetTestRepo => (
            // The composite's exact moves and deletions are computed at
            // execution time from the recorded seed; the reset also wipes the
            // journal, which is what makes it irrecoverable.
            RiskLevel::Destructive,
            vec![Precondition::SeedRecorded],
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
    }
}

// ---------------------------------------------------------------------------
// Validate
// ---------------------------------------------------------------------------

/// The structural checks a plan must pass before execution. With plans built
/// and executed in the same request these can only fail on a server bug — but
/// they are the exact seam #145 widens into generation-equality, precondition
/// and staleness enforcement for client-reviewed plans, so they run (and
/// refuse, loudly) rather than being asserted away.
fn validate(plan: &Plan) -> Result<(), (StatusCode, String)> {
    if operation_hash(&plan.operation).as_str() != plan.operation_hash.as_str() {
        return Err((
            StatusCode::CONFLICT,
            "This plan doesn't match the operation it approves — refusing to execute.".to_string(),
        ));
    }
    if crate::activity::now_secs() > plan.expires_at.0 {
        return Err((
            StatusCode::CONFLICT,
            "This plan has expired — refresh and try again.".to_string(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Execute — the only mutating-git-argv construction in the server
// ---------------------------------------------------------------------------

/// Execute a validated plan. One arm per operation; each arm is the old write
/// handler's execution code moved here unchanged — same argv, same journaling,
/// same responses.
async fn execute(repo: &Path, plan: Plan, observed: Observed) -> (StatusCode, String) {
    match plan.operation {
        GitOperation::CreateBranch { name, at } => exec_create_branch(repo, &name, &at).await,
        GitOperation::CommitOnHead {
            message,
            allow_empty,
        } => exec_commit_on_head(repo, &message, allow_empty, &observed).await,
        GitOperation::EmptyCommitOnBranch {
            branch,
            message,
            expected_tip,
        } => exec_empty_commit_on_branch(repo, &branch, &message, &expected_tip).await,
        GitOperation::StageAll => exec_stage_all(repo).await,
        GitOperation::UnstageAll => exec_unstage_all(repo).await,
        GitOperation::CheckoutBranch { branch } => exec_checkout(repo, &branch, &observed).await,
        GitOperation::MergeBranch { branch } => exec_merge(repo, &branch, &observed).await,
        GitOperation::PushBranch { branch, remote } => exec_push(repo, &branch, &remote).await,
        GitOperation::DeleteBranch { branch } => {
            exec_delete(repo, &branch, &observed, false).await
        }
        GitOperation::ForceDeleteBranch { branch } => {
            exec_delete(repo, &branch, &observed, true).await
        }
        GitOperation::RebaseOntoBase { base } => exec_rebase(repo, &base, &observed).await,
        GitOperation::RestoreBranch { name, tip } => exec_restore_branch(repo, &name, &tip).await,
        GitOperation::ResetBranch {
            branch,
            to,
            expected_tip,
        } => exec_reset_branch(repo, &branch, &to, &expected_tip, &observed).await,
        GitOperation::RevertCommit { commit } => exec_revert(repo, &commit, &observed).await,
        GitOperation::ResetTestRepo => exec_reset_test_repo(repo).await,
    }
}

// --- small shared runners ---------------------------------------------------

/// Spawn `git -C <repo> <args…>` and collect its output; `Err` is the
/// "couldn't run git at all" case every endpoint maps to a 500.
async fn run_git(repo: &Path, args: &[&str]) -> std::io::Result<Output> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
}

/// The uniform 500 for a git binary that couldn't be spawned, with the same
/// per-endpoint log line the handlers printed.
fn couldnt_run(endpoint: &str, e: &std::io::Error) -> (StatusCode, String) {
    eprintln!("git-vista: {endpoint} couldn't run git: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Couldn't run git: {e}"),
    )
}

/// git's own explanation from stderr, or `fallback` when it said nothing.
fn stderr_or(output: &Output, fallback: &str) -> String {
    let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if msg.is_empty() {
        fallback.to_string()
    } else {
        msg
    }
}

/// git's explanation preferring stderr, falling back to stdout (some notices —
/// an up-to-date merge, "nothing to commit" — go there with a non-zero exit),
/// and only then to `fallback`.
fn stderr_stdout_or(output: &Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        fallback.to_string()
    } else {
        stdout
    }
}

/// Run one git command, mapping failure to git's own explanation (stderr, then
/// stdout, then a generic line) — the undo executions' shared runner, moved
/// from `crate::activity`.
async fn git(repo: &Path, args: &[&str]) -> Result<(), String> {
    let output = run_git(repo, args)
        .await
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(stderr_stdout_or(&output, "git failed."))
}

/// Whether the working tree has any change at all (`git status --porcelain`
/// non-empty): staged, unstaged, untracked or conflicted — any of them makes
/// a hard reset unsafe. Moved from `crate::activity`.
async fn worktree_dirty(repo: &Path) -> Result<bool, String> {
    let output = run_git(repo, &["status", "--porcelain"])
        .await
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    if !output.status.success() {
        return Err(stderr_or(&output, "git status failed."));
    }
    Ok(!output.stdout.is_empty())
}

/// The conventional 7-char short id, for labels and log lines.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

// --- per-operation executions (moved verbatim from the handlers) ------------

/// `git branch <name> <at>` (`/api/branch`). B3 posture: git validates the
/// name, refuses a duplicate, and its stderr is forwarded verbatim on failure.
async fn exec_create_branch(repo: &Path, name: &BranchName, at: &CommitOid) -> (StatusCode, String) {
    let output = match run_git(repo, &["branch", name.as_str(), at.as_str()]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/branch", &e),
    };
    if output.status.success() {
        println!("[/api/branch] created branch '{name}' at {at}");
        // Journal the creation with the resolved tip (the user may have given
        // an abbreviated or symbolic start point).
        let tip = rev_parse(repo, name.as_str()).await;
        journal_app_event(
            repo,
            ActivityKind::BranchCreated,
            Some(name.as_str().to_string()),
            None,
            tip,
            format!("created branch ‘{name}’"),
        );
        (StatusCode::OK, format!("Created branch '{name}'."))
    } else {
        let msg = stderr_or(&output, "git branch failed.");
        eprintln!("git-vista: /api/branch failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git commit [--allow-empty] -m <message>` on HEAD (`/api/commit`).
async fn exec_commit_on_head(
    repo: &Path,
    message: &CommitMessage,
    allow_empty: bool,
    observed: &Observed,
) -> (StatusCode, String) {
    // The pre-commit tip, captured for the journal before git moves anything.
    // `None` on an unborn HEAD (first commit) — journaled as a creation-like
    // event with no old state, which is exactly what it is.
    let old = observed.head_tip.clone();

    let mut args = vec!["commit"];
    if allow_empty {
        args.push("--allow-empty");
    }
    args.push("-m");
    args.push(message.as_str());

    let output = match run_git(repo, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/commit", &e),
    };
    if output.status.success() {
        println!("[/api/commit] created commit (allow_empty={allow_empty})");
        let new = rev_parse(repo, "HEAD").await;
        // The branch the commit landed on; "HEAD" when detached.
        let branch = git_vista_git::read_head_branch(repo).unwrap_or_else(|| "HEAD".into());
        let summary = message
            .as_str()
            .lines()
            .next()
            .unwrap_or(message.as_str())
            .to_string();
        journal_app_event(repo, ActivityKind::Commit, Some(branch), old, new, summary);
        (StatusCode::OK, "Created commit.".to_string())
    } else {
        // "nothing to commit, working tree clean" goes to *stdout* with a
        // non-zero exit — prefer stderr, fall back to stdout.
        let msg = stderr_stdout_or(&output, "git commit failed.");
        eprintln!("git-vista: /api/commit failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// The branch-stub path of `/api/commit`: `git commit-tree` on the branch
/// tip's own tree (an empty commit by construction), then a compare-and-swap
/// `git update-ref` from `expected_tip`. HEAD, index and working tree are
/// untouched throughout.
async fn exec_empty_commit_on_branch(
    repo: &Path,
    branch: &BranchName,
    message: &CommitMessage,
    expected_tip: &CommitOid,
) -> (StatusCode, String) {
    let refname = format!("refs/heads/{branch}");
    let tip = expected_tip.as_str();

    // Write the commit object: the parent's own tree, so nothing changes.
    let output = match run_git(
        repo,
        &[
            "commit-tree",
            &format!("{tip}^{{tree}}"),
            "-p",
            tip,
            "-m",
            message.as_str(),
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git commit-tree: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git commit-tree failed.");
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    let new = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if new.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "git commit-tree returned no commit id.".to_string(),
        );
    }

    // Advance the ref — compare-and-swap on the expected tip, with a reflog
    // line in git's own "commit (empty): …" shape so the activity feed reads
    // it like any other commit.
    let summary = message
        .as_str()
        .lines()
        .next()
        .unwrap_or(message.as_str())
        .to_string();
    let output = match run_git(
        repo,
        &[
            "update-ref",
            "-m",
            &format!("commit (empty): {summary}"),
            refname.as_str(),
            new.as_str(),
            tip,
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git update-ref: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            format!("‘{branch}’ has moved since this was offered — refresh and try again.")
        } else {
            msg
        };
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::CONFLICT, msg);
    }

    println!("[/api/commit] created empty commit on '{branch}' ({new})");
    journal_app_event(
        repo,
        ActivityKind::Commit,
        Some(branch.as_str().to_string()),
        Some(tip.to_string()),
        Some(new),
        summary,
    );
    (StatusCode::OK, "Created commit.".to_string())
}

/// `git add -A` (`/api/stage`).
async fn exec_stage_all(repo: &Path) -> (StatusCode, String) {
    let output = match run_git(repo, &["add", "-A"]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/stage", &e),
    };
    if output.status.success() {
        println!("[/api/stage] staged all changes (git add -A)");
        (StatusCode::OK, "Staged changes.".to_string())
    } else {
        let msg = stderr_or(&output, "git add failed.");
        eprintln!("git-vista: /api/stage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git reset -q HEAD` (`/api/unstage`) — the exact inverse of stage-all; the
/// working tree keeps every edit, so nothing is lost.
async fn exec_unstage_all(repo: &Path) -> (StatusCode, String) {
    let output = match run_git(repo, &["reset", "-q", "HEAD"]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/unstage", &e),
    };
    if output.status.success() {
        println!("[/api/unstage] unstaged all changes (git reset -q HEAD)");
        (StatusCode::OK, "Unstaged changes.".to_string())
    } else {
        let msg = stderr_or(&output, "git reset failed.");
        eprintln!("git-vista: /api/unstage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// One `git <args…> <branch>` branch operation with the shared error posture
/// (stderr, then stdout, then a generic line) — the old `run_branch_op` core.
async fn run_branch_cmd(
    repo: &Path,
    endpoint: &str,
    args: &[&str],
    branch: &BranchName,
    ok_msg: String,
) -> (StatusCode, String) {
    let mut argv: Vec<&str> = args.to_vec();
    argv.push(branch.as_str());
    let output = match run_git(repo, &argv).await {
        Ok(o) => o,
        Err(e) => return couldnt_run(endpoint, &e),
    };
    if output.status.success() {
        println!("[{endpoint}] {ok_msg}");
        (StatusCode::OK, ok_msg)
    } else {
        let msg = stderr_stdout_or(&output, "git command failed.");
        eprintln!("git-vista: {endpoint} failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git checkout <branch>` (`/api/checkout`). Asking for the branch already
/// checked out is a no-op — git exits 0 — so journalling it would put a
/// phantom event in the Activity feed; a real switch is journaled and the
/// feed's dedup collapses git's own reflog copy into it.
async fn exec_checkout(
    repo: &Path,
    branch: &BranchName,
    observed: &Observed,
) -> (StatusCode, String) {
    let resp = run_branch_cmd(
        repo,
        "/api/checkout",
        &["checkout"],
        branch,
        format!("checked out '{branch}'"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        if observed.head_branch.as_deref() == Some(branch.as_str()) {
            return (
                StatusCode::OK,
                format!("Already on ‘{branch}’ — it's the checked-out branch."),
            );
        }
        let new = rev_parse(repo, "HEAD").await;
        journal_app_event(
            repo,
            ActivityKind::Checkout,
            Some(branch.as_str().to_string()),
            observed.head_tip.clone(),
            new,
            format!("checked out ‘{branch}’"),
        );
    }
    resp
}

/// `git merge --no-edit <branch>` into the checked-out branch (`/api/merge`).
async fn exec_merge(repo: &Path, branch: &BranchName, observed: &Observed) -> (StatusCode, String) {
    let resp = run_branch_cmd(
        repo,
        "/api/merge",
        &["merge", "--no-edit"],
        branch,
        format!("merged '{branch}' into HEAD"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let new = rev_parse(repo, "HEAD").await;
        // git exits 0 with "Already up to date." when the branch brings
        // nothing in — HEAD hasn't moved. That's no merge: journalling one
        // would put an event in the Activity feed that never happened.
        if new == observed.head_tip {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{branch}’ has no commits the current branch doesn’t already have."),
            );
        }
        let into = git_vista_git::read_head_branch(repo).unwrap_or_else(|| "HEAD".into());
        journal_app_event(
            repo,
            ActivityKind::Merge,
            Some(into.clone()),
            observed.head_tip.clone(),
            new,
            format!("merged ‘{branch}’ into ‘{into}’"),
        );
    }
    resp
}

/// `git push <remote> <branch>` (`/api/push`).
async fn exec_push(repo: &Path, branch: &BranchName, remote: &RemoteName) -> (StatusCode, String) {
    let resp = run_branch_cmd(
        repo,
        "/api/push",
        &["push", remote.as_str()],
        branch,
        format!("pushed '{branch}' to {remote}"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let tip = rev_parse(repo, branch.as_str()).await;
        journal_app_event(
            repo,
            ActivityKind::Push,
            Some(branch.as_str().to_string()),
            None,
            tip,
            format!("pushed ‘{branch}’ to {remote}"),
        );
    }
    resp
}

/// `git branch -d`/`-D <branch>` (`/api/delete-branch` /
/// `/api/force-delete-branch`). The tip was captured BEFORE the delete (git
/// removes the branch's reflog with the branch) — that journaled oid is
/// precisely what "Restore branch" replays, and after a force-delete it may be
/// the ONLY path back to the commits (until gc).
async fn exec_delete(
    repo: &Path,
    branch: &BranchName,
    observed: &Observed,
    force: bool,
) -> (StatusCode, String) {
    let (endpoint, flag, verb) = if force {
        ("/api/force-delete-branch", "-D", "force-deleted")
    } else {
        ("/api/delete-branch", "-d", "deleted")
    };
    let resp = run_branch_cmd(
        repo,
        endpoint,
        &["branch", flag],
        branch,
        format!("{verb} branch '{branch}'"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        journal_app_event(
            repo,
            ActivityKind::BranchDeleted,
            Some(branch.as_str().to_string()),
            observed.branch_tip.clone(),
            None,
            format!("{verb} branch ‘{branch}’"),
        );
        // Drop it from the snapshot now, so the feed's snapshot diff can't
        // also report this app deletion as an external one.
        journal::remove_from_snapshot(repo, branch.as_str());
    }
    resp
}

/// `git rebase <base>` of the checked-out branch (`/api/rebase`). A failed
/// rebase (almost always conflicts) is `--abort`ed so a browser-only user is
/// never left mid-rebase with no shell to fix it.
async fn exec_rebase(repo: &Path, base: &RefName, observed: &Observed) -> (StatusCode, String) {
    let old = observed.head_tip.clone();
    let base = base.as_str();

    let output = match run_git(repo, &["rebase", base]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/rebase", &e),
    };
    if output.status.success() {
        let new = rev_parse(repo, "HEAD").await;
        let branch = git_vista_git::read_head_branch(repo).unwrap_or_else(|| "HEAD".into());
        // git exits 0 without moving HEAD when the branch is already based on
        // the base — that's no rebase, and journalling one would put a phantom
        // event in the Activity feed. Say what (didn't) happen instead.
        if new == old {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{branch}’ is already based on {base}."),
            );
        }
        println!("[/api/rebase] rebased HEAD onto {base}");
        journal_app_event(
            repo,
            ActivityKind::Rebase,
            Some(branch.clone()),
            old,
            new,
            format!("rebased ‘{branch}’ onto {base}"),
        );
        (StatusCode::OK, format!("Rebased onto {base}."))
    } else {
        let msg = stderr_stdout_or(&output, "git rebase failed.");
        // Best-effort: back out of the half-applied rebase so the working tree
        // isn't stuck mid-rebase. Harmless (exits non-zero, ignored) when none
        // is running.
        let _ = run_git(repo, &["rebase", "--abort"]).await;
        eprintln!("git-vista: /api/rebase failed (aborted): {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git branch <name> <tip>` — re-create a deleted branch at its journaled tip
/// (`/api/undo`). The safe undo: `git branch` creates, never destroys, and
/// fails by itself if the name came back into use since the hint.
async fn exec_restore_branch(repo: &Path, name: &BranchName, tip: &CommitOid) -> (StatusCode, String) {
    match git(repo, &["branch", name.as_str(), tip.as_str()]).await {
        Ok(()) => {
            println!(
                "[/api/undo] restored branch '{name}' at {}",
                short(tip.as_str())
            );
            journal_app_event(
                repo,
                ActivityKind::BranchCreated,
                Some(name.as_str().to_string()),
                None,
                Some(tip.as_str().to_string()),
                format!("restored branch ‘{name}’ at {}", short(tip.as_str())),
            );
            (StatusCode::OK, format!("Restored branch ‘{name}’."))
        }
        Err(msg) => {
            eprintln!("git-vista: /api/undo restore failed: {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// Move a branch back to `to` (`/api/undo`): `git reset --hard` when it's the
/// checked-out branch with a clean worktree, else `git branch -f` (no worktree
/// involved). `expected_tip` is compare-and-swap — a hint from a stale menu
/// can never reset away work that happened after it was shown.
async fn exec_reset_branch(
    repo: &Path,
    branch: &BranchName,
    to: &CommitOid,
    expected_tip: &CommitOid,
    observed: &Observed,
) -> (StatusCode, String) {
    // Compare-and-swap: the hint was computed against `expected_tip`; if the
    // branch has moved since, this undo would discard newer work the user
    // never saw in the dialog — refuse instead.
    if observed.branch_tip.as_deref() != Some(expected_tip.as_str()) {
        return (
            StatusCode::CONFLICT,
            format!("‘{branch}’ has moved since this undo was offered — refresh and try again."),
        );
    }
    let checked_out = observed.head_branch.as_deref() == Some(branch.as_str());
    let result = if checked_out {
        // `git reset --hard` rewrites the working tree, so it runs only
        // against a fully clean one — never eat uncommitted work.
        match worktree_dirty(repo).await {
            Err(msg) => Err(msg),
            Ok(true) => {
                return (
                    StatusCode::CONFLICT,
                    "The working tree has uncommitted changes — commit them first \
                     so the undo can't destroy them."
                        .to_string(),
                );
            }
            Ok(false) => git(repo, &["reset", "--hard", to.as_str()]).await,
        }
    } else {
        // Not checked out: move the ref alone, no worktree involved.
        git(repo, &["branch", "-f", branch.as_str(), to.as_str()]).await
    };
    match result {
        Ok(()) => {
            println!(
                "[/api/undo] reset branch '{branch}' to {}",
                short(to.as_str())
            );
            journal_app_event(
                repo,
                ActivityKind::Reset,
                Some(branch.as_str().to_string()),
                Some(expected_tip.as_str().to_string()),
                Some(to.as_str().to_string()),
                format!("undid — reset ‘{branch}’ to {}", short(to.as_str())),
            );
            (
                StatusCode::OK,
                format!("Reset ‘{branch}’ to {}.", short(to.as_str())),
            )
        }
        Err(msg) => {
            eprintln!("git-vista: /api/undo reset failed: {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// `git revert --no-edit <commit>` (`/api/undo`) — the history-preserving
/// undo; a conflicted revert is auto-aborted (like `/api/rebase`) so a
/// browser-only user is never left mid-revert.
async fn exec_revert(repo: &Path, commit: &CommitOid, observed: &Observed) -> (StatusCode, String) {
    let commit = commit.as_str();
    match git(repo, &["revert", "--no-edit", commit]).await {
        Ok(()) => {
            println!("[/api/undo] reverted {}", short(commit));
            let new = rev_parse(repo, "HEAD").await;
            let branch = git_vista_git::read_head_branch(repo).unwrap_or_else(|| "HEAD".into());
            journal_app_event(
                repo,
                ActivityKind::Revert,
                Some(branch),
                observed.head_tip.clone(),
                new,
                format!("reverted {}", short(commit)),
            );
            (StatusCode::OK, format!("Reverted {}.", short(commit)))
        }
        Err(msg) => {
            // Back out of a conflicted half-applied revert so the tree isn't
            // stuck. Harmless when no revert is in progress.
            let _ = git(repo, &["revert", "--abort"]).await;
            eprintln!("git-vista: /api/undo revert failed (aborted): {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// The parsed seed, if this repo has one. `None` => not a test repo;
/// `Some(Err)` => the seed files exist but are corrupt (refuse to reset).
fn read_seed(repo: &Path) -> Option<Result<Seed, String>> {
    let dir = journal::state_dir(repo)?;
    let refs = std::fs::read_to_string(dir.join("seed-refs")).ok()?;
    let head = std::fs::read_to_string(dir.join("seed-head")).ok()?;
    Some(parse_seed(&refs, &head))
}

/// Reset a *test repo* to its recorded seed (`/api/reset-test-repo`): move
/// every seeded branch back to its recorded tip, check out the seeded HEAD
/// branch, force the worktree clean, DELETE branches the seed doesn't know —
/// allowed nowhere else in git-vista — and wipe the app journal (its events
/// describe history that no longer exists). Hard-gated: only a repo
/// explicitly opted in with `gv --seed <path>` has seed files.
async fn exec_reset_test_repo(repo: &Path) -> (StatusCode, String) {
    let seed = match read_seed(repo) {
        None => {
            return (
                StatusCode::NOT_FOUND,
                "This repo has no recorded seed — it isn't marked as a test repo. \
                 Run `gv --seed <path>` once on the server machine to record its reset point."
                    .to_string(),
            )
        }
        Some(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The recorded seed is corrupt ({e}) — re-record it with `gv --seed`."),
            )
        }
        Some(Ok(seed)) => seed,
    };

    // Objects first, verification second: unbundle is best-effort (idempotent,
    // cheap), then every seeded tip must resolve or the reset refuses to start —
    // never a half-restore.
    if let Some(dir) = journal::state_dir(repo) {
        let bundle = dir.join("seed.bundle");
        if bundle.exists() {
            let _ = git_ok(
                repo,
                &["bundle", "unbundle", &bundle.display().to_string()],
            )
            .await;
        }
    }
    for r in &seed.refs {
        if rev_parse(repo, &format!("{}^{{commit}}", r.oid))
            .await
            .is_none()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "Seed commit {} for ‘{}’ no longer exists in this repo — \
                     re-record the seed with `gv --seed`.",
                    &r.oid[..7],
                    r.name
                ),
            );
        }
    }

    // What the repo looks like NOW, then the pure plan of moves + deletions.
    let current_refs = match run_git(
        repo,
        &[
            "for-each-ref",
            "refs/heads",
            "--format=%(objectname) %(refname:short)",
        ],
    )
    .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                l.split_once(' ')
                    .map(|(oid, name)| (name.to_string(), oid.to_string()))
            })
            .collect::<Vec<_>>(),
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't list the repo's current branches.".to_string(),
            )
        }
    };
    let plan = reset_plan(&seed, &current_refs);

    // Apply, in an order where each step makes the next valid: refs back first
    // (so the seed HEAD branch exists at the right tip), then a forced checkout
    // + hard reset + clean (so HEAD is off any branch about to be deleted and
    // the worktree matches the seed exactly), then the deletions.
    for r in &plan.update {
        if let Err(e) = git_ok(
            repo,
            &["update-ref", &format!("refs/heads/{}", r.name), &r.oid],
        )
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped while restoring ‘{}’: {e}", r.name),
            );
        }
    }
    for step in [
        &["checkout", "-f", seed.head.as_str()] as &[&str],
        &["reset", "--hard"],
        &["clean", "-fd"],
    ] {
        if let Err(e) = git_ok(repo, step).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped at `git {}`: {e}", step.join(" ")),
            );
        }
    }
    let mut deleted = 0;
    for name in &plan.delete {
        // The ONLY place git-vista deletes a branch: a seeded test repo, inside
        // an explicit reset, for branches created after the seed was recorded.
        match git_ok(repo, &["branch", "-D", name]).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Reset stopped deleting test branch ‘{name}’: {e}"),
                )
            }
        }
    }
    // The journal now describes history that no longer exists (dead undo
    // targets included) — wipe it with the snapshot; both regenerate.
    journal::clear(repo);

    let msg = format!(
        "Reset to seed: {} branch(es) restored, {} deleted, HEAD → ‘{}’, working tree clean.",
        plan.update.len(),
        deleted,
        seed.head
    );
    println!("[/api/reset-test-repo] {msg}");
    (StatusCode::OK, msg)
}
