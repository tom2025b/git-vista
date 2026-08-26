//! The activity feed: shared types, the reflog-message parser, and the pure
//! feed-assembly logic behind `GET /api/activity`.
//!
//! "What happened in this repo?" is answered from three sources the server
//! collects (see the server's `activity` module) and this module folds:
//!
//!  1. the **app journal** — one [`ActivityEvent`] per write the app itself
//!     performed, recorded by the server at op time ([`ActivitySource::App`]);
//!  2. **reflog entries** — every ref's log, i.e. everything that moved a ref,
//!     whoever moved it, parsed here from git's own reflog messages;
//!  3. synthesized events (e.g. a branch deleted *outside* the app, noticed by
//!     diffing ref snapshots) — journaled by the server and arriving via 1.
//!
//! The folding rules live here, pure and unit-tested, because they're the
//! subtle part: a single `git merge` writes reflog lines on *both* HEAD and
//! the branch, and an app-performed merge additionally has a journal entry —
//! one user action must come out as **one event**, attributed to the app when
//! the app did it. A rebase writes one reflog line per replayed commit; those
//! collapse into one event. Everything is sorted newest-first and capped.
//!
//! Undo *hints* are attached during assembly ([`Undoable`]): a deleted branch
//! whose tip we know can be restored; a merge/rebase/commit still sitting at
//! a branch's tip can be reset away. The hints carry everything the undo
//! endpoint needs, plus a compare-and-swap `expected_tip` so a stale menu
//! can't reset a branch that has since moved.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// What kind of repo event this is — drives the feed row's glyph and the undo
/// mapping. `Other` carries anything a future git writes that we don't know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Commit,
    Amend,
    Merge,
    Rebase,
    Checkout,
    Reset,
    CherryPick,
    Revert,
    BranchCreated,
    BranchDeleted,
    Push,
    Fetch,
    Pull,
    Clone,
    Other,
}

/// Who performed the event: the app (recorded in its journal at op time), or
/// anything else — the terminal, another tool — seen only via reflogs and
/// snapshot diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivitySource {
    App,
    External,
}

/// One undoable operation — the body of `POST /api/undo`, and the payload
/// inside an [`Undoable`] hint. Tagged so the JSON is self-describing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UndoAction {
    /// Re-create a deleted branch at its last known tip (`git branch <name>
    /// <tip>`). The safe undo for a deletion — creates, never destroys.
    RestoreBranch { name: String, tip: String },
    /// Move a branch back to `to`, undoing a merge/rebase/commit/reset whose
    /// result still sits at the branch tip. `expected_tip` is compare-and-swap:
    /// the server refuses if the branch no longer points there, so a stale
    /// menu can never reset away work that happened after it was shown.
    ResetBranch {
        branch: String,
        to: String,
        expected_tip: String,
    },
    /// `git revert --no-edit <commit>` — the history-preserving undo for a
    /// commit that's already shared.
    RevertCommit { commit: String },
}

/// An [`UndoAction`] dressed for a menu: the action itself, a human label, and
/// whether the state being discarded is already on the remote (in which case
/// undoing locally leaves the remote ahead, because **no undo force-pushes** —
/// and the confirm dialog says so).
///
/// That is a statement about this path only, and it became one that had to be
/// said precisely in M2.20e (#231): git-vista can now force-publish, on an
/// explicit user-initiated push carrying
/// [`ForcePublish::WithLease`](git_vista_protocol::ForcePublish::WithLease). An
/// undo still never does. The distinction is the point — a user who chooses to
/// rewrite the remote reviews a plan that says so and is ranked
/// `RiskLevel::Destructive`; an undo they asked to apply *locally* must never
/// quietly reach out and do the same thing to a colleague's clone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Undoable {
    pub action: UndoAction,
    pub label: String,
    pub warn_pushed: bool,
}

/// One event in the activity feed — also the exact shape journaled to
/// `.git/git-vista/journal.jsonl` (one JSON object per line; `undo` is never
/// journaled — it's recomputed against the *current* repo on every read,
/// because whether an event is still undoable changes as the repo moves).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// Unix seconds.
    pub time: i64,
    pub kind: ActivityKind,
    /// The ref the event happened on: `"main"`, `"origin/main"`, `"HEAD"`.
    /// `None` only for events that aren't about one ref.
    pub ref_name: Option<String>,
    /// Human line for the feed row (a commit's summary, "main → feature", …).
    pub summary: String,
    /// The ref's tip before/after. Deletions have no `new_oid`; creations no
    /// meaningful `old_oid`.
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub source: ActivitySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<Undoable>,
    /// The repository's refs as they stood when this event was journaled —
    /// HEAD, local branches, tags and remote-tracking branches (#131, #449).
    /// This is what lets a future time scrubber replay history *losslessly*
    /// — including refs that no longer exist — instead of depending on the
    /// reflog, which expires at ~90 days, keeps only 200 entries per ref, and
    /// is deleted outright with its branch.
    ///
    /// `None` means no capture is recorded: the event predates this field, or
    /// it was journaled somewhere without a real `.git` directory. It does
    /// NOT mean the repo had no branches — see [`RefsAtEvent`], whose whole
    /// purpose is keeping those three answers apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<RefsAtEvent>,
}

/// The ref capture attached to a journaled event: HEAD, local branches, tags
/// and remote-tracking refs as they stood at that instant.
///
/// **Why this is an enum and not a map.** A replayer asking "which refs
/// existed at this moment?" must be able to tell three answers apart:
///
/// | value | meaning | what a replay may conclude |
/// |---|---|---|
/// | field absent (`None`) | no capture was attempted | nothing — this event carries no ref history |
/// | [`Self::CaptureFailed`] | we tried and could not read the refs | nothing — and it must NOT infer deletions |
/// | [`Self::Captured`] | a real observation, possibly of zero refs | the maps are the truth at that instant |
///
/// Collapsing the middle row into an empty map would make a failed read
/// indistinguishable from "every branch was deleted" — the most destructive
/// reading available, produced by the least informative event. That is the
/// exact defect class this codebase spent 2026-08-18 removing, so the storage
/// format refuses to allow it in the first place.
///
/// **The same rule, one level down (#449).** The three-state honesty is a
/// property of *every* field, not just of this enum. A new field must
/// distinguish **not recorded** from **recorded as empty**, or it reintroduces
/// at the field level the defect this type exists to prevent at the record
/// level:
///
/// | `tags` | meaning | what a replay may conclude |
/// |---|---|---|
/// | absent (`None`) | this line predates #449 | nothing about tags |
/// | `Some` with an empty map | observed | the repo genuinely had no tags |
/// | `Some` with entries | observed | these tags, at these tips |
///
/// Declaring `tags` a bare `BTreeMap` with `#[serde(default)]` would make
/// every journal line written before #449 deserialize as a confident
/// observation that the repository had zero tags — a claim never made,
/// produced by the least informative line available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RefsAtEvent {
    /// The refs were read. Each map is a short-name -> tip map at that
    /// instant, and an empty map is a legitimate observation (a repo before
    /// its first commit genuinely has no branches).
    ///
    /// `branches` and `truncated_at` keep the names, positions and meanings
    /// they had under #131 exactly: the journal is append-only, and a line
    /// written last month must keep meaning what it meant when it was
    /// written. Everything #449 adds is an additional optional field. The
    /// resulting shape is asymmetric — `branches` is a bare map with a
    /// sibling `truncated_at` while `tags` and `remotes` are
    /// [`CapturedRefs`] — and that asymmetry is the price of not rewriting
    /// the meaning of lines already on disk. Uniformity was available only by
    /// breaking them.
    Captured {
        /// Local branches, under their short names (`main`, `feature/ui`).
        branches: BTreeMap<String, String>,
        /// `Some(total)` when the repo had more branches than the journal
        /// records per event, carrying the true count. The map then holds the
        /// first [`REFS_PER_EVENT_CAP`] by name order. Never silently capped:
        /// a replayer that cannot see the truncation would read the missing
        /// branches as deleted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated_at: Option<usize>,
        /// Where HEAD pointed (#449). `None` means this line predates the
        /// field — which is not "HEAD was unreadable", a state
        /// [`HeadAtEvent::Unreadable`] records explicitly and with a reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<HeadAtEvent>,
        /// Tags, under their short names (`v1.0`), each **peeled** to the
        /// commit it ultimately points at — see [`CapturedRefs`] for what
        /// peeling costs. `None` = not recorded; `Some` = observed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tags: Option<CapturedRefs>,
        /// Remote-tracking refs, under their short names (`origin/main`).
        /// `None` = not recorded; `Some` = observed. Kept in their own map
        /// rather than merged with `branches`: a fork of a busy upstream can
        /// hold hundreds, and under one shared cap they would evict the local
        /// branches — the data of record — to make room for refs that change
        /// rarely. See ADR 0070 for why they are recorded at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remotes: Option<CapturedRefs>,
        /// Names the *batch* this capture was taken for, when it was taken
        /// for more than one event (#485, ADR 0080). The other events of the
        /// batch carry [`Self::InBatch`] with the same id and no maps of
        /// their own, so one operation that moves N refs stores one snapshot
        /// rather than N copies of it.
        ///
        /// `None` is the ordinary single-event capture — one line, its own
        /// snapshot, anchoring nothing. Every journal line written before
        /// #485 is that, which is exactly what it meant then.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch: Option<String>,
    },
    /// The read failed, and the reason is preserved. A replayer must treat
    /// this as "no information", never as "no branches".
    CaptureFailed { reason: String },
    /// The refs *were* read for this event — once, together with the rest of
    /// the batch it was written in — and the maps live on a different line of
    /// the same journal: the one whose [`Self::Captured`] carries the same
    /// `batch` id. Resolve it with [`refs_at`].
    ///
    /// **This is a fourth answer, not a spelling of one of the other three.**
    /// It says "a capture exists and here is where"; the field being absent
    /// says "none was attempted"; `CaptureFailed` says "one was attempted and
    /// failed". Writing the referrers as `None` instead would have been the
    /// cheap change, and it would have told a replayer that N-1 of every N
    /// journal lines carry no history — a claim that is false and, being
    /// indistinguishable from the pre-#131 lines, uncorrectable later.
    ///
    /// A referrer whose anchor is not in the slice being read — the anchor
    /// aged out of the window, or its line was corrupt — resolves to `None`:
    /// *no information*, never an empty map. [`refs_at`] is what enforces
    /// that, and it is why a replayer must not match on this variant itself.
    InBatch { batch: String },
}

