//! The app's operation journal and the branch-tip snapshots — the two pieces
//! of server-side state behind the activity feed, both living under
//! `.git/git-vista/` in the served repository.
//!
//! **Journal** (`journal.jsonl`): one JSON [`ActivityEvent`] per line, appended
//! by every write endpoint the moment its git command succeeds. It's what lets
//! the feed (a) attribute an event to the app rather than "the terminal", and
//! (b) undo a branch deletion — git deletes a branch's reflog *with* the
//! branch, so the journal is the only place its last tip survives.
//!
//! **Snapshot** (`refs.json`): the local branch → tip map as of the last feed
//! read. A branch present in the snapshot but missing from the repo — with no
//! journal record of the app deleting it — was deleted *outside* the app; the
//! feed synthesizes a deletion event (carrying the snapshot's tip, so even
//! terminal deletions get a Restore) and journals it so it's remembered once.
//!
//! Location rationale: inside `.git` so it's per-repository, survives server
//! restarts, travels with the repo, and can never be committed. Everything
//! here is best-effort by design — a journal that can't be written degrades
//! the feed's attribution, which must never break the git operation itself.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use git_vista_core::activity::{ActivityEvent, RefsAtEvent, REFS_PER_EVENT_CAP};
use git_vista_core::model::RefKind;
use git_vista_git::read_refs;

/// Only this many of the newest journal lines are read back. The journal is
/// append-only and unbounded; the feed shows nothing like this many events.
const JOURNAL_READ_CAP: usize = 1_000;

/// The state directory, `.git/git-vista/`, if this repo has a real `.git`
/// *directory*. (A linked worktree's `.git` is a file; journaling is quietly
/// skipped there rather than guessed at.) Public because the test-repo seed
/// files (`seed-refs` / `seed-head` / `seed.bundle`, written by `gv --seed`)
/// live in the same directory.
pub fn state_dir(repo: &Path) -> Option<PathBuf> {
    let git = repo.join(".git");
    git.is_dir().then(|| git.join("git-vista"))
}

fn journal_path(repo: &Path) -> Option<PathBuf> {
    state_dir(repo).map(|d| d.join("journal.jsonl"))
}

fn snapshot_path(repo: &Path) -> Option<PathBuf> {
    state_dir(repo).map(|d| d.join("refs.json"))
}

/// Read the repo's local branch -> tip map for journaling with an event
/// (#131).
///
/// The return type is the point. A failed read yields
/// [`RefsAtEvent::CaptureFailed`] carrying the reason — never an empty map,
/// which a replayer would read as "every branch was deleted at this instant".
/// An empty map is reserved for the genuine observation of a repo with no
/// branches, which is a real state a fresh repo is in.
pub fn capture_refs(repo: &Path) -> RefsAtEvent {
    let refs = match read_refs(repo) {
        Ok(refs) => refs,
        Err(e) => {
            return RefsAtEvent::CaptureFailed {
                reason: e.to_string(),
            }
        }
    };
    let mut branches: BTreeMap<String, String> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Branch)
        .map(|r| (r.name.clone(), r.target.0.clone()))
        .collect();
    // Cap loudly, never silently: `truncated_at` carries the true count so a
    // replayer can tell "these are all the branches" from "these are the
    // first 500 of N".
    let total = branches.len();
    let truncated_at = (total > REFS_PER_EVENT_CAP).then_some(total);
    if truncated_at.is_some() {
        let keep: Vec<String> = branches.keys().take(REFS_PER_EVENT_CAP).cloned().collect();
        branches.retain(|name, _| keep.binary_search(name).is_ok());
    }
    RefsAtEvent::Captured {
        branches,
        truncated_at,
    }
}

