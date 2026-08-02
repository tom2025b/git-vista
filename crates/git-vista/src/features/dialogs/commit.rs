//! The commit dialog's decisions — framework-free, host-tested (M2.19c, #224).
//!
//! `dialogs/commit.rs` (the view) is wasm-only: it never compiles under
//! `cargo test --workspace`, so anything decided there is decided untested.
//! Everything this milestone adds — which of the three commit modes is active,
//! what each one says it will do, which files it will actually contain, the
//! request body an amend sends, how the server's typed refusal is read back,
//! and what the dialog does when the compare-and-swap says HEAD moved — is
//! therefore a pure function here. The view's job is to render what these
//! return and to call what they decide.
//!
//! # The three modes are one enum, not three flags
//!
//! Before this, the dialog carried `allow_empty: bool` plus
//! `branch: Option<String>`, with an unwritten rule that `branch` is only ever
//! `Some` together with `allow_empty`. [`CommitIntent`] makes the three modes
//! the app actually has — commit the index, record an empty commit, rewrite
//! the tip — the only representable ones, and makes "an amend with a branch
//! target" un-constructible rather than merely unwise. That matters here
//! specifically: `GitOperation::AmendCommit` has no branch field at all (it
//! always targets the checked-out branch's own tip), so a shape that could
//! carry one would be a shape the server cannot honour.
//!
//! # What the amend flow refuses to guess
//!
//! `POST /api/amend-commit` answers a failure with a *typed* kind
//! ([`AmendFailureKind`]), which is what lets this module branch without
//! regex-sniffing git's translated stderr. One of those kinds — `StaleTip`,
//! the compare-and-swap refusing because HEAD moved — must never be shown as
//! a plain error, because retrying it unchanged would amend a commit the user
//! never reviewed. So [`AmendRefusal`], the client-side refusal vocabulary,
//! **has no stale-tip variant**: a stale tip can only reach the UI through
//! [`AmendPhase::Stale`], whose guided re-check keeps the confirm button
//! disabled until a fresh tip has been read and shown. The type is the
//! enforcement; [`phase_view`] is where the rule is stated and tested.

use git_vista_core::model::{GitRef, RefKind};
use git_vista_core::status::{ChangeKind, RepoStatus};
use git_vista_protocol::{
    AmendCommitError, AmendCommitRequest, AmendCommitSuccess, AmendFailureKind,
};

use crate::features::dialogs::core::commit_draft_key;

// ---------------------------------------------------------------------------
// What the dialog is collecting a message for
// ---------------------------------------------------------------------------

/// What the commit dialog will do when it is confirmed.
///
/// Replaces the `allow_empty` + `branch` pair the dialog used to carry (#33,
/// widened by #224). Each variant maps onto exactly one server operation:
/// `CommitOnHead`, `CommitOnHead { allow_empty }` / `EmptyCommitOnBranch`,
/// and `AmendCommit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitIntent {
    /// A plain `git commit` of whatever is in the index, on the checked-out
    /// branch.
    Staged,
    /// `git commit --allow-empty`. `branch` is `None` for the checked-out
    /// branch (a real `git commit --allow-empty`, which — see
    /// [`dialog_copy`] — still commits a non-empty index if there is one), or
    /// `Some(name)` for a branch stub, where the server writes the commit
    /// object and moves that one ref without a checkout.
    Empty { branch: Option<String> },
    /// `git commit --amend` on the checked-out branch's tip, compare-and-swapped
    /// against the tip the user reviewed (M2.19a #222, M2.19b #223).
    Amend { expected_tip: String },
}

impl CommitIntent {
    /// The `allow_empty` field the request carries.
    ///
    /// Amend sends `true` **unconditionally**, and that is a decision, not a
    /// leak of the empty-commit flag: `git commit --amend` keeps the tip's own
    /// tree, so `--allow-empty` changes nothing at all unless the commit being
    /// amended is *itself* empty — in which case rewriting only its message
    /// fails without the flag. Amending an empty commit's message is
    /// legitimate and the user has no way to see, let alone set, a flag that
    /// would make it work. There is no case where the flag can make an amend
    /// remove content: the content comes from the existing commit plus the
    /// index either way.
    pub fn allow_empty(&self) -> bool {
        match self {
            CommitIntent::Staged => false,
            CommitIntent::Empty { .. } => true,
            CommitIntent::Amend { .. } => true,
        }
    }

    /// The branch a commit should land on, when it is not the checked-out one.
    pub fn branch(&self) -> Option<&str> {
        match self {
            CommitIntent::Empty { branch } => branch.as_deref(),
            CommitIntent::Staged | CommitIntent::Amend { .. } => None,
        }
    }

    /// The tip an amend is compare-and-swapped against.
    pub fn expected_tip(&self) -> Option<&str> {
        match self {
            CommitIntent::Amend { expected_tip } => Some(expected_tip.as_str()),
            CommitIntent::Staged | CommitIntent::Empty { .. } => None,
        }
    }
}

