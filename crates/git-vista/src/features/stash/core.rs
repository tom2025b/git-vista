//! The pure decision model for the stash drawer (M3.24, #77).
//!
//! Framework-free and host-tested, per the `features/*/core.rs` rule: this
//! file must build and run under `cargo test --workspace`, so it holds every
//! decision the drawer makes and the wasm view holds none. `mod app` and every
//! view module are `#[cfg(target_arch = "wasm32")]`, so a decision left inside
//! markup can never be host-tested — and "renders nothing" is exactly how such
//! a defect presents.
//!
//! # The decision this file exists for
//!
//! **A pop is not one operation here, and the UI is what makes it honest.**
//!
//! There is no `POST /api/stash/pop`. That is deliberate and documented in
//! `crates/git-vista-server/src/main.rs` beside the three write routes: pop is
//! apply-then-drop, and one durable operation row cannot distinguish "nothing
//! ran" from "your changes were applied and the entry is still there". Two
//! independent operations produce two rows, and two rows can tell the truth.
//! `docs/superpowers/specs/m3.24-stash.md` §5 prescribes exactly this shape
//! ("Run `git stash apply <selector>`, and on a clean apply run `git stash
//! drop <selector>`") and states the payoff: the acceptance criterion *"pop is
//! not reported complete while conflicts remain"* becomes **true by
//! construction** rather than by a status-parsing check that could drift.
//!
//! So the client composes it, and [`DropGate`] is the gate. The one input that
//! permits the destructive half is a successful apply *plus a conflict scan
//! that actually ran and came back clear*. Every other combination halts with
//! a [`PopVerdict`] that says what really happened.
//!
//! ## Why a failed scan is not a clear scan
//!
//! [`Continuation::from_files`]'s own doc comment carries the trap: *"An empty
//! input means `Clear`, and that is only safe because the caller is required
//! to have actually looked."* A client that mapped a failed `GET /api/conflicts`
//! to an empty vector would hand this module a green light meaning "I did not
//! check", and the drop would destroy the entry on the strength of it. So the
//! scan arrives as [`ConflictScan`], which has no way to spell "failed" that
//! also reads as clear.

use git_vista_protocol::conflict::{ConflictedFile, Continuation};

use crate::features::status::core::{StatusSection, StatusSections};

// ---------------------------------------------------------------------------
// The wire shape
// ---------------------------------------------------------------------------

/// One entry of `GET /api/stashes` — the shared DTO, not a transcription of
/// one (#495, ADR 0079).
///
/// # This shape used to have two authors
///
/// The server built the listing's JSON by hand with `serde_json::json!`, and
/// this module declared its own struct to read it back. A rename on either
/// side presented as **an empty drawer** — not an error, not a 400 — because a
/// field serde cannot find is a field that was not sent. That is exactly the
/// "no stashes" / "couldn't look" merge `git_vista_git::stash::read_stashes`
/// goes out of its way to prevent one layer down.
///
/// Both ends now deserialize [`git_vista_protocol::StashEntry`], so a rename
/// is a compile error in one crate and a red test in the other rather than a
/// panel that renders nothing. The wire is pinned in the protocol crate
/// (`dto::tests::the_stash_listing_pins_its_wire_keys_and_their_types`) and at
/// the server's own mapping boundary
/// (`handlers::stash::listing_tests::a_record_becomes_the_wire_bytes_the_frontend_parses`).
///
/// Re-exported rather than aliased so every existing `use` here keeps working
/// and there is still exactly one place the drawer's wire shape is written
/// down — the protocol crate.
pub use git_vista_protocol::StashEntry;

// ---------------------------------------------------------------------------
// A1 — what a row says, and inspection as the default motion
// ---------------------------------------------------------------------------

/// How many characters of a stash subject a row shows before eliding.
pub const SUBJECT_PREVIEW_CHARS: usize = 72;

/// What a stash message decomposes into for display.
///
/// Git writes one of two forms, and they carry different information:
///
/// - `WIP on main: 1a2b3c4 some subject` — the automatic message. The branch
///   is `main` and the tail names the commit the stash was taken on top of.
/// - `On main: my own text` — what `git stash push -m 'my own text'` writes.
///
/// Anything else is a message this module does not recognise, and it is shown
/// **verbatim** rather than being forced into one of the two shapes. A stash
/// list is a reflog, and a reflog line can be written by any tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashSubject {
    /// The branch the stash was taken on, when the message names one.
    pub branch: Option<String>,
    /// The human-facing text of the row: the user's own message, or the
    /// subject of the commit the WIP stash sat on. Never empty — an
    /// unparseable or blank message falls back to [`NO_SUBJECT`].
    pub subject: String,
    /// True when the message was git's own `WIP on …` form rather than one the
    /// user typed. A view can mark the difference; the user's own words are
    /// worth more trust than a generated line.
    pub automatic: bool,
}

/// What a row shows when the stash message carries no usable text at all.
/// An empty string here would render as a blank row, which reads as *a stash
/// with no changes* — a different and false claim.
pub const NO_SUBJECT: &str = "(no message)";

/// Split a stash reflog message into its branch and its subject.
pub fn stash_subject(message: &str) -> StashSubject {
    let trimmed = message.trim();

    // `WIP on <branch>: <sha> <subject>` — the automatic form. The sha is
    // dropped from the subject: the row already shows the stash's own oid, and
    // a second unexplained hash beside it is noise.
    if let Some(rest) = trimmed.strip_prefix("WIP on ") {
        if let Some((branch, tail)) = rest.split_once(": ") {
            let tail = tail.trim();
            // Drop a leading hex token only when it really is one. A user
            // cannot reach this branch, but a foreign tool can, and eating the
            // first word of a real subject would be worse than leaving a hash.
            let subject = match tail.split_once(' ') {
                Some((head, remainder))
                    if !head.is_empty()
                        && head.len() >= 4
                        && head.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    remainder.trim()
                }
                _ => tail,
            };
            return StashSubject {
                branch: non_empty(branch),
                subject: elide(subject),
                automatic: true,
            };
        }
    }

    // `On <branch>: <message>` — what `-m` writes.
    if let Some(rest) = trimmed.strip_prefix("On ") {
        if let Some((branch, tail)) = rest.split_once(": ") {
            return StashSubject {
                branch: non_empty(branch),
                subject: elide(tail.trim()),
                automatic: false,
            };
        }
    }

    // Not a shape this module claims to understand. Show it as written.
    StashSubject {
        branch: None,
        subject: elide(trimmed),
        automatic: false,
    }
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Elide at [`SUBJECT_PREVIEW_CHARS`] *characters*, so a multi-byte character
/// can never be cut in half, and never return an empty string.
fn elide(subject: &str) -> String {
    let first = subject.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return NO_SUBJECT.to_string();
    }
    if first.chars().count() <= SUBJECT_PREVIEW_CHARS {
        return first.to_string();
    }
    let head: String = first.chars().take(SUBJECT_PREVIEW_CHARS).collect();
    format!("{head}…")
}

/// One row of the drawer, with every display decision already made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashRow {
    /// The selector to send back, verbatim from the wire.
    pub selector: String,
    /// The oid to send back as `expected_oid`, verbatim from the wire.
    pub oid: String,
    /// The first seven characters of the oid, for display only.
    pub oid_short: String,
    /// The decomposed message.
    pub subject: StashSubject,
    /// Unix seconds; the view formats it.
    pub when: i64,
    /// What this entry offers, and what it refuses and why.
    pub actions: Vec<ActionOffer>,
}

/// Build a row from one wire entry.
///
/// `write_gate` is whether this session may write at all — a visualize-only or
/// read-only session sees the drawer (the listing is not write-gated) but must
/// not be offered mutations it cannot perform.
pub fn stash_row(entry: &StashEntry, write_gate: WriteGate) -> StashRow {
    StashRow {
        // `as_str().to_string()` and not a re-derivation: the selector and the
        // oid are what the compare-and-swap is sent back, and the newtypes are
        // unwrapped here only because the view and its signals speak `&str`.
        selector: entry.entry.as_str().to_string(),
        oid: entry.oid.as_str().to_string(),
        oid_short: entry.oid.as_str().chars().take(7).collect(),
        subject: stash_subject(&entry.message),
        when: entry.time,
        actions: action_offers(write_gate),
    }
}

/// The empty-state line, so "no stashes" is testable wording rather than an
/// empty panel that looks like a failed fetch.
pub const NO_STASHES: &str = "Nothing stashed. Your working tree changes are all still here.";

/// The drawer's accessible name, and the handle a browser spec scopes to.
///
/// # Why the section needs a name at all
///
/// Two reasons, and the second was found by a browser run rather than reasoned
/// about. The first is ordinary: a bare `<div>` of rows is an anonymous
/// container, so assistive tech announces the rows with no idea what list they
/// belong to — the same argument `GRAPH_REGION_LABEL` makes for the graph.
///
/// The second: a stash's subject is frequently *also* a commit subject — git's
/// own `WIP on <branch>: <sha> <subject>` copies it verbatim from the commit the
/// stash sat on. So the same string legitimately appears in the graph's SVG
/// `<title>`, in the activity feed, and in this drawer at once, and a page-wide
/// text query cannot tell them apart. A named region gives a spec something to
/// scope to, which is the difference between an assertion about the drawer and
/// an assertion about the page.
pub const DRAWER_REGION_LABEL: &str = "Stashes";

/// The in-flight line. Distinct from [`NO_STASHES`] on purpose: "we have not
/// asked yet" and "we asked and the drawer is empty" are different facts, and
/// collapsing them would tell a user with stashes that they have none.
pub const LOADING_STASHES: &str = "Loading stashes…";