/// The refs as they stood at `event`, resolving a [`RefsAtEvent::InBatch`]
/// referrer against the batch anchor in `journal`.
///
/// **Every reader of [`ActivityEvent::refs`] must go through here** (#485,
/// ADR 0080). Reading the field directly was correct while every line carried
/// its own maps; it now silently sees "no maps" on the N-1 lines of an N-ref
/// batch that reference the anchor instead.
///
/// `journal` is the slice the event was read from — [`crate::activity`]'s
/// callers get it from `journal::read_all`, which returns the newest window of
/// the file in file order. The anchor is written *last* within its batch
/// precisely so that a window trimmed mid-batch keeps it, but a trim is not
/// the only way to lose one (a corrupt anchor line is skipped by the parser),
/// so an unresolvable referrer is `None` — the replayer concludes nothing,
/// which is the same reading the absent field gets.
///
/// Linear in `journal` per call, which is bounded by the read cap and only
/// paid on the referrer path. A replayer resolving every event of a full
/// window should build the batch index once instead.
pub fn refs_at<'a>(
    event: &'a ActivityEvent,
    journal: &'a [ActivityEvent],
) -> Option<&'a RefsAtEvent> {
    match event.refs.as_ref()? {
        RefsAtEvent::InBatch { batch } => {
            journal.iter().find_map(|other| match other.refs.as_ref() {
                Some(
                    anchor @ RefsAtEvent::Captured {
                        batch: Some(id), ..
                    },
                ) if id == batch => Some(anchor),
                _ => None,
            })
        }
        own => Some(own),
    }
}

/// A captured ref map plus its own truncation count, so one kind overflowing
/// can never be mistaken for another kind's completeness.
///
/// **What peeling costs, recorded so nobody rediscovers it as a bug.** Every
/// entry is peeled to a commit, because that is what the graph badges and
/// what a replay draws. In the capture a lightweight tag and an annotated tag
/// on the same commit are therefore indistinguishable, and the tag object's
/// own id is not recoverable: a replay can show *that* `v1.0` pointed at
/// commit X, never the tag's message or tagger. If a viewer ever needs that,
/// it is a follow-up that adds a field — not a reason to store unpeeled ids
/// now and make every consumer peel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedRefs {
    pub entries: BTreeMap<String, String>,
    /// `Some(total)` when there were more than [`REFS_PER_EVENT_CAP`] of this
    /// kind, carrying the true count. As with `branches`' `truncated_at`,
    /// never silently capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_at: Option<usize>,
}

/// How many refs of **each** map — branches, tags, remotes — one journal line
/// records. #131 budgeted "a few KB per operation"; at ~60 bytes per entry
/// this holds that line for any repo a person actually works in, and repos
/// past it record the overflow honestly via the matching `truncated_at`
/// rather than lying by omission.
///
/// The cap is applied per map rather than as one shared budget. A shared one
/// has to decide *which* kind loses its tail — truncation would become
/// order-dependent, whether tags were cut would depend on how many branches
/// happened to exist, and a single count could not say which kind lost
/// entries. Three independent caps make each map's honesty self-contained,
/// and branches keep exactly the guarantee they had under #131.
///
/// The price: the pathological worst case for one line is ~1500 entries,
/// roughly 90 KB, against ~25 KB before #449. A typical repository (10
/// branches, 30 tags, 20 remotes) lands near 3.5 KB. See ADR 0070.
pub const REFS_PER_EVENT_CAP: usize = 500;

/// Where HEAD pointed when an event was journaled (#449).
///
/// **Why an enum rather than a symbolic-name/oid pair.** Two independent
/// `Option`s would be the flag-pair shape ADR 0068 was written against: four
/// combinations, a reader who must remember which are possible, and a
/// renderer free to assert a fact the data never carried. An enum makes the
/// states total and named.
///
/// Every variant below was reproduced against `gix` 0.84 before it was
/// written; none is defensive padding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum HeadAtEvent {
    /// HEAD is symbolic and resolves: the ref it names, plus the commit.
    ///
    /// `symbolic` is the **full** ref name (`refs/heads/main`), never
    /// shortened — a symbolic ref names a full ref path, and a short name
    /// would collide with a same-named tag. The join back to `branches` is a
    /// lossless `strip_prefix("refs/heads/")`.
    OnBranch { symbolic: String, oid: String },
    /// Detached and resolving: a commit, and deliberately no name.
    Detached { oid: String },
    /// Symbolic, pointing at a ref that has no commit yet (a fresh repo, a
    /// new orphan branch). A name with nothing behind it — not a branch at
    /// zero.
    Unborn { symbolic: String },
    /// Neither a name nor a commit: HEAD read, and held an object id nothing
    /// resolves. Recorded rather than smoothed into one of the three above.
    Unresolvable,
    /// HEAD itself could not be read, and the reason is preserved.
    ///
    /// Distinct from [`Self::Unresolvable`], which is a HEAD that *was* read
    /// and pointed nowhere. This is reachable while the surrounding capture
    /// succeeds: a repository whose ref store opens and lists normally can
    /// still have a corrupt `.git/HEAD`, or a branch ref that will not
    /// instantiate. Recording that as "no HEAD" — or letting it fail the
    /// whole capture, discarding branches that read perfectly well — is the
    /// collapse this type exists to forbid.
    Unreadable { reason: String },
}

/// One raw reflog line, as read natively by `git-vista-git` — ref name plus
/// the entry's old/new oids, timestamp and message. Defined here (not in the
/// git crate) so [`assemble_feed`] can take them without core depending on
/// anything platform-specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    /// Short ref name: `"HEAD"`, `"main"`, `"origin/main"`.
    pub ref_name: String,
    pub time: i64,
    pub old_oid: String,
    pub new_oid: String,
    pub message: String,
}

/// The conventional 7-char short id, for labels.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Parse a reflog message into an [`ActivityKind`] and a feed-ready summary.
///
/// Reflog messages are the operation's own one-liner ("merge feature:
/// Fast-forward", "reset: moving to HEAD~1"), so most pass through as the
/// summary verbatim. Commits strip their "commit: " prefix — the commit's
/// message *is* the summary — and checkouts reduce to "from → to".
pub fn parse_reflog_message(message: &str) -> (ActivityKind, String) {
    let msg = message.trim();
    if let Some(rest) = msg.strip_prefix("commit (amend):") {
        return (ActivityKind::Amend, rest.trim().to_string());
    }
    if let Some(rest) = msg.strip_prefix("commit (initial):") {
        return (ActivityKind::Commit, rest.trim().to_string());
    }
    if let Some(rest) = msg.strip_prefix("commit:") {
        return (ActivityKind::Commit, rest.trim().to_string());
    }
    if let Some(rest) = msg.strip_prefix("checkout: moving from ") {
        // "checkout: moving from <a> to <b>" — ref names can't contain spaces,
        // so the last " to " is unambiguous.
        if let Some((from, to)) = rest.rsplit_once(" to ") {
            return (ActivityKind::Checkout, format!("{from} → {to}"));
        }
        return (ActivityKind::Checkout, rest.to_string());
    }
    if msg.starts_with("rebase") {
        // "rebase (start|pick|finish): …", plus older "rebase -i" / "rebase
        // finished" spellings. Consecutive entries coalesce in assemble_feed.
        return (ActivityKind::Rebase, msg.to_string());
    }
    if msg.starts_with("reset:") {
        return (ActivityKind::Reset, msg.to_string());
    }
    if let Some(rest) = msg.strip_prefix("cherry-pick:") {
        return (ActivityKind::CherryPick, rest.trim().to_string());
    }
    if let Some(rest) = msg.strip_prefix("revert:") {
        return (ActivityKind::Revert, rest.trim().to_string());
    }
    if msg.starts_with("branch: Created") {
        return (ActivityKind::BranchCreated, msg.to_string());
    }
    if msg.starts_with("branch: Reset") {
        // `git branch -f <name> <target>` — how the undo endpoint moves a
        // branch that isn't checked out. Classing it Reset lets the journal's
        // Reset entry absorb this reflog echo like any other app op.
        return (ActivityKind::Reset, msg.to_string());
    }
    if msg.starts_with("merge ") {
        return (ActivityKind::Merge, msg.to_string());
    }
    if msg.starts_with("pull") {
        return (ActivityKind::Pull, msg.to_string());
    }
    if msg.starts_with("clone") {
        return (ActivityKind::Clone, msg.to_string());
    }
    if msg == "update by push" || msg.starts_with("push") {
        return (ActivityKind::Push, msg.to_string());
    }
    if msg.starts_with("fetch") {
        return (ActivityKind::Fetch, msg.to_string());
    }
    (ActivityKind::Other, msg.to_string())
}

/// True for a `checkout: moving from X to X` reflog message — a same-branch
/// checkout that moved nothing. git logs it anyway (exit 0, "Already on 'X'"),
/// but a "main → main" row is noise to a reader whoever ran it, so the feed
/// skips these. Same last-` to `-split as [`parse_reflog_message`]: ref names
/// can't contain spaces.
fn is_self_checkout(message: &str) -> bool {
    message
        .trim()
        .strip_prefix("checkout: moving from ")
        .and_then(|rest| rest.rsplit_once(" to "))
        .is_some_and(|(from, to)| from == to)
}

/// How close (seconds) a reflog entry must be to a journal entry with the same
/// kind and new oid to count as *the same event*. Reflog timestamps have
/// one-second granularity and the journal stamps its own clock right after
/// git returns, so they're usually equal — 5s is generous slack.
const JOURNAL_MATCH_SLACK: i64 = 5;

/// How close a HEAD reflog entry must be to a branch entry with the same kind
/// and new oid to count as the same movement (one `git commit` logs on both).
const HEAD_MATCH_SLACK: i64 = 2;

/// The largest gap (seconds) between two consecutive `Fetch` events that still
/// counts as *one* fetch. One `git fetch` updates every stale remote-tracking
/// ref, and both sources record it per-ref: the app journals one entry per ref
/// it watched change, and git writes one reflog line per ref. The ref-update
/// phase is fast but not instantaneous, so a burst chains by gap rather than
/// sitting inside a fixed window — 94 refs spanning several seconds is still
/// one burst, while two fetches half a minute apart are two.
///
/// The cost of the heuristic, stated plainly: two *deliberate* fetches within
/// five seconds of each other read as one. That is the same trade the rebase
/// coalescing makes, and it errs toward the reading a person would give it.
const FETCH_BURST_GAP: i64 = 5;