/// The body of `POST /api/amend-commit`, built from the reviewed tip and the
/// message.
///
/// Typed as [`AmendCommitRequest`] — the same struct the server deserializes,
/// which carries `#[serde(deny_unknown_fields)]`. A field renamed or added on
/// either side is a compile error here, not a runtime 400.
pub fn amend_body(message: &str, expected_tip: &str) -> AmendCommitRequest {
    AmendCommitRequest {
        message: message.trim().to_string(),
        allow_empty: CommitIntent::Amend {
            expected_tip: expected_tip.to_string(),
        }
        .allow_empty(),
        expected_tip: expected_tip.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Which text buffer the dialog edits — and which one persists
// ---------------------------------------------------------------------------

/// Which message the dialog is editing.
///
/// #226 gave the commit dialog a per-repository draft that survives an iOS
/// suspension by persisting to `sessionStorage` on every keystroke. Amend
/// mode must not touch it. Two separate reasons, both of which would be
/// silent data loss rather than a visible bug:
///
/// 1. Amend mode *pre-fills* the box with the tip's existing message. Writing
///    that pre-fill through the draft path would overwrite whatever the user
///    had half-typed for a normal commit — and persist the overwrite — so
///    cancelling the amend would leave their draft permanently replaced by a
///    commit message they did not write.
/// 2. A persisted amend message is scoped to a *commit*, not to a repository.
///    Restoring one after a suspension could put text written against a tip
///    that no longer exists into an amend of a different commit. Losing the
///    text is recoverable (the pre-fill is re-derived from HEAD); silently
///    re-targeting it is not.
///
/// So the amend buffer is in memory only. That is a real, stated gap — an
/// iOS suspension mid-amend loses the edit, where a plain commit draft
/// survives it — accepted because the alternative revives text against the
/// wrong commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageBuffer {
    /// The #226 draft: persisted per repository, restored on suspension.
    Draft,
    /// Amend's own buffer: in memory, never persisted.
    Amend,
}

/// Which buffer an intent edits.
pub fn message_buffer(intent: &CommitIntent) -> MessageBuffer {
    match intent {
        CommitIntent::Staged | CommitIntent::Empty { .. } => MessageBuffer::Draft,
        CommitIntent::Amend { .. } => MessageBuffer::Amend,
    }
}

/// The `sessionStorage` key a buffer persists under, or `None` for a buffer
/// that must not persist.
///
/// The single decision point for #226's storage writes: the signal layer
/// persists **iff** this returns `Some`, so "amend never writes the draft
/// key" is a property of this tested function rather than of a branch each
/// call site has to remember.
pub fn persist_key(buffer: MessageBuffer, worktree_id: &str) -> Option<String> {
    match buffer {
        MessageBuffer::Draft => Some(commit_draft_key(worktree_id)),
        MessageBuffer::Amend => None,
    }
}

/// Whether an incoming pre-fill may replace what is in the box.
///
/// `current` is the live text, `seed` is what the box was last seeded with
/// (empty before any seed landed). The pre-fill is adopted **only** if the
/// user has not touched the box since — an amend pre-fill arriving late (the
/// `GET /api/commit/{id}` behind it is a network round trip) must never
/// overwrite a message the user has already typed. Returns `None` when there
/// is nothing to do, so the caller writes no signal at all.
pub fn adopt_seed(current: &str, seed: &str, incoming: &str) -> Option<String> {
    if current != seed || current == incoming {
        return None;
    }
    Some(incoming.to_string())
}

// ---------------------------------------------------------------------------
// The staged-scope review
// ---------------------------------------------------------------------------

/// How much of the working tree is staged — the distinction #224 asks the
/// dialog to make visible instead of leaving the user to infer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedBreadth {
    /// Nothing in the index.
    Nothing,
    /// Everything dirty is staged: committing leaves a clean tree.
    Everything,
    /// Some staged, some not — the "partial commit" case.
    Partial,
}

/// Classify the index against the rest of the working tree.
///
/// Untracked files count as "not staged": a commit made now leaves them
/// behind, which is exactly what the user needs to be told. Ignored files do
/// not appear in [`RepoStatus`] at all, so they cannot skew this.
pub fn staged_breadth(status: &RepoStatus) -> StagedBreadth {
    if status.staged.is_empty() {
        return StagedBreadth::Nothing;
    }
    if status.unstaged.is_empty() && status.untracked.is_empty() {
        StagedBreadth::Everything
    } else {
        StagedBreadth::Partial
    }
}

/// One line of the staged-scope review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeLine {
    pub path: String,
    /// `added` / `modified` / `deleted` — the staged side's own verb.
    pub kind: &'static str,
    /// The file has *also* been edited since it was staged, so the commit will
    /// contain the staged version and not what is on disk. Surfaced per line
    /// because it is a per-file surprise, not a repository-wide one.
    pub also_modified: bool,
}

/// What the dialog shows above the buttons: exactly what the commit will
/// contain, and what it will leave out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeReview {
    pub heading: String,
    pub lines: Vec<ScopeLine>,
    /// Paths beyond the display cap — counted, never silently dropped.
    pub hidden: usize,
    /// Caveats and exclusions, each already a full sentence.
    pub notes: Vec<String>,
}

fn change_verb(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        // git's `R`/`C`. The path shown is the *new* one (the parser drops the
        // original, see `git_vista_core::status::parse_renamed`), so "renamed"
        // is the honest verb for it.
        ChangeKind::Renamed => "renamed",
    }
}

/// Pluralise a count of files without a dependency.
fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// The staged-scope review for one intent.
///
/// `status` is `None` while the working-tree read is in flight or after it
/// failed. That case gets a note saying so rather than an empty list: an
/// empty list is a claim ("this commit contains nothing"), and a status probe
/// that has not answered is not entitled to make it.
///
/// Reads [`RepoStatus`] — the v1 `GET /api/status` payload the app already
/// has a single shared owner for (`features::status::signals`, M1.11 Task 7)
/// — rather than opening a second `GET /api/status/v2` resource inside the
/// modal. Both are parsed from the same `git status --porcelain=v2` output;
/// the file-level facts this review needs (staged path, staged verb, whether
/// the same path is also dirty in the worktree) are present in both. #224's
/// bullet names the v2 DTO, but a second per-modal status read is precisely
/// the duplication that shared owner exists to prevent.
pub fn scope_review(
    intent: &CommitIntent,
    status: Option<&RepoStatus>,
    limit: usize,
) -> ScopeReview {
    // A branch-stub empty commit never touches the index: the server writes
    // the commit object and moves that ref, so the checked-out repository's
    // staged files are irrelevant to it.
    if let CommitIntent::Empty {
        branch: Some(branch),
    } = intent
    {
        return ScopeReview {
            heading: "What this commit will contain".to_string(),
            lines: Vec::new(),
            hidden: 0,
            notes: vec![format!(
                "Nothing. An empty commit is written straight onto ‘{branch}’ without \
                 checking it out, so your index and working tree are untouched."
            )],
        };
    }

    let heading = match intent {
        CommitIntent::Amend { .. } => "What the amended commit will contain (staged)",
        _ => "What this commit will contain (staged)",
    }
    .to_string();

    let Some(status) = status else {
        return ScopeReview {
            heading,
            lines: Vec::new(),
            hidden: 0,
            notes: vec![
                "The working-tree status hasn't been read yet, so this list is not \
                 the full picture."
                    .to_string(),
            ],
        };
    };

    let unstaged_paths: Vec<&str> = status.unstaged.iter().map(|c| c.path.as_str()).collect();
    let all: Vec<ScopeLine> = status
        .staged
        .iter()
        .map(|c| ScopeLine {
            path: c.path.clone(),
            kind: change_verb(c.kind),
            also_modified: unstaged_paths.contains(&c.path.as_str()),
        })
        .collect();
    let hidden = all.len().saturating_sub(limit);
    let lines: Vec<ScopeLine> = all.into_iter().take(limit).collect();

    let mut notes = Vec::new();
    if lines.is_empty() {
        match intent {
            CommitIntent::Amend { .. } => notes.push(
                "Nothing is staged, so the amended commit keeps exactly the files the \
                 current one has — only its message changes."
                    .to_string(),
            ),
            CommitIntent::Empty { .. } => {
                notes.push("Nothing is staged, so this commit records no file changes.".to_string())
            }
            CommitIntent::Staged => notes.push(
                "Nothing is staged. Stage something first — there is nothing to commit."
                    .to_string(),
            ),
        }
    }
    let left_out = status.unstaged.len() + status.untracked.len();
    if left_out > 0 {
        notes.push(format!(
            "{} of unstaged changes and {} untracked stay out of this commit.",
            files(status.unstaged.len()),
            files(status.untracked.len()),
        ));
    }
    if !status.conflicted.is_empty() {
        notes.push(format!(
            "{} still have unresolved merge conflicts; git refuses to commit while \
             any path is unmerged.",
            files(status.conflicted.len()),
        ));
    }
    ScopeReview {
        heading,
        lines,
        hidden,
        notes,
    }
}