/// Everything the drawer can be showing, with the decision already made.
///
/// Same posture as [`crate::features::tags::core::TagListView`], and the same
/// reason: the view lives in `activity.rs`, which is
/// `#[cfg(target_arch = "wasm32")]` and therefore compiled by `trunk build` and
/// by nothing that asserts anything. A `match` on
/// `Option<Result<Vec<StashEntry>, String>>` written there could swap two arms
/// — rendering every populated drawer as the empty state — and still build,
/// lint, and pass the whole suite.
///
/// **[`Self::Failed`] and [`Self::Empty`] are the pair that must not merge.**
/// `git_vista_git::stash::read_stashes` goes out of its way to keep "read and
/// empty" apart from "could not read", and the server's handler refuses to
/// serialise a failure as `[]` for exactly that reason. A client that rendered
/// an error as the empty state would undo both, and tell a user their stashes
/// are gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrawerView {
    /// The fetch has not answered yet — show [`LOADING_STASHES`].
    Loading,
    /// The fetch failed. The payload is the finished user-facing line.
    Failed(String),
    /// The drawer was read and holds nothing — show [`NO_STASHES`].
    Empty,
    /// One row per entry, in the server's order (newest first).
    Rows(Vec<StashRow>),
}

/// Classify what the stash resource currently holds.
///
/// `state` is the Activity panel's resource after `.flatten()`: `None` while
/// the fetch is unresolved (or the panel is shut), `Some(Err)` for a failed
/// fetch, `Some(Ok)` for an answer.
pub fn drawer_view(
    state: Option<Result<Vec<StashEntry>, String>>,
    write_gate: WriteGate,
) -> DrawerView {
    match state {
        None => DrawerView::Loading,
        Some(Err(e)) => DrawerView::Failed(format!("Couldn't read the stash list: {e}")),
        Some(Ok(entries)) if entries.is_empty() => DrawerView::Empty,
        Some(Ok(entries)) => {
            DrawerView::Rows(entries.iter().map(|e| stash_row(e, write_gate)).collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Which actions an entry offers, and which are refused with a reason
// ---------------------------------------------------------------------------

/// Whether this session may perform writes, and why not when it may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGate {
    /// Writes are available.
    Allowed,
    /// A read-only or visualize session. The drawer still lists.
    ReadOnly,
}

/// Map the Activity panel's `read_only` flag onto the gate.
///
/// A one-line mapping, and it lives here rather than at the call site for the
/// reason every other decision in this file does: the call site is
/// `#[cfg(target_arch = "wasm32")]`, so an inverted bool there would be caught
/// by nothing. Inverted, every read-only session would be offered every
/// destructive control — which is the failure a `bool` parameter invites and a
/// named type does not.
pub fn write_gate(read_only: bool) -> WriteGate {
    if read_only {
        WriteGate::ReadOnly
    } else {
        WriteGate::Allowed
    }
}

/// Everything a stash entry can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashAction {
    /// `GET /api/stash/show` — read the patch. A read, so it survives the
    /// write gate, and it is listed **first** deliberately: see
    /// [`action_offers`].
    Inspect,
    /// `POST /api/stash/apply` — restore the changes, keep the entry.
    Apply,
    /// apply-then-drop, composed client-side. See [`DropGate`].
    Pop,
    /// `POST /api/stash/branch` — create a branch at the stash's own base and
    /// apply there.
    Branch,
    /// `POST /api/stash/drop` — discard the entry.
    Drop,
}

impl StashAction {
    /// The label a control carries.
    pub fn label(self) -> &'static str {
        match self {
            StashAction::Inspect => "Show changes",
            StashAction::Apply => "Apply",
            StashAction::Pop => "Pop",
            StashAction::Branch => "Branch from stash",
            StashAction::Drop => "Drop",
        }
    }

    /// Whether performing this can lose the user's work if it goes wrong.
    ///
    /// What actually keys on it: the view's danger styling, and the core
    /// test pinning that Inspect is offered ahead of every destructive
    /// action. The confirmation dialogs themselves are hand-written per
    /// action arm rather than gated on this flag — Pop currently has none
    /// at all — which is #525's business, and this comment claims only
    /// what the code delivers.
    pub fn destructive(self) -> bool {
        match self {
            StashAction::Inspect | StashAction::Apply => false,
            // All three remove the entry. `Branch` only consumes it after a
            // successful apply onto the commit the stash was taken from, where
            // by construction it fits — but that sequencing does not change
            // what success costs: the stash entry, the user's recovery object,
            // is deleted. The server classifies the same operation
            // `RiskLevel::Destructive` for exactly that reason; classing it
            // safe here let the view skip the warning entirely (#516).
            StashAction::Pop | StashAction::Drop | StashAction::Branch => true,
        }
    }
}

/// One action and whether it is on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOffer {
    pub action: StashAction,
    pub availability: Availability,
}

/// Offered, or refused with a reason the user can read.
///
/// A refusal carries its own sentence rather than being represented by absence.
/// A control that silently vanishes teaches the user nothing about why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Offered,
    Refused(&'static str),
}

/// The reason every write is refused in a read-only session.
pub const READ_ONLY_REFUSAL: &str =
    "This session can look but not change anything. Open the repository locally to act on a stash.";

/// What one entry offers, in the order a view should present them.
///
/// **`Inspect` is first, and that is the acceptance criterion, not taste.**
/// A1 is *"stash content is inspectable before apply or drop"*, and a drop is
/// irreversible from the user's point of view. An inspect affordance that
/// exists but sits behind a menu, below the destructive controls, is the defect
/// the criterion names — so the order is fixed here, where it can be tested,
/// rather than left to markup.
pub fn action_offers(write_gate: WriteGate) -> Vec<ActionOffer> {
    [
        StashAction::Inspect,
        StashAction::Apply,
        StashAction::Pop,
        StashAction::Branch,
        StashAction::Drop,
    ]
    .into_iter()
    .map(|action| ActionOffer {
        // A read survives the write gate: listing and inspecting are what a
        // visualize session is for.
        availability: match (action, write_gate) {
            (StashAction::Inspect, _) | (_, WriteGate::Allowed) => Availability::Offered,
            (_, WriteGate::ReadOnly) => Availability::Refused(READ_ONLY_REFUSAL),
        },
        action,
    })
    .collect()
}

// ---------------------------------------------------------------------------
// A2 — staged and untracked options are explicit
// ---------------------------------------------------------------------------

/// What a push with the chosen options will and will not put in the drawer.
///
/// # "Explicit" means visible before the push, not merely available
///
/// A2 is *"staged and untracked options are explicit"*. A `--include-untracked`
/// flag that exists somewhere satisfies the letter and misses the point: the
/// failure it guards against is a user believing they stashed a new file that
/// git left sitting in the worktree. So this is a **preview**, computed from
/// the real `WorktreeStatus` read, naming both halves — what goes in and what
/// stays behind — before the button is pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPreview {
    /// What this push captures. One line per category, with its count.
    pub captures: Vec<String>,
    /// What this push leaves in the working tree. **This is the load-bearing
    /// half** — an omission the user does not expect is the defect A2 names.
    pub leaves_behind: Vec<String>,
    /// Set when there is nothing for a push to do, in which case no push
    /// should be offered at all. `git stash push` on a clean tree exits
    /// non-zero with "No local changes to save"; refusing here means the user
    /// gets a sentence instead of an error.
    pub refusal: Option<&'static str>,
}

/// What [`push_preview`] says when the tree holds nothing stashable.
pub const NOTHING_TO_STASH: &str =
    "There are no local changes to stash. The working tree is already clean.";

/// The refusal for a tree whose *only* changes are untracked files that this
/// push would exclude.
///
/// A separate sentence from [`NOTHING_TO_STASH`] because the two authorise
/// different next actions, and the generic one contradicts what is on screen
/// beside it: the preview is simultaneously listing an untracked file as left
/// behind and claiming the tree is "already clean". It is not — there is
/// something there, and one tick would capture it.
pub const ONLY_EXCLUDED_UNTRACKED: &str =
    "Nothing here would be stashed. The only changes are untracked files — tick \
     \u{201c}Include untracked files\u{201d} to put them in the drawer.";

impl PushPreview {
    /// Whether a push may be offered at all.
    pub fn may_push(&self) -> bool {
        self.refusal.is_none()
    }
}

