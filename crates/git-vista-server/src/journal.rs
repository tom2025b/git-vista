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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use git_vista_core::activity::{ActivityEvent, CapturedRefs, RefsAtEvent, REFS_PER_EVENT_CAP};
use git_vista_core::model::{GitRef, RefKind};
use git_vista_git::read_refs_at;

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

/// Collect one ref kind into a [`CapturedRefs`], capped at
/// [`REFS_PER_EVENT_CAP`] entries by name order.
///
/// `truncated_at` carries the true count whenever the repo held more than the
/// cap — never a silently short map, which a replayer would read as "the rest
/// were deleted". The cap is applied here, per kind, so one kind overflowing
/// can never evict another's entries.
fn collect(refs: &[GitRef], kind: RefKind) -> CapturedRefs {
    let mut entries: BTreeMap<String, String> = refs
        .iter()
        .filter(|r| r.kind == kind)
        .map(|r| (r.name.clone(), r.target.0.clone()))
        .collect();
    let total = entries.len();
    let truncated_at = (total > REFS_PER_EVENT_CAP).then_some(total);
    if truncated_at.is_some() {
        let keep: Vec<String> = entries.keys().take(REFS_PER_EVENT_CAP).cloned().collect();
        entries.retain(|name, _| keep.binary_search(name).is_ok());
    }
    CapturedRefs {
        entries,
        truncated_at,
    }
}

/// Read the repo's refs for journaling with an event: HEAD, local branches,
/// tags and remote-tracking refs (#131, extended by #449).
///
/// The return type is the point. A failed read yields
/// [`RefsAtEvent::CaptureFailed`] carrying the reason — never an empty map,
/// which a replayer would read as "every branch was deleted at this instant".
/// An empty map is reserved for the genuine observation of a repo with no
/// branches, which is a real state a fresh repo is in.
///
/// Everything comes from **one** [`read_refs_at`] call, so HEAD and the three
/// maps describe the same instant rather than three successive ones.
///
/// Why HEAD and tags at all: #131's snapshot exists so "a future time
/// scrubber can replay history losslessly", and a snapshot of local branches
/// alone cannot show the HEAD moving — the one thing such a scrubber is for.
/// Why remote-tracking refs: the story a scrubber mostly tells is divergence,
/// "your branch moved, origin did not", and local branches alone cannot tell
/// it. See ADR 0070.
pub fn capture_refs(repo: &Path) -> RefsAtEvent {
    let read = match read_refs_at(repo) {
        Ok(read) => read,
        Err(e) => {
            return RefsAtEvent::CaptureFailed {
                reason: e.to_string(),
            }
        }
    };
    let branches = collect(&read.refs, RefKind::Branch);
    RefsAtEvent::Captured {
        branches: branches.entries,
        truncated_at: branches.truncated_at,
        head: Some(read.head),
        tags: Some(collect(&read.refs, RefKind::Tag)),
        remotes: Some(collect(&read.refs, RefKind::RemoteBranch)),
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

/// How much of the journal is pulled back per backward step. One step
/// already covers the whole window in any realistic journal; the loop exists
/// for the pathological case of very long lines.
const TAIL_CHUNK: usize = 64 * 1024;

/// The tail of `source` guaranteed to contain the last `cap` lines — read
/// backwards from the end and stopping the moment it has seen `cap + 1`
/// newlines (#464).
///
/// **The overshoot is the whole design.** Stopping one newline late means the
/// returned text holds at least `cap + 1` lines, so the caller's existing
/// `len().saturating_sub(cap)` window always discards the first of them. That
/// single invariant pays for three things at once: the leading line is
/// allowed to be a fragment (it is dropped), a multi-byte character split by
/// the chunk boundary can only land in that fragment, and a file with no
/// trailing newline needs no special case.
///
/// Decoding is lossy on purpose: bytes older than the window are already
/// outside the answer, and refusing the whole file over one of them is what
/// `read_to_string` used to do.
fn tail_window<R: Read + Seek>(source: &mut R, cap: usize) -> std::io::Result<String> {
    let mut pos = source.seek(SeekFrom::End(0))?;
    let mut window: Vec<u8> = Vec::new();
    let mut newlines = 0usize;
    while pos > 0 && newlines <= cap {
        let step = TAIL_CHUNK.min(pos as usize);
        pos -= step as u64;
        source.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; step];
        source.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|b| **b == b'\n').count();
        chunk.append(&mut window);
        window = chunk;
    }
    Ok(String::from_utf8_lossy(&window).into_owned())
}