// ---------------------------------------------------------------------------
// The dialog's own words
// ---------------------------------------------------------------------------

/// Title, explanation and button label for one dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogCopy {
    pub title: String,
    /// What confirming will actually do, in one or two sentences.
    pub body: String,
    pub confirm_label: &'static str,
    /// `Some` when the dialog *knows* the operation cannot succeed — the
    /// confirm button is inert and this is why. `None` when it may proceed,
    /// including when the status read has not answered: a probe that has not
    /// come back does not get to block the user.
    pub blocked_reason: Option<String>,
}

/// Everything the dialog says, for one intent and one working-tree status.
///
/// The three states #224 requires be unmistakable — empty, partial, amend —
/// differ in all three of title, body and button label, not in a subtitle
/// someone has to notice.
pub fn dialog_copy(intent: &CommitIntent, status: Option<&RepoStatus>) -> DialogCopy {
    let on_branch = status
        .and_then(|s| s.branch.clone())
        .map(|b| format!(" on ‘{b}’"))
        .unwrap_or_default();
    let breadth = status.map(staged_breadth);
    let staged_count = status.map(|s| s.staged.len()).unwrap_or(0);

    match intent {
        CommitIntent::Staged => {
            let body = match breadth {
                Some(StagedBreadth::Nothing) => {
                    "Nothing is staged, so there is nothing to record.".to_string()
                }
                Some(StagedBreadth::Everything) => format!(
                    "Records {}{on_branch}. Nothing dirty is left behind.",
                    files(staged_count)
                ),
                Some(StagedBreadth::Partial) => format!(
                    "Records only the {} listed below{on_branch} — a partial commit. \
                     Everything else in the working tree stays uncommitted.",
                    files(staged_count)
                ),
                None => format!(
                    "Records the staged changes{on_branch}. The working-tree status \
                     hasn't been read yet, so check the list below once it appears."
                ),
            };
            DialogCopy {
                title: "Commit staged changes".to_string(),
                body,
                confirm_label: "Commit",
                blocked_reason: matches!(breadth, Some(StagedBreadth::Nothing)).then(|| {
                    "Nothing is staged — stage a change first, or close this and use \
                     ‘Create empty commit’ if a marker commit is what you want."
                        .to_string()
                }),
            }
        }
        CommitIntent::Empty { branch: Some(b) } => DialogCopy {
            title: format!("Create empty commit on ‘{b}’"),
            // Named explicitly because this is the one commit in the app that
            // does *not* land where the user is standing: the server writes the
            // object and moves ‘b’, leaving HEAD and the working tree alone.
            body: format!(
                "Writes a commit with no file changes onto ‘{b}’ without checking it \
                 out. Your index and working tree are untouched, and the branch you \
                 are on does not move."
            ),
            confirm_label: "Create empty commit",
            blocked_reason: None,
        },
        CommitIntent::Empty { branch: None } => {
            // The honest part: `git commit --allow-empty` does NOT ignore the
            // index. With something staged this creates an ordinary commit
            // containing it, which is the opposite of what the item's name
            // suggests.
            let body = if staged_count > 0 {
                format!(
                    "Careful: {} are staged, and an empty commit{on_branch} does not \
                     skip them — git records them, so this commit will not be empty. \
                     Unstage them first if you meant a marker commit.",
                    files(staged_count)
                )
            } else {
                format!(
                    "Records a commit with no file changes{on_branch} — a marker in the history."
                )
            };
            DialogCopy {
                title: "Create empty commit".to_string(),
                body,
                confirm_label: "Create empty commit",
                blocked_reason: None,
            }
        }
        CommitIntent::Amend { expected_tip } => {
            let tail = if staged_count > 0 {
                format!(
                    " The {} listed below are folded into it.",
                    files(staged_count)
                )
            } else {
                " Nothing is staged, so only the message changes.".to_string()
            };
            DialogCopy {
                title: "Amend last commit".to_string(),
                body: format!(
                    "Rewrites commit {}{on_branch}: the existing commit is replaced, \
                     not added to, and the new message below becomes its whole \
                     message.{tail}",
                    short_tip(expected_tip),
                ),
                confirm_label: "Amend commit",
                blocked_reason: None,
            }
        }
    }
}

/// The first seven hex characters of a commit id — what the rest of the UI
/// shows. Falls back to the whole string for anything shorter or not
/// splittable there.
pub fn short_tip(tip: &str) -> &str {
    tip.get(..7).unwrap_or(tip)
}

// ---------------------------------------------------------------------------
// Reading `POST /api/amend-commit` back
// ---------------------------------------------------------------------------

/// A refusal the client can show as an error and let the user retry from.
///
/// **No stale-tip variant, deliberately.** `AmendFailureKind::StaleTip` means
/// the commit that would be rewritten is not the commit the user reviewed, so
/// "here is an error, press the button again" would amend something unseen.
/// It is routed to [`AmendPhase::Stale`] instead, and leaving it out of this
/// enum is what makes that routing structural rather than a convention a
/// later edit can drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmendRefusal {
    /// A repository hook exited non-zero.
    Hook,
    /// Commit signing was configured and the signer failed.
    Signing,
    /// Anything else git said no to.
    Other,
}

/// What one `POST /api/amend-commit` answer means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmendOutcome {
    /// 200 with a parseable body: the rewrite happened.
    Amended(Box<AmendCommitSuccess>),
    /// 400 `stale_tip`: HEAD moved, nothing was rewritten.
    TipMoved { message: String },
    /// 400 with any other typed kind.
    Refused {
        refusal: AmendRefusal,
        message: String,
    },
    /// Anything this endpoint's typed contract does not cover — transport
    /// failure, 403/409/5xx prose, or a body that would not parse. Carries
    /// text fit to show.
    Unavailable(String),
}