/// Compute the preview for one combination of options.
///
/// `keep_index` is `git stash push --keep-index`: the staged changes are
/// stashed *and* left staged in the worktree. `include_untracked` is
/// `--include-untracked`.
///
/// Reuses [`StatusSections`] rather than re-deriving the categories: the
/// staged/unstaged/untracked split is already decided and host-tested there,
/// and a second classifier would be a second thing to keep in agreement.
pub fn push_preview(
    sections: &StatusSections,
    keep_index: bool,
    include_untracked: bool,
) -> PushPreview {
    let staged = sections.count(StatusSection::Staged);
    let unstaged = sections.count(StatusSection::Unstaged);
    let untracked = sections.count(StatusSection::Untracked);

    let mut captures = Vec::new();
    let mut leaves_behind = Vec::new();

    if staged > 0 {
        captures.push(plural(staged, "staged change", "staged changes"));
        if keep_index {
            // The subtle one, and the reason `keep_index` is previewed rather
            // than left as a checkbox label: the changes ARE stashed, and they
            // also remain staged. A user who reads "kept" as "not stashed" has
            // it backwards, so the wording says both facts.
            leaves_behind.push(format!(
                "{} — also kept staged in the working tree",
                plural(staged, "staged change", "staged changes")
            ));
        }
    }
    if unstaged > 0 {
        captures.push(plural(unstaged, "unstaged change", "unstaged changes"));
    }
    if untracked > 0 {
        if include_untracked {
            captures.push(plural(untracked, "untracked file", "untracked files"));
        } else {
            leaves_behind.push(format!(
                "{} — NOT stashed",
                plural(untracked, "untracked file", "untracked files")
            ));
        }
    }

    // Ignored files are never stashed by either flag this UI offers
    // (`--all` would, and is deliberately not exposed: it sweeps build output
    // into the drawer, and a stash the user cannot recognise is a stash they
    // will not restore). Not mentioned in `leaves_behind` because nobody
    // expects an ignored file to move, and a line for it would bury the
    // untracked warning that matters.

    // `git stash push` with no tracked changes and no `--include-untracked`
    // has nothing to save even when untracked files exist — but which refusal
    // to show depends on whether there is something a tick would capture.
    let refusal = match (captures.is_empty(), untracked > 0 && !include_untracked) {
        (false, _) => None,
        (true, true) => Some(ONLY_EXCLUDED_UNTRACKED),
        (true, false) => Some(NOTHING_TO_STASH),
    };

    PushPreview {
        captures,
        leaves_behind,
        refusal,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

// ---------------------------------------------------------------------------
// A4 — pop is not reported complete while conflicts remain
// ---------------------------------------------------------------------------

/// What `POST /api/stash/apply` came back with.
///
/// # A refusal does not mean nothing happened
///
/// This was got wrong here first, and the fixture caught it. Measured against
/// git 2.43.0 (`ci/browser/fixture.mjs`'s stash repo, applied by hand):
///
/// ```text
/// $ git stash apply 'stash@{0}'      # an entry that cannot merge
/// CONFLICT (content): Merge conflict in collision.txt
/// $ echo $?
/// 1
/// $ git status --porcelain
/// UU collision.txt
/// ```
///
/// **Exit 1, and the conflict markers are in the working tree.** So the server
/// returns a 4xx (`exec_apply_stash` branches on the exit status alone), this
/// client sees `Refused`, and a verdict that concluded "nothing was applied"
/// from that would be a false claim about the user's files — the same class of
/// lie A4 exists to prevent, pointing the other way.
///
/// A refusal therefore settles nothing on its own. Only the conflict scan can
/// tell a refusal that left work behind from one that did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 2xx. The changes are in the working tree and git reported no conflict.
    Applied,
    /// A refusal the server actually SAID — an answered non-2xx, or a reply
    /// recovered from the operation record after the response was lost. May
    /// or may not have left changes behind — see this type's doc comment.
    Refused(String),
    /// The reply was lost AND the operation record could not settle it
    /// (#515). Not a refusal: the server may have applied cleanly. Encoding
    /// this as `Refused` is exactly the lie #508 removed on the tree axis —
    /// asserting an outcome this client never observed.
    Unknown(String),
}

/// What one stash write actually established, once a lost reply is accounted
/// for (#515).
///
/// A dropped HTTP response does not abort the request — `api::with_deadline`'s
/// own doc says the server may still complete the work. Every stash POST
/// enters the tracked planner, so the truth survives in the operation record,
/// reachable by the idempotency key (`operations::lookup_by_key` server-side,
/// `api::resolve_operation_id` here). This type is the honest vocabulary for
/// that recovery: the api layer produces it, and the pure classifiers below
/// turn it into apply/drop outcomes where a host test can reach the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashWriteOutcome {
    /// The server answered this request directly. `ok` is `Response::ok()`.
    Answered { ok: bool, message: String },
    /// Both attempts were lost, but the operation record was found terminal —
    /// this is the RECORDED answer, recovered by the key. Same standing as
    /// [`Self::Answered`]: the server said it, we just heard it late.
    Reconciled { ok: bool, message: String },
    /// Lost, and the record could not settle it (never admitted, still
    /// running past the reconciliation budget, or unreachable). The one case
    /// with no answer to report — and it must be REPORTED as no answer.
    Unknown { why: String },
}

impl ApplyOutcome {
    /// Classify an apply's wire outcome. The outer `Err` is a LOCAL refusal —
    /// offline, visualize mode, a malformed selector — where nothing was ever
    /// sent, so "refused" is certain, not inferred.
    pub fn from_write(sent: Result<StashWriteOutcome, String>) -> Self {
        match sent {
            Err(local) => ApplyOutcome::Refused(local),
            Ok(StashWriteOutcome::Answered { ok: true, .. })
            | Ok(StashWriteOutcome::Reconciled { ok: true, .. }) => ApplyOutcome::Applied,
            Ok(StashWriteOutcome::Answered { ok: false, message })
            | Ok(StashWriteOutcome::Reconciled { ok: false, message }) => {
                ApplyOutcome::Refused(message)
            }
            Ok(StashWriteOutcome::Unknown { why }) => ApplyOutcome::Unknown(why),
        }
    }
}

/// What the conflict scan after an apply came back with.
///
/// Two variants, and there is deliberately no third that a failed read could
/// collapse into. See this module's header: an empty conflict list means
/// "clear" **only** when the caller actually looked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictScan {
    /// `GET /api/conflicts` answered, and this is the verdict built from it.
    Read(Continuation),
    /// The scan itself failed. Never evidence of absence.
    Failed(String),
}

impl ConflictScan {
    /// Build the scan result from a `GET /api/conflicts` response.
    ///
    /// The `Err` arm is what keeps [`Continuation::from_files`]'s precondition
    /// honoured: a failed fetch never reaches `from_files` with an empty
    /// vector, so it can never be read as a green light.
    pub fn from_fetch(fetched: Result<Vec<ConflictedFile>, String>) -> Self {
        match fetched {
            Ok(files) => ConflictScan::Read(Continuation::from_files(&files)),
            Err(why) => ConflictScan::Failed(why),
        }
    }
}

/// What `POST /api/stash/drop` came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropOutcome {
    Dropped,
    /// An answered or record-recovered refusal — the server said no.
    Refused(String),
    /// The reply was lost and the record could not settle it (#515). The
    /// entry may or may not still exist; only a fresh list read can say.
    Unknown(String),
}

impl DropOutcome {
    /// Classify a drop's wire outcome; same contract as
    /// [`ApplyOutcome::from_write`].
    pub fn from_write(sent: Result<StashWriteOutcome, String>) -> Self {
        match sent {
            Err(local) => DropOutcome::Refused(local),
            Ok(StashWriteOutcome::Answered { ok: true, .. })
            | Ok(StashWriteOutcome::Reconciled { ok: true, .. }) => DropOutcome::Dropped,
            Ok(StashWriteOutcome::Answered { ok: false, message })
            | Ok(StashWriteOutcome::Reconciled { ok: false, message }) => {
                DropOutcome::Refused(message)
            }
            Ok(StashWriteOutcome::Unknown { why }) => DropOutcome::Unknown(why),
        }
    }
}

/// What is true of the working tree, as far as this client can actually tell.
///
/// Two states, not a `bool`, because the honest answer is often *"unknown"* —
/// and a `bool` has nowhere to put it, so it would have to guess.
///
/// # There is deliberately no `Untouched`
///
/// There used to be, produced only by what is now
/// [`PopVerdict::ApplyRefused`] and justified as *"the apply was refused **and**
/// a scan that really ran found nothing conflicted"*. **That inference is
/// false, and it shipped** (#508).
///
/// A conflict scan establishes that nothing **conflicts**. It cannot establish
/// that nothing **landed**. `git stash apply --include-untracked`, run where
/// the untracked file already exists on disk, writes the tracked change, *then*
/// fails restoring the untracked one, exits non-zero, and leaves no unmerged
/// index entries. The scan comes back clear, and the app told the user *"Your
/// working tree was left untouched"* over a file it had just rewritten.
///
/// There **is** a genuinely untouched case — a compare-and-swap refusal, where
/// git never ran at all — and the test that pinned this arm used exactly that
/// input, then let the arm generalise to every input. [`ApplyOutcome::Refused`]
/// carries no fact about whether git ran, so [`PopVerdict::tree`] cannot tell
/// the two apart.
///
/// The variant is removed rather than made conditional: **a state this client
/// has no way to establish should not be sayable.** Restoring the precision
/// honestly requires the server to report structurally whether git ran before
/// it failed, which is a wire change and is filed separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeState {
    /// There is work in the working tree from this operation.
    Changed,
    /// It could not be established either way. Says so rather than picking a
    /// side — see this type's own doc for why that is the common case.
    Unknown,
}

impl TreeState {
    /// The line shown to the user.
    pub fn line(self) -> &'static str {
        match self {
            TreeState::Changed => "Your working tree has changes from this stash.",
            TreeState::Unknown => {
                "Whether anything reached your working tree could not be established — \
                 check `git status` before retrying."
            }
        }
    }
}

/// Whether the destructive half of a pop may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropGate {
    /// The apply succeeded and a scan that really ran came back clear.
    Run,
    /// Stop here. The verdict says what actually happened; no drop is sent.
    Halt(PopVerdict),
}

/// The outcome of a composed pop. **Exactly one variant means it finished.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopVerdict {
    /// Applied, verified clear, entry dropped. The only completed pop.
    Popped,
    /// The apply was refused **and** a scan that really ran found nothing
    /// conflicted, so nothing landed. The entry is intact.
    ApplyRefused { why: String },
    /// Conflicted paths remain. **A4's case.**
    ///
    /// Reached two ways, kept distinguishable by `apply_refusal`:
    /// - `None` — git reported the apply a success and left conflicts anyway;
    /// - `Some(sentence)` — the apply itself reported failure, which for
    ///   `git stash apply` is what a content conflict looks like (exit 1 with
    ///   the markers already written).
    ///
    /// Either way the entry was not removed, so nothing is lost.
    Conflicted {
        apply_refusal: Option<String>,
        unresolved: Vec<String>,
        unreadable: Vec<String>,
    },
    /// Applied, but the conflict scan could not be made. The drop is withheld
    /// on a check that could not be completed rather than run on an assumption.
    AppliedUnverified { why: String },
    /// The apply was refused *and* the conflict scan failed, so whether
    /// anything reached the tree is genuinely unknown. Distinct from
    /// [`Self::ApplyRefused`], which is a verified claim.
    RefusedUnverified { why: String, scan_why: String },
    /// Applied, verified clear, and then the drop failed. The composite state a
    /// single operation row could not express, and the reason pop is two
    /// requests here.
    AppliedNotDropped { why: String },
    /// The apply's reply was lost and the operation record could not settle
    /// it (#515). One thing IS certain and the headline says it: an apply
    /// never removes the entry, ran or not, so the stash is still listed.
    /// What the tree holds is not.
    ApplyUnknown { why: String },
    /// Applied and verified clear, then the DROP's reply was lost and the
    /// record could not settle it (#515). The changes are in the tree — the
    /// gate proved that before the drop was sent — but whether the entry
    /// left the drawer is exactly what nobody observed.
    DropUnknown { why: String },
}

impl PopVerdict {
    /// Whether the pop finished. **The single predicate any caller may use to
    /// claim completion**, so no view writes `!matches!(…)` and gets the
    /// polarity wrong.
    pub fn is_complete(&self) -> bool {
        matches!(self, PopVerdict::Popped)
    }

