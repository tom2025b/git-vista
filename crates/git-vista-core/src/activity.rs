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

use std::collections::{HashMap, HashSet};

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
    ResetBranch { branch: String, to: String, expected_tip: String },
    /// `git revert --no-edit <commit>` — the history-preserving undo for a
    /// commit that's already shared.
    RevertCommit { commit: String },
}

/// An [`UndoAction`] dressed for a menu: the action itself, a human label, and
/// whether the state being discarded is already on the remote (in which case
/// undoing locally leaves the remote ahead — we never force-push — and the
/// confirm dialog says so).
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

/// How close (seconds) a reflog entry must be to a journal entry with the same
/// kind and new oid to count as *the same event*. Reflog timestamps have
/// one-second granularity and the journal stamps its own clock right after
/// git returns, so they're usually equal — 5s is generous slack.
const JOURNAL_MATCH_SLACK: i64 = 5;

/// How close a HEAD reflog entry must be to a branch entry with the same kind
/// and new oid to count as the same movement (one `git commit` logs on both).
const HEAD_MATCH_SLACK: i64 = 2;

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
                summary: if steps > 1 { format!("rebase ({steps} steps)") } else { summary },
                old_oid: Some(reflog[span].old_oid.clone()),
                new_oid: Some(entry.new_oid.clone()),
                source: ActivitySource::External,
                undo: None,
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
        let Some(new_oid) = &e.new_oid else { return true };
        !branch_moves.iter().any(|(kind, oid, time)| {
            *kind == e.kind && oid == new_oid && (e.time - time).abs() <= HEAD_MATCH_SLACK
        })
    });

    // -- 3. Attribute app events: a reflog entry matching a journal entry ----
    // (same kind, same resulting oid, near-same moment) *is* that journal
    // entry — keep the journal copy, which knows the source and has the
    // richer summary.
    events.retain(|e| {
        let Some(new_oid) = &e.new_oid else { return true };
        !journal.iter().any(|j| {
            j.kind == e.kind
                && j.new_oid.as_deref() == Some(new_oid)
                && (e.time - j.time).abs() <= JOURNAL_MATCH_SLACK
        })
    });

    events.extend(journal);

    // -- 4. Undo hints, computed against the repo's *current* state. ---------
    for event in &mut events {
        event.undo = undo_hint(event, branches, remote);
    }

    // -- 5. Newest first, capped. sort_by_key is stable, so same-second -------
    // events keep their source order (reflog order within a ref).
    events.sort_by_key(|e| std::cmp::Reverse(e.time));
    events.truncate(limit);
    events
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
                // have it after a local reset (we never force-push).
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
            ("checkout: moving from main to feature", ActivityKind::Checkout, "main → feature"),
            ("merge feature: Fast-forward", ActivityKind::Merge, "merge feature: Fast-forward"),
            ("rebase (finish): returning to refs/heads/f", ActivityKind::Rebase,
             "rebase (finish): returning to refs/heads/f"),
            ("reset: moving to HEAD~1", ActivityKind::Reset, "reset: moving to HEAD~1"),
            ("branch: Created from main", ActivityKind::BranchCreated, "branch: Created from main"),
            ("cherry-pick: pick me", ActivityKind::CherryPick, "pick me"),
            ("revert: Revert \"oops\"", ActivityKind::Revert, "Revert \"oops\""),
            ("pull: Fast-forward", ActivityKind::Pull, "pull: Fast-forward"),
            ("clone: from https://example.com/r.git", ActivityKind::Clone,
             "clone: from https://example.com/r.git"),
            ("update by push", ActivityKind::Push, "update by push"),
            ("fetch: fast-forward", ActivityKind::Fetch, "fetch: fast-forward"),
            ("frobnicate: unknown", ActivityKind::Other, "frobnicate: unknown"),
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
            entry("feature", 100, "c3", "c4", "rebase (finish): returning to refs/heads/feature"),
            entry("feature", 100, "c2", "c3", "rebase (pick): two"),
            entry("feature", 99, "c1", "c2", "rebase (pick): one"),
            entry("feature", 99, "c0", "c1", "rebase (start): checkout main"),
            entry("feature", 50, "c9", "c0", "commit: before"),
        ];
        let feed = assemble_feed(vec![], reflog, &HashMap::new(), &HashSet::new(), 10);
        let rebases: Vec<_> = feed.iter().filter(|e| e.kind == ActivityKind::Rebase).collect();
        assert_eq!(rebases.len(), 1, "one coalesced rebase, got {feed:#?}");
        assert_eq!(rebases[0].old_oid.as_deref(), Some("c0"), "pre-rebase state");
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
        let commits: Vec<_> = feed.iter().filter(|e| e.kind == ActivityKind::Commit).collect();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].ref_name.as_deref(), Some("main"), "branch copy wins");
        // The checkout (HEAD-only) survives.
        assert!(feed.iter().any(|e| e.kind == ActivityKind::Checkout));
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
            UndoAction::RestoreBranch { name: "old-work".into(), tip: "abc1234567".into() }
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
        let older = feed.iter().find(|e| e.kind == ActivityKind::Commit).unwrap();
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
}