/// Fold the journal and the raw reflogs into the final feed: parse, coalesce
/// rebases, collapse HEAD/branch duplicates, attribute app events, attach undo
/// hints, sort newest-first, cap at `limit`.
///
/// `branches` is the repo's *current* local branch → tip map and `remote` the
/// set of commit ids known to be on the remote; both feed the undo hints.
pub fn assemble_feed(
    journal: Vec<ActivityEvent>,
    reflog: Vec<ReflogEntry>,
    branches: &HashMap<String, String>,
    remote: &HashSet<String>,
    limit: usize,
) -> Vec<ActivityEvent> {
    // -- 1. Parse each reflog line, coalescing rebase bursts per ref. --------
    // A rebase writes start/one-per-pick/finish lines back to back on the same
    // ref; entries arrive newest-first per ref, so a consecutive run of Rebase
    // entries on one ref is one user action: newest new_oid ← oldest old_oid.
    let mut events: Vec<ActivityEvent> = Vec::with_capacity(reflog.len());
    let mut i = 0;
    while i < reflog.len() {
        let entry = &reflog[i];
        let (kind, summary) = parse_reflog_message(&entry.message);
        // Same-branch checkouts moved nothing — drop them as noise.
        if kind == ActivityKind::Checkout && is_self_checkout(&entry.message) {
            i += 1;
            continue;
        }
        if kind == ActivityKind::Rebase {
            let mut span = i;
            while span + 1 < reflog.len() {
                let next = &reflog[span + 1];
                if next.ref_name != entry.ref_name {
                    break;
                }
                let (next_kind, _) = parse_reflog_message(&next.message);
                if next_kind != ActivityKind::Rebase {
                    break;
                }
                span += 1;
            }
            let steps = span - i + 1;
            events.push(ActivityEvent {
                time: entry.time,
                kind: ActivityKind::Rebase,
                ref_name: Some(entry.ref_name.clone()),
                summary: if steps > 1 {
                    format!("rebase ({steps} steps)")
                } else {
                    summary
                },
                old_oid: Some(reflog[span].old_oid.clone()),
                new_oid: Some(entry.new_oid.clone()),
                source: ActivitySource::External,
                undo: None,
                // Reflog-derived: no branch-tip capture exists for it (#131).
                refs: None,
            });
            i = span + 1;
            continue;
        }
        events.push(ActivityEvent {
            time: entry.time,
            kind,
            ref_name: Some(entry.ref_name.clone()),
            summary,
            old_oid: Some(entry.old_oid.clone()),
            new_oid: Some(entry.new_oid.clone()),
            source: ActivitySource::External,
            undo: None,
            // Reflog-derived: no branch-tip capture exists for it (#131).
            refs: None,
        });
        i += 1;
    }

    // -- 2. Collapse the HEAD copy of a branch movement. ---------------------
    // One `git commit`/`merge`/`reset` on a checked-out branch logs on both
    // HEAD and the branch; the branch-named copy is the informative one. HEAD-
    // only kinds (checkout, clone) survive — no branch copy exists to collide.
    let branch_moves: Vec<(ActivityKind, String, i64)> = events
        .iter()
        .filter(|e| e.ref_name.as_deref() != Some("HEAD"))
        .filter_map(|e| e.new_oid.clone().map(|oid| (e.kind, oid, e.time)))
        .collect();
    events.retain(|e| {
        if e.ref_name.as_deref() != Some("HEAD") {
            return true;
        }
        let Some(new_oid) = &e.new_oid else {
            return true;
        };
        !branch_moves.iter().any(|(kind, oid, time)| {
            *kind == e.kind && oid == new_oid && (e.time - time).abs() <= HEAD_MATCH_SLACK
        })
    });

    // -- 3. Attribute app events: a reflog entry matching a journal entry ----
    // (same kind, same resulting oid, near-same moment) *is* that journal
    // entry — keep the journal copy, which knows the source and has the
    // richer summary.
    events.retain(|e| {
        let Some(new_oid) = &e.new_oid else {
            return true;
        };
        !journal.iter().any(|j| {
            j.kind == e.kind
                && j.new_oid.as_deref() == Some(new_oid)
                && (e.time - j.time).abs() <= JOURNAL_MATCH_SLACK
        })
    });

    // Cloned rather than moved: the journal window is needed again in step 7,
    // to resolve the batched captures (#485). Every event copied here is a
    // small one — the ref maps live on one line per batch, and that one is
    // cloned once.
    events.extend(journal.iter().cloned());

    // -- 4. Fold a burst of remote-tracking ref updates into one row. --------
    // One `git fetch` is one user action with one outcome they care about
    // ("N refs updated"), but it lands one entry per updated ref in *both*
    // sources. #329: a fetch of 94 refs put 94 rows in the feed and buried the
    // revert the user was actually looking for. A pull floods the same way,
    // with one row that must survive — see [`fold_ref_update_bursts`].
    let mut events = fold_ref_update_bursts(events, branches);

    // -- 5. Undo hints, computed against the repo's *current* state. ---------
    for event in &mut events {
        event.undo = undo_hint(event, branches, remote);
    }

    // -- 6. Newest first, capped. sort_by_key is stable, so same-second -------
    // events keep their source order (reflog order within a ref).
    events.sort_by_key(|e| std::cmp::Reverse(e.time));
    events.truncate(limit);

    // -- 7. Resolve batched ref captures, so the feed is self-contained. -----
    // A journaled event may carry [`RefsAtEvent::InBatch`]: its maps were read
    // and stored once, on another line of the same journal (#485, ADR 0080).
    // Whoever receives this feed has no journal to resolve that against, so
    // **nothing leaves here as a reference** — an event carries its maps, its
    // failure, or nothing at all, which are the three answers that have always
    // been on the wire.
    //
    // After the truncation on purpose: a batch's one snapshot is copied only
    // onto rows actually being sent, which is at most `limit` of them.
    // Resolving before step 4 would copy it onto every line of the batch —
    // reinstating, in memory and on every feed read, exactly the duplication
    // #485 took out of the file.
    for event in &mut events {
        if matches!(event.refs, Some(RefsAtEvent::InBatch { .. })) {
            // `None` when the anchor is not in this window: no information,
            // the same reading an absent field gets. Never an empty map.
            event.refs = refs_at(event, &journal).cloned();
        }
    }
    events
}

/// True when this event's ref is a *local branch* rather than a
/// remote-tracking ref — the distinction [`fold_ref_update_bursts`] turns on.
///
/// The two sources spell refs differently: the journal writes them in full
/// (`refs/heads/main`, `refs/remotes/origin/main`) and reflog entries carry the
/// short name (`main`, `origin/main`). A short name is ambiguous on its face —
/// a local branch may legitimately be called `origin/main` — so it is resolved
/// against the repo's *actual* branch list rather than by guessing at the
/// slash.
fn names_a_local_branch(ref_name: Option<&str>, branches: &HashMap<String, String>) -> bool {
    let Some(name) = ref_name else {
        return false;
    };
    // A full ref path answers for itself, whether or not the branch still
    // exists; only the short form needs the branch list to disambiguate.
    if name.starts_with("refs/heads/") {
        return true;
    }
    if name.starts_with("refs/remotes/") {
        return false;
    }
    branches.contains_key(name)
}

/// True for the foldable-kind events that must never be folded into a count:
/// the admissions `planner::fetch::journal_unobserved` and
/// `planner::push::journal_unobserved` write when the git command itself
/// succeeded and only the re-read of `refs/remotes/<remote>/*` failed.
///
/// **The discriminator is the whole shape, not any one field** (ADR 0081). No
/// ref name *and* no old oid *and* no new oid is what those two writers
/// produce and nothing else this fold looks at does:
///
/// - Reflog-derived events are built in [`assemble_feed`]'s step 1 from a
///   [`ReflogEntry`], which carries a ref name and both oids by construction —
///   every one of them fails all three tests.
/// - Journalled fetches come from `planner::fetch::journal_updates`, one per
///   ref that moved, each naming its ref. `Obs::Absent` does flatten to `None`
///   the same way `Obs::Unknown` does, so an oid pair says nothing on its own —
///   the `ref_name` is what separates those entries from this one.
/// - Journalled pulls come from `planner::branch_exec`, which names the branch
///   the pull landed on.
/// - `planner::worktree_exec`'s two all-`None` admissions carry
///   `ActivityKind::Other`, a kind this fold never looks at.
/// - `planner::push::journal_unobserved` is all-`None` too, and since #487 made
///   `ActivityKind::Push` a fold candidate this test is what keeps it out —
///   for precisely the reason it keeps fetch's out. The push succeeded, so git
///   logged every ref it moved; the admission carries no `new_oid`, so it
///   suppresses none of those reflog lines in attribution and would fold in
///   with them, counting itself as a ref and deleting the one row that says
///   what reached the remote is unknown. For a push that row matters more than
///   for a fetch: what may have changed is not on this machine, and no later
///   local read will reveal it.
///
/// Keying on `ref_name` alone would be wider than that: it would also exclude
/// any future foldable-kind event that names no ref but does know an oid, and
/// such an event knows what it moved and belongs in the count.
fn admits_it_could_not_read_the_refs(event: &ActivityEvent) -> bool {
    event.ref_name.is_none() && event.old_oid.is_none() && event.new_oid.is_none()
}

