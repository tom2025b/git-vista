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

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use git_vista_core::activity::ActivityEvent;

/// Only this many of the newest journal lines are read back. The journal is
/// append-only and unbounded; the feed shows nothing like this many events.
const JOURNAL_READ_CAP: usize = 1_000;

/// The state directory, `.git/git-vista/`, if this repo has a real `.git`
/// *directory*. (A linked worktree's `.git` is a file; journaling is quietly
/// skipped there rather than guessed at.)
fn state_dir(repo: &Path) -> Option<PathBuf> {
    let git = repo.join(".git");
    git.is_dir().then(|| git.join("git-vista"))
}

fn journal_path(repo: &Path) -> Option<PathBuf> {
    state_dir(repo).map(|d| d.join("journal.jsonl"))
}

fn snapshot_path(repo: &Path) -> Option<PathBuf> {
    state_dir(repo).map(|d| d.join("refs.json"))
}

/// Append one event to the journal, creating the directory on first use.
/// Best-effort: failure is logged to the terminal and swallowed — the git
/// operation this records already succeeded, and must stay succeeded.
pub fn append(repo: &Path, event: &ActivityEvent) {
    let Some(path) = journal_path(repo) else { return };
    let Ok(line) = serde_json::to_string(event) else { return };
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
        eprintln!("git-vista: couldn't append to the journal at {}: {e}", path.display());
    }
}

/// Read the newest [`JOURNAL_READ_CAP`] journaled events (file order — oldest
/// first — is preserved within the returned slice). Unparsable lines are
/// skipped loudly: one corrupt line must not hide the rest of the history.
pub fn read_all(repo: &Path) -> Vec<ActivityEvent> {
    let Some(path) = journal_path(repo) else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
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
    let Some(path) = snapshot_path(repo) else { return };
    let Ok(json) = serde_json::to_string_pretty(branches) else { return };
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(&path, json));
    if let Err(e) = result {
        eprintln!("git-vista: couldn't write the ref snapshot at {}: {e}", path.display());
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
        }
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
        let branches =
            HashMap::from([("main".to_string(), "aaa".to_string()),
                           ("feat".to_string(), "bbb".to_string())]);
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
