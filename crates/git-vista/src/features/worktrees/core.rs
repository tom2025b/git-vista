//! The pure decision model for the worktree drawer (M11.03, #548).
//!
//! Framework-free and host-tested, per the `features/*/core.rs` rule: every
//! decision the drawer makes lives here and the wasm view holds none. `mod
//! app` and every view module are `#[cfg(target_arch = "wasm32")]`, so a
//! decision left inside markup can never be host-tested — and "renders
//! nothing" is exactly how such a defect presents.
//!
//! # The decision this file exists for
//!
//! **Three separate facts about one row, and they must not be folded into
//! one.** `docs/superpowers/specs/m3.23-worktrees.md` §1 makes this the
//! design's load-bearing distinction, and #548's acceptance states it as a
//! failure condition: *"a single 'unusable' badge covering both is a failure
//! of this criterion"*.
//!
//! | fact | who says it | example |
//! |---|---|---|
//! | `locked` / `prunable` / `bare` | **git**, read verbatim from porcelain | a locked worktree is one git refuses to `remove` |
//! | [`Serviceable`] | **this application's fence** | a worktree outside the allowed roots is one *this app* refuses to open, which git has no opinion about |
//! | the offer | this module, from the two above | open it, refuse with a reason, or "you are here" |
//!
//! A locked worktree inside the allowed roots is `Serviceable::Yes` and
//! **openable** — locking is git's business with `worktree remove`, not a
//! statement about whether this app may serve the directory. Collapsing the
//! two would make that row unopenable for a reason nobody holds.
//!
//! # A refusal is shown, never merely greyed out
//!
//! [`RowOffer::Refused`] carries the sentence, and the sentence comes from
//! [`Serviceable::refusal`] in the protocol crate — the same text the server
//! answers `POST /api/select-worktree` with. One source, two consumers: the
//! failure mode of two copies is that the drawer promises one thing and the
//! server says another about the identical row. M11.02's `collision_refusal`
//! earned that rule.
//!
//! #65's finding is why it is *text*: a reason carried only in `title=` or
//! only in `aria-label` never surfaces on a tap and is never announced.

use git_vista_protocol::{Serviceable, WorktreeCensus, WorktreeSibling};

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Who is making a claim about a row.
///
/// The type exists so the view cannot render two different kinds of claim
/// through one code path: git reporting a flag and this application declaring
/// a verdict are different sentences with different remedies, and a reader who
/// cannot tell them apart has been handed one badge wearing two hats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactSource {
    /// Read verbatim from `git worktree list --porcelain`.
    Git,
    /// This application's own fence and verdict.
    App,
}

/// One badge on a row, and who said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowFact {
    pub label: &'static str,
    pub source: FactSource,
}

/// What the drawer offers for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowOffer {
    /// The worktree already being served. Nothing to switch to.
    Current,
    /// Openable. `id` is the opaque worktree id to send to
    /// `POST /api/select-worktree` — never a path, which is why the drawer can
    /// offer this without the operator having opted into path exposure.
    Open { id: String },
    /// Refused, with the reason to render as visible text.
    Refused { reason: &'static str },
}

/// Whether the drawer offers to close a row's desk (M11.05, #550) — kept as
/// its own field on [`WorktreeRow`] rather than folded into [`RowOffer`],
/// because the two are independent questions about a serviceable,
/// non-current row: [`RowOffer::Open`] answers "can this be switched to?" and
/// this answers "can this be closed?". A row can (and typically will) answer
/// yes to both at once, which one `RowOffer` variant per row cannot express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOffer {
    /// Not offered. Three cases collapse here, deliberately: the current
    /// worktree (it cannot close itself), a `Missing` sibling (its directory
    /// is already gone — releasing the branch it still holds is `git
    /// worktree prune`, a different operation this design omits), and one
    /// `OutsideAllowedRoots` (visible for collision detection only; that
    /// visibility must never widen into a mutation the app cannot verify).
    NotOffered,
    /// `git worktree remove` may be attempted. `id` is the opaque worktree
    /// id to send to `POST /api/remove-worktree` — the server resolves it to
    /// a real path itself, via a fresh census, immediately before acting
    /// (see `GitOperation::RemoveWorktree`'s doc comment). The drawer never
    /// learns or sends a path.
    Offer { id: String },
}

