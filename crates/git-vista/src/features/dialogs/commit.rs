//! The commit dialog's decisions — framework-free, host-tested (M2.19c, #224;
//! the published-history ceremony and the actionable refusal copy, M2.19d,
//! #225).
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
//!
//! # Rewriting pushed history is the client's ceremony to run
//!
//! ADR 0040 records that `POST /api/amend-commit` does **not** refuse an amend
//! of a commit that is already on a remote: it runs it and reports it
//! afterwards, because refusing would make a legitimate operation impossible
//! and because the server cannot know whether the user was told. #225 is the
//! other half of that decision. [`amend_preflight`] is what stands between the
//! confirm button and the POST, and [`PreflightKnowledge`] is why a
//! confirmation given for one commit cannot be spent on a different one — the
//! guided re-check above retargets an *open* dialog, so "the user already
//! agreed" has to carry which commit they agreed about.

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

// ---------------------------------------------------------------------------
// Which submit path the confirm button takes
// ---------------------------------------------------------------------------

/// A [`CommitIntent`] that is **statically not an amend**.
///
/// The field is private and this module has no public constructor for it, so
/// the only way to obtain one is [`submit_path`]. That is the whole point: the
/// confirm button used to choose between the two submit closures with a match
/// written in `dialogs/commit.rs`, which is wasm-only and therefore never
/// compiled by `cargo test`. A copy-paste that sent `CommitIntent::Amend` to
/// the plain-commit closure compiled cleanly, passed every test, and turned
/// "amend the tip" into "write a second commit" — a rewritten-history bug with
/// nothing but a manual walkthrough standing in front of it.
///
/// With the two closures taking `PlainCommit` and [`AmendTarget`], that
/// mis-dispatch is a type error rather than a discipline lapse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainCommit(CommitIntent);

impl PlainCommit {
    /// The intent this path commits — never `CommitIntent::Amend`.
    pub fn intent(&self) -> &CommitIntent {
        &self.0
    }

    /// Consume the wrapper; the submit closure needs the intent by value for
    /// the buffer read and the draft clear.
    pub fn into_intent(self) -> CommitIntent {
        self.0
    }
}

/// The reviewed tip an amend is compare-and-swapped against, already extracted.
///
/// The twin of [`PlainCommit`]: a value the amend closure can accept and the
/// plain-commit closure cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmendTarget(String);

impl AmendTarget {
    /// The compare-and-swap pin, verbatim — never shortened, since it is what
    /// the request carries.
    pub fn expected_tip(&self) -> &str {
        &self.0
    }

    /// The intent this path amends, for the buffer read (`message_buffer`
    /// routes an amend to its own non-persisted buffer).
    pub fn intent(&self) -> CommitIntent {
        CommitIntent::Amend {
            expected_tip: self.0.clone(),
        }
    }
}

/// Which of the dialog's two submit paths an intent takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitPath {
    /// `POST /api/commit` — the plain and empty modes.
    Commit(PlainCommit),
    /// `POST /api/amend-commit` — the compare-and-swapped rewrite.
    Amend(AmendTarget),
}

/// The confirm button's dispatch, decided here rather than in the view.
///
/// Two modes collapse onto one path and one mode onto the other, which is
/// exactly why this is worth naming: `Staged` and `Empty` differ in what they
/// *send* (`allow_empty`, `branch`) but not in which endpoint they reach,
/// while `Amend` reaches a different endpoint with a different failure
/// vocabulary and a different buffer.
pub fn submit_path(intent: &CommitIntent) -> SubmitPath {
    match intent {
        CommitIntent::Amend { expected_tip } => {
            SubmitPath::Amend(AmendTarget(expected_tip.clone()))
        }
        CommitIntent::Staged | CommitIntent::Empty { .. } => {
            SubmitPath::Commit(PlainCommit(intent.clone()))
        }
    }
}

// ---------------------------------------------------------------------------
// The context menu's amend gate
// ---------------------------------------------------------------------------

/// Whether the context menu offers "Amend last commit" on the tapped row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmendOffer {
    /// Enabled: this row is the commit an amend would rewrite.
    Offered,
    /// Disabled, carrying the reason the item must show — #65's rule is that a
    /// disabled control states why, on screen, not only in a `title=`.
    Blocked(&'static str),
}