    /// What is true of the working tree.
    pub fn tree(&self) -> TreeState {
        match self {
            // Applied and dropped: the changes are in the tree, that was the
            // point.
            PopVerdict::Popped
            | PopVerdict::AppliedUnverified { .. }
            | PopVerdict::AppliedNotDropped { .. } => TreeState::Changed,
            // Conflicted paths are in the tree whichever step put them there.
            PopVerdict::Conflicted { .. } => TreeState::Changed,
            // Refused, with a scan that came back clear. That establishes no
            // CONFLICTS; it does not establish that nothing landed, because git
            // can write a file and then fail without leaving an unmerged entry.
            // See `TreeState`'s own doc (#508).
            PopVerdict::ApplyRefused { .. } => TreeState::Unknown,
            PopVerdict::RefusedUnverified { .. } => TreeState::Unknown,
            // A lost apply reply establishes nothing about the tree (#515).
            PopVerdict::ApplyUnknown { .. } => TreeState::Unknown,
            // A lost DROP reply is different: the apply half was verified
            // before the drop was sent, so the changes are in the tree.
            PopVerdict::DropUnknown { .. } => TreeState::Changed,
        }
    }

    /// Whether the stash entry is still in the drawer — `None` when this
    /// client has no way to know.
    ///
    /// This used to return `bool` under a doc claiming it was "knowable in
    /// every case". That was true only while every drop reply was assumed to
    /// arrive: a lost drop response (#515) leaves the entry's fate exactly
    /// unobserved, and a `bool` would have to guess — the same shape of lie
    /// `TreeState` removed in #508. `Some(true)` is still provable for every
    /// pre-drop halt (an apply never consumes the entry, ran or not).
    pub fn entry_retained(&self) -> Option<bool> {
        match self {
            PopVerdict::Popped => Some(false),
            PopVerdict::DropUnknown { .. } => None,
            _ => Some(true),
        }
    }

    /// The sentence shown to the user.
    ///
    /// Only [`PopVerdict::Popped`] uses the word "Popped". Every other variant
    /// leads with what is true about the user's data, because that is what the
    /// criterion protects.
    pub fn headline(&self) -> String {
        match self {
            PopVerdict::Popped => {
                "Popped the stash. It has been removed from your stash list.".to_string()
            }
            PopVerdict::ApplyRefused { why } => format!(
                "The apply was refused, so the stash was not popped. It is still in \
                 your list — but check `git status` before retrying: a refused apply \
                 is not proof that nothing reached your files.\n\n{why}"
            ),
            PopVerdict::Conflicted { apply_refusal, .. } => {
                let opening = match apply_refusal {
                    // git called the apply a success and left conflicts anyway.
                    None => "The changes were applied but left conflicts",
                    // The refusal WAS the conflict: exit 1, markers written.
                    Some(_) => "Applying the stash hit conflicts",
                };
                format!(
                    "{opening}, so the stash was NOT popped. It is still in your list, and \
                     the conflicted paths are in your working tree — resolve them below. \
                     Nothing is lost either way."
                )
            }
            PopVerdict::AppliedUnverified { why } => format!(
                "The changes were applied, but whether any conflicts remain could not be \
                 checked, so the stash was NOT popped and is still in your list. Check your \
                 working tree before continuing.\n\n{why}"
            ),
            PopVerdict::RefusedUnverified { why, scan_why } => format!(
                "Applying the stash was refused, and the working tree could not then be \
                 checked — so whether anything reached your files is unknown. The stash was \
                 NOT popped and is still in your list.\n\n{why}\n\n{scan_why}"
            ),
            PopVerdict::AppliedNotDropped { why } => format!(
                "The changes were applied cleanly, but removing the stash entry failed — so \
                 it is STILL in your list and the changes are also in your working tree. \
                 Applying it again would duplicate them.\n\n{why}"
            ),
            PopVerdict::ApplyUnknown { why } => format!(
                "The reply to the apply was lost, and its outcome could not be recovered — \
                 so whether anything reached your working tree is unknown. The stash was NOT \
                 popped and IS still in your list (an apply never removes it). Check \
                 `git status`, and reload before retrying.\n\n{why}"
            ),
            PopVerdict::DropUnknown { why } => format!(
                "The stash's changes ARE in your working tree — that was verified — but the \
                 reply to the remove step was lost and its outcome could not be recovered. \
                 Whether the entry is still in your list is unknown: reload the drawer to \
                 see, and do NOT apply it again meanwhile, which would duplicate the \
                 changes.\n\n{why}"
            ),
        }
    }

    /// The conflicted paths this verdict carries, for routing into the shared
    /// continuation workflow (A3). Empty for every other variant.
    ///
    /// Returns the paths rather than rendering anything: the conflict panes
    /// (#428/#429/#432) are the one conflict UI, and a second stash-shaped one
    /// would be the drift this repository already argued against.
    pub fn conflicted_paths(&self) -> &[String] {
        match self {
            PopVerdict::Conflicted { unresolved, .. } => unresolved,
            _ => &[],
        }
    }

    /// Paths where a side could not be read — a fault to report, not work the
    /// user can do by choosing a side.
    pub fn unreadable_paths(&self) -> &[String] {
        match self {
            PopVerdict::Conflicted { unreadable, .. } => unreadable,
            _ => &[],
        }
    }
}

/// Decide whether the drop half of a pop may run.
///
/// This is the whole of A4. The destructive half runs on exactly one input: an
/// applied stash plus a conflict scan that ran and came back clear.
///
/// # The scan is consulted on BOTH apply outcomes
///
/// Not only on success. A conflicting `git stash apply` exits non-zero with the
/// markers already in the tree (see [`ApplyOutcome`]), so the refusal alone
/// cannot distinguish "nothing landed" from "your files have conflicts in them
/// right now". Only the scan can, and a verdict that skipped it would report
/// the second case as the first.
pub fn drop_gate(apply: &ApplyOutcome, scan: &ConflictScan) -> DropGate {
    match (apply, scan) {
        // Conflicts remain. The one thing both apply outcomes share: the drop
        // does not run, and the report never reads as complete.
        (
            _,
            ConflictScan::Read(Continuation::Blocked {
                unresolved,
                unreadable,
            }),
        ) => DropGate::Halt(PopVerdict::Conflicted {
            apply_refusal: match apply {
                ApplyOutcome::Applied => None,
                ApplyOutcome::Refused(why) => Some(why.clone()),
                // The conflicts are real either way — the scan read the live
                // index — so Conflicted is the actionable truth even when the
                // apply's own reply was lost. The sentence says which kind of
                // non-success this was rather than dressing it as a refusal.
                ApplyOutcome::Unknown(why) => Some(format!(
                    "the apply's own outcome could not be learned ({why}) — \
                     but the conflicts below are from a live read"
                )),
            },
            unresolved: unresolved.clone(),
            unreadable: unreadable.clone(),
        }),

        // Applied and verifiably clear: the only input that opens the gate.
        (ApplyOutcome::Applied, ConflictScan::Read(Continuation::Clear)) => DropGate::Run,

        // Applied, but the check could not be made.
        (ApplyOutcome::Applied, ConflictScan::Failed(why)) => {
            DropGate::Halt(PopVerdict::AppliedUnverified { why: why.clone() })
        }

        // Refused, and a real scan found nothing conflicted. That is ALL it
        // found: the scan reads unmerged index entries, and git can write a
        // tracked file and then fail on an untracked one without creating any
        // (#508). The gate stays shut, and the verdict declines to describe a
        // working tree it did not look at.
        (ApplyOutcome::Refused(why), ConflictScan::Read(Continuation::Clear)) => {
            DropGate::Halt(PopVerdict::ApplyRefused { why: why.clone() })
        }

        // Refused AND unscannable: genuinely unknown, and said so.
        (ApplyOutcome::Refused(why), ConflictScan::Failed(scan_why)) => {
            DropGate::Halt(PopVerdict::RefusedUnverified {
                why: why.clone(),
                scan_why: scan_why.clone(),
            })
        }

        // The apply's reply was lost and the record could not settle it
        // (#515). A clear or failed scan cannot upgrade that to either
        // "applied" or "refused" — a clear scan proves no CONFLICTS, not
        // that nothing ran (#508's lesson, same axis) — so the gate stays
        // shut on an outcome nobody observed. The one certainty worth
        // stating rides in the verdict: an apply never removes the entry,
        // ran or not, so the stash is still in the drawer.
        (ApplyOutcome::Unknown(why), ConflictScan::Read(Continuation::Clear))
        | (ApplyOutcome::Unknown(why), ConflictScan::Failed(_)) => {
            DropGate::Halt(PopVerdict::ApplyUnknown { why: why.clone() })
        }
    }
}