/// One row of the drawer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    /// The census's short, non-path display label.
    pub name: String,
    /// The absolute path, when the operator opted into path exposure
    /// (`GIT_VISTA_EXPOSE_PATHS`). `None` is the default and is not a defect —
    /// the row is fully usable without it, because every action is by id.
    pub path: Option<String>,
    /// What this worktree has checked out, already worded: a branch name, or
    /// why there is no branch to name.
    pub branch: BranchCell,
    /// The short oid HEAD resolves to, or `None` for an unborn branch or a
    /// bare record — never a fabricated all-zero oid.
    pub head: Option<String>,
    /// Whether this is the worktree currently being served.
    pub is_current: bool,
    /// git's own flags. Empty when git reported none — which is the ordinary
    /// case and must not be confused with "this app has no verdict".
    pub git_facts: Vec<RowFact>,
    /// This application's verdict. **Always present**, on every row, including
    /// openable ones: a badge that appears only on refusal teaches a reader
    /// that its absence means nothing was checked.
    pub app_fact: RowFact,
    pub offer: RowOffer,
    /// Whether this row's desk may be closed (M11.05, #550) — see
    /// [`RemoveOffer`] for why it is a field of its own rather than a
    /// third case folded into `offer`.
    pub remove_offer: RemoveOffer,
}

/// What a row has checked out. Three answers, not an `Option<String>` plus a
/// convention: "detached" and "bare" are real, healthy states with different
/// meanings, and a view given `None` would have to guess which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchCell {
    /// A branch, by short name.
    Branch(String),
    /// A detached HEAD — a normal state, not an error.
    Detached,
    /// The repository's own bare administrative directory, which holds no
    /// working tree and therefore no branch.
    Bare,
}

impl BranchCell {
    /// The words the drawer renders. Kept here rather than in the view so a
    /// host test can read them.
    pub fn label(&self) -> String {
        match self {
            Self::Branch(name) => name.clone(),
            Self::Detached => "detached HEAD".to_string(),
            Self::Bare => "bare — no working tree".to_string(),
        }
    }
}

/// What the drawer shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrawerView {
    /// The census was read. Rows in census order — **including the ones this
    /// application refuses to open**, which is the spec's decision: hiding a
    /// refused sibling is "a wrong answer produced by a deliberate omission",
    /// and it would also make the drawer disagree with the collision check,
    /// which counts every worktree git counts.
    Rows(Vec<WorktreeRow>),
    /// The census could not be read. Lists nothing and claims nothing — an
    /// empty row list here would say "this repository has no other worktrees",
    /// which is precisely the fail-open `WorktreeCensus` exists to prevent.
    Unreadable { reason: String },
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// Build the drawer from what `GET /api/worktrees` returned.
///
/// The argument is the fetch's own `Result`, so the two ways of learning
/// nothing — the request failed, or the server answered `CensusFailed` — both
/// arrive and both become [`DrawerView::Unreadable`]. Neither may become an
/// empty row list.
pub fn drawer_view(fetched: Result<WorktreeCensus, String>) -> DrawerView {
    match fetched {
        Err(reason) => DrawerView::Unreadable { reason },
        Ok(WorktreeCensus::CensusFailed { reason }) => DrawerView::Unreadable { reason },
        Ok(WorktreeCensus::Observed { siblings }) => {
            DrawerView::Rows(siblings.iter().map(row).collect())
        }
    }
}

