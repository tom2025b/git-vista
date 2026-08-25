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
    },
    /// The read failed, and the reason is preserved. A replayer must treat
    /// this as "no information", never as "no branches".
    CaptureFailed { reason: String },
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

    events.extend(journal);

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

/// Collapse each run of remote-tracking ref updates — [`ActivityKind::Fetch`]
/// and [`ActivityKind::Pull`] — that happened within [`FETCH_BURST_GAP`] of one
/// another into a single counted row.
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
/// A run of one is returned untouched — a single-ref fetch already says the
/// useful thing ("fetched ‘origin/main’ from origin") and rewriting it as
/// "1 ref updated" would lose information to no purpose.
///
/// # Two known defects, measured 2026-08-25
///
/// Both are recorded with their evidence in
/// `docs/investigations/2026-08-25-issue-329-fetch-feed-volume.md`. Neither is
/// fixed here; do not read the paragraphs above as covering them.
///
/// 1. **The "tips unknown — git could not be read" entry is not safe here.**
///    This comment used to claim the entry "is journaled *instead of* per-ref
///    entries, never alongside them, so it is always a run of one". That
///    accounts for the journal and forgets git's reflog — the same shape of
///    mistake that got the first #329 attempt reverted. `journal_unobserved`
///    fires when the fetch *succeeded* and only the re-read of the refs
///    failed, so git wrote a reflog line for every ref it moved; the admission
///    carries no `new_oid`, so it suppresses none of them and folds in with
///    them instead. Measured: four refs moved renders as
///    "fetch — 5 refs updated", with the admission gone and the count one too
///    high. It is a run of one only when the fetch moved nothing at all.
/// 2. **The count inflates at scale.** The app stamps one journal entry per
///    ref, each performing a full ref capture, so entry *i* lands later and
///    later after git's reflog lines. Past roughly 170 refs that drift exceeds
///    [`JOURNAL_MATCH_SLACK`], the unmatched reflog lines survive attribution,
///    and the fold counts both copies. Measured: 250 refs reported as 297,
///    500 reported as 891. The feed stays at one row, so #329's symptom holds
///    — but the number in it stops being true.
///
/// **Safe for undo by construction, not by luck:** [`undo_hint`] has no arm for
/// `Fetch` or `Pull`, so neither row has ever carried a hint and dropping the
/// per-ref oids cannot take one away. The same fold would be *wrong* for, say,
/// `BranchDeleted`, whose `old_oid` is precisely what its undo needs.
fn fold_ref_update_bursts(
    events: Vec<ActivityEvent>,
    branches: &HashMap<String, String>,
) -> Vec<ActivityEvent> {
    let (candidates, mut out): (Vec<_>, Vec<_>) = events.into_iter().partition(|e| {
        matches!(e.kind, ActivityKind::Fetch | ActivityKind::Pull)
            && !names_a_local_branch(e.ref_name.as_deref(), branches)
    });

    // Fetch and Pull group separately: they are different actions, and a fetch
    // immediately followed by a pull is two of them.
    let (fetches, pulls): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|e| e.kind == ActivityKind::Fetch);
    fold_one_kind(&mut out, fetches, "fetch");
    fold_one_kind(&mut out, pulls, "pull");
    out
}

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
}