/// The gate for the "Amend last commit" menu item.
///
/// `is_head` — the tapped row is the commit HEAD resolves to. `is_stub` — the
/// row is a branch stub rather than a commit dot.
///
/// Both conditions are load-bearing and neither implies the other. HEAD is
/// required because `git commit --amend` rewrites the checked-out branch's own
/// tip and nothing else, so offering it on any other dot would rewrite a
/// commit the user did not tap. The stub exclusion is separate:
/// `GitOperation::AmendCommit` has no branch field at all, so there is no
/// "amend that stub" for the server to honour — and the stub case is checked
/// first so that a row which is somehow both gets the accurate reason rather
/// than the HEAD one.
///
/// Lives here, and not as a condition inlined in `menu.rs`, because that file
/// is wasm-only: an inverted or dropped condition there would put the item on
/// every stub (or take it away everywhere) without a single test going red.
pub fn amend_offer(is_head: bool, is_stub: bool) -> AmendOffer {
    if is_stub {
        return AmendOffer::Blocked("Amending rewrites the checked-out branch's tip, not a stub");
    }
    if !is_head {
        return AmendOffer::Blocked("Only the commit at HEAD can be amended");
    }
    AmendOffer::Offered
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

/// What a pre-fill actually did to the message box.
///
/// The guided re-check has to *say* which of these happened, and it used to
/// guess: the banner announced "Your message below is unchanged" and the very
/// next statement called `seed_amend_msg`, which — when the box still held the
/// old tip's pre-fill verbatim, the common case for an amend that only folds in
/// staged files — replaced the text the banner had just vouched for. A user who
/// trusts the banner and presses Amend without re-reading the box is then
/// committing a message they never saw.
///
/// Reporting it instead of predicting it is what keeps the two honest:
/// `Dialogs::seed_amend_msg` returns this, and [`Recheck::Retargeted`] carries
/// it, so the banner renders from what happened rather than from an assumption
/// about what would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedOutcome {
    /// The box was left alone — the user had typed their own message, or the
    /// incoming message is already what is in there.
    Kept,
    /// The box was replaced with the incoming message.
    Replaced,
}

/// [`adopt_seed`]'s answer restated as the fact the banner reports.
///
/// A thin wrapper on purpose: the *decision* stays in `adopt_seed` (one rule,
/// one place), and this is only the classification of its outcome, so the two
/// cannot drift apart into a box that says one thing and a banner that says
/// another.
pub fn seed_outcome(adopted: Option<&String>) -> SeedOutcome {
    match adopted {
        Some(_) => SeedOutcome::Replaced,
        None => SeedOutcome::Kept,
    }
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
// The pre-flight published-history ceremony (#225)
// ---------------------------------------------------------------------------

/// Whether the commit an amend is about to rewrite is already on a remote.
///
/// The input is `CommitDetail::on_remote` — an *exact* answer for one commit
/// (`git_vista_git::remote_membership`'s bounded walk), not membership of
/// whatever page of history happens to be loaded. The dialog already fetches
/// that detail to pre-fill the message box, so the pre-flight costs no extra
/// request.
///
/// Three states, not two, because "we never read it" is not "it is not
/// published": a failed `GET /api/commit/{id}` leaves the box empty and the
/// dialog would otherwise have to pretend it knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPublication {
    /// Read for this exact tip, and it is reachable from a remote-tracking ref.
    Published,
    /// Read for this exact tip, and it is not.
    Unpublished,
    /// Never read, the read failed, or what was read was read for a *different*
    /// commit — the guided re-check retargets the open dialog without clearing
    /// this, so a stale answer must not be mistaken for this tip's answer.
    Unknown,
}

/// What the pre-flight gate consults, all of it scoped to a specific tip.
///
/// Tip-scoping is the load-bearing part. The dialog can be retargeted at a
/// different commit while it is open — that is what the post-stale-tip guided
/// re-check does, deliberately without going through `Dialogs::open` so the
/// typed message survives — so both halves of this ("what we read" and "what
/// the user agreed to") carry the commit they were true of. A confirmation
/// given for one commit is not a confirmation for the next one, and that is a
/// property of this type rather than of a reset some later edit can forget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightKnowledge {
    /// The last `CommitDetail` read: which commit it was for, and its
    /// `on_remote`.
    read: Option<(String, bool)>,
    /// The commit the user explicitly agreed to rewrite, if any.
    confirmed: Option<String>,
}