/// Collapse each run of remote-tracking ref updates — [`ActivityKind::Fetch`],
/// [`ActivityKind::Pull`] and [`ActivityKind::Push`] — that happened within
/// [`FETCH_BURST_GAP`] of one another into a single counted row.
///
/// Deliberately here rather than at the write path: a fetch run from the
/// terminal has reflog lines and no journal entry at all, so an operation id
/// stamped by the app could never group it. Folding both sources in the pure
/// core covers the app's fetches and everyone else's with one rule.
///
/// **A pull's own branch movement is never folded.** Probed against real git:
/// one `git pull` writes `pull: Fast-forward` on the local branch *and*
/// `pull: fast-forward` on every updated remote-tracking ref. Those parse to
/// the same kind, but they do not mean the same thing — the branch move is the
/// entire point of a pull and the remote-ref updates are its bookkeeping. Only
/// the bookkeeping folds. (Git's own hint here is the capital letter on
/// `Fast-forward`; keying on that would be far too fragile, so the local branch
/// list decides — see [`names_a_local_branch`].)
///
/// **A push's local-branch movement is never folded either (#487).** Push
/// carries the same asymmetry, and it is not hypothetical. Probed against real
/// git on 2026-08-26 (`git push` between two local repositories, reflogs read
/// on both sides): the *pushing* repository logs `update by push` on
/// `refs/remotes/<remote>/<branch>`, and the *receiving* repository logs plain
/// `push` on `refs/heads/<branch>`. Both parse to [`ActivityKind::Push`] — see
/// [`parse_reflog_message`], which matches `update by push` and any message
/// starting `push`. Only the first is bookkeeping. A repository that others
/// push into watches its own branches move under that second message, and
/// those rows *are* the event, exactly as a pull's branch move is. So the same
/// [`names_a_local_branch`] exemption covers both, for the same reason.
///
/// **Why the gate opened before the flood (#487).** The push endpoint pushes
/// one named branch, so N = 1 and no burst can reach here from production
/// today; an unfolded Push row is what a user sees now, and it is the right
/// row. The gate is opened ahead of the multi-ref push path — `--all`, a
/// matching refspec, tags pushed alongside — so the day it lands it does not
/// reproduce #329 under a different kind. The fold cannot know what the writer
/// is currently able to emit, so the pins for it build the multi-ref burst
/// directly; see `a_multi_ref_push_folds_into_one_counted_row`.
///
/// A run of one is returned untouched — a single-ref fetch already says the
/// useful thing ("fetched ‘origin/main’ from origin") and rewriting it as
/// "1 ref updated" would lose information to no purpose.
///
/// # The "tips unknown — git could not be read" admission never folds
///
/// `planner::fetch::journal_unobserved` writes one entry with no `ref_name`
/// and no oids when `git fetch` *succeeded* and only the re-read of
/// `refs/remotes/<remote>/*` failed. It used to fold in with the refs it could
/// not name, and this comment used to argue that it could not: the entry "is
/// journaled *instead of* per-ref entries, never alongside them, so it is
/// always a run of one". That accounts for the journal and forgets git's
/// reflog — the same shape of mistake that got the first #329 attempt
/// reverted. The fetch succeeded, so git logged every ref it moved; the
/// admission carries no `new_oid`, so it suppresses none of those lines in
/// attribution and folded in with them instead. Measured 2026-08-25: four refs
/// moved rendered as "fetch — 5 refs updated", the admission gone and the
/// count one too high because the admission was counted as a ref.
///
/// It is excluded from the fold outright — see [`admits_it_could_not_read_the_refs`]
/// for the shape and why that shape is not shared (ADR 0081). The refs that
/// really moved still fold, from their reflog lines, into a row of their own;
/// the admission sits beside them saying that the app could not confirm any of
/// it. Two rows, both true, rather than one confident wrong number.
///
/// # The count that used to inflate at scale — fixed at the writer (#485)
///
/// Both defects this function was found to have were measured on 2026-08-25
/// while verifying #329's fix, and recorded with their evidence in
/// `docs/investigations/2026-08-25-issue-329-fetch-feed-volume.md`. The
/// admission above is one. This is the other. **Neither is outstanding** —
/// they were fixed by #486 and #485 respectively, hours apart, and this
/// paragraph is kept because the reasoning is worth more than the defect was.
///
/// A journal entry that lands more than [`JOURNAL_MATCH_SLACK`] after git's
/// reflog line for the same movement stops suppressing it; the unmatched
/// reflog line survives attribution and the fold counts both copies. Measured
/// against the app's fetch as it then was — one entry per ref, each performing
/// a full ref capture and taking its own timestamp afterwards — 250 refs
/// reported as 297 and 500 as 891. The feed stayed at one row, so #329's
/// symptom held, but the number in it was not true.
///
/// **Fixed at the writer (#485), not in the fold — and only the relative
/// half of it.** A fetch now takes one ref capture and one timestamp for the
/// whole batch (`handlers::journal_app_events`), so its entries can no
/// longer drift *apart from each other*: whatever that one moment turns out
/// to be, every entry of the batch shares it, and pull journals through the
/// same path. That bounds drift **within** a batch to zero; it says nothing
/// about the **absolute** gap between git's reflog line and that one shared
/// moment. The timestamp is sampled only after the post-fetch re-read of
/// every remote-tracking ref (`planner::fetch::run_fetch` calling
/// `planner::transfer::remote_tracking_refs`) — one `git for-each-ref` whose
/// cost plausibly scales with the size of the ref namespace but **has never
/// been measured**. A large enough namespace or slow enough
/// storage could in principle push that re-read, and so the whole batch's
/// shared moment, past [`JOURNAL_MATCH_SLACK`] of the reflog lines it needs
/// to match. Batching would not save it: every entry in the batch would miss
/// together instead of the tail of them missing one by one, still 2N rather
/// than N. Whether that gap is ever actually crossed has not been measured
/// (#522) — this paragraph states what #485 rules out, not that the window
/// can't be exceeded. The F1 pin below is unaffected either way: it pins
/// drift *between entries of one batch*, which #485 did fix, not drift
/// between the batch and the reflog, which it did not touch.
///
/// **Push does not inherit even that relative half — reported, not fixed
/// here (#487).** `planner::push::journal_updates` loops over the refs that
/// moved calling `journal_app_event`, the *singular*, and that one delegates
/// to `journal_app_events` with a batch of one. So each ref takes its own
/// `now_secs()` reading and its own `journal::append_all` ref capture:
/// exactly the writer shape #485 removed from fetch, still present in push —
/// and it is drift *between entries of one operation*, the half batching does
/// rule out for fetch. It cannot bite today, because that loop runs over the
/// single ref a one-branch push moves and one entry cannot drift from itself;
/// it would arrive **with** the multi-ref push path rather than after it.
/// Batching push's writer is a change to `planner::push` with its own
/// before/after measurement, not a rider on this gate. Nothing pins push's
/// drift here because push cannot yet produce it; what is pinned is that the
/// fold counts a push burst correctly when the entries do *not* drift.
///
/// **Safe for undo by construction, not by luck:** [`undo_hint`] has no arm for
/// `Fetch`, `Pull` or `Push`, so none of those rows has ever carried a hint and
/// dropping the per-ref oids cannot take one away. The same fold would be
/// *wrong* for, say, `BranchDeleted`, whose `old_oid` is precisely what its
/// undo needs.
fn fold_ref_update_bursts(
    events: Vec<ActivityEvent>,
    branches: &HashMap<String, String>,
) -> Vec<ActivityEvent> {
    let (mut candidates, mut out): (Vec<_>, Vec<_>) = events.into_iter().partition(|e| {
        FOLDABLE_KINDS.iter().any(|(kind, _)| *kind == e.kind)
            && !names_a_local_branch(e.ref_name.as_deref(), branches)
            && !admits_it_could_not_read_the_refs(e)
    });

    // Each kind groups separately: they are different actions, and a fetch
    // immediately followed by a pull is two of them, not one run of six refs.
    for (kind, noun) in FOLDABLE_KINDS {
        let (group, rest): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|e| e.kind == kind);
        candidates = rest;
        fold_one_kind(&mut out, group, noun);
    }
    // Unreachable while [`FOLDABLE_KINDS`] is also the gate — and kept anyway,
    // because a kind admitted above and not grouped here would otherwise be
    // *deleted* from the feed, which is a far worse way to learn about it than
    // an unfolded row.
    out.extend(candidates);
    out
}

/// The kinds this fold collapses, each with the noun its counted row uses
/// ("fetch — 94 refs updated").
///
/// One table, read twice — once as the gate in [`fold_ref_update_bursts`] and
/// once as the grouping — so the two cannot drift apart. Adding a kind is
/// this list and nothing else; #487 added `Push` to it.
const FOLDABLE_KINDS: [(ActivityKind, &str); 3] = [
    (ActivityKind::Fetch, "fetch"),
    (ActivityKind::Pull, "pull"),
    (ActivityKind::Push, "push"),
];

/// Fold one kind's bursts into `out`. `noun` names the action in the counted
/// summary ("fetch — 94 refs updated").
fn fold_one_kind(out: &mut Vec<ActivityEvent>, mut group: Vec<ActivityEvent>, noun: &str) {
    // Group by time, independent of where these sat among other events.
    group.sort_by_key(|e| std::cmp::Reverse(e.time));

    let mut rest = group.into_iter().peekable();
    while let Some(first) = rest.next() {
        let mut refs = 1usize;
        let mut previous = first.time;
        let mut by_app = first.source == ActivitySource::App;
        while let Some(next) = rest.peek() {
            // Descending, so this is a non-negative gap to the older entry.
            if previous - next.time > FETCH_BURST_GAP {
                break;
            }
            previous = next.time;
            by_app |= next.source == ActivitySource::App;
            refs += 1;
            rest.next();
        }
        if refs == 1 {
            out.push(first);
            continue;
        }
        out.push(ActivityEvent {
            time: first.time,
            kind: first.kind,
            // No single ref: the row is about the action, not about any one of
            // the refs it moved. The per-ref detail stays in the reflog, which
            // is where a reader who wants it can still see it.
            ref_name: None,
            summary: format!("{noun} — {refs} refs updated"),
            // The tips are per-ref and there were many; asserting either here
            // would be inventing one.
            old_oid: None,
            new_oid: None,
            // Attributed to the app if the app performed any part of it — a
            // burst is one action, and the app either ran it or it did not.
            source: if by_app {
                ActivitySource::App
            } else {
                ActivitySource::External
            },
            undo: None,
            // Reflog-derived: no branch-tip capture exists for it (#131).
            refs: None,
        });
    }
}