/// Append one event to the journal, creating the directory on first use.
/// Best-effort: failure is logged to the terminal and swallowed — the git
/// operation this records already succeeded, and must stay succeeded.
///
/// The branch-tip capture (#131) happens *here* rather than at each call site,
/// so no caller can forget it and no future write endpoint can quietly ship
/// without history. An event that arrives already carrying `refs` keeps its
/// own — the feed's synthesized external-deletion event needs to attach the
/// map as it stood *before* the deletion it just noticed.
pub fn append(repo: &Path, event: &ActivityEvent) {
    let Some(path) = journal_path(repo) else {
        return;
    };
    let captured;
    let event = if event.refs.is_some() {
        event
    } else {
        captured = ActivityEvent {
            refs: Some(capture_refs(repo)),
            ..event.clone()
        };
        &captured
    };
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
        })
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = result {
        eprintln!(
            "git-vista: couldn't append to the journal at {}: {e}",
            path.display()
        );
    }
}

/// Read the newest [`JOURNAL_READ_CAP`] journaled events (file order — oldest
/// first — is preserved within the returned slice). Unparsable lines are
/// skipped loudly: one corrupt line must not hide the rest of the history.
pub fn read_all(repo: &Path) -> Vec<ActivityEvent> {
    let Some(path) = journal_path(repo) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(JOURNAL_READ_CAP);
    lines[start..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<ActivityEvent>(l) {
            Ok(event) => Some(event),
            Err(e) => {
                eprintln!("git-vista: skipping an unreadable journal line: {e}");
                None
            }
        })
        .collect()
}

/// The branch → tip map as of the last snapshot, or `None` when no snapshot
/// exists yet (first run: nothing to diff against, only a baseline to write).
pub fn read_snapshot(repo: &Path) -> Option<HashMap<String, String>> {
    let path = snapshot_path(repo)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Overwrite the snapshot with the repo's current branch → tip map.
pub fn write_snapshot(repo: &Path, branches: &HashMap<String, String>) {
    let Some(path) = snapshot_path(repo) else {
        return;
    };
    let Ok(json) = serde_json::to_string_pretty(branches) else {
        return;
    };
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(&path, json));
    if let Err(e) = result {
        eprintln!(
            "git-vista: couldn't write the ref snapshot at {}: {e}",
            path.display()
        );
    }
}

/// Drop one branch from the snapshot immediately. Called by the app's own
/// delete endpoints (which journal the deletion themselves), so the feed's
/// snapshot diff can't also synthesize a duplicate "deleted outside the app"
/// event for a deletion the app performed.
pub fn remove_from_snapshot(repo: &Path, branch: &str) {
    if let Some(mut snapshot) = read_snapshot(repo) {
        if snapshot.remove(branch).is_some() {
            write_snapshot(repo, &snapshot);
        }
    }
}