/// Read one response into an outcome.
///
/// The endpoint's contract (`handlers::commit::amend_commit`) is that
/// **every** 400 body is an [`AmendCommitError`], and a 200 body is an
/// [`AmendCommitSuccess`]. Everything else is the server-wide prose contract.
/// Both parses go through the protocol crate's own types, so this cannot
/// drift from the shapes the server serializes.
///
/// The one case worth naming: a 200 whose body will not parse. The amend
/// *happened* — the server only writes that status after git succeeded — so
/// the text says so rather than reporting a failure that would invite a
/// second amend.
pub fn classify_amend_response(status: u16, body: &str) -> AmendOutcome {
    if (200..300).contains(&status) {
        return match serde_json::from_str::<AmendCommitSuccess>(body) {
            Ok(success) => AmendOutcome::Amended(Box::new(success)),
            Err(_) => AmendOutcome::Unavailable(
                "The amend was accepted, but the server's reply couldn't be read. \
                 Refresh to see the rewritten commit before amending again."
                    .to_string(),
            ),
        };
    }
    if status == 400 {
        return match serde_json::from_str::<AmendCommitError>(body) {
            Ok(err) => match err.kind {
                AmendFailureKind::StaleTip => AmendOutcome::TipMoved {
                    message: err.message,
                },
                AmendFailureKind::HookRejected => AmendOutcome::Refused {
                    refusal: AmendRefusal::Hook,
                    message: err.message,
                },
                AmendFailureKind::SigningFailed => AmendOutcome::Refused {
                    refusal: AmendRefusal::Signing,
                    message: err.message,
                },
                AmendFailureKind::Other => AmendOutcome::Refused {
                    refusal: AmendRefusal::Other,
                    message: err.message,
                },
            },
            // A 400 that is not this endpoint's typed shape is not silently
            // filed under `Other`: `Other` is a *classified* git failure, and
            // claiming a classification nobody made is the kind of quiet lie
            // the typed contract exists to remove.
            Err(_) => AmendOutcome::Unavailable(non_empty_or(
                body,
                "The server refused the amend but gave no reason it could state.",
            )),
        };
    }
    AmendOutcome::Unavailable(non_empty_or(body, &format!("HTTP {status}")))
}

fn non_empty_or(body: &str, fallback: &str) -> String {
    if body.trim().is_empty() {
        fallback.to_string()
    } else {
        body.trim().to_string()
    }
}

/// The advisory shown after a successful amend that rewrote *published*
/// history (#223's `amended_published_commit`, ADR 0040).
///
/// Three-state on purpose, matching the field: `Some(true)` warns,
/// `Some(false)` says nothing, and `None` — the reachability walk itself
/// failed — says that it does not know, rather than reporting the silence of
/// a failed check as an all-clear.
pub fn published_advisory(success: &AmendCommitSuccess) -> Option<String> {
    match success.amended_published_commit {
        Some(true) => Some(format!(
            "Heads up: {} was already on a remote. Local and remote history have \
             now diverged, so a plain push will be refused.",
            short_tip(&success.old_tip),
        )),
        Some(false) => None,
        None => Some(
            "The amend succeeded. Whether the old commit had already been pushed \
             couldn't be checked, so check before you push."
                .to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// The guided re-check after a stale tip
// ---------------------------------------------------------------------------

/// How far the post-stale-tip re-check has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recheck {
    /// Nothing tried yet — the user is being offered the check.
    Idle,
    /// Reading the current tip.
    Checking,
    /// The current tip is known and shown; the dialog now targets it.
    Retargeted { new_tip: String, summary: String },
    /// The re-check itself failed (the read, not the amend).
    Unavailable(String),
}

/// Where an amend attempt has got to, as far as the dialog is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmendPhase {
    /// Nothing attempted (or the dialog was just opened).
    Idle,
    /// The request is in flight.
    InFlight,
    /// The compare-and-swap refused: HEAD moved and **nothing was rewritten**.
    Stale {
        /// The tip the user reviewed and pressed Amend against.
        reviewed_tip: String,
        /// The server's own wording.
        message: String,
        recheck: Recheck,
    },
    /// A classified refusal that is safe to retry as-is once fixed.
    Refused {
        refusal: AmendRefusal,
        message: String,
    },
    /// Off-contract: transport, auth, 5xx, unparseable body.
    Unavailable(String),
}

/// A banner in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// The label of the banner's own button, when it has one. `None` means
    /// the banner is informational and the dialog's own buttons are the way
    /// forward.
    pub action: Option<&'static str>,
}

/// What the view renders and enables for a given phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseView {
    pub notice: Option<Notice>,
    /// Whether the confirm button may be pressed **as far as this phase is
    /// concerned** — the view still requires a non-empty message on top.
    pub confirm_enabled: bool,
    /// A request is in flight: the view shows it and blocks a second press.
    pub busy: bool,
}

/// The whole rule for what a phase looks like and what it permits.
///
/// The property that matters, and that the tests pin: while the phase is
/// [`AmendPhase::Stale`], `confirm_enabled` is false in **every** re-check
/// state except [`Recheck::Retargeted`]. Pressing Amend again after a stale
/// tip is only possible once a fresh tip has been read *and* shown, which is
/// what makes this a guided re-check rather than a blind retry. Refusals
/// behave the opposite way on purpose: a hook or signing failure leaves the
/// reviewed tip untouched, so retrying after fixing it amends exactly the
/// commit the user already approved.
pub fn phase_view(phase: &AmendPhase) -> PhaseView {
    match phase {
        AmendPhase::Idle => PhaseView {
            notice: None,
            confirm_enabled: true,
            busy: false,
        },
        AmendPhase::InFlight => PhaseView {
            notice: Some(Notice {
                title: "Amending…".to_string(),
                body: "Waiting for the server.".to_string(),
                action: None,
            }),
            confirm_enabled: false,
            busy: true,
        },
        AmendPhase::Stale {
            reviewed_tip,
            message,
            recheck,
        } => {
            let reviewed = short_tip(reviewed_tip);
            let (body, action, confirm_enabled) = match recheck {
                Recheck::Idle => (
                    format!(
                        "Nothing was rewritten — the server refused because {reviewed} is \
                         no longer the tip. {message} Check what the tip is now, then \
                         decide whether to amend that commit instead."
                    ),
                    Some("Check the current tip"),
                    false,
                ),
                Recheck::Checking => ("Reading the current tip…".to_string(), None, false),
                Recheck::Retargeted { new_tip, summary } => (
                    format!(
                        "The tip is now {} — “{summary}”. Your message below is unchanged. \
                         Amend that commit instead?",
                        short_tip(new_tip),
                    ),
                    Some("Check again"),
                    true,
                ),
                Recheck::Unavailable(why) => (
                    format!(
                        "Nothing was rewritten, and the current tip couldn't be read \
                         either: {why} Until it can be, there is no way to know what an \
                         amend would rewrite."
                    ),
                    Some("Try again"),
                    false,
                ),
            };
            PhaseView {
                notice: Some(Notice {
                    title: "HEAD moved — nothing was amended".to_string(),
                    body,
                    action,
                }),
                confirm_enabled,
                busy: matches!(recheck, Recheck::Checking),
            }
        }
        AmendPhase::Refused { refusal, message } => {
            let (title, next) = match refusal {
                AmendRefusal::Hook => (
                    "A repository hook refused the amend",
                    "Nothing was rewritten. Fix what the hook checks, then amend again — \
                     your message is still here.",
                ),
                AmendRefusal::Signing => (
                    "Signing the amended commit failed",
                    "Nothing was rewritten. This is a signing-setup problem, not a problem \
                     with the message; fix the signing key and amend again.",
                ),
                AmendRefusal::Other => (
                    "Git refused the amend",
                    "Nothing was rewritten. Your message is still here.",
                ),
            };
            PhaseView {
                notice: Some(Notice {
                    title: title.to_string(),
                    body: format!("{message} {next}"),
                    action: None,
                }),
                confirm_enabled: true,
                busy: false,
            }
        }
        AmendPhase::Unavailable(why) => PhaseView {
            notice: Some(Notice {
                title: "The amend didn't complete".to_string(),
                body: format!(
                    "{why} If the request reached the server, the commit may already have \
                     been rewritten — refresh and look before amending again."
                ),
                action: None,
            }),
            // Not disabled: the request may simply never have arrived, and the
            // amend is compare-and-swapped anyway — a retry against a tip that
            // *did* move is refused by the server, which lands back in the
            // guided re-check above rather than rewriting anything unseen.
            confirm_enabled: true,
            busy: false,
        },
    }
}