/// The undo hint for one event, if it's still undoable *now*:
///
///  * a deleted branch whose name is currently free → restore it at its old
///    tip (works for terminal deletions too, via the snapshot-synthesized
///    journal event that carries the last known tip);
///  * a merge/rebase/commit/amend/reset whose result is **still the branch's
///    tip** → reset the branch back to the pre-event oid. Only the newest
///    event on a branch can qualify (older events' `new_oid` no longer equals
///    the tip), which is exactly the "undo the last thing" semantics wanted.
fn undo_hint(
    event: &ActivityEvent,
    branches: &HashMap<String, String>,
    remote: &HashSet<String>,
) -> Option<Undoable> {
    let ref_name = event.ref_name.as_deref()?;
    match event.kind {
        ActivityKind::BranchDeleted => {
            let tip = event.old_oid.as_deref()?;
            if branches.contains_key(ref_name) {
                return None; // name in use again — nothing to restore onto
            }
            Some(Undoable {
                action: UndoAction::RestoreBranch {
                    name: ref_name.to_string(),
                    tip: tip.to_string(),
                },
                label: format!("Restore branch ‘{ref_name}’ at {}", short(tip)),
                warn_pushed: false,
            })
        }
        ActivityKind::Merge
        | ActivityKind::Rebase
        | ActivityKind::Commit
        | ActivityKind::Amend
        | ActivityKind::Reset => {
            if ref_name == "HEAD" {
                return None; // only a named branch can be reset safely
            }
            let (old, new) = (event.old_oid.as_deref()?, event.new_oid.as_deref()?);
            if branches.get(ref_name).map(String::as_str) != Some(new) {
                return None; // the branch has moved on — this isn't its tip
            }
            // A creation-like entry (old oid all zeros) can't be "reset back".
            if old.bytes().all(|b| b == b'0') {
                return None;
            }
            let verb = match event.kind {
                ActivityKind::Merge => "merge",
                ActivityKind::Rebase => "rebase",
                ActivityKind::Amend => "amend",
                ActivityKind::Reset => "reset",
                _ => "commit",
            };
            Some(Undoable {
                action: UndoAction::ResetBranch {
                    branch: ref_name.to_string(),
                    to: old.to_string(),
                    expected_tip: new.to_string(),
                },
                label: format!("Undo {verb} — reset ‘{ref_name}’ to {}", short(old)),
                // The state being discarded is public: the remote will still
                // have it after a local reset, because no undo force-pushes.
                // (Since M2.20e (#231) a *push* can force-publish under a
                // lease; that is an explicit, separately-approved operation and
                // never something an undo reaches for. See `Undoable`'s doc.)
                warn_pushed: remote.contains(new),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ref_name: &str, time: i64, old: &str, new: &str, message: &str) -> ReflogEntry {
        ReflogEntry {
            ref_name: ref_name.to_string(),
            time,
            old_oid: old.to_string(),
            new_oid: new.to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn messages_parse_to_kinds() {
        let cases = [
            ("commit: fix the bug", ActivityKind::Commit, "fix the bug"),
            ("commit (initial): root", ActivityKind::Commit, "root"),
            ("commit (amend): better", ActivityKind::Amend, "better"),
            (
                "checkout: moving from main to feature",
                ActivityKind::Checkout,
                "main → feature",
            ),
            (
                "merge feature: Fast-forward",
                ActivityKind::Merge,
                "merge feature: Fast-forward",
            ),
            (
                "rebase (finish): returning to refs/heads/f",
                ActivityKind::Rebase,
                "rebase (finish): returning to refs/heads/f",
            ),
            (
                "reset: moving to HEAD~1",
                ActivityKind::Reset,
                "reset: moving to HEAD~1",
            ),
            (
                "branch: Created from main",
                ActivityKind::BranchCreated,
                "branch: Created from main",
            ),
            (
                "branch: Reset to abc1234",
                ActivityKind::Reset,
                "branch: Reset to abc1234",
            ),
            ("cherry-pick: pick me", ActivityKind::CherryPick, "pick me"),
            (
                "revert: Revert \"oops\"",
                ActivityKind::Revert,
                "Revert \"oops\"",
            ),
            (
                "pull: Fast-forward",
                ActivityKind::Pull,
                "pull: Fast-forward",
            ),
            (
                "clone: from https://example.com/r.git",
                ActivityKind::Clone,
                "clone: from https://example.com/r.git",
            ),
            ("update by push", ActivityKind::Push, "update by push"),
            (
                "fetch: fast-forward",
                ActivityKind::Fetch,
                "fetch: fast-forward",
            ),
            (
                "frobnicate: unknown",
                ActivityKind::Other,
                "frobnicate: unknown",
            ),
        ];
        for (msg, kind, summary) in cases {
            let (k, s) = parse_reflog_message(msg);
            assert_eq!(k, kind, "kind of {msg:?}");
            assert_eq!(s, summary, "summary of {msg:?}");
        }
    }

    #[test]
    fn rebase_burst_coalesces_to_one_event() {
        // Newest-first on one ref: finish, two picks, start — one rebase.
        let reflog = vec![
            entry(
                "feature",
                100,
                "c3",
                "c4",
                "rebase (finish): returning to refs/heads/feature",
            ),
            entry("feature", 100, "c2", "c3", "rebase (pick): two"),
            entry("feature", 99, "c1", "c2", "rebase (pick): one"),
            entry("feature", 99, "c0", "c1", "rebase (start): checkout main"),
            entry("feature", 50, "c9", "c0", "commit: before"),
        ];
        let feed = assemble_feed(vec![], reflog, &HashMap::new(), &HashSet::new(), 10);
        let rebases: Vec<_> = feed
            .iter()
            .filter(|e| e.kind == ActivityKind::Rebase)
            .collect();
        assert_eq!(rebases.len(), 1, "one coalesced rebase, got {feed:#?}");
        assert_eq!(
            rebases[0].old_oid.as_deref(),
            Some("c0"),
            "pre-rebase state"
        );
        assert_eq!(rebases[0].new_oid.as_deref(), Some("c4"), "post-rebase tip");
        assert_eq!(rebases[0].summary, "rebase (4 steps)");
        // The plain commit below the burst survives untouched.
        assert!(feed.iter().any(|e| e.kind == ActivityKind::Commit));
    }

    #[test]
    fn head_copy_of_a_branch_move_is_dropped() {
        let reflog = vec![
            entry("HEAD", 100, "a", "b", "commit: same change"),
            entry("main", 100, "a", "b", "commit: same change"),
            entry("HEAD", 90, "x", "a", "checkout: moving from f to main"),
        ];
        let feed = assemble_feed(vec![], reflog, &HashMap::new(), &HashSet::new(), 10);
        let commits: Vec<_> = feed
            .iter()
            .filter(|e| e.kind == ActivityKind::Commit)
            .collect();
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0].ref_name.as_deref(),
            Some("main"),
            "branch copy wins"
        );
        // The checkout (HEAD-only) survives.
        assert!(feed.iter().any(|e| e.kind == ActivityKind::Checkout));
    }

    #[test]
    fn a_same_branch_checkout_is_dropped_as_noise() {
        // `git checkout main` while on main: exit 0, "Already on 'main'", yet
        // git still logs "moving from main to main" — nothing moved, so the
        // feed drops it. A real switch right next to it survives.
        let reflog = vec![
            entry("HEAD", 100, "b", "b", "checkout: moving from main to main"),
            entry(
                "HEAD",
                90,
                "a",
                "b",
                "checkout: moving from feature to main",
            ),
        ];
        let feed = assemble_feed(vec![], reflog, &HashMap::new(), &HashSet::new(), 10);
        let checkouts: Vec<_> = feed
            .iter()
            .filter(|e| e.kind == ActivityKind::Checkout)
            .collect();
        assert_eq!(checkouts.len(), 1, "self-checkout dropped: {feed:#?}");
        assert_eq!(checkouts[0].summary, "feature → main");
    }

    #[test]
    fn journal_entry_absorbs_its_reflog_echo() {
        let journal = vec![ActivityEvent {
            time: 101, // journal stamps right after git returns — 1s off
            kind: ActivityKind::Merge,
            ref_name: Some("main".into()),
            summary: "merged ‘feature’ into ‘main’".into(),
            old_oid: Some("a".into()),
            new_oid: Some("m".into()),
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];
        let reflog = vec![
            entry("main", 100, "a", "m", "merge feature: Merge made by 'ort'"),
            entry("HEAD", 100, "a", "m", "merge feature: Merge made by 'ort'"),
        ];
        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 10);
        assert_eq!(feed.len(), 1, "one event for one merge: {feed:#?}");
        assert_eq!(feed[0].source, ActivitySource::App);
        assert_eq!(feed[0].summary, "merged ‘feature’ into ‘main’");
    }

    #[test]
    fn an_undo_reset_journal_absorbs_its_branch_reset_echo() {
        // The undo endpoint moves a non-checked-out branch with `git branch
        // -f`, whose reflog echo is "branch: Reset to <target>" — same Reset
        // kind as the journal entry, so the App-attributed copy wins.
        let journal = vec![ActivityEvent {
            time: 100,
            kind: ActivityKind::Reset,
            ref_name: Some("feature".into()),
            summary: "reset ‘feature’ to abc1234".into(),
            old_oid: Some("m".into()),
            new_oid: Some("a".into()),
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];
        let reflog = vec![entry("feature", 100, "m", "a", "branch: Reset to a")];
        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 10);
        assert_eq!(feed.len(), 1, "one event for one undo: {feed:#?}");
        assert_eq!(feed[0].source, ActivitySource::App);
    }

    #[test]
    fn an_unrelated_commit_next_to_a_push_is_not_absorbed() {
        // Same oid, near-same time, different kind: the journal Push must not
        // swallow the branch's Commit entry.
        let journal = vec![ActivityEvent {
            time: 100,
            kind: ActivityKind::Push,
            ref_name: Some("main".into()),
            summary: "pushed ‘main’ to origin".into(),
            old_oid: None,
            new_oid: Some("x".into()),
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];
        let reflog = vec![entry("main", 99, "w", "x", "commit: quick fix")];
        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 10);
        assert_eq!(feed.len(), 2, "commit and push both present: {feed:#?}");
    }

    #[test]
    fn deleted_branch_gets_a_restore_hint_until_recreated() {
        let deleted = ActivityEvent {
            time: 100,
            kind: ActivityKind::BranchDeleted,
            ref_name: Some("old-work".into()),
            summary: "deleted branch ‘old-work’".into(),
            old_oid: Some("abc1234567".into()),
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: None,
        };
        // Branch absent → restorable.
        let feed = assemble_feed(
            vec![deleted.clone()],
            vec![],
            &HashMap::new(),
            &HashSet::new(),
            10,
        );
        let undo = feed[0].undo.as_ref().expect("restore hint");
        assert_eq!(
            undo.action,
            UndoAction::RestoreBranch {
                name: "old-work".into(),
                tip: "abc1234567".into()
            }
        );
        assert!(undo.label.contains("Restore branch ‘old-work’"));

        // Branch name back in use → no hint.
        let branches = HashMap::from([("old-work".to_string(), "zzz".to_string())]);
        let feed = assemble_feed(vec![deleted], vec![], &branches, &HashSet::new(), 10);
        assert!(feed[0].undo.is_none());
    }

    #[test]
    fn merge_at_tip_gets_reset_hint_with_cas_and_push_warning() {
        let reflog = vec![
            entry("main", 100, "a", "m", "merge feature: Merge made by 'ort'"),
            entry("main", 90, "z", "a", "commit: earlier"),
        ];
        let branches = HashMap::from([("main".to_string(), "m".to_string())]);
        let remote = HashSet::from(["m".to_string()]);
        let feed = assemble_feed(vec![], reflog, &branches, &remote, 10);
        let merge = feed.iter().find(|e| e.kind == ActivityKind::Merge).unwrap();
        let undo = merge.undo.as_ref().expect("reset hint");
        assert_eq!(
            undo.action,
            UndoAction::ResetBranch {
                branch: "main".into(),
                to: "a".into(),
                expected_tip: "m".into()
            }
        );
        assert!(undo.warn_pushed, "discarded tip is on the remote");
        // The older commit is no longer the tip: no hint on it.
        let older = feed
            .iter()
            .find(|e| e.kind == ActivityKind::Commit)
            .unwrap();
        assert!(older.undo.is_none());
    }

    #[test]
    fn moved_on_branch_gets_no_reset_hint() {
        let reflog = vec![entry("main", 100, "a", "m", "merge feature: fast-forward")];
        // Tip is already past `m`: the merge is buried, not undoable by reset.
        let branches = HashMap::from([("main".to_string(), "newer".to_string())]);
        let feed = assemble_feed(vec![], reflog, &branches, &HashSet::new(), 10);
        assert!(feed[0].undo.is_none());
    }

    #[test]
    fn feed_sorts_newest_first_and_caps() {
        let reflog = vec![
            entry("main", 10, "a", "b", "commit: one"),
            entry("main", 30, "c", "d", "commit: three"),
            entry("main", 20, "b", "c", "commit: two"),
        ];
        let feed = assemble_feed(vec![], reflog, &HashMap::new(), &HashSet::new(), 2);
        assert_eq!(feed.len(), 2);
        assert_eq!(feed[0].summary, "three");
        assert_eq!(feed[1].summary, "two");
    }

    /// Build the exact shape one `git fetch` leaves behind: the app journals
    /// one entry per ref it watched change, and git writes one reflog line per
    /// remote-tracking ref, at the same moment with the same resulting oid.
    fn fetch_of(refs: &[&str], time: i64) -> (Vec<ActivityEvent>, Vec<ReflogEntry>) {
        let journal = refs
            .iter()
            .map(|r| ActivityEvent {
                time,
                kind: ActivityKind::Fetch,
                ref_name: Some(format!("refs/remotes/origin/{r}")),
                summary: format!("fetched ‘origin/{r}’ from origin"),
                old_oid: Some(format!("old-{r}")),
                new_oid: Some(format!("new-{r}")),
                source: ActivitySource::App,
                undo: None,
                refs: None,
            })
            .collect();
        let reflog = refs
            .iter()
            .map(|r| {
                entry(
                    &format!("origin/{r}"),
                    time,
                    &format!("old-{r}"),
                    &format!("new-{r}"),
                    "fetch origin: fast-forward",
                )
            })
            .collect();
        (journal, reflog)
    }

    #[test]
    fn one_fetch_is_one_row_however_many_refs_moved() {
        // #329: a fetch that updated 94 remote-tracking refs put 94 rows in the
        // feed and buried the one revert the user actually cared about. One
        // user action, one outcome they care about ("N refs updated"), one row.
        let (journal, reflog) = fetch_of(&["main", "dev", "topic", "release"], 100);
        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);

        let fetches: Vec<_> = feed
            .iter()
            .filter(|e| e.kind == ActivityKind::Fetch)
            .collect();
        assert_eq!(fetches.len(), 1, "one fetch is one row, got {feed:#?}");
        assert_eq!(fetches[0].summary, "fetch — 4 refs updated");
        assert_eq!(
            fetches[0].source,
            ActivitySource::App,
            "the app performed it, so the row is attributed to the app"
        );
        assert_eq!(
            fetches[0].ref_name, None,
            "a multi-ref row is about no single ref"
        );
        assert_eq!(feed.len(), 1, "nothing else invented: {feed:#?}");
    }

    #[test]
    fn a_fetch_that_moved_one_ref_keeps_its_own_words() {
        // The common case must not be flattened into the counted phrasing: a
        // single-ref fetch already says the useful thing.
        let (journal, reflog) = fetch_of(&["main"], 100);
        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 1, "one fetch, one row: {feed:#?}");
        assert_eq!(feed[0].summary, "fetched ‘origin/main’ from origin");
        assert_eq!(
            feed[0].ref_name.as_deref(),
            Some("refs/remotes/origin/main"),
            "one ref moved, so the row still names it"
        );
    }

    #[test]
    fn two_separate_fetches_stay_two_rows() {
        // Folding groups a burst, not the kind. Two fetches minutes apart are
        // two user actions and must not collapse into each other.
        let (mut journal, mut reflog) = fetch_of(&["main", "dev"], 100);
        let (j2, r2) = fetch_of(&["topic", "release"], 400);
        journal.extend(j2);
        reflog.extend(r2);

        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);
        let fetches: Vec<_> = feed
            .iter()
            .filter(|e| e.kind == ActivityKind::Fetch)
            .collect();
        assert_eq!(fetches.len(), 2, "two actions, two rows: {feed:#?}");
        assert_eq!(fetches[0].time, 400, "newest first");
        assert_eq!(fetches[1].time, 100);
    }

    #[test]
    fn a_fetch_burst_does_not_swallow_the_events_around_it() {
        // The whole point of #329: the flood buried an undoable revert. The
        // fold must leave every non-fetch event of that moment untouched.
        let (mut journal, reflog) = fetch_of(&["main", "dev", "topic"], 100);
        journal.push(ActivityEvent {
            time: 100,
            kind: ActivityKind::Revert,
            ref_name: Some("main".into()),
            summary: "reverted ‘bad commit’".into(),
            old_oid: Some("a".into()),
            new_oid: Some("b".into()),
            source: ActivitySource::App,
            undo: None,
            refs: None,
        });

        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);
        assert_eq!(feed.len(), 2, "one fetch row + the revert: {feed:#?}");
        assert!(feed
            .iter()
            .any(|e| e.kind == ActivityKind::Revert && e.summary == "reverted ‘bad commit’"));
        assert!(feed
            .iter()
            .any(|e| e.kind == ActivityKind::Fetch && e.summary == "fetch — 3 refs updated"));
    }

    #[test]
    fn a_pull_folds_its_ref_updates_but_keeps_the_branch_move() {
        // #329 asked whether Pull has the same shape. Probed against real git:
        // one `git pull` writes "pull: Fast-forward" on the local branch AND
        // "pull: fast-forward" on every updated remote-tracking ref — same
        // flood, but with one row that must survive. The branch move is the
        // whole point of a pull; only the remote-ref updates are noise.
        let reflog = vec![
            entry("main", 100, "a", "b", "pull: Fast-forward"),
            entry("origin/main", 100, "a", "b", "pull: fast-forward"),
            entry("origin/feat-a", 100, "p", "q", "pull: fast-forward"),
            entry("origin/feat-b", 100, "r", "s", "pull: fast-forward"),
            entry("origin/feat-c", 100, "t", "u", "pull: fast-forward"),
        ];
        let branches = HashMap::from([("main".to_string(), "b".to_string())]);
        let feed = assemble_feed(vec![], reflog, &branches, &HashSet::new(), 50);

        assert_eq!(feed.len(), 2, "branch move + one counted row: {feed:#?}");
        let branch_move = feed
            .iter()
            .find(|e| e.ref_name.as_deref() == Some("main"))
            .expect("the branch move survives the fold");
        assert_eq!(branch_move.summary, "pull: Fast-forward");
        assert_eq!(branch_move.new_oid.as_deref(), Some("b"));
        assert!(
            feed.iter()
                .any(|e| e.summary == "pull — 4 refs updated" && e.ref_name.is_none()),
            "the four remote-tracking updates fold: {feed:#?}"
        );
    }

    /// #487: push journals and reflogs one entry per remote-tracking ref it
    /// moved, structurally identical to fetch — so a push that moves four refs
    /// must render as one counted row, not four.
    ///
    /// **The fixture builds a burst production cannot emit yet, deliberately.**
    /// The push endpoint pushes one named branch, so N = 1 today and no real
    /// push reaches this shape. The fold has no way to know that, and the case
    /// it exists to handle is the multi-ref one: `git push --all`, a matching
    /// refspec, or tags pushed alongside all produce exactly these reflog
    /// lines. #487 was filed to open the gate *before* that path lands rather
    /// than after it floods the feed the way #329's fetch did. Asserting
    /// against the old behaviour would have been asserting against a
    /// hypothetical; asserting against this one is not — the fold is real code
    /// with a real gate, and this is what it now does.
    #[test]
    fn a_multi_ref_push_folds_into_one_counted_row() {
        // "update by push" is what git writes on the pusher's remote-tracking
        // refs — probed against real git, both sides, 2026-08-26.
        let reflog = vec![
            entry("origin/main", 100, "a", "b", "update by push"),
            entry("origin/feat-a", 100, "p", "q", "update by push"),
            entry("origin/feat-b", 100, "r", "s", "update by push"),
            entry("origin/feat-c", 100, "t", "u", "update by push"),
        ];
        let feed = assemble_feed(vec![], reflog, &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 1, "one push is one row: {feed:#?}");
        assert_eq!(
            feed[0].summary, "push — 4 refs updated",
            "the four remote-tracking updates fold into one counted row: {feed:#?}"
        );
        assert_eq!(feed[0].kind, ActivityKind::Push);
        assert!(
            feed[0].ref_name.is_none(),
            "a counted row names no single ref: {feed:#?}"
        );
    }

    /// The other half of #487, and the reason Push could not simply be added to
    /// the gate: a push row naming a *local branch* must survive the fold, the
    /// same asymmetry `a_pull_folds_its_ref_updates_but_keeps_the_branch_move`
    /// pins for pull.
    ///
    /// Probed against real git on 2026-08-26 by pushing between two local
    /// repositories and reading both reflogs: the pusher logs `update by push`
    /// on `refs/remotes/origin/<branch>`, and the repository being pushed
    /// *into* logs plain `push` on `refs/heads/<branch>`. Both parse to
    /// `ActivityKind::Push`. The second is a branch of this repository moving —
    /// the event itself, not bookkeeping about it — so it is exempted by
    /// `names_a_local_branch` and never counted away. A repository can see both
    /// at once: it is pushed into by a colleague and pushes to its own remote.
    #[test]
    fn a_push_that_names_a_local_branch_survives_the_fold() {
        let reflog = vec![
            entry("main", 100, "a", "b", "push"),
            entry("origin/main", 100, "a", "b", "update by push"),
            entry("origin/feat-a", 100, "p", "q", "update by push"),
            entry("origin/feat-b", 100, "r", "s", "update by push"),
            entry("origin/feat-c", 100, "t", "u", "update by push"),
        ];
        let branches = HashMap::from([("main".to_string(), "b".to_string())]);
        let feed = assemble_feed(vec![], reflog, &branches, &HashSet::new(), 50);

        assert_eq!(feed.len(), 2, "branch move + one counted row: {feed:#?}");
        let branch_move = feed
            .iter()
            .find(|e| e.ref_name.as_deref() == Some("main"))
            .expect("the local branch row survives the fold");
        assert_eq!(branch_move.kind, ActivityKind::Push);
        assert_eq!(branch_move.summary, "push");
        assert_eq!(branch_move.new_oid.as_deref(), Some("b"));
        assert!(
            feed.iter()
                .any(|e| e.summary == "push — 4 refs updated" && e.ref_name.is_none()),
            "the four remote-tracking updates still fold: {feed:#?}"
        );
    }

    #[test]
    fn a_terminal_fetch_folds_too_even_with_no_journal() {
        // A fetch run outside the app has reflog lines and no journal entry —
        // the case an operation-id scheme could never cover, and the reason the
        // fold lives here rather than in the write path.
        let (_, reflog) = fetch_of(&["main", "dev", "topic"], 100);
        let feed = assemble_feed(vec![], reflog, &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 1, "one external fetch is one row: {feed:#?}");
        assert_eq!(feed[0].summary, "fetch — 3 refs updated");
        assert_eq!(
            feed[0].source,
            ActivitySource::External,
            "nothing claimed the app did it"
        );
    }

    // -- The two defects #329's fix was found to still have. ------------------
    //
    // Both were written as `#[should_panic]` pins asserting what the fold
    // *should* do, on the `test.fail()` convention
    // `ci/browser/tests/hunk-keyboard.spec.mjs` uses and for the reason it was
    // adopted there: #210 survived for months behind a green gate. A test
    // asserting today's wrong answer would go quietly green and stay green
    // after a fix; `#[ignore]` says nothing at all. A pin goes RED the moment
    // its defect is fixed, and demands to be looked at.
    //
    // **Both pins have now gone red and been retired, hours apart.** F1 is
    // fixed at the writer (#485) and its fixture models what that writer
    // actually produces; F2 is fixed in the fold (#486, ADR 0081) on the
    // assertions it was written with, unedited. Neither is a pin any more,
    // and the convention is recorded here because it worked twice.
    //
    // Evidence and the proposed fixes:
    // `docs/investigations/2026-08-25-issue-329-fetch-feed-volume.md`.

    /// **F2 — the "tips unknown — git could not be read" admission is erased.**
    /// Fixed by #486; this was a `#[should_panic]` pin until then.
    ///
    /// `planner::fetch::journal_unobserved` fires when `git fetch` *succeeded*
    /// and only the re-read of the refs failed, so git wrote a reflog line for
    /// every ref it moved. The admission carries no `new_oid`, so it suppresses
    /// none of them in attribution and folded in with them instead — replacing
    /// a deliberate "we could not read this" with a confident count, and a
    /// count that was one too high, since the admission was itself counted.
    ///
    /// [`admits_it_could_not_read_the_refs`] now keeps it out of the fold. The
    /// four refs that really moved still fold, from their reflog lines, into a
    /// row of their own; the assertions below are the ones the pin was written
    /// with and are unchanged.
    #[test]
    fn an_unobserved_fetch_keeps_its_admission_instead_of_being_counted() {
        // Four refs really moved, so git wrote four reflog lines...
        let reflog: Vec<ReflogEntry> = ["main", "dev", "topic", "release"]
            .iter()
            .map(|r| {
                entry(
                    &format!("origin/{r}"),
                    100,
                    &format!("old-{r}"),
                    &format!("new-{r}"),
                    "fetch origin: fast-forward",
                )
            })
            .collect();
        // ...and the app, unable to re-read them, journaled one admission.
        let journal = vec![ActivityEvent {
            time: 100,
            kind: ActivityKind::Fetch,
            ref_name: None,
            summary: "fetched from ‘origin’, but refs/remotes/origin could not be re-read \
                      afterwards (tips unknown — git could not be read)"
                .into(),
            old_oid: None,
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];

        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);

        assert!(
            feed.iter().any(|e| e.summary.contains("tips unknown")),
            "F2: the admission did not survive the fold — an honest \"we could not \
             read this\" became a confident count. Feed: {feed:#?}"
        );
        assert!(
            !feed.iter().any(|e| e.summary == "fetch — 5 refs updated"),
            "F2: four refs moved, not five — the admission was counted as a ref. \
             Feed: {feed:#?}"
        );
    }

    /// The other half of the F2 fixture's answer: what the four refs that
    /// *did* move render as, now that they no longer carry the admission with
    /// them.
    ///
    /// Two rows, from the feed's two sources, neither speaking for the other.
    /// git's reflog saw four ref movements and says so; the app says it could
    /// not confirm any of them. Making the admission swallow the reflog rows
    /// would need it to suppress lines it has no oid to match — which is
    /// exactly the move `0a7ba777` reverted — and would hide real ref
    /// movements behind an admission of ignorance about them. See ADR 0081.
    #[test]
    fn an_unobserved_fetch_renders_beside_the_refs_the_reflog_saw() {
        let (journal, reflog) = unobserved_fetch_of(&["main", "dev", "topic", "release"], 100);
        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(
            feed.len(),
            2,
            "the admission and the fold, no more: {feed:#?}"
        );
        let admission = feed
            .iter()
            .find(|e| e.summary.contains("tips unknown"))
            .expect("the admission survives");
        assert_eq!(
            admission.source,
            ActivitySource::App,
            "the admission is the app's own statement"
        );
        assert!(
            admission.ref_name.is_none() && admission.new_oid.is_none(),
            "the admission is passed through untouched, not rebuilt: {admission:#?}"
        );
        let counted = feed
            .iter()
            .find(|e| e.summary.starts_with("fetch — "))
            .expect("the refs that moved still fold into one row");
        assert_eq!(
            counted.summary, "fetch — 4 refs updated",
            "four refs moved, and the admission is not one of them"
        );
        assert_eq!(
            counted.source,
            ActivitySource::External,
            "those four rows came from git's reflog, which the app never read"
        );
    }

    /// A fetch that moved *nothing at all* and could not be re-read: the one
    /// case the fold's old reasoning was actually right about, since there are
    /// no reflog lines for the admission to be folded in with.
    ///
    /// It is here because it is the case a too-wide exclusion cannot break and
    /// a too-*narrow* rendering can: the admission must still come out as
    /// itself, a run of one returned untouched, rather than as "1 ref updated"
    /// or as nothing at all.
    #[test]
    fn a_fetch_that_moved_nothing_still_keeps_its_admission_as_a_run_of_one() {
        let (journal, _) = unobserved_fetch_of(&[], 100);
        let feed = assemble_feed(journal, vec![], &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 1, "one admission, one row: {feed:#?}");
        assert!(
            feed[0].summary.contains("tips unknown"),
            "the admission is the row, in its own words: {feed:#?}"
        );
        assert_eq!(feed[0].source, ActivitySource::App);
    }

    /// The exclusion turns on the **whole** shape — no ref name *and* no oids.
    ///
    /// A Fetch row that names no ref but does know where the ref landed is not
    /// an admission of ignorance: it knows what moved, and belongs in the
    /// count. No production path builds one today
    /// ([`admits_it_could_not_read_the_refs`] says which paths build what), so
    /// this fixture is synthetic on purpose — it exists so that narrowing the
    /// discriminator to `ref_name.is_none()` alone has something to fail.
    #[test]
    fn a_fetch_that_names_no_ref_but_knows_an_oid_is_still_counted() {
        let reflog = vec![
            entry(
                "origin/main",
                100,
                "old-main",
                "new-main",
                "fetch origin: fast-forward",
            ),
            entry(
                "origin/dev",
                100,
                "old-dev",
                "new-dev",
                "fetch origin: fast-forward",
            ),
        ];
        let journal = vec![ActivityEvent {
            time: 100,
            kind: ActivityKind::Fetch,
            ref_name: None,
            summary: "fetched a ref whose name we did not record".into(),
            old_oid: None,
            // The one field that separates this from the admission: something
            // *was* observed.
            new_oid: Some("new-unnamed".into()),
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];

        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 1, "one burst, one row: {feed:#?}");
        assert_eq!(
            feed[0].summary, "fetch — 3 refs updated",
            "all three moved a ref and all three are counted: {feed:#?}"
        );
    }

    /// The mirror of the test above: a Fetch row that names a ref but carries
    /// no oids is still an ordinary ref update, so narrowing the discriminator
    /// to "both oids `None`" alone must fail here.
    ///
    /// Synthetic for the same reason — no production path builds this shape
    /// either, and `Obs::Absent` flattening to `None` is why the oid pair
    /// cannot carry the decision on its own.
    #[test]
    fn a_fetch_that_names_a_ref_without_oids_is_still_counted() {
        let reflog = vec![
            entry(
                "origin/main",
                100,
                "old-main",
                "new-main",
                "fetch origin: fast-forward",
            ),
            entry(
                "origin/dev",
                100,
                "old-dev",
                "new-dev",
                "fetch origin: fast-forward",
            ),
        ];
        let journal = vec![ActivityEvent {
            time: 100,
            kind: ActivityKind::Fetch,
            // The one field that separates this from the admission: a ref was
            // named, so this row is about that ref.
            ref_name: Some("refs/remotes/origin/topic".into()),
            summary: "fetched ‘origin/topic’ from origin".into(),
            old_oid: None,
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];

        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 1, "one burst, one row: {feed:#?}");
        assert_eq!(
            feed[0].summary, "fetch — 3 refs updated",
            "all three name a ref and all three are counted: {feed:#?}"
        );
    }

    /// The F2 situation as fixtures: `refs` really moved, so git logged each
    /// one, and the app — unable to re-read `refs/remotes/origin` — journaled
    /// the single admission `planner::fetch::journal_unobserved` writes.
    ///
    /// An empty `refs` is the fetch that moved nothing at all: the admission
    /// with no reflog lines beside it.
    fn unobserved_fetch_of(refs: &[&str], time: i64) -> (Vec<ActivityEvent>, Vec<ReflogEntry>) {
        let reflog = refs
            .iter()
            .map(|r| {
                entry(
                    &format!("origin/{r}"),
                    time,
                    &format!("old-{r}"),
                    &format!("new-{r}"),
                    "fetch origin: fast-forward",
                )
            })
            .collect();
        let journal = vec![ActivityEvent {
            time,
            kind: ActivityKind::Fetch,
            ref_name: None,
            summary: "fetched from ‘origin’, but refs/remotes/origin could not be re-read \
                      afterwards (tips unknown — git could not be read)"
                .into(),
            old_oid: None,
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];
        (journal, reflog)
    }

    /// **F1, now a regression test: a slow fetch counts only the refs that
    /// moved.**
    ///
    /// A journal entry that lands more than [`JOURNAL_MATCH_SLACK`] after
    /// git's reflog line for the same movement stops matching it; that reflog
    /// line survives attribution, and the fold counts both copies of one ref
    /// movement. When this was filed the app produced exactly that: one entry
    /// per ref, each taking its own timestamp *after* its own full ref
    /// capture, so entry *i* drifted ~29.63 ms further at 250 refs — past the
    /// window around ref 170, and 500 refs reported as 891.
    ///
    /// **#485 removed the drift at its source**, and the fixture below is now
    /// the writer's real output: `handlers::journal_app_events` takes **one**
    /// `now_secs()` reading for the whole batch, so every entry of one fetch
    /// carries the same moment however long the fetch itself took. Hence the
    /// name — the fetch is still slow, and the count is now true anyway.
    ///
    /// **Neither half of this proves the chain alone, and both exist.** That
    /// the writer emits one moment is pinned at the writer, in
    /// `planner::fetch::tests::one_fetch_journals_every_ref_at_one_moment_under_one_capture`;
    /// this pins that the fold then counts correctly. Read on its own, this
    /// test would go green for a writer that had regressed to per-entry
    /// timestamps, because it builds its own journal — which is why the pair
    /// is named here rather than left to be discovered.
    ///
    /// **What is no longer pinned, said plainly.** The fold still counts both
    /// copies of anything that *does* drift; that is not fixed, and after this
    /// change nothing here exercises it. What batching rules out is entries of
    /// one fetch drifting *apart from each other* — no writer of this class can
    /// still produce that, since fetch and pull journal through the batched
    /// `fetch::journal_updates` (pull runs `fetch::run_fetch`) with one shared
    /// timestamp, and the one remaining unbatched per-ref writer cannot yet
    /// emit a burst for the fold to count (see the paragraph below). It does
    /// **not** rule out
    /// the whole batch's one shared timestamp landing more than
    /// [`JOURNAL_MATCH_SLACK`] after the reflog lines it is meant to match —
    /// that gap is set by however long the post-fetch ref re-read
    /// (`planner::transfer::remote_tracking_refs`) takes before the
    /// timestamp is sampled (`handlers::journal_app_events`). That re-read
    /// is one `git for-each-ref`, so its cost plausibly scales with the ref
    /// namespace — but that is reasoning, not a measurement, and none exists.
    /// Whether it is ever actually crossed has not been measured (#522); if
    /// it is, every entry in the batch drifts together and the fold counts
    /// 2N, the same symptom this test pins in a different shape.
    ///
    /// **#487 narrowed the first half of that; it did not break it.** That
    /// sentence used to dismiss `planner::push::journal_updates` — the one
    /// remaining unbatched per-ref writer — on the grounds that
    /// `fold_ref_update_bursts` folds only `Fetch` and `Pull`, so push could
    /// not reach this code path at all. It can now: #487 made `Push` a fold
    /// candidate, and that writer is still unbatched, so it *is* a writer of
    /// this class. What holds instead is narrower and still true — the push
    /// endpoint pushes one named branch, so that loop runs once and a single
    /// entry cannot drift from itself. The day a multi-ref push path lands,
    /// this is the paragraph to come back to.
    #[test]
    fn a_slow_fetch_still_counts_only_the_refs_that_moved() {
        const REFS: i64 = 250;

        // git wrote every reflog line during the fetch, at one moment.
        let reflog: Vec<ReflogEntry> = (0..REFS)
            .map(|i| {
                entry(
                    &format!("origin/b{i}"),
                    100,
                    &format!("o{i}"),
                    &format!("n{i}"),
                    "fetch origin: fast-forward",
                )
            })
            .collect();
        // And the app journals its whole batch at one moment — the same one,
        // since #485. Before it, entry `i` landed at `100 + i * 30ms`.
        let journal: Vec<ActivityEvent> = (0..REFS)
            .map(|i| ActivityEvent {
                time: 100,
                kind: ActivityKind::Fetch,
                ref_name: Some(format!("refs/remotes/origin/b{i}")),
                summary: format!("fetched ‘origin/b{i}’ from origin"),
                old_oid: Some(format!("o{i}")),
                new_oid: Some(format!("n{i}")),
                source: ActivitySource::App,
                undo: None,
                refs: None,
            })
            .collect();

        let feed = assemble_feed(journal, reflog, &HashMap::new(), &HashSet::new(), 500);
        let counted: i64 = feed
            .iter()
            .find_map(|e| {
                e.summary
                    .strip_prefix("fetch — ")?
                    .strip_suffix(" refs updated")?
                    .parse()
                    .ok()
            })
            .expect("F1: expected one counted fetch row");

        assert_eq!(
            counted, REFS,
            "F1: the fold counted {counted} refs but only {REFS} moved — the \
             app's entries have drifted past the dedup window again, so each \
             movement is being counted twice (see #485: one `now_secs()` \
             reading for the whole batch)"
        );
    }
    // -----------------------------------------------------------------------
    // #485 — resolving a batched ref capture (ADR 0080)
    // -----------------------------------------------------------------------

    /// One journaled event, with whatever `refs` the case under test needs.
    fn with_refs(summary: &str, refs: Option<RefsAtEvent>) -> ActivityEvent {
        ActivityEvent {
            time: 100,
            kind: ActivityKind::Fetch,
            ref_name: Some("refs/remotes/origin/main".into()),
            summary: summary.into(),
            old_oid: Some("old".into()),
            new_oid: Some("new".into()),
            source: ActivitySource::App,
            undo: None,
            refs,
        }
    }

    /// A journaled commit — a kind the burst fold leaves alone, so the row
    /// reaches the end of `assemble_feed` as itself.
    fn commit_event(time: i64, summary: &str, refs: Option<RefsAtEvent>) -> ActivityEvent {
        ActivityEvent {
            time,
            kind: ActivityKind::Commit,
            ref_name: Some("main".into()),
            summary: summary.into(),
            old_oid: Some("old".into()),
            new_oid: Some(format!("new{time}")),
            source: ActivitySource::App,
            undo: None,
            refs,
        }
    }

    fn anchor(batch: Option<&str>, branch: (&str, &str)) -> RefsAtEvent {
        RefsAtEvent::Captured {
            branches: BTreeMap::from([(branch.0.to_string(), branch.1.to_string())]),
            truncated_at: None,
            head: None,
            tags: None,
            remotes: None,
            batch: batch.map(str::to_string),
        }
    }

    /// The point of the whole format: a referrer resolves to the maps its
    /// batch stored, so an event whose line carries no maps is not an event
    /// with no history.
    ///
    /// MUTATION: have `refs_at` return `event.refs` verbatim — the referrer
    /// resolves to `InBatch`, which carries no branches, and this goes red.
    #[test]
    fn a_batched_referrer_resolves_to_the_capture_its_batch_stored() {
        let journal = vec![
            with_refs("first", Some(RefsAtEvent::InBatch { batch: "b1".into() })),
            with_refs("second", Some(RefsAtEvent::InBatch { batch: "b1".into() })),
            with_refs("last", Some(anchor(Some("b1"), ("main", "cafe")))),
        ];

        for (i, event) in journal.iter().enumerate() {
            let Some(RefsAtEvent::Captured { branches, .. }) = refs_at(event, &journal) else {
                panic!("line {i} resolved to nothing: {:?}", event.refs);
            };
            assert_eq!(branches.get("main").map(String::as_str), Some("cafe"));
        }
    }

    /// A referrer whose anchor is not in the slice — trimmed off by the read
    /// window, or lost with a corrupt line — resolves to `None`: **no
    /// information**. Never an empty map, which a replayer reads as "every
    /// branch was deleted at this instant", and never another batch's maps.
    ///
    /// MUTATION: resolve to the nearest following `Captured` regardless of
    /// the id and this goes red on the second leg, with the wrong repository
    /// asserted for the orphan.
    #[test]
    fn an_orphaned_referrer_is_no_information_not_an_empty_map() {
        let orphan = with_refs(
            "orphan",
            Some(RefsAtEvent::InBatch {
                batch: "gone".into(),
            }),
        );
        assert!(
            refs_at(&orphan, std::slice::from_ref(&orphan)).is_none(),
            "an unresolvable referrer must claim nothing"
        );

        // And it must not latch onto a different batch's anchor.
        let journal = vec![
            orphan.clone(),
            with_refs("other", Some(anchor(Some("b2"), ("main", "beef")))),
        ];
        assert!(
            refs_at(&journal[0], &journal).is_none(),
            "the orphan resolved to another batch's capture: {:?}",
            refs_at(&journal[0], &journal)
        );
    }

    /// The three pre-#485 answers are returned unchanged, and a lone capture
    /// (`batch: None`) anchors nothing — so it cannot be found by a referrer
    /// that names no id either.
    #[test]
    fn an_unbatched_capture_a_failure_and_an_absent_field_answer_for_themselves() {
        let lone = with_refs("lone", Some(anchor(None, ("main", "cafe"))));
        assert!(matches!(
            refs_at(&lone, std::slice::from_ref(&lone)),
            Some(RefsAtEvent::Captured { batch: None, .. })
        ));

        let failed = with_refs(
            "failed",
            Some(RefsAtEvent::CaptureFailed {
                reason: "ref store gone".into(),
            }),
        );
        assert!(
            matches!(
                refs_at(&failed, std::slice::from_ref(&failed)),
                Some(RefsAtEvent::CaptureFailed { .. })
            ),
            "a failure must stay a failure — resolving it to None would lose \
             the one thing it records"
        );

        let silent = with_refs("pre-#131", None);
        assert!(refs_at(&silent, std::slice::from_ref(&silent)).is_none());
    }

    /// A journal line written before #485 has no `batch` key at all. It must
    /// still parse, and must still be the self-contained observation it was —
    /// the format is additive, and the journal is append-only.
    ///
    /// No mutation is offered, because the obvious one is not real: dropping
    /// `#[serde(default)]` from `batch` was tried and this test stayed green.
    /// `serde`'s derive already reads a missing `Option` field as `None`, so
    /// the attribute is belt-and-braces on this field and only the `Option`
    /// itself is load-bearing. Recorded rather than left as a mutation note
    /// that would quietly never have fired.
    #[test]
    fn a_pre_485_capture_line_still_parses_and_anchors_nothing() {
        let line = r#"{"time":1,"kind":"Fetch","ref_name":"origin/main","summary":"s",
            "old_oid":null,"new_oid":"n","source":"App",
            "refs":{"status":"captured","branches":{"main":"cafe"}}}"#;
        let event: ActivityEvent = serde_json::from_str(line).expect("a pre-#485 line parses");
        let Some(RefsAtEvent::Captured {
            branches, batch, ..
        }) = refs_at(&event, std::slice::from_ref(&event))
        else {
            panic!("expected the line's own capture: {:?}", event.refs);
        };
        assert_eq!(branches.get("main").map(String::as_str), Some("cafe"));
        assert_eq!(*batch, None);
    }
    /// **Nothing leaves the feed as a reference.** A journaled event may point
    /// at its batch's capture instead of carrying one (#485); whoever receives
    /// the feed has no journal to follow that with, so `assemble_feed`
    /// resolves it.
    ///
    /// `Commit`, not `Fetch`, so the rows survive the burst fold and reach the
    /// end of the pipeline individually — which is where the resolution has to
    /// hold.
    ///
    /// MUTATION: delete step 7 and the first two rows ship `in_batch` — a
    /// value whose only possible reading, to a client, is "no history".
    #[test]
    fn the_feed_resolves_a_batched_capture_rather_than_shipping_the_reference() {
        let journal = vec![
            commit_event(
                1,
                "first",
                Some(RefsAtEvent::InBatch { batch: "b1".into() }),
            ),
            commit_event(
                2,
                "second",
                Some(RefsAtEvent::InBatch { batch: "b1".into() }),
            ),
            commit_event(3, "last", Some(anchor(Some("b1"), ("main", "cafe")))),
        ];

        let feed = assemble_feed(journal, Vec::new(), &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 3);
        for event in &feed {
            let Some(RefsAtEvent::Captured { branches, .. }) = &event.refs else {
                panic!(
                    "'{}' left the feed carrying {:?} — a client cannot resolve that",
                    event.summary, event.refs
                );
            };
            assert_eq!(branches.get("main").map(String::as_str), Some("cafe"));
        }
    }

    /// The paired negative, and the one that must not be "fixed" into an
    /// empty map: when the anchor is outside the window the feed was read
    /// from, the row ships **nothing** — the same silence a line that never
    /// captured ships, which a replayer already knows to conclude nothing
    /// from.
    #[test]
    fn a_row_whose_batch_anchor_is_outside_the_window_ships_nothing_at_all() {
        let journal = vec![commit_event(
            1,
            "orphan",
            Some(RefsAtEvent::InBatch {
                batch: "trimmed-away".into(),
            }),
        )];

        let feed = assemble_feed(journal, Vec::new(), &HashMap::new(), &HashSet::new(), 50);

        assert_eq!(feed.len(), 1);
        assert!(
            feed[0].refs.is_none(),
            "an unresolvable reference must become silence, never a map: {:?}",
            feed[0].refs
        );
    }
}