/// Wipe the journal and the branch snapshot. Used by the test-repo reset: its
/// whole point is that the recorded history no longer describes the repo, and
/// keeping it would resurface undone events (with dead undo targets) in the
/// feed. Both files regenerate naturally — the journal on the next app write,
/// the snapshot on the next feed read. Best-effort, like the other writers.
pub fn clear(repo: &Path) {
    for path in [journal_path(repo), snapshot_path(repo)]
        .into_iter()
        .flatten()
    {
        if path.exists() {
            if let Err(e) = fs::remove_file(&path) {
                eprintln!("git-vista: couldn't clear {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_core::activity::{ActivityKind, ActivitySource};
    use std::process::Command;

    /// A tempdir with a real `.git` directory (git init), since the state dir
    /// deliberately requires one.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git runs")
            .success());
        dir
    }

    fn event(summary: &str) -> ActivityEvent {
        ActivityEvent {
            time: 42,
            kind: ActivityKind::Commit,
            ref_name: Some("main".into()),
            summary: summary.into(),
            old_oid: Some("a".into()),
            new_oid: Some("b".into()),
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }
    }

    /// Commit once so the repo actually has a branch to capture.
    fn commit(dir: &Path, branch: &str) {
        for args in [
            vec!["checkout", "-q", "-B", branch],
            vec!["commit", "-q", "--allow-empty", "-m", "x"],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .current_dir(dir)
                .status()
                .expect("git runs")
                .success());
        }
    }

    /// #131's core promise: an appended event carries the branch -> tip map,
    /// so a replayer can reconstruct the moment without the reflog.
    ///
    /// MUTATION: drop the `refs: Some(capture_refs(repo))` fill-in from
    /// `append` and this goes red — the whole feature reduces to a no-op.
    #[test]
    fn an_appended_event_carries_the_branch_tips_of_its_moment() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "feature");
        append(dir.path(), &event("recorded"));

        let read = read_all(dir.path());
        let RefsAtEvent::Captured {
            branches,
            truncated_at,
        } = read[0].refs.clone().expect("a capture is attached")
        else {
            panic!("a readable repo must capture, not fail");
        };
        assert_eq!(truncated_at, None, "two branches is under any cap");
        assert!(branches.contains_key("main"));
        assert!(branches.contains_key("feature"));
        assert_eq!(branches.len(), 2);
    }

    /// The lossless part: a branch deleted AFTER the event still appears in
    /// that event's capture. This is what the reflog cannot give us — git
    /// deletes a branch's reflog together with the branch.
    ///
    /// MUTATION: have the replay read live refs instead of the stored map and
    /// this goes red.
    #[test]
    fn a_deleted_branch_survives_in_the_event_that_predates_its_deletion() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "doomed");
        append(dir.path(), &event("before the deletion"));
        assert!(Command::new("git")
            .args(["checkout", "-q", "main"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["branch", "-qD", "doomed"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());

        let read = read_all(dir.path());
        let RefsAtEvent::Captured { branches, .. } =
            read[0].refs.clone().expect("capture attached")
        else {
            panic!("expected a capture");
        };
        assert!(
            branches.contains_key("doomed"),
            "the journal must still know the branch existed, and at which tip"
        );
    }

    /// The third state, and the reason this field is an enum. A repo whose
    /// refs cannot be read must record CaptureFailed — never an empty map,
    /// which a replayer would read as "every branch was deleted here".
    ///
    /// MUTATION: change `capture_refs`'s Err arm to
    /// `RefsAtEvent::Captured { branches: BTreeMap::new(), truncated_at: None }`
    /// and this goes red.
    #[test]
    fn an_unreadable_repo_records_capture_failed_never_an_empty_map() {
        let dir = repo();
        // Destroy the ref store, keeping .git a directory so journaling still
        // engages — the failure must reach the record, not skip it.
        std::fs::remove_dir_all(dir.path().join(".git/refs")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();

        let captured = capture_refs(dir.path());
        match captured {
            RefsAtEvent::CaptureFailed { reason } => {
                assert!(!reason.is_empty(), "the failure must say what happened");
            }
            RefsAtEvent::Captured { branches, .. } => panic!(
                "a failed read must not masquerade as an observation of {} branches",
                branches.len()
            ),
        }
    }

    /// An empty capture is a real answer — a repo before its first commit
    /// genuinely has no branches — and must stay distinct from a failure.
    ///
    /// MUTATION: make `capture_refs` return CaptureFailed for an empty
    /// branch set and this goes red.
    #[test]
    fn a_repo_with_no_branches_captures_an_empty_map_not_a_failure() {
        let dir = repo(); // git init, no commits: readable, zero branches
        match capture_refs(dir.path()) {
            RefsAtEvent::Captured {
                branches,
                truncated_at,
            } => {
                assert!(branches.is_empty());
                assert_eq!(truncated_at, None);
            }
            RefsAtEvent::CaptureFailed { reason } => {
                panic!("a readable empty repo is an observation, not a failure: {reason}")
            }
        }
    }

    /// An event that already carries a capture keeps it. The feed's
    /// synthesized external-deletion event depends on this: it must record the
    /// map from BEFORE the deletion it just noticed, not the live present that
    /// has already lost that branch.
    ///
    /// MUTATION: make `append` overwrite `refs` unconditionally and this goes
    /// red — and the external-deletion event silently stops recording the very
    /// branch it exists to remember.
    #[test]
    fn a_caller_supplied_capture_is_never_overwritten() {
        let dir = repo();
        commit(dir.path(), "main");
        let mut e = event("synthesized");
        e.refs = Some(RefsAtEvent::Captured {
            branches: BTreeMap::from([("long-gone".to_string(), "deadbeef".to_string())]),
            truncated_at: None,
        });
        append(dir.path(), &e);

        let read = read_all(dir.path());
        let RefsAtEvent::Captured { branches, .. } = read[0].refs.clone().unwrap() else {
            panic!("expected the caller's capture");
        };
        assert!(
            branches.contains_key("long-gone"),
            "append must not replace a capture the caller deliberately supplied"
        );
        assert!(
            !branches.contains_key("main"),
            "and must not merge live refs in"
        );
    }

    /// A journal line written before #131 has no `refs` field at all. It must
    /// still parse, and must read as None — "no capture recorded" — rather
    /// than any claim about branches.
    ///
    /// MUTATION: drop `#[serde(default)]` from the field and this goes red.
    #[test]
    fn a_pre_131_journal_line_still_parses_and_claims_nothing() {
        let dir = repo();
        let path = dir.path().join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"time\":1,\"kind\":\"Commit\",\"ref_name\":\"main\",\"summary\":\"old\",\"old_oid\":\"a\",\"new_oid\":\"b\",\"source\":\"App\"}\n",
        )
        .unwrap();

        let read = read_all(dir.path());
        assert_eq!(read.len(), 1, "an old line must not be dropped as corrupt");
        assert_eq!(read[0].summary, "old");
        assert!(
            read[0].refs.is_none(),
            "absent means no capture recorded — never an empty observation"
        );
    }

    #[test]
    fn journal_round_trips_events_in_order() {
        let dir = repo();
        append(dir.path(), &event("first"));
        append(dir.path(), &event("second"));
        let read = read_all(dir.path());
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].summary, "first");
        assert_eq!(read[1].summary, "second");
        assert_eq!(read[0].source, ActivitySource::App);
        // The undo field is never journaled (recomputed per read).
        assert!(read[0].undo.is_none());
    }

    #[test]
    fn corrupt_lines_are_skipped_not_fatal() {
        let dir = repo();
        append(dir.path(), &event("good"));
        let path = dir.path().join(".git/git-vista/journal.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json}\n");
        std::fs::write(&path, text).unwrap();
        append(dir.path(), &event("after"));
        let read = read_all(dir.path());
        assert_eq!(read.len(), 2, "good lines on both sides of the corruption");
    }

    #[test]
    fn snapshot_round_trips_and_removes() {
        let dir = repo();
        assert!(read_snapshot(dir.path()).is_none(), "no baseline yet");
        let branches = HashMap::from([
            ("main".to_string(), "aaa".to_string()),
            ("feat".to_string(), "bbb".to_string()),
        ]);
        write_snapshot(dir.path(), &branches);
        assert_eq!(read_snapshot(dir.path()).unwrap(), branches);

        remove_from_snapshot(dir.path(), "feat");
        let after = read_snapshot(dir.path()).unwrap();
        assert_eq!(after.len(), 1);
        assert!(after.contains_key("main"));
    }

    #[test]
    fn missing_git_dir_degrades_to_no_ops() {
        let dir = tempfile::tempdir().unwrap(); // no .git at all
        append(dir.path(), &event("ignored"));
        assert!(read_all(dir.path()).is_empty());
        assert!(read_snapshot(dir.path()).is_none());
        write_snapshot(dir.path(), &HashMap::new()); // must not create anything
        assert!(!dir.path().join(".git").exists());
    }
}