/// Fold the drop's own outcome into the final verdict. Only reached when
/// [`drop_gate`] returned [`DropGate::Run`].
pub fn verdict_after_drop(drop: &DropOutcome) -> PopVerdict {
    match drop {
        DropOutcome::Dropped => PopVerdict::Popped,
        DropOutcome::Refused(why) => PopVerdict::AppliedNotDropped { why: why.clone() },
        // The drop's reply was lost and the record could not settle it
        // (#515). The apply half is verified — this arm is only reachable
        // after the gate opened — but whether the entry is still in the
        // drawer is exactly what a lost drop reply leaves unknown, and
        // AppliedNotDropped's "it is still in your list" would be an
        // asserted fact nobody observed.
        DropOutcome::Unknown(why) => PopVerdict::DropUnknown { why: why.clone() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::status::{ChangeKind, ChangeSides, StatusEntry, WorktreeStatus};
    use git_vista_protocol::GenerationToken;

    fn blocked(unresolved: &[&str], unreadable: &[&str]) -> Continuation {
        Continuation::Blocked {
            unresolved: unresolved.iter().map(|s| s.to_string()).collect(),
            unreadable: unreadable.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Every `PopVerdict` there is, constructed by hand.
    ///
    /// Written out rather than derived, because the tests that iterate this
    /// list are asserting a property of the *whole* enum. A helper that built
    /// the list by calling the code under test would let a new variant slip in
    /// unexamined — and a new variant is exactly when "does this claim
    /// completion?" needs asking again.
    fn every_verdict() -> Vec<PopVerdict> {
        vec![
            PopVerdict::Popped,
            PopVerdict::ApplyRefused {
                why: "the entry no longer exists".to_string(),
            },
            PopVerdict::Conflicted {
                apply_refusal: None,
                unresolved: vec!["src/main.rs".to_string()],
                unreadable: vec![],
            },
            PopVerdict::Conflicted {
                apply_refusal: Some("CONFLICT (content)".to_string()),
                unresolved: vec!["collision.txt".to_string()],
                unreadable: vec![],
            },
            PopVerdict::AppliedUnverified {
                why: "HTTP 500".to_string(),
            },
            PopVerdict::RefusedUnverified {
                why: "HTTP 400".to_string(),
                scan_why: "HTTP 500".to_string(),
            },
            PopVerdict::AppliedNotDropped {
                why: "the list moved underneath it".to_string(),
            },
        ]
    }

    // -----------------------------------------------------------------------
    // A4 — the load-bearing negative, written first
    // -----------------------------------------------------------------------

    /// **A4: "pop is not reported complete while conflicts remain."**
    ///
    /// The negative is the whole point, so it is asserted first — but on its
    /// own it would be satisfied by a `drop_gate` that halted unconditionally,
    /// which would break every pop instead. So the clean path is asserted in
    /// the same test: the gate must open on a clear scan and close on a
    /// blocked one, and the closed case must not read as complete.
    ///
    /// MUTATION 1 (removes the mechanism): return `DropGate::Run` for the
    ///   `Blocked` arm of `drop_gate` — red, the destructive half would run
    ///   with conflicts on disk. Verified by hand: red.
    /// MUTATION 2 (keeps it, corrupts the report): leave the halt in place but
    ///   add `PopVerdict::AppliedWithConflicts` to `is_complete`'s `matches!`
    ///   — red on the completion assertion while the gate still closes.
    ///   Verified by hand: red.
    #[test]
    fn a_pop_that_conflicts_is_never_reported_complete() {
        let halted = drop_gate(
            &ApplyOutcome::Applied,
            &ConflictScan::Read(blocked(&["src/main.rs", "README.md"], &["logo.png"])),
        );

        let DropGate::Halt(verdict) = halted else {
            panic!("a conflicted apply must not open the drop gate, got {halted:?}");
        };

        // The entry survives, the tree changed, and the pop is NOT complete.
        assert!(
            !verdict.is_complete(),
            "a pop with conflicts on disk claimed to be complete"
        );
        assert_eq!(
            verdict.tree(),
            TreeState::Changed,
            "the user's files did move; say so"
        );
        assert!(
            verdict.entry_retained() == Some(true),
            "git stash apply leaves the entry, and this path never drops it"
        );
        assert_eq!(verdict.conflicted_paths(), ["src/main.rs", "README.md"]);
        assert_eq!(verdict.unreadable_paths(), ["logo.png"]);

        // The wording must not read as success. Asserted against the literal
        // string, not against another call into the code that produces it.
        let headline = verdict.headline();
        assert!(
            headline.contains("NOT popped"),
            "headline must say the pop did not happen, got: {headline}"
        );
        assert!(
            !headline.starts_with("Popped"),
            "headline opens by claiming success, got: {headline}"
        );

        // And the gate really does open when it should — otherwise the
        // assertions above would be satisfied by a gate that never opens.
        assert_eq!(
            drop_gate(
                &ApplyOutcome::Applied,
                &ConflictScan::Read(Continuation::Clear)
            ),
            DropGate::Run,
            "a clean apply with a clear scan must let the drop run"
        );
    }

    /// The case the fixture caught, and the reason [`ApplyOutcome`] carries the
    /// warning it does.
    ///
    /// Measured against git 2.43.0: `git stash apply` on an entry that cannot
    /// merge exits **1** and leaves `UU` in the index. The server branches on
    /// the exit status, so this client sees `Refused` — and concluding "nothing
    /// was applied" from that is a false claim about the user's files, the same
    /// class of lie as A4's, pointing the other way.
    ///
    /// MUTATION 1 (removes the mechanism): decide the refused case on the apply
    ///   alone, before consulting the scan — red, the conflicted refusal
    ///   reports an untouched tree. Verified by hand: red.
    /// MUTATION 2 (keeps the arm, drops the distinction): set `apply_refusal`
    ///   to `None` unconditionally — red, a refusal becomes indistinguishable
    ///   from git calling the apply a success and leaving conflicts anyway, and
    ///   the headline then claims the changes "were applied". Verified by
    ///   hand: red.
    #[test]
    fn a_refused_apply_that_left_conflicts_does_not_claim_an_untouched_tree() {
        let verdict = match drop_gate(
            // What the server really returns for the fixture's stash@{0}.
            &ApplyOutcome::Refused(
                "CONFLICT (content): Merge conflict in collision.txt".to_string(),
            ),
            &ConflictScan::Read(blocked(&["collision.txt"], &[])),
        ) {
            DropGate::Halt(v) => v,
            DropGate::Run => panic!("a conflicted tree must never open the drop gate"),
        };

        assert!(!verdict.is_complete());
        assert_eq!(
            verdict.tree(),
            TreeState::Changed,
            "the conflict markers ARE in the working tree; reporting it untouched is the bug"
        );
        assert_eq!(verdict.conflicted_paths(), ["collision.txt"]);
        assert!(
            verdict.entry_retained() == Some(true),
            "git stash apply leaves the entry"
        );

        let headline = verdict.headline();
        assert!(headline.contains("NOT popped"), "got: {headline}");
        assert!(
            !headline.contains("Nothing was applied"),
            "this is exactly the false claim the fixture caught, got: {headline}"
        );
        // The refusal route says "hit conflicts"; the success-with-conflicts
        // route says "were applied but left conflicts". Both true, and kept
        // apart — without the next block the M2 mutation would survive.
        assert!(
            headline.starts_with("Applying the stash hit conflicts"),
            "got: {headline}"
        );

        let reported_success = match drop_gate(
            &ApplyOutcome::Applied,
            &ConflictScan::Read(blocked(&["collision.txt"], &[])),
        ) {
            DropGate::Halt(v) => v,
            DropGate::Run => panic!("still must not open the gate"),
        };
        assert!(
            reported_success
                .headline()
                .starts_with("The changes were applied but left conflicts"),
            "got: {}",
            reported_success.headline()
        );
    }

    /// A refused apply whose conflict scan ALSO failed knows nothing, and says
    /// so rather than picking a side.
    ///
    /// MUTATION 1: return `ApplyRefused` for this pair — red, it would claim a
    ///   verified-untouched tree on two failed observations. Verified: red.
    /// MUTATION 2: return `AppliedUnverified` instead — red, it would claim the
    ///   changes were applied when the apply was refused. Verified: red.
    #[test]
    fn a_refused_apply_with_an_unreadable_tree_reports_unknown_not_untouched() {
        let DropGate::Halt(verdict) = drop_gate(
            &ApplyOutcome::Refused("HTTP 400".to_string()),
            &ConflictScan::Failed("HTTP 500".to_string()),
        ) else {
            panic!("two failed observations must never open the drop gate");
        };

        assert!(!verdict.is_complete());
        assert_eq!(
            verdict.tree(),
            TreeState::Unknown,
            "neither fact was established; a bool would have had to guess"
        );
        assert!(verdict.entry_retained() == Some(true));
        assert!(
            verdict.headline().contains("unknown"),
            "got: {}",
            verdict.headline()
        );

        // The two TreeStates are distinct, so the assertion above cannot be
        // satisfied by a `tree()` that returns one value for everything.
        assert_eq!(PopVerdict::Popped.tree(), TreeState::Changed);
        assert_eq!(
            PopVerdict::ApplyRefused {
                why: "gone".to_string()
            }
            .tree(),
            // Was `Untouched` until #508. A refused apply says nothing about
            // the tree — see `TreeState`'s doc.
            TreeState::Unknown
        );
    }

    /// A scan that could not be made is not a scan that came back clear.
    ///
    /// `Continuation::from_files`' own doc comment states the precondition:
    /// an empty input means `Clear` *only* because the caller actually looked.
    /// `ConflictScan::from_fetch` is where that precondition is kept, and this
    /// pins it — a failed `GET /api/conflicts` must never open the gate.
    ///
    /// MUTATION 1 (removes the mechanism): map `ConflictScan::Failed` to
    ///   `DropGate::Run` in `drop_gate` — red, the entry would be destroyed on
    ///   a check that never completed. Verified by hand: red.
    /// MUTATION 2 (keeps the arm, corrupts the input): make `from_fetch`'s
    ///   `Err` arm return `ConflictScan::Read(Continuation::from_files(&[]))`
    ///   — red on the `from_fetch` assertion, because the failure has been
    ///   laundered into a clear verdict before `drop_gate` ever sees it.
    ///   Verified by hand: red.
    #[test]
    fn a_failed_conflict_scan_never_opens_the_drop_gate() {
        // The failure survives the constructor rather than becoming `Clear`.
        let scan = ConflictScan::from_fetch(Err("HTTP 500".to_string()));
        assert_eq!(scan, ConflictScan::Failed("HTTP 500".to_string()));

        let DropGate::Halt(verdict) = drop_gate(&ApplyOutcome::Applied, &scan) else {
            panic!("an unreadable conflict state must not open the drop gate");
        };
        assert!(!verdict.is_complete());
        assert_eq!(verdict.tree(), TreeState::Changed);
        assert!(
            verdict.entry_retained() == Some(true),
            "the apply happened and the entry was left alone; both must be reported"
        );
        assert!(
            verdict.headline().contains("could not be checked"),
            "the user must be told the check failed, got: {}",
            verdict.headline()
        );

        // A real empty read is still Clear — the distinction is the point, and
        // without this the test would pass against a `from_fetch` that reported
        // everything as a failure.
        assert_eq!(
            ConflictScan::from_fetch(Ok(vec![])),
            ConflictScan::Read(Continuation::Clear)
        );
    }

    /// Applied cleanly, then the drop failed: the composite state that a single
    /// operation row could not express, and the reason this pop is two
    /// requests. The user must learn that their changes are in the tree AND
    /// still in the drawer, because re-applying would duplicate them.
    ///
    /// MUTATION 1 (removes the mechanism): return `PopVerdict::Popped` from
    ///   `verdict_after_drop`'s `Refused` arm — red, a failed drop reported as
    ///   a finished pop. Verified by hand: red.
    /// MUTATION 2 (keeps the variant, weakens the report): make
    ///   `AppliedNotDropped`'s `tree()` return `TreeState::Unknown` — red, the
    ///   tree demonstrably changed and the report retreats to "could not
    ///   establish". Verified by hand: red. (This mutation named
    ///   `TreeState::Untouched` until #508 removed that variant; `Unknown` is
    ///   the same weakening now that it is the only other arm.)
    #[test]
    fn an_applied_stash_whose_drop_failed_says_both_things() {
        let verdict = verdict_after_drop(&DropOutcome::Refused(
            "stash@{0} now holds a different stash".to_string(),
        ));

        assert!(!verdict.is_complete(), "the drop failed; this is not a pop");
        assert_eq!(
            verdict.tree(),
            TreeState::Changed,
            "the changes ARE in the working tree"
        );
        assert!(
            verdict.entry_retained() == Some(true),
            "the entry is STILL in the drawer"
        );

        let headline = verdict.headline();
        assert!(
            headline.contains("STILL in your list"),
            "must warn the entry survived, got: {headline}"
        );
        assert!(
            headline.contains("duplicate"),
            "must warn that re-applying duplicates the changes, got: {headline}"
        );

        // The success path still works, so the assertions above cannot be
        // satisfied by a `verdict_after_drop` that never reports completion.
        assert_eq!(
            verdict_after_drop(&DropOutcome::Dropped),
            PopVerdict::Popped
        );
    }

    /// Exactly one of the five verdicts claims completion, and it is the one
    /// where all three steps really happened.
    ///
    /// This is the guard against a later variant being added and quietly
    /// inheriting a permissive `is_complete`.
    ///
    /// MUTATION 1: add any other variant to `is_complete`'s `matches!` — red,
    ///   the count is 2. Verified by hand: red.
    /// MUTATION 2: make `is_complete` return `false` for `Popped` too — red,
    ///   the count is 0 and a real pop could never be reported. Verified by
    ///   hand: red.
    #[test]
    fn exactly_one_verdict_means_the_pop_finished() {
        let all = every_verdict();
        assert_eq!(
            all.len(),
            7,
            "every_verdict must list every variant, both Conflicted routes included"
        );

        let complete: Vec<&PopVerdict> = all.iter().filter(|v| v.is_complete()).collect();
        assert_eq!(
            complete,
            vec![&PopVerdict::Popped],
            "exactly one verdict may claim the pop finished"
        );

        // The four that do not claim completion each leave the entry in the
        // drawer, so nothing the user had is gone.
        for verdict in all.iter().filter(|v| !v.is_complete()) {
            assert!(
                verdict.entry_retained() == Some(true),
                "{verdict:?} did not finish, so the entry must still be there"
            );
        }
        assert!(
            PopVerdict::Popped.entry_retained() == Some(false),
            "a finished pop removed the entry"
        );
    }

    /// A refused apply whose tree is verifiably clear really did leave
    /// everything alone — the one case where "nothing was applied" is a claim
    /// this client is entitled to make.
    ///
    /// MUTATION 1: return `DropGate::Run` for a refused apply — red, a drop
    ///   would be sent for a stash that was never applied. Verified: red.
    /// MUTATION 2: make `ApplyRefused`'s `tree()` return `TreeState::Changed` —
    ///   red, it would tell the user their files moved when nothing ran.
    ///   Verified: red.
    /// A refused apply over a clear scan says the stash is still yours — and
    /// **declines to describe the working tree**.
    ///
    /// This test asserted `TreeState::Untouched` until #508, on the strength of
    /// its own input: a compare-and-swap refusal, where git genuinely never
    /// ran. That input is real. The inference from it was not — the arm under
    /// test receives every refusal, and `ApplyOutcome::Refused` carries no fact
    /// about whether git ran. `git stash apply --include-untracked` can write a
    /// tracked file, fail on the untracked one, exit non-zero and leave no
    /// unmerged entry, and this same arm would have called that "untouched".
    ///
    /// The assertion changed because it was wrong, not to accommodate a fix.
    /// What it pins now is the honest floor: the gate stays shut, the entry is
    /// retained, and the tree is `Unknown`.
    #[test]
    fn a_refused_apply_over_a_clear_scan_declines_to_describe_the_tree() {
        let DropGate::Halt(verdict) = drop_gate(
            // A compare-and-swap refusal: the entry moved, so git never ran.
            // The verdict cannot know that, which is the whole point.
            &ApplyOutcome::Refused("stash@{3} no longer exists".to_string()),
            &ConflictScan::Read(Continuation::Clear),
        ) else {
            panic!("a refused apply must not open the drop gate");
        };
        assert!(!verdict.is_complete());
        assert_eq!(
            verdict.tree(),
            TreeState::Unknown,
            "a clear conflict scan proves nothing CONFLICTS, not that nothing LANDED"
        );
        assert!(verdict.entry_retained() == Some(true));
        assert!(verdict.conflicted_paths().is_empty());
        assert!(
            verdict.headline().contains("check `git status`"),
            "got: {}",
            verdict.headline()
        );
    }

    // -----------------------------------------------------------------------
    // A1 — inspectable before apply or drop
    // -----------------------------------------------------------------------

    /// **A1: "stash content is inspectable before apply or drop."**
    ///
    /// Order is the criterion. Inspect must be offered, and offered *ahead of*
    /// every destructive action — an inspect control that exists but sits below
    /// Drop is the defect the criterion names.
    ///
    /// MUTATION 1 (removes the mechanism): drop `StashAction::Inspect` from
    ///   `action_offers`' list — red, there is no inspect affordance at all.
    ///   Verified by hand: red.
    /// MUTATION 2 (keeps it, breaks the ordering): move `Inspect` to the end of
    ///   the list — red on the position assertions while the action still
    ///   exists. Verified by hand: red.
    #[test]
    fn inspect_is_offered_before_every_destructive_action() {
        let offers = action_offers(WriteGate::Allowed);

        let position = |want: StashAction| {
            offers
                .iter()
                .position(|o| o.action == want)
                .unwrap_or_else(|| panic!("{want:?} is not offered at all"))
        };

        let inspect = position(StashAction::Inspect);
        assert_eq!(inspect, 0, "inspection must be the first thing on offer");
        assert_eq!(offers[inspect].availability, Availability::Offered);

        // Branch is destructive since #516: success removes the stash entry,
        // matching the server's RiskLevel::Destructive classification.
        for destructive in [StashAction::Drop, StashAction::Pop, StashAction::Branch] {
            assert!(
                inspect < position(destructive),
                "{destructive:?} is offered before the user can look at the stash"
            );
            assert!(
                destructive.destructive(),
                "{destructive:?} must be classed destructive so the view can ask first"
            );
        }

        // The non-destructive ones must NOT be classed destructive, or the
        // classification would be satisfied by marking everything dangerous.
        for safe in [StashAction::Inspect, StashAction::Apply] {
            assert!(!safe.destructive(), "{safe:?} cannot lose the user's work");
        }
    }

    /// A read-only session still sees the drawer and can still read a stash;
    /// every write is refused *with a reason*, not by vanishing.
    ///
    /// MUTATION 1: refuse `Inspect` in a read-only session too — red, a
    ///   visualize session loses the one thing it is for. Verified: red.
    /// MUTATION 2: represent a refusal as absence (filter the action out
    ///   instead of marking it `Refused`) — red, the reason is gone and the
    ///   user is told nothing. Verified: red.
    #[test]
    fn a_read_only_session_can_inspect_but_every_write_is_refused_with_a_reason() {
        let offers = action_offers(WriteGate::ReadOnly);

        for offer in &offers {
            match offer.action {
                StashAction::Inspect => assert_eq!(
                    offer.availability,
                    Availability::Offered,
                    "a read cannot be refused by a write gate"
                ),
                write => assert_eq!(
                    offer.availability,
                    Availability::Refused(READ_ONLY_REFUSAL),
                    "{write:?} must be refused with a readable reason, not removed"
                ),
            }
        }

        // Every action is still present — a refusal is a state, not a deletion.
        assert_eq!(
            offers.len(),
            action_offers(WriteGate::Allowed).len(),
            "a read-only session must still be told what it cannot do"
        );
    }

    /// The `read_only` bool maps to the gate in one direction only.
    ///
    /// MUTATION 1: swap the two arms — red, and it is the swap that would offer
    ///   every destructive control to a session that may not write. Verified
    ///   by hand: red.
    /// MUTATION 2: return `WriteGate::Allowed` unconditionally — red on the
    ///   read-only case. Verified by hand: red.
    #[test]
    fn read_only_maps_to_the_refusing_gate() {
        assert_eq!(write_gate(true), WriteGate::ReadOnly);
        assert_eq!(write_gate(false), WriteGate::Allowed);
    }

    // -----------------------------------------------------------------------
    // A2 — staged and untracked options are explicit
    // -----------------------------------------------------------------------

    fn status_with(entries: Vec<StatusEntry>) -> StatusSections {
        StatusSections::from_worktree_status(&WorktreeStatus {
            generation: GenerationToken::new("status-v1:1").unwrap(),
            branch: Some("main".to_string()),
            upstream: None,
            ahead: 0,
            behind: 0,
            entries,
        })
    }

    fn untracked(path: &str) -> StatusEntry {
        StatusEntry::Untracked {
            path: path.to_string(),
            binary: false,
        }
    }

    /// A path changed on one side only — `staged = true` puts it in the Staged
    /// section, `false` in Unstaged.
    fn changed(path: &str, staged: bool) -> StatusEntry {
        StatusEntry::Changed {
            path: path.to_string(),
            sides: if staged {
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Modified,
                }
            } else {
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                }
            },
            submodule: None,
            binary: false,
        }
    }

    /// **A2: "staged and untracked options are explicit."**
    ///
    /// The failure this guards is a user believing they stashed a new file that
    /// git left in the worktree. So an untracked file with
    /// `include_untracked = false` must be named in `leaves_behind` *before*
    /// the push, and named as not stashed.
    ///
    /// MUTATION 1 (removes the mechanism): stop pushing the untracked line into
    ///   `leaves_behind` — red, the omission becomes silent, which is the
    ///   defect. Verified by hand: red.
    /// MUTATION 2 (keeps the line, inverts the condition): push it into
    ///   `captures` instead — red, the file is claimed as stashed when it is
    ///   not. Verified by hand: red.
    #[test]
    fn an_untracked_file_left_behind_is_named_before_the_push() {
        let sections = status_with(vec![untracked("notes.txt"), changed("src/main.rs", false)]);

        let excluded = push_preview(&sections, false, false);
        assert!(excluded.may_push(), "there is a tracked change to stash");
        assert_eq!(
            excluded.leaves_behind,
            ["1 untracked file — NOT stashed"],
            "the file git will leave behind must be named, and named as not stashed"
        );
        assert_eq!(excluded.captures, ["1 unstaged change"]);

        // Including them moves the line to the other list — without this the
        // test would pass against a preview that always warns.
        let included = push_preview(&sections, false, true);
        assert_eq!(included.captures, ["1 unstaged change", "1 untracked file"]);
        assert!(
            included.leaves_behind.is_empty(),
            "nothing is left behind once untracked files are included, got {:?}",
            included.leaves_behind
        );
    }

    /// `--keep-index` stashes the staged changes *and* leaves them staged. A
    /// user who reads "kept" as "not stashed" has it backwards, so the preview
    /// states both facts.
    ///
    /// MUTATION 1: omit the `keep_index` line from `leaves_behind` — red, the
    ///   double state is invisible. Verified: red.
    /// MUTATION 2: drop the staged line from `captures` when `keep_index` is
    ///   set — red, it would read as "kept, not stashed", the exact inversion.
    ///   Verified: red.
    #[test]
    fn keep_index_reports_the_staged_changes_as_both_stashed_and_kept() {
        let sections = status_with(vec![changed("src/lib.rs", true)]);

        let kept = push_preview(&sections, true, false);
        assert_eq!(
            kept.captures,
            ["1 staged change"],
            "the staged change IS stashed, even with --keep-index"
        );
        assert_eq!(
            kept.leaves_behind,
            ["1 staged change — also kept staged in the working tree"],
            "and it is also left staged; both halves must be said"
        );

        let not_kept = push_preview(&sections, false, false);
        assert_eq!(not_kept.captures, ["1 staged change"]);
        assert!(
            not_kept.leaves_behind.is_empty(),
            "without --keep-index nothing is left staged, got {:?}",
            not_kept.leaves_behind
        );
    }

    /// A clean tree has nothing to stash, and untracked files alone do not
    /// change that unless they are being included. `git stash push` would exit
    /// non-zero here; refusing up front turns an error into a sentence.
    ///
    /// MUTATION 1: always return `refusal: None` — red, a push would be offered
    ///   on a clean tree. Verified: red.
    /// MUTATION 3: collapse the two refusals into `NOTHING_TO_STASH` — red on
    ///   the untracked-only case, where "the working tree is already clean"
    ///   contradicts the line printed beside it. Verified by hand: red.
    /// MUTATION 2: refuse whenever `leaves_behind` is non-empty — red, the
    ///   untracked-excluded case is a perfectly valid push. **This mutation
    ///   SURVIVED the first version of this test**, which never exercised a
    ///   push that captures something and leaves something behind; the `mixed`
    ///   case below was added because of it. Verified by hand after that
    ///   change: red.
    #[test]
    fn a_push_is_refused_only_when_it_would_capture_nothing() {
        let clean = push_preview(&status_with(vec![]), false, false);
        assert_eq!(clean.refusal, Some(NOTHING_TO_STASH));
        assert!(!clean.may_push());

        // Untracked only, excluded: still nothing to save — but a DIFFERENT
        // refusal, because one tick would capture it and the generic wording
        // ("the working tree is already clean") contradicts the untracked line
        // rendered beside it.
        let untracked_only = status_with(vec![untracked("scratch.log")]);
        let excluded_only = push_preview(&untracked_only, false, false);
        assert_eq!(excluded_only.refusal, Some(ONLY_EXCLUDED_UNTRACKED));
        assert_ne!(
            excluded_only.refusal,
            Some(NOTHING_TO_STASH),
            "an excluded untracked file is not an already-clean tree"
        );
        assert_eq!(
            excluded_only.leaves_behind,
            ["1 untracked file — NOT stashed"],
            "and it is still named, so the tick has something to refer to"
        );

        // Untracked only, included: now there is.
        let now_valid = push_preview(&untracked_only, false, true);
        assert_eq!(now_valid.refusal, None);
        assert!(now_valid.may_push());
        assert_eq!(now_valid.captures, ["1 untracked file"]);

        // The case the name of this test claims and an earlier version of it
        // missed: something IS captured *and* something is left behind. A
        // refusal keyed on `leaves_behind` rather than on `captures` survives
        // every other case in this test, so without this line the assertion
        // "refused ONLY when it would capture nothing" was not actually made.
        // Found by the M2 mutation below surviving; kept as the regression.
        let mixed = push_preview(
            &status_with(vec![untracked("new.txt"), changed("src/main.rs", false)]),
            false,
            false,
        );
        assert_eq!(
            mixed.refusal, None,
            "a push that captures a tracked change is valid even though it leaves an \
             untracked file behind"
        );
        assert!(mixed.may_push());
        assert!(
            !mixed.leaves_behind.is_empty(),
            "and it does leave one behind"
        );
    }

    // -----------------------------------------------------------------------
    // The drawer's four states
    // -----------------------------------------------------------------------

    /// A listing entry, built through the shared DTO's own newtypes — so a
    /// fixture cannot spell an oid or a selector the server could never send.
    /// `hex` is repeated to a full 40-character object id.
    fn entry(selector: &str, hex: char) -> StashEntry {
        StashEntry {
            entry: git_vista_protocol::StashSelector::new(selector).expect("a valid selector"),
            oid: git_vista_protocol::CommitOid::new(hex.to_string().repeat(40))
                .expect("a valid oid"),
            message: "On main: work".to_string(),
            time: 1_700_000_000,
        }
    }

    /// A drawer that could not be read is never shown as a drawer that is
    /// empty.
    ///
    /// `read_stashes` in `git-vista-git` and the server's own handler both go
    /// out of their way to keep these apart — the handler refuses to serialise
    /// a failure as `[]`. A client that merged them would undo both and tell a
    /// user with stashes that they have none.
    ///
    /// MUTATION 1 (removes the mechanism): map `Some(Err(_))` to
    ///   `DrawerView::Empty` — red, a failed read renders as "nothing
    ///   stashed". Verified by hand: red.
    /// MUTATION 2 (keeps the arm, loses the distinction): map `None` to
    ///   `Empty` as well, so an unresolved fetch reads as an answered one —
    ///   red on the Loading assertion. Verified by hand: red.
    #[test]
    fn a_drawer_that_could_not_be_read_is_never_shown_as_an_empty_drawer() {
        // Unresolved: we have not asked yet.
        assert_eq!(
            drawer_view(None, WriteGate::Allowed),
            DrawerView::Loading,
            "an unresolved fetch is not an answer"
        );

        // Failed: we asked and could not look.
        let failed = drawer_view(Some(Err("HTTP 500".to_string())), WriteGate::Allowed);
        assert_eq!(
            failed,
            DrawerView::Failed("Couldn't read the stash list: HTTP 500".to_string()),
            "a failed read must carry its reason, not become an empty list"
        );

        // Empty: we asked and there is genuinely nothing.
        assert_eq!(
            drawer_view(Some(Ok(vec![])), WriteGate::Allowed),
            DrawerView::Empty
        );

        // And the three are mutually distinct, so none of the above can be
        // satisfied by a classifier that returns one value for everything.
        let all = [
            drawer_view(None, WriteGate::Allowed),
            drawer_view(Some(Err("x".to_string())), WriteGate::Allowed),
            drawer_view(Some(Ok(vec![])), WriteGate::Allowed),
            drawer_view(Some(Ok(vec![entry("stash@{0}", 'a')])), WriteGate::Allowed),
        ];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "two different drawer states classified the same");
            }
        }

        // The wording of the two "nothing to show" lines must differ, or the
        // user cannot tell which one they are looking at.
        assert_ne!(NO_STASHES, LOADING_STASHES);
    }

    /// The rows keep the server's order — newest first — and each carries the
    /// write gate it was built under.
    ///
    /// MUTATION 1: reverse the row order — red, `stash@{0}` is no longer
    ///   first and the newest stash sorts to the bottom. Verified: red.
    /// MUTATION 2: build every row with `WriteGate::Allowed` regardless of the
    ///   argument — red, a read-only session would be offered writes.
    ///   Verified: red.
    #[test]
    fn rows_keep_the_servers_order_and_carry_the_write_gate() {
        let entries = vec![entry("stash@{0}", 'a'), entry("stash@{1}", 'b')];

        let DrawerView::Rows(rows) = drawer_view(Some(Ok(entries.clone())), WriteGate::Allowed)
        else {
            panic!("a populated drawer must classify as Rows");
        };
        assert_eq!(
            rows.iter().map(|r| r.selector.as_str()).collect::<Vec<_>>(),
            ["stash@{0}", "stash@{1}"],
            "the server returns newest first and the view must not resort"
        );

        let DrawerView::Rows(locked) = drawer_view(Some(Ok(entries)), WriteGate::ReadOnly) else {
            panic!("a read-only session still sees its stashes");
        };
        let drop_offer = locked[0]
            .actions
            .iter()
            .find(|o| o.action == StashAction::Drop)
            .expect("the action must still be listed");
        assert_eq!(
            drop_offer.availability,
            Availability::Refused(READ_ONLY_REFUSAL),
            "a read-only drawer must not offer a drop"
        );
    }

    // -----------------------------------------------------------------------
    // Rows
    // -----------------------------------------------------------------------

    /// Git writes two message shapes and a foreign tool can write anything.
    /// Each is shown as what it is; nothing is ever blank.
    ///
    /// MUTATION 1: return the whole message as `subject` and drop the branch
    ///   split — red on the `branch` assertions. Verified: red.
    /// MUTATION 2: return `String::new()` instead of `NO_SUBJECT` for a blank
    ///   message — red, a blank row reads as a stash with no changes.
    ///   Verified: red.
    #[test]
    fn every_stash_message_shape_produces_a_readable_row() {
        // Git's automatic form: the base commit's sha is dropped, the subject kept.
        let wip = stash_subject("WIP on main: 1a2b3c4 tidy the parser");
        assert_eq!(wip.branch.as_deref(), Some("main"));
        assert_eq!(wip.subject, "tidy the parser");
        assert!(
            wip.automatic,
            "this message was written by git, not the user"
        );

        // The `-m` form: the user's own words, kept whole.
        let named = stash_subject("On feature/x: half-finished refactor");
        assert_eq!(named.branch.as_deref(), Some("feature/x"));
        assert_eq!(named.subject, "half-finished refactor");
        assert!(!named.automatic, "the user typed this");

        // Not a shape this module claims to understand: shown verbatim.
        let foreign = stash_subject("something else entirely");
        assert_eq!(foreign.branch, None);
        assert_eq!(foreign.subject, "something else entirely");

        // Nothing usable at all still renders something.
        assert_eq!(stash_subject("   ").subject, NO_SUBJECT);
        assert_eq!(stash_subject("").subject, NO_SUBJECT);

        // A subject that only looks like it starts with a sha keeps its word.
        let short = stash_subject("WIP on main: fix the thing");
        assert_eq!(
            short.subject, "fix the thing",
            "'fix' is not a hex token and must not be eaten"
        );
    }

    /// A row carries the selector and oid **verbatim from the wire**, because
    /// both are sent back to act on the entry and the server pairs them in a
    /// compare-and-swap. Re-deriving either in the client would give the wire
    /// form a second author — which is the whole defect #495 closed.
    ///
    /// The fixture is deliberately a drawer whose selectors do not match their
    /// positions in the list. That is not a contrived shape: the git crate
    /// stops a listing at an unreadable reflog line rather than renumbering
    /// what is below it, so a gap is a real thing to receive, and a
    /// concurrent drop renumbers the drawer under any list already read.
    ///
    /// MUTATION 1: build `selector` from the row's position in the vector
    ///   (`format!("stash@{{{i}}}")`) — red, the second row addresses
    ///   `stash@{5}` and would be sent as `stash@{1}`. Verified: red.
    /// MUTATION 2: truncate `oid` to `oid_short` for the round-trip field —
    ///   red, the compare-and-swap would be sent an abbreviated oid.
    ///   Verified: red.
    #[test]
    fn a_row_round_trips_the_selector_and_oid_untouched() {
        let entries = vec![entry("stash@{2}", '0'), entry("stash@{5}", 'f')];

        let DrawerView::Rows(rows) = drawer_view(Some(Ok(entries)), WriteGate::Allowed) else {
            panic!("a populated drawer must classify as Rows");
        };
        assert_eq!(
            rows.iter().map(|r| r.selector.as_str()).collect::<Vec<_>>(),
            ["stash@{2}", "stash@{5}"],
            "each row addresses the entry the server addressed, not its place in the list"
        );
        assert_eq!(
            rows[1].oid,
            "f".repeat(40),
            "the full oid is what the compare-and-swap needs"
        );
        assert_eq!(rows[1].oid_short, "fffffff", "display only");
        assert_eq!(rows[1].when, 1_700_000_000);
    }

    /// The listing shape, pinned against the JSON literal the server's own
    /// mapping test pins the other end of
    /// (`handlers::stash::listing_tests::a_record_becomes_the_wire_bytes_the_frontend_parses`).
    ///
    /// Since #495 this is no longer a transcription that notices a rename only
    /// when someone re-reads the handler: the type here *is* the type there.
    /// The literal stays because the shared type is what agrees with itself —
    /// only a literal can say what a browser receives.
    ///
    /// MUTATION 1 (rename): `entry` → `selector` in
    ///   `git_vista_protocol::StashEntry` — red, `missing field \`entry\``.
    /// MUTATION 2 (retype): `oid: CommitOid` → `oid: String` — red, the
    ///   abbreviated-oid listing below stops being refused.
    #[test]
    fn the_listing_shape_the_server_actually_sends() {
        let wire = r#"[
            {
              "entry": "stash@{0}",
              "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "message": "WIP on main: 1a2b3c4 tidy the parser",
              "time": 1700000000
            }
        ]"#;

        let parsed: Vec<StashEntry> = serde_json::from_str(wire).expect("listing must deserialize");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].entry.as_str(), "stash@{0}");
        assert_eq!(parsed[0].oid.as_str().len(), 40);
        assert_eq!(parsed[0].message, "WIP on main: 1a2b3c4 tidy the parser");
        assert_eq!(parsed[0].time, 1_700_000_000);

        // The position is no longer on the wire, and is not missed: it reads
        // back out of the selector through the protocol crate's own accessor,
        // so no client parses `stash@{…}` for itself.
        assert_eq!(parsed[0].entry.index(), Some(0));

        // And a listing the server could not have produced does not become a
        // drawer full of unusable rows — it fails to deserialize at all.
        let bad = r#"[{"entry":"stash@{0}","oid":"aaaaaaa","message":"m","time":1}]"#;
        assert!(
            serde_json::from_str::<Vec<StashEntry>>(bad).is_err(),
            "an abbreviated oid cannot be compare-and-swapped, so it is not an entry"
        );
    }

    /// **#515: a lost apply reply is reported as UNKNOWN, never as a
    /// refusal.** Encoding transport loss as `Refused` was H3's core lie —
    /// the UI asserting an outcome nobody observed. The gate must hold the
    /// drop shut AND the verdict must be the unknown-shaped one, because
    /// `ApplyRefused`'s headline claims the server said no, which here it
    /// never did.
    ///
    /// MUTATION 1 (restore the lie): make `drop_gate` map
    /// `(Unknown, Clear)` to `ApplyRefused` — red on the variant assert.
    /// MUTATION 2 (weaken the one certainty): make `ApplyUnknown`'s
    /// `entry_retained()` return `None` — red on the `Some(true)` assert;
    /// an apply never consumes the entry, ran or not, and the headline
    /// leans on exactly that.
    #[test]
    fn a_lost_apply_reply_is_reported_unknown_never_refused() {
        let apply = ApplyOutcome::Unknown("the reply was lost".to_string());
        let scan = ConflictScan::Read(Continuation::Clear);
        let DropGate::Halt(verdict) = drop_gate(&apply, &scan) else {
            panic!("a lost apply reply must never open the drop gate");
        };
        assert_eq!(
            verdict,
            PopVerdict::ApplyUnknown {
                why: "the reply was lost".to_string()
            },
            "unknown must stay unknown — ApplyRefused claims the server said no"
        );
        assert!(!verdict.is_complete());
        assert_eq!(verdict.tree(), TreeState::Unknown);
        assert_eq!(
            verdict.entry_retained(),
            Some(true),
            "an apply never removes the entry, ran or not — this is the one certainty"
        );
        let line = verdict.headline();
        assert!(line.contains("lost"), "the headline names the actual event");
        assert!(
            !line.to_lowercase().contains("was refused"),
            "the headline must not dress a lost reply as a refusal"
        );
    }

    /// **#515: a lost DROP reply leaves the entry's fate genuinely unknown.**
    /// `AppliedNotDropped` asserts "it is STILL in your list"; after a lost
    /// drop reply that is a guess — the server may have removed it. The
    /// verdict must be `DropUnknown`, whose `entry_retained()` is `None`,
    /// and whose headline still owns the verified half (the changes ARE in
    /// the tree) while warning that a re-apply would duplicate them.
    ///
    /// MUTATION 1 (restore the lie): make `verdict_after_drop` map
    /// `Unknown` to `AppliedNotDropped` — red on the variant assert.
    /// MUTATION 2 (overclaim): make `DropUnknown`'s `entry_retained()`
    /// return `Some(true)` — red on the `None` assert.
    #[test]
    fn a_lost_drop_reply_leaves_entry_fate_unknown() {
        let verdict = verdict_after_drop(&DropOutcome::Unknown("reply lost".to_string()));
        assert_eq!(
            verdict,
            PopVerdict::DropUnknown {
                why: "reply lost".to_string()
            },
            "unknown must stay unknown — AppliedNotDropped asserts the entry is retained"
        );
        assert!(
            !verdict.is_complete(),
            "an unobserved drop is not a finished pop"
        );
        assert_eq!(
            verdict.tree(),
            TreeState::Changed,
            "the apply half WAS verified before the drop was sent"
        );
        assert_eq!(
            verdict.entry_retained(),
            None,
            "nobody observed the drop's outcome; a bool here would be a guess"
        );
        assert!(verdict.headline().contains("duplicate"));
    }

    /// **#515: the classifiers keep the three-way distinction.** A recovered
    /// record answer has the same standing as a direct answer (the server
    /// said it — we heard it late); a local refusal is CERTAIN (nothing was
    /// sent); only a lost-and-unrecoverable reply may say Unknown.
    #[test]
    fn wire_outcomes_classify_without_inventing_answers() {
        use StashWriteOutcome as W;
        let msg = |m: &str| m.to_string();
        assert_eq!(
            ApplyOutcome::from_write(Ok(W::Reconciled {
                ok: true,
                message: msg("done")
            })),
            ApplyOutcome::Applied,
            "a recovered success is a success"
        );
        assert_eq!(
            ApplyOutcome::from_write(Ok(W::Reconciled {
                ok: false,
                message: msg("no")
            })),
            ApplyOutcome::Refused(msg("no")),
            "a recovered refusal is a refusal — not an unknown"
        );
        assert_eq!(
            ApplyOutcome::from_write(Err(msg("offline"))),
            ApplyOutcome::Refused(msg("offline")),
            "a local refusal never left the device; refused is certain"
        );
        assert_eq!(
            ApplyOutcome::from_write(Ok(W::Unknown { why: msg("lost") })),
            ApplyOutcome::Unknown(msg("lost"))
        );
        assert_eq!(
            DropOutcome::from_write(Ok(W::Answered {
                ok: false,
                message: msg("409")
            })),
            DropOutcome::Refused(msg("409"))
        );
        assert_eq!(
            DropOutcome::from_write(Ok(W::Unknown { why: msg("lost") })),
            DropOutcome::Unknown(msg("lost"))
        );
    }
}