/// Read the newest [`JOURNAL_READ_CAP`] journaled events (file order — oldest
/// first — is preserved within the returned slice). Unparsable lines are
/// skipped loudly: one corrupt line must not hide the rest of the history.
///
/// The cap bounds the *read*, not just the parse (#464): the journal is
/// append-only and unbounded, and both production callers are on hot paths —
/// the activity feed and `/api/undoables`, which the graph menu hits on every
/// open. [`tail_window`] seeks from the end rather than loading the file, so
/// the cost of a feed request stops growing with the age of the repository.
///
/// One deliberate behaviour change comes with it: bytes older than the window
/// can no longer blank the feed. `read_to_string` refused the entire file over
/// a single invalid byte anywhere in it, which contradicted this function's
/// own rule about corrupt lines.
pub fn read_all(repo: &Path) -> Vec<ActivityEvent> {
    let Some(path) = journal_path(repo) else {
        return Vec::new();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let Ok(text) = tail_window(&mut file, JOURNAL_READ_CAP) else {
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
    use git_vista_core::activity::HeadAtEvent;
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
            ..
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
                ..
            } => {
                assert!(branches.is_empty());
                assert_eq!(truncated_at, None);
            }
            RefsAtEvent::CaptureFailed { reason } => {
                panic!("a readable empty repo is an observation, not a failure: {reason}")
            }
        }
    }

    /// Run a git command in `dir`, with a fixed identity, asserting success.
    fn git_ok(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .current_dir(dir)
                .status()
                .expect("git runs")
                .success(),
            "git {args:?} failed"
        );
    }

    /// Ask **git** what a revision resolves to.
    ///
    /// Every assertion about a captured oid compares against this, never
    /// against a second call into the capture code: a capture that agrees with
    /// itself proves only that it is consistent, which is the "assert a
    /// mapping by calling the function that defines it" trap.
    fn git_says(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// One capture, unwrapped into its parts — or a panic naming the failure.
    /// Named rather than a tuple so a test that reads `tags` cannot quietly be
    /// reading `remotes`.
    struct Capture {
        branches: BTreeMap<String, String>,
        truncated_at: Option<usize>,
        head: HeadAtEvent,
        tags: CapturedRefs,
        remotes: CapturedRefs,
    }

    fn captured(repo: &Path) -> Capture {
        match capture_refs(repo) {
            RefsAtEvent::Captured {
                branches,
                truncated_at,
                head,
                tags,
                remotes,
            } => Capture {
                branches,
                truncated_at,
                head: head.expect("#449: a fresh capture always records HEAD"),
                tags: tags.expect("#449: a fresh capture always records tags"),
                remotes: remotes.expect("#449: a fresh capture always records remotes"),
            },
            RefsAtEvent::CaptureFailed { reason } => panic!("expected a capture: {reason}"),
        }
    }

    /// #449's headline: the snapshot exists so a scrubber can replay the HEAD
    /// moving, and until now it recorded local branches only — so it could not
    /// say which branch HEAD was on at any event in its own history.
    ///
    /// MUTATION-a: drop the `head` fill-in from `capture_refs` (`head: None`)
    /// and this goes red — the replay loses the one fact it is for.
    /// MUTATION-b: record the *short* branch name instead of the full ref name
    /// and this goes red on the exact-string assertion. That is why the
    /// assertion compares the whole string rather than `contains("feature")`.
    #[test]
    fn a_capture_records_which_branch_head_was_on_and_where_that_branch_was() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "feature");
        let tip = git_says(dir.path(), &["rev-parse", "HEAD"]);

        let c = captured(dir.path());
        assert_eq!(
            c.head,
            HeadAtEvent::OnBranch {
                symbolic: "refs/heads/feature".to_string(),
                oid: tip.clone(),
            },
            "the full ref name, so a replay can tell a branch from any other ref"
        );
        assert_eq!(c.branches.get("feature"), Some(&tip));
        assert!(
            c.branches.contains_key("main"),
            "sibling branches still captured"
        );
    }

    /// A detached HEAD is a different state from being on a branch, and the
    /// record has to keep them apart: a replay that reads a detached HEAD as
    /// "on the branch that happens to share the commit" draws a checkout that
    /// never happened.
    ///
    /// MUTATION-a: map a `None` symbolic name to `OnBranch { symbolic: "HEAD" }`
    /// and this goes red.
    /// MUTATION-b: fall through to `Unresolvable` when the oid is present and
    /// this goes red, differently — the commit is known, and dropping it throws
    /// away a fact the repo gave us.
    #[test]
    fn a_detached_head_is_recorded_as_detached_not_as_the_branch_it_sits_on() {
        let dir = repo();
        commit(dir.path(), "main");
        git_ok(dir.path(), &["checkout", "-q", "--detach"]);
        let at = git_says(dir.path(), &["rev-parse", "HEAD"]);

        let c = captured(dir.path());
        assert_eq!(c.head, HeadAtEvent::Detached { oid: at.clone() });
        assert_eq!(
            c.branches.get("main"),
            Some(&at),
            "detached at main's commit — the same commit, recorded as a different state"
        );
    }

    /// A repo before its first commit has a HEAD that names a branch which does
    /// not exist yet. That is an observation, not a failure and not a detached
    /// HEAD: the branch name is real and worth recording, the commit genuinely
    /// is not there.
    ///
    /// MUTATION-a: treat an unresolved HEAD as `CaptureFailed` and this goes
    /// red — a fresh repo is readable, and saying otherwise also loses the
    /// empty observation `a_repo_with_no_branches_captures_an_empty_map_not_a_failure`
    /// pins.
    /// MUTATION-b: record it as `Unresolvable`, discarding the symbolic name,
    /// and this goes red — the name is the whole content of this state.
    #[test]
    fn an_unborn_head_records_the_branch_it_names_with_no_commit() {
        let dir = repo(); // git init, no commits
                          // Don't assume the host's `init.defaultBranch`; ask git what it chose.
        let expected = git_says(dir.path(), &["symbolic-ref", "HEAD"]);

        let c = captured(dir.path());
        assert_eq!(
            c.head,
            HeadAtEvent::Unborn { symbolic: expected },
            "the branch HEAD would create, with no commit — not a failure"
        );
        assert!(c.branches.is_empty() && c.tags.entries.is_empty() && c.remotes.entries.is_empty());
    }

    /// HEAD read fine and held an object id nothing resolves. Neither a name
    /// nor a commit — and forcing it into `Detached` would mean inventing an
    /// oid to put there.
    ///
    /// MUTATION-a: `CaptureFailed` on the both-absent case and this goes red —
    /// the branches read perfectly well and must survive.
    /// MUTATION-b: collapse it into `Detached { oid: String::new() }` and this
    /// goes red on the variant, having manufactured a commit that never was.
    #[test]
    fn a_head_pointing_at_nothing_is_unresolvable_and_the_branches_survive() {
        let dir = repo();
        commit(dir.path(), "main");
        let tip = git_says(dir.path(), &["rev-parse", "main"]);
        // A well-formed object id with no object behind it.
        std::fs::write(dir.path().join(".git/HEAD"), "0".repeat(40) + "\n").unwrap();

        let c = captured(dir.path());
        assert_eq!(c.head, HeadAtEvent::Unresolvable);
        assert_eq!(
            c.branches.get("main"),
            Some(&tip),
            "the readable half of the repo is still an observation worth keeping"
        );
    }

    /// The state the design's probe did not reach, and the reason this enum has
    /// a fifth variant: the ref store opens and lists normally while HEAD
    /// *itself* will not read. Recording that as "no HEAD" would be the same
    /// lie the record-level enum forbids — and failing the whole capture would
    /// throw away branches that read perfectly well.
    ///
    /// MUTATION-a: let the HEAD read error propagate as a `RepoError` (what
    /// `read_history_materials` does) and this goes red — `main` disappears
    /// with it.
    /// MUTATION-b: record the failure as `Unresolvable`, dropping the reason,
    /// and this goes red — "we could not read it" and "it pointed nowhere" are
    /// different answers.
    #[test]
    fn an_unreadable_head_records_the_reason_while_the_branches_still_capture() {
        let dir = repo();
        commit(dir.path(), "main");
        let tip = git_says(dir.path(), &["rev-parse", "main"]);
        // Corrupt HEAD only — `.git/refs` stays intact, so the ref store opens
        // and lists as usual and the failure is HEAD's alone.
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();

        let c = captured(dir.path());
        let HeadAtEvent::Unreadable { reason } = &c.head else {
            panic!(
                "a HEAD that will not read must say so, not go quiet: {:?}",
                c.head
            );
        };
        assert!(!reason.is_empty(), "the failure must say what happened");
        assert_eq!(c.branches.get("main"), Some(&tip));
    }

    /// Tags, the other half of #449's gap. Both spellings are captured, and an
    /// annotated tag records the *commit* it peels to — not the tag object,
    /// which is on no commit graph a replay can draw.
    ///
    /// MUTATION-a: drop `RefKind::Tag` from the partition and this goes red.
    /// MUTATION-b: record the unpeeled id and this goes red on the annotated
    /// tag alone — which is why the fixture carries both flavours and asserts
    /// the tag object's own id is *not* what was stored.
    #[test]
    fn tags_are_captured_and_an_annotated_tag_records_the_commit_it_peels_to() {
        let dir = repo();
        commit(dir.path(), "main");
        git_ok(dir.path(), &["tag", "light"]);
        git_ok(dir.path(), &["tag", "-a", "annot", "-m", "annotated"]);

        let commit_oid = git_says(dir.path(), &["rev-parse", "annot^{commit}"]);
        let tag_object = git_says(dir.path(), &["rev-parse", "annot"]);
        assert_ne!(
            commit_oid, tag_object,
            "fixture check: an annotated tag must really be a separate object"
        );

        let c = captured(dir.path());
        assert_eq!(c.tags.entries.get("light"), Some(&commit_oid));
        assert_eq!(
            c.tags.entries.get("annot"),
            Some(&commit_oid),
            "an annotated tag records the commit it peels to"
        );
        assert_ne!(
            c.tags.entries.get("annot"),
            Some(&tag_object),
            "never the tag object's own id"
        );
        assert_eq!(c.tags.entries.len(), 2);
    }

    /// The distinction that makes these fields `Option` rather than bare maps,
    /// pinned from both sides: a line that predates #449 claims nothing, and a
    /// repo genuinely observed to have no tags records an empty map.
    ///
    /// Making them bare `BTreeMap`s does not compile — every construction site
    /// would have to claim an observation it does not have, so the type refuses
    /// the collapse before a test can. What a test still has to catch is the
    /// same lie told through serde, and its mirror image.
    ///
    /// MUTATION-a: give `tags` `#[serde(default = "..")]` returning
    /// `Some(CapturedRefs::default())` — "absent means there were none", the
    /// natural reading and the wrong one — and the first half goes red.
    /// MUTATION-b: emit `None` for a genuinely tagless repo (the "don't write
    /// empty objects" optimisation) and the second half goes red.
    #[test]
    fn absent_and_observed_empty_are_different_answers_about_tags() {
        // Half one: a pre-#449 line claims nothing.
        let old = repo();
        let path = old.path().join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"time\":1,\"kind\":\"Commit\",\"ref_name\":\"main\",\"summary\":\"old\",\
             \"old_oid\":\"a\",\"new_oid\":\"b\",\"source\":\"App\",\
             \"refs\":{\"status\":\"captured\",\"branches\":{\"main\":\"aaa\"}}}\n",
        )
        .unwrap();
        let read = read_all(old.path());
        assert_eq!(
            read.len(),
            1,
            "a #131-era line must not be dropped as corrupt"
        );
        let RefsAtEvent::Captured {
            branches,
            head,
            tags,
            remotes,
            ..
        } = read[0]
            .refs
            .clone()
            .expect("the branch capture still parses")
        else {
            panic!("expected a capture");
        };
        assert_eq!(branches.get("main").map(String::as_str), Some("aaa"));
        assert_eq!(head, None, "absent HEAD means nobody recorded one");
        assert_eq!(tags, None, "absent tags is not the observation 'no tags'");
        assert_eq!(remotes, None);

        // Half two: a real repo with no tags records an observation.
        let live = repo();
        commit(live.path(), "main");
        let c = captured(live.path());
        assert_eq!(
            c.tags,
            CapturedRefs {
                entries: BTreeMap::new(),
                truncated_at: None
            },
            "observed-and-empty, never absent — absent means nobody looked"
        );
        assert_eq!(c.remotes.entries, BTreeMap::new());
    }

    /// Remote-tracking refs are recorded (ADR 0070): the story a scrubber
    /// mostly tells is divergence, and local branches alone cannot tell it. The
    /// remote's symbolic default-branch pointer is not a tip and stays out.
    ///
    /// MUTATION-a: drop `RefKind::RemoteBranch` from the partition and this
    /// goes red.
    /// MUTATION-b: remove the `/HEAD` skip in the ref classification and this
    /// goes red on the exclusion assertion instead.
    #[test]
    fn remote_tracking_refs_are_captured_and_origin_head_is_not() {
        let origin = repo();
        commit(origin.path(), "main");
        let clone = tempfile::tempdir().unwrap();
        let dest = clone.path().join("work");
        git_ok(
            clone.path(),
            &[
                "clone",
                "-q",
                origin.path().to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        );
        // Fixture check: the pointer this test excludes must really exist.
        assert!(
            git_says(&dest, &["symbolic-ref", "refs/remotes/origin/HEAD"]).starts_with("refs/"),
            "fixture check: the clone must have created refs/remotes/origin/HEAD"
        );
        let tip = git_says(&dest, &["rev-parse", "refs/remotes/origin/main"]);

        let c = captured(&dest);
        assert_eq!(c.remotes.entries.get("origin/main"), Some(&tip));
        assert!(
            !c.remotes.entries.keys().any(|k| k.ends_with("/HEAD")),
            "the remote's symbolic default pointer is not a tip worth recording"
        );
    }

    /// Caps are per map, so one kind overflowing cannot evict another's
    /// entries, and each map reports its own overflow.
    ///
    /// MUTATION-a: share one budget across the maps (cap tags at what is left
    /// after branches) and this goes red — with 501 tags the branches are
    /// evicted.
    /// MUTATION-b: cap without setting `truncated_at` and this goes red — the
    /// silent-truncation defect the cap's own doc comment names.
    #[test]
    fn caps_are_per_map_and_each_reports_its_own_overflow() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "second");
        let tip = git_says(dir.path(), &["rev-parse", "HEAD"]);

        // 501 real tags, created in one git process — a genuine fixture.
        let over = REFS_PER_EVENT_CAP + 1;
        let mut stdin = String::new();
        for i in 0..over {
            stdin.push_str(&format!("create refs/tags/v{i:04} {tip}\n"));
        }
        let mut child = Command::new("git")
            .args(["update-ref", "--stdin"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("git runs");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "git update-ref failed");

        let c = captured(dir.path());
        assert_eq!(c.tags.entries.len(), REFS_PER_EVENT_CAP);
        assert_eq!(
            c.tags.truncated_at,
            Some(over),
            "the true count travels with the capped map"
        );
        assert_eq!(
            c.branches.len(),
            2,
            "one map's overflow must not evict another's entries"
        );
        assert_eq!(c.truncated_at, None, "branches did not overflow");
    }

    /// The lossless promise extends to the new kinds. Mirrors
    /// `a_deleted_branch_survives_in_the_event_that_predates_its_deletion`: git
    /// deletes a tag outright, so the journal is the only place its tip
    /// survives.
    ///
    /// MUTATION-a: have the read consult live refs instead of the stored map
    /// and this goes red.
    /// MUTATION-b: capture at read time rather than at append time and this
    /// goes red — the deletion would already have happened.
    #[test]
    fn a_deleted_tag_survives_in_the_event_that_predates_its_deletion() {
        let dir = repo();
        commit(dir.path(), "main");
        git_ok(dir.path(), &["tag", "doomed"]);
        let tip = git_says(dir.path(), &["rev-parse", "doomed^{commit}"]);
        append(dir.path(), &event("before the deletion"));
        git_ok(dir.path(), &["tag", "-d", "doomed"]);
        assert!(
            !git_says(dir.path(), &["tag", "--list"]).contains("doomed"),
            "fixture check: the tag must really be gone from the repo"
        );

        let read = read_all(dir.path());
        let RefsAtEvent::Captured { tags, .. } = read[0].refs.clone().expect("capture attached")
        else {
            panic!("expected a capture");
        };
        assert_eq!(
            tags.expect("tags recorded").entries.get("doomed"),
            Some(&tip),
            "the journal must still know the tag existed, and at which commit"
        );
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
            head: None,
            tags: None,
            remotes: None,
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

    /// A journal file holding `count` events, summaries `{prefix}0` upward,
    /// written directly rather than through `append` — 1,000+ real captures
    /// would spend the whole test in git.
    fn seed_journal(dir: &Path, prefix: &str, count: usize, trailing_newline: bool) -> PathBuf {
        let path = dir.join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = String::new();
        for i in 0..count {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&serde_json::to_string(&event(&format!("{prefix}{i}"))).unwrap());
        }
        if trailing_newline {
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A `Read + Seek` source that tallies every byte handed out, so a test
    /// can assert on I/O volume rather than on the answer alone.
    struct CountingCursor {
        inner: std::io::Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl CountingCursor {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: std::io::Cursor::new(bytes),
                bytes_read: 0,
            }
        }
    }

    impl Read for CountingCursor {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    impl Seek for CountingCursor {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// #464, half one: the window is the NEWEST `JOURNAL_READ_CAP` events and
    /// its boundary is exact — including the case a naive backward newline
    /// count gets wrong, a file of exactly the cap with no trailing newline.
    ///
    /// This is the off-by-one net. It says nothing about how much was read;
    /// `the_tail_window_reads_only_the_tail_of_the_journal` owns that.
    ///
    /// MUTATION: scan back for `cap` newlines instead of `cap + 1` and the
    /// oldest-kept assertion goes red — the window silently loses its first
    /// line to the partial-line fragment.
    #[test]
    fn the_read_window_is_the_newest_events_and_its_boundary_is_exact() {
        let dir = repo();
        seed_journal(dir.path(), "e", JOURNAL_READ_CAP + 50, true);
        let read = read_all(dir.path());
        assert_eq!(read.len(), JOURNAL_READ_CAP, "the cap bounds the answer");
        assert_eq!(
            read[0].summary, "e50",
            "the oldest kept event is the 51st, not the 52nd"
        );
        assert_eq!(
            read[JOURNAL_READ_CAP - 1].summary,
            format!("e{}", JOURNAL_READ_CAP + 49),
            "the newest event is the last line of the file"
        );

        // Exactly the cap, and no trailing newline: the shape that costs a
        // naive implementation its oldest line.
        let dir = repo();
        seed_journal(dir.path(), "x", JOURNAL_READ_CAP, false);
        let read = read_all(dir.path());
        assert_eq!(
            read.len(),
            JOURNAL_READ_CAP,
            "a missing trailing newline must not cost an event"
        );
        assert_eq!(read[0].summary, "x0");
    }

    /// #464's actual defect: the cap bounded *parsing*, not I/O — the whole
    /// journal was read into memory first, so disk cost grew without limit.
    ///
    /// The only test here that can tell the two implementations apart: a
    /// whole-file read returns exactly the same events, so it can only be
    /// caught by counting bytes.
    ///
    /// MUTATION: restore the old `read_to_string` body and this goes red on
    /// the byte count while every other journal test stays green.
    #[test]
    fn the_tail_window_reads_only_the_tail_of_the_journal() {
        // A pre-window prefix that dwarfs the window: 200 padded events the
        // cap must push out of view.
        let pad = "p".repeat(8 * 1024);
        let mut bytes: Vec<u8> = Vec::new();
        for i in 0..200 {
            writeln!(bytes, "{{\"old\":{i},\"pad\":\"{pad}\"}}").unwrap();
        }
        let prefix_len = bytes.len();
        for i in 0..(JOURNAL_READ_CAP + 5) {
            writeln!(bytes, "{{\"new\":{i}}}").unwrap();
        }
        let total = bytes.len();

        let mut source = CountingCursor::new(bytes);
        let window = tail_window(&mut source, JOURNAL_READ_CAP).unwrap();

        assert!(
            source.bytes_read < prefix_len,
            "read {} bytes of a {total}-byte journal whose pre-window prefix \
             alone is {prefix_len} — the read is not bounded by the cap",
            source.bytes_read
        );
        assert!(
            source.bytes_read <= 4 * TAIL_CHUNK,
            "the window is ~14 KiB; reading {} bytes for it means the backward \
             scan is not stopping where it should",
            source.bytes_read
        );

        // The tail may overshoot into the prefix by up to a chunk — that is
        // the design. What must hold is that the capped window inside it is
        // entirely post-prefix, and that it ends at the end of the file.
        let lines: Vec<&str> = window.lines().collect();
        assert!(
            lines.len() > JOURNAL_READ_CAP,
            "the window must overshoot the cap by at least one line so the \
             leading partial line is always trimmed away"
        );
        let capped = &lines[lines.len() - JOURNAL_READ_CAP..];
        assert!(
            capped.iter().all(|l| l.starts_with("{\"new\":")),
            "the capped window must hold only events newer than the prefix"
        );
        assert_eq!(
            *capped.last().unwrap(),
            format!("{{\"new\":{}}}", JOURNAL_READ_CAP + 4),
            "the tail must end at the end of the file"
        );
    }

    /// A consequence of the tail read, and an intended one: corruption older
    /// than the window can no longer blank the feed. `read_to_string` refused
    /// the whole file over one invalid byte anywhere in it, which contradicted
    /// this module's own rule that one bad line must not hide the history.
    ///
    /// MUTATION: decode the window with `String::from_utf8` (strict) instead
    /// of `from_utf8_lossy` and this goes red whenever the chunk boundary
    /// lands inside the corrupt prefix.
    #[test]
    fn corruption_older_than_the_window_no_longer_blanks_the_feed() {
        let dir = repo();
        let path = seed_journal(dir.path(), "c", JOURNAL_READ_CAP + 2, true);
        let good = std::fs::read(&path).unwrap();
        let mut bytes: Vec<u8> = vec![0xff, 0xfe, 0xff, b'\n'];
        bytes.extend_from_slice(&good);
        std::fs::write(&path, bytes).unwrap();

        let read = read_all(dir.path());
        assert_eq!(
            read.len(),
            JOURNAL_READ_CAP,
            "invalid bytes older than the window must not cost the feed"
        );
        assert_eq!(read[0].summary, "c2");
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