impl PreflightKnowledge {
    /// Record a `CommitDetail` answer. Called wherever the dialog reads one —
    /// opening amend mode from the menu, and again after the guided re-check
    /// retargets.
    pub fn record_detail(&mut self, tip: &str, on_remote: bool) {
        self.read = Some((tip.to_string(), on_remote));
    }

    /// Record that the user took the ceremony's explicit second step for `tip`.
    pub fn confirm(&mut self, tip: &str) {
        self.confirmed = Some(tip.to_string());
    }

    /// What is known about `tip` specifically.
    pub fn publication(&self, tip: &str) -> TargetPublication {
        match &self.read {
            Some((read_tip, on_remote)) if read_tip == tip => {
                if *on_remote {
                    TargetPublication::Published
                } else {
                    TargetPublication::Unpublished
                }
            }
            _ => TargetPublication::Unknown,
        }
    }

    /// Whether the user has confirmed rewriting `tip` — and no other commit.
    pub fn confirmed_for(&self, tip: &str) -> bool {
        self.confirmed.as_deref() == Some(tip)
    }
}

/// What the confirm button's press may do right now.
///
/// Both arms carry the [`AmendTarget`] back out rather than letting the caller
/// rebuild one: an [`AmendTarget`] can still only originate in [`submit_path`],
/// so the ceremony cannot become a second way to conjure an amend of some other
/// commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// Nothing to escalate — `POST /api/amend-commit` now.
    Send(AmendTarget),
    /// The target is published and the user has not agreed to that yet. The
    /// view must send **nothing** and enter
    /// [`AmendPhase::AwaitingPublishedConfirm`].
    Confirm(AmendTarget),
}