/// The commit HEAD points at, from a frame's ref list — the fresh tip the
/// guided re-check reads.
///
/// `read_refs` always emits HEAD (peeled to a commit) when it resolves,
/// whether or not it is on a branch, so this is the one ref that always
/// answers "what would an amend rewrite right now". A branch ref is *not* a
/// substitute: it answers for its own branch, which may not be the checked-out
/// one.
pub fn head_tip(refs: &[GitRef]) -> Option<String> {
    refs.iter()
        .find(|r| r.kind == RefKind::Head)
        .map(|r| r.target.0.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_core::model::Oid;
    use git_vista_core::status::FileChange;

    fn staged(paths: &[(&str, ChangeKind)]) -> Vec<FileChange> {
        paths
            .iter()
            .map(|(p, k)| FileChange {
                path: (*p).to_string(),
                kind: *k,
            })
            .collect()
    }

    fn status_with(
        staged_paths: &[(&str, ChangeKind)],
        unstaged_paths: &[(&str, ChangeKind)],
        untracked: &[&str],
    ) -> RepoStatus {
        RepoStatus {
            branch: Some("main".to_string()),
            upstream: None,
            ahead: 0,
            behind: 0,
            staged: staged(staged_paths),
            unstaged: staged(unstaged_paths),
            untracked: untracked.iter().map(|p| (*p).to_string()).collect(),
            conflicted: Vec::new(),
        }
    }

    const TIP: &str = "1111111111111111111111111111111111111111";
    const NEW_TIP: &str = "2222222222222222222222222222222222222222";

    // -----------------------------------------------------------------
    // The intent vocabulary
    // -----------------------------------------------------------------

    #[test]
    fn each_intent_maps_to_its_own_request_fields() {
        assert!(!CommitIntent::Staged.allow_empty());
        assert!(CommitIntent::Empty { branch: None }.allow_empty());
        assert!(CommitIntent::Amend {
            expected_tip: TIP.into()
        }
        .allow_empty());

        // Only the stub-branch empty commit names a branch. The other two
        // target the checked-out branch, and amend has no branch field on the
        // wire at all.
        assert_eq!(
            CommitIntent::Empty {
                branch: Some("feature".into())
            }
            .branch(),
            Some("feature")
        );
        assert_eq!(CommitIntent::Staged.branch(), None);
        assert_eq!(
            CommitIntent::Amend {
                expected_tip: TIP.into()
            }
            .branch(),
            None
        );
        assert_eq!(
            CommitIntent::Amend {
                expected_tip: TIP.into()
            }
            .expected_tip(),
            Some(TIP)
        );
        assert_eq!(CommitIntent::Staged.expected_tip(), None);
    }

    /// The request body is the server's own struct, which carries
    /// `deny_unknown_fields`: this round trip proves the client sends the
    /// exact three fields `POST /api/amend-commit` accepts, and would fail if
    /// either side renamed one.
    #[test]
    fn amend_body_matches_the_wire_contract_exactly() {
        let body = amend_body("  fix: typo  ", TIP);
        assert_eq!(body.message, "fix: typo", "the message is trimmed");
        assert_eq!(body.expected_tip, TIP);
        assert!(body.allow_empty);

        let json = serde_json::to_string(&body).unwrap();
        let back: AmendCommitRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, body);

        // Paired negative: the deny_unknown_fields contract is real, so a body
        // carrying anything else is refused — which is what makes the round
        // trip above evidence rather than decoration.
        let widened = json.replace('{', r#"{"branch":"main","#);
        assert!(
            serde_json::from_str::<AmendCommitRequest>(&widened).is_err(),
            "the amend request must reject fields the server does not define"
        );
    }

    // -----------------------------------------------------------------
    // The draft interaction (#226)
    // -----------------------------------------------------------------

    #[test]
    fn amend_edits_its_own_buffer_and_never_the_persisted_draft() {
        assert_eq!(message_buffer(&CommitIntent::Staged), MessageBuffer::Draft);
        assert_eq!(
            message_buffer(&CommitIntent::Empty { branch: None }),
            MessageBuffer::Draft
        );
        assert_eq!(
            message_buffer(&CommitIntent::Amend {
                expected_tip: TIP.into()
            }),
            MessageBuffer::Amend
        );

        // The persistence half. The draft keeps #226's per-repository key;
        // amend has no key at all, so there is no way for an amend keystroke
        // to reach storage — and specifically not the key a half-typed commit
        // message is sitting under.
        assert_eq!(
            persist_key(MessageBuffer::Draft, "wt-1"),
            Some(commit_draft_key("wt-1"))
        );
        assert_eq!(persist_key(MessageBuffer::Amend, "wt-1"), None);
    }

    #[test]
    fn a_late_prefill_never_overwrites_what_the_user_typed() {
        // Untouched box (current == the seed it was given): adopt.
        assert_eq!(
            adopt_seed("", "", "the tip's message"),
            Some("the tip's message".to_string())
        );
        assert_eq!(
            adopt_seed("old tip message", "old tip message", "new tip message"),
            Some("new tip message".to_string())
        );

        // Edited box: keep the user's work, whatever arrives.
        assert_eq!(adopt_seed("my own words", "", "the tip's message"), None);
        assert_eq!(
            adopt_seed("edited", "old tip message", "new tip message"),
            None
        );

        // Nothing to do when it already says that.
        assert_eq!(adopt_seed("same", "same", "same"), None);
    }

    // -----------------------------------------------------------------
    // Staged scope
    // -----------------------------------------------------------------

    #[test]
    fn staged_breadth_separates_nothing_partial_and_everything() {
        let nothing = status_with(&[], &[("a.rs", ChangeKind::Modified)], &[]);
        assert_eq!(staged_breadth(&nothing), StagedBreadth::Nothing);

        let all = status_with(&[("a.rs", ChangeKind::Modified)], &[], &[]);
        assert_eq!(staged_breadth(&all), StagedBreadth::Everything);

        let partial = status_with(
            &[("a.rs", ChangeKind::Modified)],
            &[("b.rs", ChangeKind::Modified)],
            &[],
        );
        assert_eq!(staged_breadth(&partial), StagedBreadth::Partial);

        // An untracked file is "not staged" too — a commit made now leaves it
        // behind, so this must not read as Everything.
        let untracked_only = status_with(&[("a.rs", ChangeKind::Added)], &[], &["notes.txt"]);
        assert_eq!(staged_breadth(&untracked_only), StagedBreadth::Partial);
    }

    #[test]
    fn the_scope_review_lists_staged_paths_with_their_verbs() {
        let status = status_with(
            &[
                ("src/a.rs", ChangeKind::Modified),
                ("src/b.rs", ChangeKind::Added),
                ("src/c.rs", ChangeKind::Deleted),
            ],
            &[],
            &[],
        );
        let review = scope_review(&CommitIntent::Staged, Some(&status), 12);
        assert_eq!(review.lines.len(), 3);
        assert_eq!(review.hidden, 0);
        assert_eq!(review.lines[0].path, "src/a.rs");
        assert_eq!(review.lines[0].kind, "modified");
        assert_eq!(review.lines[1].kind, "added");
        assert_eq!(review.lines[2].kind, "deleted");
        assert!(review.notes.is_empty(), "{:?}", review.notes);
    }

    #[test]
    fn a_file_edited_after_staging_is_flagged_on_its_own_line() {
        let status = status_with(
            &[("src/a.rs", ChangeKind::Modified)],
            &[("src/a.rs", ChangeKind::Modified)],
            &[],
        );
        let review = scope_review(&CommitIntent::Staged, Some(&status), 12);
        assert_eq!(review.lines.len(), 1);
        assert!(
            review.lines[0].also_modified,
            "a path dirty on both sides commits its staged version, and the line \
             has to say so"
        );

        // Paired negative: a path dirty only in the index is not flagged, so
        // the flag means something.
        let clean = status_with(&[("src/a.rs", ChangeKind::Modified)], &[], &[]);
        assert!(!scope_review(&CommitIntent::Staged, Some(&clean), 12).lines[0].also_modified);
    }

    #[test]
    fn the_scope_review_counts_what_it_does_not_show() {
        let many: Vec<(&str, ChangeKind)> = vec![
            ("a", ChangeKind::Modified),
            ("b", ChangeKind::Modified),
            ("c", ChangeKind::Modified),
            ("d", ChangeKind::Modified),
        ];
        let status = status_with(&many, &[], &[]);
        let review = scope_review(&CommitIntent::Staged, Some(&status), 2);
        assert_eq!(review.lines.len(), 2);
        assert_eq!(review.hidden, 2, "the cut paths are still counted");
    }

    #[test]
    fn an_unread_status_says_so_instead_of_claiming_an_empty_commit() {
        let review = scope_review(&CommitIntent::Staged, None, 12);
        assert!(review.lines.is_empty());
        assert!(
            review.notes.iter().any(|n| n.contains("hasn't been read")),
            "an empty list with no note would assert the commit contains nothing, \
             which a status probe that never answered cannot know: {:?}",
            review.notes
        );
        // And it does NOT claim nothing is staged — that is a different
        // sentence, and only the answered case is entitled to it.
        assert!(
            !review.notes.iter().any(|n| n.contains("Nothing is staged")),
            "{:?}",
            review.notes
        );
    }

    #[test]
    fn a_stub_branch_empty_commit_reports_an_untouched_index() {
        let status = status_with(&[("staged.rs", ChangeKind::Modified)], &[], &[]);
        let review = scope_review(
            &CommitIntent::Empty {
                branch: Some("feature".into()),
            },
            Some(&status),
            12,
        );
        assert!(
            review.lines.is_empty(),
            "a stub-branch empty commit is written with commit-tree/update-ref, so \
             the checked-out index is not part of it"
        );
        assert!(review.notes[0].contains("feature"));
        assert!(review.notes[0].contains("untouched"));
    }

    #[test]
    fn exclusions_are_stated_for_a_partial_commit() {
        let status = status_with(
            &[("a.rs", ChangeKind::Modified)],
            &[("b.rs", ChangeKind::Modified)],
            &["c.txt", "d.txt"],
        );
        let review = scope_review(&CommitIntent::Staged, Some(&status), 12);
        let note = review
            .notes
            .iter()
            .find(|n| n.contains("stay out"))
            .expect("a partial commit states what it leaves behind");
        assert!(note.contains("1 file of unstaged"), "{note}");
        assert!(note.contains("2 files untracked"), "{note}");
    }

    #[test]
    fn an_amend_with_an_empty_index_says_the_files_are_unchanged() {
        let status = status_with(&[], &[], &[]);
        let review = scope_review(
            &CommitIntent::Amend {
                expected_tip: TIP.into(),
            },
            Some(&status),
            12,
        );
        assert!(
            review.notes[0].contains("only its message changes"),
            "{:?}",
            review.notes
        );
        // The plain-commit wording for the same empty index is the opposite
        // advice, so the two must not share a sentence.
        let plain = scope_review(&CommitIntent::Staged, Some(&status), 12);
        assert_ne!(plain.notes[0], review.notes[0]);
        assert!(
            plain.notes[0].contains("nothing to commit"),
            "{:?}",
            plain.notes
        );
    }

    // -----------------------------------------------------------------
    // The three states are visibly different
    // -----------------------------------------------------------------

    #[test]
    fn empty_partial_and_amend_share_no_wording() {
        let partial_status = status_with(
            &[("a.rs", ChangeKind::Modified)],
            &[("b.rs", ChangeKind::Modified)],
            &[],
        );
        let clean_status = status_with(&[], &[], &[]);

        let partial = dialog_copy(&CommitIntent::Staged, Some(&partial_status));
        let empty = dialog_copy(&CommitIntent::Empty { branch: None }, Some(&clean_status));
        let amend = dialog_copy(
            &CommitIntent::Amend {
                expected_tip: TIP.into(),
            },
            Some(&clean_status),
        );

        let all = [&partial, &empty, &amend];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a.title, b.title, "two states share a title");
                assert_ne!(a.body, b.body, "two states share their explanation");
                assert_ne!(
                    a.confirm_label, b.confirm_label,
                    "two states share a button label"
                );
            }
        }

        // And each one says its own thing, so the distinctness above is not
        // three arbitrary strings. Checked on words the *other* two must not
        // contain, which is the half that can actually fail.
        assert!(partial.body.contains("partial commit"), "{}", partial.body);
        assert!(!partial.body.contains("empty"), "{}", partial.body);
        assert!(!partial.body.contains("Rewrites"), "{}", partial.body);

        assert!(empty.body.contains("no file changes"), "{}", empty.body);
        assert!(!empty.body.contains("Rewrites"), "{}", empty.body);

        assert!(amend.body.contains("Rewrites"), "{}", amend.body);
        assert!(amend.body.contains("replaced"), "{}", amend.body);
        assert!(amend.body.contains(short_tip(TIP)), "{}", amend.body);
    }

    #[test]
    fn an_empty_commit_over_a_dirty_index_admits_it_will_not_be_empty() {
        let status = status_with(&[("a.rs", ChangeKind::Modified)], &[], &[]);
        let copy = dialog_copy(&CommitIntent::Empty { branch: None }, Some(&status));
        assert!(
            copy.body.contains("will not be empty"),
            "`git commit --allow-empty` commits a non-empty index; saying otherwise \
             would be the dialog lying about the outcome: {}",
            copy.body
        );

        // Paired positive: with a clean index the same mode makes the plain
        // claim, so the warning is conditional rather than always-on.
        let clean = dialog_copy(
            &CommitIntent::Empty { branch: None },
            Some(&status_with(&[], &[], &[])),
        );
        assert!(!clean.body.contains("will not be empty"), "{}", clean.body);
    }

    #[test]
    fn only_a_known_empty_index_blocks_a_plain_commit() {
        let blocked = dialog_copy(&CommitIntent::Staged, Some(&status_with(&[], &[], &[])));
        assert!(blocked.blocked_reason.is_some());

        // Known non-empty: allowed.
        let ok = dialog_copy(
            &CommitIntent::Staged,
            Some(&status_with(&[("a.rs", ChangeKind::Added)], &[], &[])),
        );
        assert!(ok.blocked_reason.is_none());

        // Unknown: allowed. A status read that has not answered must not
        // disable the button — that would make a slow probe look like a
        // repository that cannot commit.
        assert!(dialog_copy(&CommitIntent::Staged, None)
            .blocked_reason
            .is_none());

        // The other two modes are never blocked by an empty index: an empty
        // commit wants one, and an amend can be message-only.
        assert!(dialog_copy(
            &CommitIntent::Empty { branch: None },
            Some(&status_with(&[], &[], &[]))
        )
        .blocked_reason
        .is_none());
        assert!(dialog_copy(
            &CommitIntent::Amend {
                expected_tip: TIP.into()
            },
            Some(&status_with(&[], &[], &[]))
        )
        .blocked_reason
        .is_none());
    }

    // -----------------------------------------------------------------
    // Reading the endpoint back
    // -----------------------------------------------------------------

    fn error_body(kind: AmendFailureKind, message: &str) -> String {
        // Built from the server's own DTO and serialized the way the server
        // serializes it — not a hand-written JSON string that could agree
        // with a client-side assumption while disagreeing with the wire.
        serde_json::to_string(&AmendCommitError {
            kind,
            message: message.to_string(),
        })
        .unwrap()
    }

    #[test]
    fn a_success_body_is_read_back_whole() {
        let success = AmendCommitSuccess {
            message: "Amended commit.".into(),
            old_tip: TIP.into(),
            new_tip: Some(NEW_TIP.into()),
            amended_published_commit: Some(true),
        };
        let outcome = classify_amend_response(200, &serde_json::to_string(&success).unwrap());
        match outcome {
            AmendOutcome::Amended(got) => assert_eq!(*got, success),
            other => panic!("expected Amended, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_tip_can_only_arrive_as_the_guided_recheck() {
        let outcome = classify_amend_response(
            400,
            &error_body(
                AmendFailureKind::StaleTip,
                "HEAD has moved since this amend was reviewed — refresh and try again.",
            ),
        );
        match outcome {
            AmendOutcome::TipMoved { message } => {
                assert!(message.contains("HEAD has moved"), "{message}");
            }
            other => panic!(
                "a stale tip must never become a plain refusal the user can retry \
                 blindly, got {other:?}"
            ),
        }
    }

    #[test]
    fn each_typed_kind_keeps_its_own_classification_and_gits_own_words() {
        for (kind, expected) in [
            (AmendFailureKind::HookRejected, AmendRefusal::Hook),
            (AmendFailureKind::SigningFailed, AmendRefusal::Signing),
            (AmendFailureKind::Other, AmendRefusal::Other),
        ] {
            let outcome = classify_amend_response(400, &error_body(kind, "git said this"));
            match outcome {
                AmendOutcome::Refused { refusal, message } => {
                    assert_eq!(refusal, expected, "{kind:?} was misclassified");
                    assert_eq!(
                        message, "git said this",
                        "the server's own text must survive verbatim"
                    );
                }
                other => panic!("expected a refusal for {kind:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn off_contract_answers_are_not_dressed_up_as_classifications() {
        // A 400 that is not this endpoint's shape (an older server, a proxy).
        match classify_amend_response(400, "plain prose refusal") {
            AmendOutcome::Unavailable(text) => assert!(text.contains("plain prose refusal")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        // Statuses outside the typed contract keep their prose.
        match classify_amend_response(409, "The repository moved under this request.") {
            AmendOutcome::Unavailable(text) => assert!(text.contains("moved under this request")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        // An empty body still says something.
        match classify_amend_response(503, "") {
            AmendOutcome::Unavailable(text) => assert!(text.contains("503")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_success_body_still_says_the_amend_happened() {
        match classify_amend_response(200, "not json") {
            AmendOutcome::Unavailable(text) => {
                assert!(
                    text.contains("was accepted"),
                    "a 200 means git already rewrote the commit; reporting a plain \
                     failure would invite a second amend: {text}"
                );
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn the_published_advisory_keeps_all_three_answers_apart() {
        let mut success = AmendCommitSuccess {
            message: "Amended commit.".into(),
            old_tip: TIP.into(),
            new_tip: Some(NEW_TIP.into()),
            amended_published_commit: Some(true),
        };
        let warned = published_advisory(&success).expect("a published amend warns");
        assert!(warned.contains("diverged"), "{warned}");
        assert!(warned.contains(short_tip(TIP)), "{warned}");

        success.amended_published_commit = Some(false);
        assert_eq!(
            published_advisory(&success),
            None,
            "a commit the walk proved unpublished needs no warning"
        );

        success.amended_published_commit = None;
        let unknown = published_advisory(&success).expect("an unknown answer is not an all-clear");
        assert!(unknown.contains("couldn't be checked"), "{unknown}");
        assert_ne!(unknown, warned);
    }

    // -----------------------------------------------------------------
    // The guided re-check
    // -----------------------------------------------------------------

    fn stale(recheck: Recheck) -> AmendPhase {
        AmendPhase::Stale {
            reviewed_tip: TIP.to_string(),
            message: "HEAD has moved.".to_string(),
            recheck,
        }
    }

    #[test]
    fn a_stale_tip_re_enables_amend_only_after_the_new_tip_has_been_shown() {
        for blocked in [
            stale(Recheck::Idle),
            stale(Recheck::Checking),
            stale(Recheck::Unavailable("the tunnel dropped.".into())),
        ] {
            let view = phase_view(&blocked);
            assert!(
                !view.confirm_enabled,
                "amend must stay disabled until the current tip has been read and \
                 shown, otherwise the retry rewrites a commit nobody reviewed: {blocked:?}"
            );
            assert!(view.notice.is_some(), "{blocked:?}");
        }

        let retargeted = stale(Recheck::Retargeted {
            new_tip: NEW_TIP.to_string(),
            summary: "fix: the other thing".to_string(),
        });
        let view = phase_view(&retargeted);
        assert!(
            view.confirm_enabled,
            "once the fresh tip is on screen the user can act on it — a dead end \
             would be the failure this flow exists to avoid"
        );
        let notice = view
            .notice
            .expect("the retargeted state still explains itself");
        assert!(notice.body.contains(short_tip(NEW_TIP)), "{}", notice.body);
        assert!(
            notice.body.contains("fix: the other thing"),
            "{}",
            notice.body
        );
    }

    #[test]
    fn every_stale_state_says_nothing_was_rewritten_and_offers_a_way_on() {
        for recheck in [
            Recheck::Idle,
            Recheck::Retargeted {
                new_tip: NEW_TIP.into(),
                summary: "s".into(),
            },
            Recheck::Unavailable("why".into()),
        ] {
            let view = phase_view(&stale(recheck.clone()));
            let notice = view.notice.expect("a stale tip is always explained");
            assert!(
                notice.title.contains("nothing was amended"),
                "the user's first question is whether their history changed: {}",
                notice.title
            );
            assert!(
                notice.action.is_some(),
                "every stale state but the in-flight one offers the next step: {recheck:?}"
            );
        }
        // The one state with no button is the one where a request is already
        // running — offering a second would double the read.
        let checking = phase_view(&stale(Recheck::Checking));
        assert!(checking.notice.unwrap().action.is_none());
        assert!(checking.busy);
    }

    #[test]
    fn a_refusal_is_retryable_where_a_stale_tip_is_not() {
        for refusal in [
            AmendRefusal::Hook,
            AmendRefusal::Signing,
            AmendRefusal::Other,
        ] {
            let view = phase_view(&AmendPhase::Refused {
                refusal,
                message: "git said this.".into(),
            });
            assert!(
                view.confirm_enabled,
                "a hook/signing refusal leaves the reviewed tip in place, so amending \
                 again after fixing it rewrites exactly the approved commit: {refusal:?}"
            );
            let notice = view.notice.expect("a refusal is explained");
            assert!(notice.body.contains("git said this."), "{}", notice.body);
            assert!(
                notice.body.contains("Nothing was rewritten"),
                "{}",
                notice.body
            );
        }

        // The three refusals do not share a title, so "a hook refused this" and
        // "signing failed" are never the same screen.
        let titles: Vec<String> = [
            AmendRefusal::Hook,
            AmendRefusal::Signing,
            AmendRefusal::Other,
        ]
        .into_iter()
        .map(|refusal| {
            phase_view(&AmendPhase::Refused {
                refusal,
                message: String::new(),
            })
            .notice
            .unwrap()
            .title
        })
        .collect();
        assert_ne!(titles[0], titles[1]);
        assert_ne!(titles[1], titles[2]);
        assert_ne!(titles[0], titles[2]);
    }

    #[test]
    fn idle_shows_no_banner_and_in_flight_blocks_a_second_press() {
        let idle = phase_view(&AmendPhase::Idle);
        assert!(idle.notice.is_none());
        assert!(idle.confirm_enabled);
        assert!(!idle.busy);

        let flying = phase_view(&AmendPhase::InFlight);
        assert!(flying.busy);
        assert!(
            !flying.confirm_enabled,
            "a second press while the first amend is in flight is a second rewrite"
        );
    }

    #[test]
    fn an_off_contract_failure_warns_that_the_amend_may_have_landed() {
        let view = phase_view(&AmendPhase::Unavailable("the tunnel dropped.".into()));
        let notice = view.notice.unwrap();
        assert!(
            notice.body.contains("the tunnel dropped."),
            "{}",
            notice.body
        );
        assert!(
            notice.body.contains("may already have"),
            "an abandoned request is not a failed one — the server may have run it: {}",
            notice.body
        );
    }

    // -----------------------------------------------------------------
    // The fresh tip
    // -----------------------------------------------------------------

    fn git_ref(name: &str, kind: RefKind, target: &str) -> GitRef {
        GitRef {
            name: name.to_string(),
            kind,
            target: Oid(target.to_string()),
        }
    }

    #[test]
    fn the_fresh_tip_comes_from_head_and_from_nothing_else() {
        let refs = vec![
            git_ref("main", RefKind::Branch, TIP),
            git_ref("HEAD", RefKind::Head, NEW_TIP),
            git_ref("origin/main", RefKind::RemoteBranch, TIP),
        ];
        assert_eq!(head_tip(&refs), Some(NEW_TIP.to_string()));

        // Paired negative: with no HEAD (an unborn repository, a ref store
        // that would not read) there is no tip to amend, and the caller must
        // hear that rather than be handed a branch's tip that HEAD may not be
        // on.
        let branches_only = vec![
            git_ref("main", RefKind::Branch, TIP),
            git_ref("v1", RefKind::Tag, TIP),
        ];
        assert_eq!(head_tip(&branches_only), None);
        assert_eq!(head_tip(&[]), None);
    }

    #[test]
    fn short_tip_shortens_and_survives_a_short_input() {
        assert_eq!(short_tip(TIP), "1111111");
        assert_eq!(short_tip("abc"), "abc");
        assert_eq!(short_tip(""), "");
    }
}