/// One sibling's row.
fn row(sibling: &WorktreeSibling) -> WorktreeRow {
    WorktreeRow {
        name: sibling.name.clone(),
        path: sibling.path.clone(),
        branch: branch_cell(sibling),
        head: sibling.head.as_ref().map(|oid| short(oid.as_str())),
        is_current: sibling.is_current,
        git_facts: git_facts(sibling),
        app_fact: app_fact(&sibling.serviceable),
        offer: offer(sibling),
        remove_offer: remove_offer(sibling),
    }
}

fn branch_cell(sibling: &WorktreeSibling) -> BranchCell {
    match (&sibling.branch, sibling.bare) {
        (Some(branch), _) => BranchCell::Branch(branch.as_str().to_string()),
        (None, true) => BranchCell::Bare,
        (None, false) => BranchCell::Detached,
    }
}

/// git's flags, verbatim and in a fixed order. Never includes anything this
/// application decided.
fn git_facts(sibling: &WorktreeSibling) -> Vec<RowFact> {
    let mut facts = Vec::new();
    if sibling.locked {
        facts.push(RowFact {
            label: "locked",
            source: FactSource::Git,
        });
    }
    if sibling.prunable {
        facts.push(RowFact {
            label: "prunable",
            source: FactSource::Git,
        });
    }
    if sibling.bare {
        facts.push(RowFact {
            label: "bare",
            source: FactSource::Git,
        });
    }
    facts
}

/// This application's verdict, one badge, always present.
fn app_fact(serviceable: &Serviceable) -> RowFact {
    RowFact {
        label: match serviceable {
            Serviceable::Yes => "can open",
            Serviceable::OutsideAllowedRoots => "outside your folders",
            Serviceable::Missing => "folder is gone",
        },
        source: FactSource::App,
    }
}

/// What the row offers.
///
/// The current worktree is checked **first**, and deliberately: it is
/// `Serviceable::Yes`, so asking the fence first would offer a switch to the
/// place the user already is.
fn offer(sibling: &WorktreeSibling) -> RowOffer {
    if sibling.is_current {
        return RowOffer::Current;
    }
    match sibling.serviceable.refusal() {
        Some(reason) => RowOffer::Refused { reason },
        // `refusal()` is `None` exactly for `Serviceable::Yes`, so this arm is
        // the openable one — derived from the protocol's own answer rather
        // than re-matching the enum here, which is how the drawer and the
        // server's `/api/select-worktree` stay one decision.
        None => RowOffer::Open {
            id: sibling.id.clone(),
        },
    }
}

/// Whether the row offers to close its desk (M11.05, #550).
///
/// Same shape as [`offer`] — current is checked first, then the fence — but a
/// **separate** function rather than a fold into it: `Serviceable::Yes` and
/// non-current is exactly the condition both `offer` and `remove_offer` agree
/// is servable, so this row can (and does) get both an [`RowOffer::Open`] and
/// a [`RemoveOffer::Offer`] at once. The `Missing`/`OutsideAllowedRoots`
/// refusals it shares with `offer` are spelled out on `RemoveOffer::NotOffered`
/// itself rather than repeated here — see that variant's doc comment for
/// the reasons, which differ from `offer`'s (a `Missing` row is offered
/// *nothing* to switch to either, but for `RemoveOffer` the distinction from
/// "refused" matters: closing a desk that is merely fenced off would still be
/// removing something this application never proved existed at all).
fn remove_offer(sibling: &WorktreeSibling) -> RemoveOffer {
    if sibling.is_current || !matches!(sibling.serviceable, Serviceable::Yes) {
        return RemoveOffer::NotOffered;
    }
    RemoveOffer::Offer {
        id: sibling.id.clone(),
    }
}

/// A commit id, shortened for display. Seven characters, matching the graph's
/// own `short_oid` — a second convention would make the same commit read as
/// two different ids on one screen.
fn short(oid: &str) -> String {
    oid.chars().take(7).collect()
}

#[cfg(test)]
#[path = "core_suite.rs"]
mod core_suite;