/// The pre-flight gate: does this press need the published-history ceremony
/// first?
///
/// ADR 0040 records that the server does **not** block an amend of pushed
/// history — it executes it and reports it afterwards, because refusing would
/// make a legitimate operation impossible and because only the client knows
/// whether the user was told. This function is that "only the client knows"
/// half: it is what stands between a press and the POST.
///
/// [`TargetPublication::Unknown`] sends. That is a deliberate, narrow choice
/// and not an assumption of safety: escalating on "we could not read the
/// detail" would put a history-rewriting ceremony in front of ordinary amends
/// whenever a request failed, training the user to click through it — and the
/// case is already covered on the other side by [`published_advisory`], which
/// reports an unknown answer as unknown rather than as an all-clear. The gap
/// is real and stated: an amend whose detail read failed reaches the server
/// with no pre-flight, and the user learns about the divergence afterwards.
pub fn amend_preflight(target: AmendTarget, knowledge: &PreflightKnowledge) -> Preflight {
    let tip = target.expected_tip();
    match knowledge.publication(tip) {
        TargetPublication::Published if !knowledge.confirmed_for(tip) => Preflight::Confirm(target),
        TargetPublication::Published
        | TargetPublication::Unpublished
        | TargetPublication::Unknown => Preflight::Send(target),
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
    ///
    /// `message` is what the retarget did to the box — reported by
    /// `Dialogs::seed_amend_msg`, never assumed. Retargeting offers the new
    /// tip's message as a pre-fill, and that offer is *accepted* whenever the
    /// user had not edited the old tip's pre-fill, so the banner cannot state
    /// the box is untouched without being told.
    Retargeted {
        new_tip: String,
        summary: String,
        message: SeedOutcome,
    },
    /// The re-check itself failed (the read, not the amend).
    Unavailable(String),
}

/// Where an amend attempt has got to, as far as the dialog is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmendPhase {
    /// Nothing attempted (or the dialog was just opened).
    Idle,
    /// Amend was pressed on a commit that is already on a remote, and
    /// **nothing has been sent**. The ceremony (#225): the press is spent on
    /// raising the warning, and a second, differently-labelled control is the
    /// only way on. Carries the target so the confirmation cannot be applied
    /// to a commit other than the one it names.
    AwaitingPublishedConfirm { target: AmendTarget },
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
///
/// [`AmendPhase::AwaitingPublishedConfirm`] (#225) shares the disabled-confirm
/// rule for a different reason: nothing has been *sent*, and the way past it is
/// the banner's own button, so that agreeing to rewrite pushed history is a
/// separate act with its own words rather than a second press of the same
/// green button.
pub fn phase_view(phase: &AmendPhase) -> PhaseView {
    match phase {
        AmendPhase::Idle => PhaseView {
            notice: None,
            confirm_enabled: true,
            busy: false,
        },
        AmendPhase::AwaitingPublishedConfirm { target } => PhaseView {
            notice: Some(Notice {
                title: "This commit has already been pushed".to_string(),
                body: format!(
                    "Nothing has been sent yet. {} is reachable from a remote-tracking \
                     ref, so it is on a remote and other clones may already have it. \
                     Amending replaces it with a different commit: your branch and the \
                     remote's will diverge, a plain push will be refused, and anyone who \
                     already pulled the old commit has to reconcile it by hand. Cancel \
                     leaves it exactly as it is.",
                    short_tip(target.expected_tip()),
                ),
                action: Some("Rewrite this pushed commit"),
            }),
            // The green Amend button goes inert on purpose: the press that
            // raised this warning must not also be the press that satisfies it,
            // and the way on carries its own, different words. That is the
            // whole difference between a ceremony and a banner.
            confirm_enabled: false,
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
                Recheck::Retargeted {
                    new_tip,
                    summary,
                    message,
                } => (
                    format!(
                        "The tip is now {} — “{summary}”. {} Amend that commit instead?",
                        short_tip(new_tip),
                        match message {
                            // Said only when it is true. The box holds what the
                            // user put there — either their own words, or a
                            // pre-fill identical to the new tip's message.
                            SeedOutcome::Kept => "Your message below is unchanged.",
                            // The box was still holding the *old* tip's
                            // pre-fill, untouched, so it has been replaced with
                            // this commit's message — read it before you
                            // confirm, because confirming replaces this
                            // commit's message with whatever is in the box.
                            SeedOutcome::Replaced =>
                                "The message below has been replaced with that commit's own \
                                 message — you hadn't edited the old one. Read it before you \
                                 confirm.",
                        },
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
            // Each of the three names something the user can actually go and
            // do (#225). Not decoration: a hook rejection and a signing failure
            // are fixed in completely different places, and the pre-#225 copy
            // ("fix what the hook checks", "fix the signing key") named the
            // category without naming a single thing to open or type. The
            // unclassified case says so out loud instead of guessing, because
            // inventing a remedy for a failure nobody classified is how a user
            // ends up editing a signing config over a full disk.
            let (title, next) = match refusal {
                AmendRefusal::Hook => (
                    "A repository hook refused the amend",
                    "Nothing was rewritten. What the hook printed is above — fix what it \
                     reports and press Amend again; your message is still here. This \
                     dialog has no bypass, so a hook you believe is wrong has to be fixed \
                     or disabled in the repository's hooks directory (.git/hooks).",
                ),
                AmendRefusal::Signing => (
                    "Signing the amended commit failed",
                    "Nothing was rewritten, and this is a signing-setup problem rather \
                     than anything about your message. Check that `git config \
                     user.signingkey` names a key you still have, that `git config \
                     commit.gpgsign` is set the way you meant, and that the key is \
                     unlocked — then press Amend again.",
                ),
                AmendRefusal::Other => (
                    "Git refused the amend",
                    "Nothing was rewritten. This isn't a hook rejection or a signing \
                     failure, and nothing classified it further, so git's own words above \
                     are all there is to go on — read them, fix what they name, and press \
                     Amend again. Your message is still here.",
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

    // -----------------------------------------------------------------
    // The confirm button's dispatch
    // -----------------------------------------------------------------

    /// The seam the M2.19c review flagged: which endpoint the confirm button
    /// reaches used to be chosen by a match in `dialogs/commit.rs`, a wasm-only
    /// file `cargo test` never compiles. The mapping is stated here against
    /// literals, so a reordered or copy-pasted arm is a red test rather than a
    /// commit the user never asked for.
    #[test]
    fn every_intent_reaches_the_endpoint_its_mode_names() {
        // The two commit modes: same endpoint, and the intent survives intact
        // so the submit closure can still read `allow_empty` and `branch`.
        let staged = submit_path(&CommitIntent::Staged);
        assert_eq!(
            staged,
            SubmitPath::Commit(PlainCommit(CommitIntent::Staged)),
            "a staged commit is a plain commit"
        );
        let empty = CommitIntent::Empty {
            branch: Some("wip".into()),
        };
        assert_eq!(
            submit_path(&empty),
            SubmitPath::Commit(PlainCommit(empty.clone())),
            "an empty commit on a stub is still the plain-commit endpoint, and \
             it must keep its branch target"
        );

        // Amend is the other endpoint, and it arrives carrying the reviewed
        // tip verbatim — the compare-and-swap pin, which a shortened or
        // trimmed copy would break.
        let SubmitPath::Amend(target) = submit_path(&CommitIntent::Amend {
            expected_tip: TIP.into(),
        }) else {
            panic!("an amend intent must not reach the plain-commit endpoint");
        };
        assert_eq!(target.expected_tip(), TIP);
        assert_eq!(
            target.intent(),
            CommitIntent::Amend {
                expected_tip: TIP.into()
            },
            "the buffer read on the amend path must resolve to the amend buffer"
        );
    }

    /// The invariant `PlainCommit`'s private field exists to hold: whatever is
    /// fed in, the plain-commit path never carries an amend. `submit_path` is
    /// the only constructor, so this is a statement about every `PlainCommit`
    /// that can exist, not only the three built here.
    #[test]
    fn the_plain_commit_path_can_never_carry_an_amend() {
        let intents = [
            CommitIntent::Staged,
            CommitIntent::Empty { branch: None },
            CommitIntent::Empty {
                branch: Some("wip".into()),
            },
            CommitIntent::Amend {
                expected_tip: TIP.into(),
            },
            CommitIntent::Amend {
                expected_tip: String::new(),
            },
        ];
        let mut amends = 0;
        for intent in &intents {
            match submit_path(intent) {
                SubmitPath::Commit(plain) => {
                    assert_eq!(
                        plain.intent().expected_tip(),
                        None,
                        "an intent with a compare-and-swap pin was routed to the \
                         endpoint that ignores it: {intent:?}"
                    );
                }
                SubmitPath::Amend(target) => {
                    amends += 1;
                    assert_eq!(
                        Some(target.expected_tip()),
                        intent.expected_tip(),
                        "the amend path must carry the tip the user reviewed, \
                         unaltered: {intent:?}"
                    );
                }
            }
        }
        // Paired positive: the loop above is only meaningful if the amend arm
        // was actually taken. Both amend intents must have reached it.
        assert_eq!(amends, 2, "the amend arm was never exercised");
    }

    // -----------------------------------------------------------------
    // The context menu's amend gate
    // -----------------------------------------------------------------

    /// All four (is_head, is_stub) combinations, with the reasons as literals.
    /// The condition used to be spelled out in `menu.rs`, which is wasm-only:
    /// inverting it would have offered "Amend last commit" on every stub and
    /// non-HEAD dot with nothing in the suite to notice.
    #[test]
    fn the_amend_item_is_offered_only_on_a_head_commit_that_is_not_a_stub() {
        assert_eq!(amend_offer(true, false), AmendOffer::Offered);

        // A non-HEAD dot: amending rewrites the checked-out branch's tip, so
        // acting on any other commit would rewrite one the user did not tap.
        assert_eq!(
            amend_offer(false, false),
            AmendOffer::Blocked("Only the commit at HEAD can be amended")
        );
        // A stub — and a stub that also claims HEAD. Both get the stub reason,
        // because `AmendCommit` has no branch target for a stub to be.
        assert_eq!(
            amend_offer(false, true),
            AmendOffer::Blocked("Amending rewrites the checked-out branch's tip, not a stub")
        );
        assert_eq!(
            amend_offer(true, true),
            AmendOffer::Blocked("Amending rewrites the checked-out branch's tip, not a stub"),
            "a stub is excluded even when it is HEAD's own ref: there is still \
             no commit dot under it to rewrite"
        );

        // The two refusals never read the same, so the disabled item's visible
        // reason always tells the user which rule stopped them.
        assert_ne!(amend_offer(false, false), amend_offer(false, true));
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
    // The pre-flight published-history ceremony (#225)
    // -----------------------------------------------------------------

    /// The only way to build an [`AmendTarget`] — deliberately, so these tests
    /// exercise the same construction path the confirm button does.
    fn target(tip: &str) -> AmendTarget {
        match submit_path(&CommitIntent::Amend {
            expected_tip: tip.to_string(),
        }) {
            SubmitPath::Amend(t) => t,
            other => panic!("an amend intent must reach the amend path, got {other:?}"),
        }
    }

    fn read_detail(tip: &str, on_remote: bool) -> PreflightKnowledge {
        let mut k = PreflightKnowledge::default();
        k.record_detail(tip, on_remote);
        k
    }

    /// The ceremony fires on a published target and on nothing else.
    ///
    /// Both directions, because a gate that always escalated would satisfy the
    /// positive case perfectly and would also make every ordinary amend a
    /// two-step ritual the user learns to click through.
    #[test]
    fn only_a_commit_read_as_published_gets_the_ceremony() {
        assert_eq!(
            amend_preflight(target(TIP), &read_detail(TIP, true)),
            Preflight::Confirm(target(TIP)),
            "amending a commit the detail read says is on a remote must stop and ask"
        );

        assert_eq!(
            amend_preflight(target(TIP), &read_detail(TIP, false)),
            Preflight::Send(target(TIP)),
            "a commit that is provably not on a remote gets no extra step — the \
             ceremony is only worth anything if it is rare"
        );

        assert_eq!(
            amend_preflight(target(TIP), &PreflightKnowledge::default()),
            Preflight::Send(target(TIP)),
            "no detail was read, so there is no flag; the stated rule is that an \
             absent flag sends, and `published_advisory` reports it afterwards"
        );

        // A read that was made for a *different* commit is not this commit's
        // answer. The dialog is retargetable while open, so this is reachable:
        // the guided re-check moves it from one tip to another.
        assert_eq!(
            amend_preflight(target(TIP), &read_detail(NEW_TIP, true)),
            Preflight::Send(target(TIP)),
            "a published answer read for another commit must not be spent on this one"
        );
        assert_eq!(
            amend_preflight(target(NEW_TIP), &read_detail(NEW_TIP, true)),
            Preflight::Confirm(target(NEW_TIP)),
            "…and the commit it *was* read for must still escalate"
        );
    }

    /// A confirmation is spent on the commit it was given for, and on no other.
    ///
    /// The failure this pins is specific and reachable: the user agrees to
    /// rewrite published commit A, the amend is refused for a stale tip, the
    /// guided re-check retargets the open dialog at published commit B — and a
    /// consent recorded as a bare boolean would carry over, rewriting B's
    /// pushed history with no warning shown at all.
    #[test]
    fn a_confirmation_does_not_carry_across_a_retarget() {
        let mut k = read_detail(TIP, true);
        k.confirm(TIP);
        assert_eq!(
            amend_preflight(target(TIP), &k),
            Preflight::Send(target(TIP)),
            "once the user has agreed for this commit, the next press must send \
             rather than loop on the same banner"
        );

        // The re-check retargets and records the new tip's own answer.
        k.record_detail(NEW_TIP, true);
        assert_eq!(
            amend_preflight(target(NEW_TIP), &k),
            Preflight::Confirm(target(NEW_TIP)),
            "a different commit is a different decision, however recently the \
             previous one was agreed to"
        );

        // And confirming the new one does not retroactively un-confirm anything
        // — it simply moves to the commit now on screen.
        k.confirm(NEW_TIP);
        assert_eq!(
            amend_preflight(target(NEW_TIP), &k),
            Preflight::Send(target(NEW_TIP))
        );
        assert!(
            k.confirmed_for(NEW_TIP) && !k.confirmed_for(TIP),
            "consent tracks one commit at a time"
        );
    }

    /// A fresh dialog inherits nothing — the state `Dialogs::reset_amend`
    /// installs is the state that escalates.
    #[test]
    fn a_fresh_dialog_has_agreed_to_nothing_and_knows_nothing() {
        let fresh = PreflightKnowledge::default();
        assert_eq!(fresh.publication(TIP), TargetPublication::Unknown);
        assert!(!fresh.confirmed_for(TIP));
    }

    #[test]
    fn the_ceremony_names_the_risk_and_makes_agreeing_a_separate_act() {
        let view = phase_view(&AmendPhase::AwaitingPublishedConfirm {
            target: target(TIP),
        });
        assert!(
            !view.confirm_enabled,
            "the press that raised the warning must not also be the press that \
             satisfies it — otherwise a double-tap rewrites pushed history"
        );
        assert!(
            !view.busy,
            "nothing has been sent, so the dialog is not waiting on anything"
        );
        let notice = view
            .notice
            .expect("the ceremony is a banner or it is nothing");
        assert!(notice.body.contains(short_tip(TIP)), "{}", notice.body);
        for phrase in [
            // What is at stake, in the user's own terms.
            "Nothing has been sent yet",
            "remote",
            "diverge",
            "push will be refused",
        ] {
            assert!(
                notice.body.contains(phrase),
                "the warning has to name the risk, not gesture at it — missing \
                 {phrase:?} in: {}",
                notice.body
            );
        }
        assert!(
            notice.action.is_some(),
            "a warning with no way past it is a dead end, not a ceremony"
        );
    }

    /// The ceremony must not be mistakable for any other banner the dialog
    /// raises — most of all not for the post-hoc advisory, which says the
    /// rewrite has *already* happened.
    #[test]
    fn the_ceremony_reads_as_its_own_screen() {
        let ceremony = phase_view(&AmendPhase::AwaitingPublishedConfirm {
            target: target(TIP),
        })
        .notice
        .unwrap();
        let others = [
            AmendPhase::InFlight,
            AmendPhase::Refused {
                refusal: AmendRefusal::Hook,
                message: "hook".into(),
            },
            AmendPhase::Unavailable("gone".into()),
            stale(Recheck::Idle),
        ];
        for phase in others {
            let notice = phase_view(&phase).notice.expect("explained");
            assert_ne!(notice.title, ceremony.title, "{phase:?}");
        }

        // The post-hoc advisory is the one that says it already happened; the
        // pre-flight is the one that says nothing has. They are different
        // sentences on purpose.
        let after = published_advisory(&AmendCommitSuccess {
            message: "Amended commit.".into(),
            old_tip: TIP.into(),
            new_tip: Some(NEW_TIP.into()),
            amended_published_commit: Some(true),
        })
        .expect("a published amend is reported afterwards too");
        assert!(after.contains("have now diverged") || after.contains("now diverged"));
        assert!(
            ceremony.body.contains("Nothing has been sent yet"),
            "{}",
            ceremony.body
        );
        assert_ne!(after, ceremony.body);
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
            message: SeedOutcome::Kept,
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
                message: SeedOutcome::Kept,
            },
            Recheck::Retargeted {
                new_tip: NEW_TIP.into(),
                summary: "s".into(),
                message: SeedOutcome::Replaced,
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

    /// The retarget banner may only claim the box is untouched when it is.
    ///
    /// The bug this pins: the banner said "Your message below is unchanged"
    /// unconditionally, and the retarget's very next step re-seeded the box from
    /// the *new* tip's message — which is adopted precisely when the user has
    /// not edited the old pre-fill, the commonest amend there is (fold in a
    /// staged file, leave the message alone). A user who trusts the banner and
    /// confirms without re-reading is committing a message they never saw.
    #[test]
    fn the_retarget_banner_only_claims_the_box_is_untouched_when_it_is() {
        let kept = phase_view(&stale(Recheck::Retargeted {
            new_tip: NEW_TIP.into(),
            summary: "fix: the other thing".into(),
            message: SeedOutcome::Kept,
        }))
        .notice
        .expect("a retarget is always explained")
        .body;
        assert!(
            kept.contains("unchanged"),
            "when the box really was left alone, saying so is the reassurance \
             the user needs: {kept}"
        );

        let replaced = phase_view(&stale(Recheck::Retargeted {
            new_tip: NEW_TIP.into(),
            summary: "fix: the other thing".into(),
            message: SeedOutcome::Replaced,
        }))
        .notice
        .expect("a retarget is always explained")
        .body;
        assert!(
            !replaced.contains("unchanged"),
            "the box was rewritten from under the user — claiming otherwise is \
             how an unread message gets committed: {replaced}"
        );
        assert!(
            replaced.contains("replaced") && replaced.contains("Read it"),
            "a replaced box must say so and send the user back to it: {replaced}"
        );

        // Both still name the commit being retargeted at: the outcome changes
        // what is said about the box, nothing else.
        for body in [&kept, &replaced] {
            assert!(body.contains(short_tip(NEW_TIP)), "{body}");
            assert!(body.contains("fix: the other thing"), "{body}");
        }
        assert_ne!(kept, replaced);
    }

    /// The end of the same rope: the outcome the banner renders from is derived
    /// from `adopt_seed`, not asserted independently of it, so the box and the
    /// banner cannot disagree.
    ///
    /// Walks the reported scenario in the pure core — the box holds tip A's
    /// pre-fill verbatim, tip B's message arrives — and the paired case where
    /// the user did type.
    #[test]
    fn the_reported_outcome_is_whatever_the_seed_rule_actually_did() {
        let a = "fix: the thing\n\nwith a body the summary would have dropped.";
        let b = "chore: something else entirely";

        // Untouched pre-fill: adopted, so the banner must not say "unchanged".
        let adopted = adopt_seed(a, a, b);
        assert_eq!(adopted.as_deref(), Some(b));
        assert_eq!(seed_outcome(adopted.as_ref()), SeedOutcome::Replaced);

        // The user typed: their words win, and the banner may reassure them.
        let typed = "my own words";
        let untouched = adopt_seed(typed, a, b);
        assert_eq!(untouched, None);
        assert_eq!(seed_outcome(untouched.as_ref()), SeedOutcome::Kept);

        // The new tip's message is already what is in the box: nothing moves,
        // so "unchanged" is true even though a seed was offered.
        let same = adopt_seed(b, b, b);
        assert_eq!(same, None);
        assert_eq!(seed_outcome(same.as_ref()), SeedOutcome::Kept);
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

    /// Each classified refusal names something the user can go and do — and
    /// the three do not name the same thing.
    ///
    /// Distinct *titles* were already pinned; this is the half that matters
    /// once the user has read the title. A hook rejection is fixed in the
    /// repository's hooks, a signing failure in git's signing config, and the
    /// two remedies have nothing in common — copy that said "fix it and try
    /// again" three times would pass a title-distinctness check and still send
    /// the user to the wrong place.
    #[test]
    fn each_refusal_names_a_remedy_of_its_own() {
        let body = |refusal| {
            phase_view(&AmendPhase::Refused {
                refusal,
                message: "git said this.".into(),
            })
            .notice
            .expect("a refusal is explained")
            .body
        };
        let hook = body(AmendRefusal::Hook);
        let signing = body(AmendRefusal::Signing);
        let other = body(AmendRefusal::Other);

        // Where the hook lives, and the honest statement that there is no
        // bypass in this dialog.
        assert!(hook.contains(".git/hooks"), "{hook}");
        assert!(
            hook.contains("no bypass") || hook.contains("has no bypass"),
            "a user whose hook is wrong will look for the escape hatch; saying \
             there isn't one here is the actionable answer: {hook}"
        );

        // The two config keys the issue names, verbatim, because they are what
        // the user has to type.
        assert!(signing.contains("user.signingkey"), "{signing}");
        assert!(signing.contains("commit.gpgsign"), "{signing}");

        // Neither remedy may leak into the other's screen: sending a user with
        // a failing pre-commit hook to their signing config is worse than
        // saying nothing.
        assert!(
            !hook.contains("gpgsign") && !hook.contains("signingkey"),
            "{hook}"
        );
        assert!(!signing.contains(".git/hooks"), "{signing}");

        // The unclassified case says it is unclassified rather than borrowing
        // one of the other two remedies.
        assert!(
            other.contains("isn't a hook rejection") && other.contains("signing failure"),
            "an unclassified failure must rule the classified ones out rather \
             than guess between them: {other}"
        );
        assert!(
            !other.contains(".git/hooks") && !other.contains("gpgsign"),
            "{other}"
        );

        // And all three are genuinely different screens, not one string with
        // prefixes glued on.
        assert_ne!(hook, signing);
        assert_ne!(signing, other);
        assert_ne!(hook, other);
    }

    /// The third, unclassified kind still reaches the user — visibly, with
    /// git's own words intact.
    ///
    /// This is the acceptance criterion that no raw, unclassified stderr is all
    /// the user gets: `Other` is a classification the *server* made, so its
    /// message is shown inside a banner that says what happened to their
    /// history, rather than being dumped alone.
    #[test]
    fn an_unclassified_refusal_is_still_a_visible_banner_with_gits_words() {
        let notice = phase_view(&AmendPhase::Refused {
            refusal: AmendRefusal::Other,
            message: "error: could not write commit object: No space left on device".into(),
        })
        .notice
        .expect("an unclassified refusal must not fall through to no banner at all");
        assert!(!notice.title.trim().is_empty());
        assert!(
            notice.body.contains("No space left on device"),
            "git's own words are the only diagnosis there is here: {}",
            notice.body
        );
        assert!(
            notice.body.contains("Nothing was rewritten"),
            "the user's first question is still whether their history changed: {}",
            notice.body
        );
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
