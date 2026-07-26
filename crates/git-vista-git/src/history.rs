//! Walking commit history and finding which commits are on a remote.
//!
//! Every walk here opens the repository in isolated mode and seeds a revision
//! walk from a set of tips: [`walk_history`] traverses newest-first from HEAD and
//! every ref tip, [`read_remote_commits`] newest-first from remote-tracking refs
//! alone, and [`remote_membership`] answers exact remote reachability for a
//! bounded set of requested ids with no cap at all (M1.10, #63).

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::Path;

use gix::refs::Category;
use gix::revision::walk::Sorting;
use gix::traverse::commit::simple::CommitTimeOrder;

use git_vista_core::model::{CommitDetail, CommitSummary, Oid};

use crate::RepoError;

/// Walk a repository's history, newest commit first, up to `limit` commits.
///
/// The walk starts from HEAD and every reference tip (branches and tags), so
/// commits on side branches that aren't ancestors of HEAD still show up. Tags
/// are peeled to the commit they point at; refs that don't resolve are skipped.
/// An empty or unborn repository yields an empty list rather than an error.
pub fn walk_history(path: &Path, limit: usize) -> Result<Vec<CommitSummary>, RepoError> {
    // Open in isolated mode: read only the repository's own config, not the
    // user's global/system git config or environment. We only ever read history,
    // so external config is irrelevant, and ignoring it keeps the walk robust to
    // a malformed global config on the host.
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    // Seed the walk from HEAD and every ref tip, de-duplicated so a tip that is
    // both HEAD and a branch isn't queued twice.
    let mut seen = HashSet::new();
    let mut tips: Vec<gix::ObjectId> = Vec::new();
    let mut add_tip = |oid: gix::ObjectId, tips: &mut Vec<gix::ObjectId>| {
        if seen.insert(oid) {
            tips.push(oid);
        }
    };

    if let Ok(head) = repo.head() {
        if let Some(id) = head.id() {
            add_tip(id.detach(), &mut tips);
        }
    }
    // Seed from every ref tip. Failing to open or list the ref store is a real
    // error, not something to swallow: silently falling back to the HEAD tip alone
    // is exactly how "the visualiser shows only the branch I'm on" goes unnoticed
    // (issue #16), so surface it instead. A single ref that won't resolve to a
    // commit is logged to the local terminal and skipped, not dropped in silence.
    let platform = repo
        .references()
        .map_err(|e| RepoError::Walk(format!("opening the ref store: {e}")))?;
    let all = platform
        .all()
        .map_err(|e| RepoError::Walk(format!("listing refs: {e}")))?;
    for reference in all {
        let reference = match reference {
            Ok(r) => r,
            Err(e) => {
                eprintln!("git-vista: skipping an unreadable ref while walking history: {e}");
                continue;
            }
        };
        match reference.into_fully_peeled_id() {
            Ok(id) => add_tip(id.detach(), &mut tips),
            Err(e) => eprintln!("git-vista: skipping a ref that won't resolve to a commit: {e}"),
        }
    }

    // No tips means an empty/unborn repo: a valid, empty history.
    if tips.is_empty() {
        return Ok(Vec::new());
    }

    let walk = repo
        .rev_walk(tips)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()
        .map_err(|e| RepoError::Walk(e.to_string()))?;

    let mut commits = Vec::new();
    for info in walk.take(limit) {
        let info = info.map_err(|e| RepoError::Walk(e.to_string()))?;
        let commit = info.object().map_err(|e| RepoError::Walk(e.to_string()))?;

        let summary = commit
            .message()
            .map(|m| m.summary().to_string())
            .unwrap_or_default();
        let author = commit
            .author()
            .map(|a| a.name.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        let parents = info
            .parent_ids()
            .map(|p| Oid(p.detach().to_string()))
            .collect();

        commits.push(CommitSummary {
            id: Oid(info.id().detach().to_string()),
            parents,
            summary,
            author,
            time: info.commit_time(),
        });
    }

    Ok(commits)
}

// INTERMEDIATE SCAFFOLD (M1.10 #63 Task 4 Step 3) — the pre-existing newest-first
// walk, lifted behind the new signature so the Step 3/5 tests can run red against
// something real. It has no shallow adapter and no topological ordering; both are
// what the tests are about.
pub fn walk_history_topo<F>(
    path: &Path,
    sorted_tips: &[(String, Oid)],
    _shallow_boundaries: &[Oid],
    mut visit: F,
) -> Result<(), RepoError>
where
    F: FnMut(CommitSummary) -> ControlFlow<()>,
{
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let mut seen = HashSet::new();
    let mut tips: Vec<gix::ObjectId> = Vec::new();
    for (full_name, oid) in sorted_tips {
        let parsed = gix::ObjectId::from_hex(oid.0.as_bytes())
            .map_err(|e| RepoError::Walk(format!("malformed tip id for {full_name}: {e}")))?;
        if seen.insert(parsed) {
            tips.push(parsed);
        }
    }
    if tips.is_empty() {
        return Ok(());
    }

    let walk = repo
        .rev_walk(tips)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()
        .map_err(|e| RepoError::Walk(e.to_string()))?;

    for info in walk {
        let info = info.map_err(|e| RepoError::Walk(e.to_string()))?;
        let commit = info.object().map_err(|e| RepoError::Walk(e.to_string()))?;
        let summary = CommitSummary {
            id: Oid(info.id().detach().to_string()),
            parents: info.parent_ids().map(|p| Oid(p.detach().to_string())).collect(),
            summary: commit
                .message()
                .map(|m| m.summary().to_string())
                .unwrap_or_default(),
            author: commit
                .author()
                .map(|a| a.name.to_string())
                .unwrap_or_default()
                .trim()
                .to_string(),
            time: info.commit_time(),
        };
        if visit(summary).is_break() {
            break;
        }
    }
    Ok(())
}

/// Read one commit in full, by its hex id (Phase 10 — the detail panel).
///
/// Unlike [`walk_history`], which flattens each commit to the summary a row needs,
/// this loads everything the panel shows: the whole message body and both the
/// author and committer signatures (name, email, and their own times). Looked up
/// directly by id rather than walked, so it's cheap regardless of history size.
///
/// A malformed id, or one that isn't a commit in this repo, is a
/// [`RepoError::CommitNotFound`] (the caller maps it to a 404), not a read error.
pub fn read_commit(path: &Path, id: &str) -> Result<CommitDetail, RepoError> {
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let oid = gix::ObjectId::from_hex(id.as_bytes())
        .map_err(|e| RepoError::CommitNotFound(format!("{id}: {e}")))?;
    let commit = repo
        .find_commit(oid)
        .map_err(|e| RepoError::CommitNotFound(format!("{id}: {e}")))?;

    let author = commit
        .author()
        .map_err(|e| RepoError::Walk(e.to_string()))?;
    let committer = commit
        .committer()
        .map_err(|e| RepoError::Walk(e.to_string()))?;
    let message = commit
        .message_raw()
        .map_err(|e| RepoError::Walk(e.to_string()))?
        .to_string();
    let parents = commit
        .parent_ids()
        .map(|p| Oid(p.detach().to_string()))
        .collect();

    // The signature time is parsed leniently; a malformed one falls back to the
    // epoch rather than failing the whole read (the panel just shows a stale date).
    let seconds = |s: &gix::actor::SignatureRef| s.time().map(|t| t.seconds).unwrap_or(0);

    Ok(CommitDetail {
        id: Oid(commit.id.to_string()),
        parents,
        author_name: author.name.to_string().trim().to_string(),
        author_email: author.email.to_string().trim().to_string(),
        author_time: seconds(&author),
        committer_name: committer.name.to_string().trim().to_string(),
        committer_email: committer.email.to_string().trim().to_string(),
        commit_time: seconds(&committer),
        message,
        // Reading one commit says nothing about remotes; the caller stamps the
        // exact answer with `remote_membership` (M1.10, #63).
        on_remote: false,
    })
}

/// Exactly which of `requested` are reachable from a remote-tracking ref.
///
/// The bounded-query counterpart to [`read_remote_commits`] (M1.10, #63). That
/// one answers "which of the newest `limit` remote commits exist" — fine for the
/// legacy whole-history graph, useless for paged history, where the commit a user
/// opens is routinely far below the loaded page and far past any cap. This walks
/// with **no `HISTORY_LIMIT`** and stops the moment every requested id has been
/// found, so the cost is bounded by how deep the *deepest requested* commit is,
/// not by the size of history.
///
/// Seeds from every remote-tracking tip (`refs/remotes/*`), exactly as
/// [`read_remote_commits`] does. Traversal order is irrelevant here — only
/// membership is — so the walk uses gix's default breadth-first sorting rather
/// than paying to keep a commit-time priority queue.
///
/// Returns only the subset of `requested` that is on a remote; ids that are
/// malformed, absent, or simply not reachable are left out rather than erroring,
/// so one bad id in a page can't fail the whole read. An empty request, or a
/// repository with no remote-tracking refs, is an empty answer with no walk.
pub fn remote_membership(path: &Path, requested: &HashSet<Oid>) -> Result<HashSet<Oid>, RepoError> {
    if requested.is_empty() {
        return Ok(HashSet::new());
    }

    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    // What we're still looking for, keyed by parsed object id so the walk does no
    // string formatting per commit. A request that isn't a well-formed hex id can
    // never be found, so it simply never enters the map.
    let mut outstanding: std::collections::HashMap<gix::ObjectId, Oid> = requested
        .iter()
        .filter_map(|oid| {
            gix::ObjectId::from_hex(oid.0.as_bytes())
                .ok()
                .map(|parsed| (parsed, oid.clone()))
        })
        .collect();
    if outstanding.is_empty() {
        return Ok(HashSet::new());
    }

    let mut seen = HashSet::new();
    let mut tips: Vec<gix::ObjectId> = Vec::new();
    let platform = repo
        .references()
        .map_err(|e| RepoError::Walk(format!("opening the ref store: {e}")))?;
    let all = platform
        .all()
        .map_err(|e| RepoError::Walk(format!("listing refs: {e}")))?;
    for reference in all {
        let reference = match reference {
            Ok(r) => r,
            Err(e) => {
                eprintln!("git-vista: skipping an unreadable ref while scanning remotes: {e}");
                continue;
            }
        };
        if !matches!(
            reference.name().category_and_short_name(),
            Some((Category::RemoteBranch, _))
        ) {
            continue;
        }
        if let Ok(id) = reference.into_fully_peeled_id() {
            let oid = id.detach();
            if seen.insert(oid) {
                tips.push(oid);
            }
        }
    }

    if tips.is_empty() {
        return Ok(HashSet::new());
    }

    let walk = repo
        .rev_walk(tips)
        .sorting(Sorting::BreadthFirst)
        .all()
        .map_err(|e| RepoError::Walk(e.to_string()))?;

    let mut found = HashSet::new();
    for info in walk {
        let info = info.map_err(|e| RepoError::Walk(e.to_string()))?;
        if let Some(oid) = outstanding.remove(&info.id().detach()) {
            found.insert(oid);
            // Every requested id accounted for: stop, however much remote history
            // is left. This is the whole point of the helper.
            if outstanding.is_empty() {
                break;
            }
        }
    }
    Ok(found)
}

/// The set of commit ids (hex) reachable from the repository's remote-tracking
/// refs (`refs/remotes/*`) — i.e. the commits that are actually on a remote
/// (GitHub). The UI links a commit/ref only when its commit is in this set, so a
/// link never points at an unpushed object whose GitHub page would 404.
///
/// Mirrors [`walk_history`]'s seeding/sorting but starts only from remote tips,
/// capped at `limit` (the same cap the displayed history uses). That cap is safe:
/// a commit's rank among remote commits is never worse than its rank among all
/// commits, so any displayed (newest-`limit`) commit that is on a remote falls
/// within the newest `limit` remote commits too. Empty when there's no remote.
pub fn read_remote_commits(path: &Path, limit: usize) -> Result<HashSet<String>, RepoError> {
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let mut seen = HashSet::new();
    let mut tips: Vec<gix::ObjectId> = Vec::new();
    let platform = repo
        .references()
        .map_err(|e| RepoError::Walk(format!("opening the ref store: {e}")))?;
    let all = platform
        .all()
        .map_err(|e| RepoError::Walk(format!("listing refs: {e}")))?;
    for reference in all {
        let reference = match reference {
            Ok(r) => r,
            Err(e) => {
                eprintln!("git-vista: skipping an unreadable ref while scanning remotes: {e}");
                continue;
            }
        };
        // Remote-tracking refs only (`refs/remotes/<remote>/…`). The remote's
        // symbolic `…/HEAD` is harmless here — it just mirrors a branch tip we
        // already seed from.
        if !matches!(
            reference.name().category_and_short_name(),
            Some((Category::RemoteBranch, _))
        ) {
            continue;
        }
        if let Ok(id) = reference.into_fully_peeled_id() {
            let oid = id.detach();
            if seen.insert(oid) {
                tips.push(oid);
            }
        }
    }

    if tips.is_empty() {
        return Ok(HashSet::new());
    }

    let walk = repo
        .rev_walk(tips)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()
        .map_err(|e| RepoError::Walk(e.to_string()))?;

    let mut ids = HashSet::new();
    for info in walk.take(limit) {
        let info = info.map_err(|e| RepoError::Walk(e.to_string()))?;
        ids.insert(info.id().detach().to_string());
    }
    Ok(ids)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use git_vista_core::model::RefKind;
    use std::process::Command;

    /// Run a git command in `dir`, failing the test loudly if git errors.
    ///
    /// `pub(crate)` so the other modules' tests can build fixtures with it.
    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            // Deterministic identity + times so ordering assertions are stable.
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .status()
            .expect("git should be runnable");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Commit (empty tree) with a fixed timestamp so commit-time order is
    /// deterministic. `ts` is whole seconds since the epoch.
    pub(crate) fn commit(dir: &Path, message: &str, ts: i64) {
        let date = format!("@{ts} +0000"); // git's raw "epoch seconds" format
        Command::new("git")
            .args(["commit", "-q", "--allow-empty", "-m", message])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .status()
            .expect("git commit should run")
            .success()
            .then_some(())
            .expect("git commit failed");
    }

    /// Run a git command in `dir`, returning its trimmed stdout; fails the test
    /// loudly if git errors.
    pub(crate) fn git_out(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git should be runnable");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Run a git command in `dir` feeding it `stdin`, returning trimmed stdout.
    ///
    /// Needed for `git hash-object --literally`, the only way to plant a commit
    /// object whose parent header names something git would never write.
    fn git_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
        use std::io::Write;

        let mut child = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("git should be runnable");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(stdin)
            .expect("writing git stdin");
        let output = child.wait_with_output().expect("git should finish");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Merge `other` into the checked-out branch as a real merge commit with a
    /// pinned timestamp, so reconvergence fixtures are byte-stable.
    fn merge(dir: &Path, message: &str, ts: i64, other: &str) {
        let date = format!("@{ts} +0000");
        Command::new("git")
            .args(["merge", "-q", "--no-ff", "-m", message, other])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .status()
            .expect("git merge should run")
            .success()
            .then_some(())
            .expect("git merge failed");
    }

    /// Path of a loose object inside `repo`'s object database.
    fn loose_object_path(repo: &Path, oid: &Oid) -> std::path::PathBuf {
        repo.join(".git")
            .join("objects")
            .join(&oid.0[..2])
            .join(&oid.0[2..])
    }

    /// Every full-named ref tip plus a `HEAD` pseudo-tip, exactly as the server's
    /// history snapshot assembles them — but deliberately in the ref store's own
    /// enumeration order, so tests can shuffle it before canonicalising.
    fn enumerated_tips(repo: &Path) -> Vec<(String, Oid)> {
        let materials = crate::read_history_materials(repo).expect("reading history materials");
        let mut tips = materials.full_ref_targets.clone();
        if let Some(head) = materials.resolved_head.clone() {
            if !tips.iter().any(|(_, oid)| *oid == head) {
                tips.push(("HEAD".to_string(), head));
            }
        }
        tips
    }

    /// The canonical tip list `HistorySnapshot` supplies: sorted by full ref name
    /// then object id, with exact duplicates dropped. Object-id de-duplication is
    /// `walk_history_topo`'s own job and deliberately not done here.
    fn canonical_tips(mut tips: Vec<(String, Oid)>) -> Vec<(String, Oid)> {
        tips.sort_by(|a, b| (&a.0, &a.1 .0).cmp(&(&b.0, &b.1 .0)));
        tips.dedup();
        tips
    }

    /// The canonical shallow-boundary set `HistorySnapshot` supplies: sorted by
    /// object id and de-duplicated.
    fn canonical_boundaries(mut boundaries: Vec<Oid>) -> Vec<Oid> {
        boundaries.sort_by(|a, b| a.0.cmp(&b.0));
        boundaries.dedup();
        boundaries
    }

    /// Drain a whole `walk_history_topo` into a vector.
    fn topo_walk(
        repo: &Path,
        tips: &[(String, Oid)],
        boundaries: &[Oid],
    ) -> Result<Vec<CommitSummary>, RepoError> {
        let mut out = Vec::new();
        walk_history_topo(repo, tips, boundaries, |commit| {
            out.push(commit);
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// A **real** repository holding `count` linear commits on `main`, with
    /// `refs/remotes/origin/main` planted at the chain's tip.
    ///
    /// Built through one `git fast-import` process rather than `count` `git
    /// commit` spawns: the fixtures this backs are deliberately deeper than the
    /// retained 5,000-commit history cap, and five thousand process spawns would
    /// take minutes. Commit times ascend with depth, so the root is unambiguously
    /// the oldest commit and a capped newest-first walk truncates before it.
    pub(crate) fn deep_remote_chain(count: usize) -> tempfile::TempDir {
        use std::io::Write;

        assert!(count > 0, "a chain needs at least one commit");
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);

        let mut stream = String::new();
        for n in 1..=count {
            let message = format!("commit {n}\n");
            stream.push_str("commit refs/heads/main\n");
            stream.push_str(&format!("mark :{n}\n"));
            stream.push_str(&format!(
                "committer Ada Lovelace <ada@example.com> {} +0000\n",
                1_000 + n
            ));
            stream.push_str(&format!("data {}\n{message}", message.len()));
            if n > 1 {
                stream.push_str(&format!("from :{}\n", n - 1));
            }
            stream.push('\n');
        }
        // The remote-tracking ref sits exactly at the imported tip.
        stream.push_str("reset refs/remotes/origin/main\n");
        stream.push_str(&format!("from :{count}\n\n"));
        stream.push_str("done\n");

        let mut child = Command::new("git")
            .args(["fast-import", "--quiet", "--done"])
            .current_dir(p)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("git fast-import should run");
        child
            .stdin
            .take()
            .expect("fast-import stdin is piped")
            .write_all(stream.as_bytes())
            .expect("writing the fast-import stream");
        let status = child.wait().expect("git fast-import should finish");
        assert!(status.success(), "git fast-import failed");
        dir
    }

    /// Build a small fixture repo:
    ///
    /// ```text
    /// A(1) - B(2) - C(3) ---- E(6)   (main, E is a merge)
    ///          \            /
    ///           D(4) ------/         (feature)
    /// ```
    pub(crate) fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        commit(p, "A root", 1);
        commit(p, "B second", 2);
        commit(p, "C third", 3);
        git(p, &["checkout", "-q", "-b", "feature", "HEAD~1"]); // branch off B
        commit(p, "D on feature", 4);
        git(p, &["checkout", "-q", "main"]);
        // Merge feature into main with a fixed time; -m keeps it a merge commit.
        Command::new("git")
            .args(["merge", "-q", "--no-ff", "-m", "E merge feature", "feature"])
            .current_dir(p)
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_AUTHOR_DATE", "@6 +0000")
            .env("GIT_COMMITTER_DATE", "@6 +0000")
            .status()
            .expect("git merge should run")
            .success()
            .then_some(())
            .expect("git merge failed");
        dir
    }

    #[test]
    fn opening_a_non_repository_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = walk_history(dir.path(), 100).unwrap_err();
        assert!(matches!(err, RepoError::Open { .. }));
    }

    #[test]
    fn walks_newest_first_across_branches() {
        let dir = fixture();
        let history = walk_history(dir.path(), 100).unwrap();

        // All five commits, ordered by commit time newest-first.
        let summaries: Vec<&str> = history.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(
            summaries,
            vec![
                "E merge feature",
                "D on feature",
                "C third",
                "B second",
                "A root",
            ]
        );

        // Times are descending, the author came through, and the merge has two
        // parents while the root has none.
        assert!(history.windows(2).all(|w| w[0].time >= w[1].time));
        assert_eq!(history[0].author, "Ada Lovelace");
        assert!(history[0].is_merge(), "E is a merge");
        assert!(history.last().unwrap().parents.is_empty(), "A is a root");

        // Every non-dangling parent id refers to another walked commit.
        let ids: HashSet<&str> = history.iter().map(|c| c.id.0.as_str()).collect();
        for c in &history {
            for p in &c.parents {
                assert!(
                    ids.contains(p.0.as_str()),
                    "parent {} should be walked",
                    p.0
                );
            }
        }
    }

    #[test]
    fn read_commit_returns_full_detail() {
        let dir = fixture();
        let p = dir.path();
        // Grab the merge commit E's id from the walk, then read it in full.
        let history = walk_history(p, 100).unwrap();
        let e = history
            .iter()
            .find(|c| c.summary == "E merge feature")
            .unwrap();

        let detail = read_commit(p, &e.id.0).unwrap();
        assert_eq!(detail.id, e.id);
        assert_eq!(detail.author_name, "Ada Lovelace");
        assert_eq!(detail.author_email, "ada@example.com");
        assert_eq!(detail.committer_name, "Ada Lovelace");
        // The fixture pins both times to @6, so author and commit time agree.
        assert_eq!(detail.author_time, 6);
        assert_eq!(detail.commit_time, 6);
        // A merge has two parents, both present in the walk.
        assert_eq!(detail.parents.len(), 2);
        // The full message starts with the summary line.
        assert!(detail.message.starts_with("E merge feature"));
    }

    #[test]
    fn read_commit_rejects_unknown_or_malformed_ids() {
        let dir = fixture();
        let p = dir.path();
        // Well-formed but absent id, and a non-hex string: both are "not found".
        let absent = "0".repeat(40);
        assert!(matches!(
            read_commit(p, &absent),
            Err(RepoError::CommitNotFound(_))
        ));
        assert!(matches!(
            read_commit(p, "not-a-hash"),
            Err(RepoError::CommitNotFound(_))
        ));
    }

    #[test]
    fn limit_caps_the_number_of_commits() {
        let dir = fixture();
        let history = walk_history(dir.path(), 3).unwrap();
        assert_eq!(history.len(), 3);
        // Still the three newest.
        assert_eq!(history[0].summary, "E merge feature");
        assert_eq!(history[2].summary, "C third");
    }

    #[test]
    fn remote_commits_are_just_those_reachable_from_remote_tracking_refs() {
        let dir = fixture();
        let p = dir.path();

        // No remotes yet => nothing is "on the remote".
        assert!(read_remote_commits(p, 100).unwrap().is_empty());

        // Simulate having pushed `main` up to C only (origin/main -> C). The
        // remote thus has A, B, C but not the later merge E nor feature's D.
        git(p, &["update-ref", "refs/remotes/origin/main", "main~1"]);

        let history = walk_history(p, 100).unwrap();
        let id = |summary: &str| {
            history
                .iter()
                .find(|c| c.summary == summary)
                .unwrap_or_else(|| panic!("commit {summary:?} should exist"))
                .id
                .0
                .clone()
        };

        let remote = read_remote_commits(p, 100).unwrap();
        assert!(remote.contains(&id("A root")));
        assert!(remote.contains(&id("B second")));
        assert!(remote.contains(&id("C third")));
        assert!(!remote.contains(&id("D on feature")), "D is unpushed");
        assert!(!remote.contains(&id("E merge feature")), "E is unpushed");
    }

    /// M1.10 (#63): remote reachability for a *bounded requested set* is exact,
    /// with no `HISTORY_LIMIT`. The commit whose detail a user opens is very often
    /// far below whatever page is loaded — in a >5,000-commit repository the
    /// retained, capped [`read_remote_commits`] walk simply cannot see it, so the
    /// panel would call a pushed commit unpushed and refuse to link it.
    #[test]
    fn remote_membership_finds_requested_commit_beyond_5000() {
        let dir = deep_remote_chain(5_001);
        let p = dir.path();
        // One local commit past the remote tip, so "on a remote" is a real
        // question and not something a walk could answer "yes" to by accident.
        commit(p, "local tip never pushed", 9_000);

        let depth: usize = git_out(p, &["rev-list", "--count", "refs/remotes/origin/main"])
            .parse()
            .expect("rev-list --count prints a number");
        assert!(
            depth > 5_000,
            "the fixture must be deeper than the retained cap, got {depth}"
        );

        let oid = |spec: &str| Oid(git_out(p, &["rev-parse", spec]));
        let remote_tip = oid("refs/remotes/origin/main");
        let local_tip = oid("HEAD");
        // An arbitrary parent that a two-row page would never hold.
        let arbitrary = oid("refs/remotes/origin/main~3");
        let root = Oid(git_out(
            p,
            &["rev-list", "--max-parents=0", "refs/remotes/origin/main"],
        ));

        // The two rows a two-row page would own — neither request below is in it.
        let page: HashSet<Oid> = [local_tip.clone(), remote_tip].into_iter().collect();
        assert!(!page.contains(&arbitrary), "the fixture's premise");
        assert!(!page.contains(&root), "the fixture's premise");

        let requested: HashSet<Oid> = [root.clone(), arbitrary.clone(), local_tip.clone()]
            .into_iter()
            .collect();
        let found = remote_membership(p, &requested).unwrap();

        assert!(found.contains(&root), "the deep root is on the remote");
        assert!(
            found.contains(&arbitrary),
            "an arbitrary unloaded parent is on the remote"
        );
        assert!(
            !found.contains(&local_tip),
            "the unpushed local tip is not on the remote"
        );
        assert_eq!(found.len(), 2, "only the requested subset comes back");

        // ...and this is precisely what the retained capped walk cannot do.
        let capped = read_remote_commits(p, 5_000).unwrap();
        assert!(
            !capped.contains(&root.0),
            "a 5,000-commit cap truncates before the root — the reason this helper exists"
        );
    }

    #[test]
    fn an_unmerged_side_branch_is_fully_discovered() {
        // Issue #16's scenario: a freshly created local branch that's never been
        // merged into (or off an ancestor of) the checked-out branch. Its commits
        // aren't reachable from HEAD, so the walk must seed from the branch tip too,
        // and the branch must be reported as a ref — otherwise it's invisible.
        //
        //   B (main, HEAD)        X — Y (full-version)
        //    \                   /
        //     A ----------------/
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        commit(p, "A root", 1);
        commit(p, "B on main", 2);
        git(p, &["checkout", "-q", "-b", "full-version", "HEAD~1"]); // branch off A
        commit(p, "X on full-version", 3);
        commit(p, "Y on full-version", 4);
        git(p, &["checkout", "-q", "main"]); // HEAD back on main, side branch unmerged

        // The walk reaches the side branch's commits even though HEAD can't.
        let history = walk_history(p, 100).unwrap();
        let summaries: HashSet<&str> = history.iter().map(|c| c.summary.as_str()).collect();
        assert!(
            summaries.contains("X on full-version"),
            "side-branch commit X missing"
        );
        assert!(
            summaries.contains("Y on full-version"),
            "side-branch tip Y missing"
        );
        assert!(summaries.contains("B on main"));

        // ...and the branch itself is reported, tip resolving to Y.
        let refs = crate::read_refs(p).unwrap();
        let mut branches: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::Branch)
            .map(|r| r.name.as_str())
            .collect();
        branches.sort();
        assert_eq!(branches, vec!["full-version", "main"]);
        let tip = history
            .iter()
            .find(|c| c.summary == "Y on full-version")
            .unwrap();
        let full_version = refs.iter().find(|r| r.name == "full-version").unwrap();
        assert_eq!(
            full_version.target, tip.id,
            "full-version must point at its tip Y"
        );
    }

    /// Issue #28, end-to-end through gix: a branch created at an interior commit
    /// of an existing branch (`git branch aaa feature~1`) must render as a stub
    /// forking off that commit, not steal the lower half of `feature`'s line. This
    /// exercises the real ref/HEAD reading + layout, not just a hand-built graph.
    #[test]
    fn a_branch_created_at_an_interior_commit_renders_as_a_stub() {
        use git_vista_core::layout::layout_with_refs;

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        commit(p, "A root", 1);
        commit(p, "B second", 2);
        git(p, &["checkout", "-q", "-b", "feature", "HEAD"]); // feature off B
        commit(p, "F1 on feature", 3);
        commit(p, "F2 on feature", 4);
        git(p, &["checkout", "-q", "main"]);
        commit(p, "C on main", 5);
        commit(p, "D on main", 6); // main tip is the newest commit
                                   // Create `aaa` at feature's interior commit F1 (feature~1), without
                                   // switching to it — HEAD stays on main.
        git(p, &["branch", "aaa", "feature~1"]);

        let commits = walk_history(p, 100).unwrap();
        let refs = crate::read_refs(p).unwrap();
        let head_branch = crate::read_head_branch(p);
        assert_eq!(head_branch.as_deref(), Some("main"));

        let g = layout_with_refs(commits, refs, head_branch.as_deref());

        let color = |summary: &str| {
            g.rows
                .iter()
                .find(|r| r.commit.summary == summary)
                .unwrap_or_else(|| panic!("commit {summary:?} missing"))
                .color
        };

        // `aaa` owns nothing of its own → it's a stub, not a real line or a badge.
        assert!(
            g.stubs.iter().any(|s| s.name == "aaa"),
            "aaa should be a stub"
        );
        assert!(
            g.stubs.iter().all(|s| s.name != "feature"),
            "feature is a real line"
        );
        // `feature` keeps ONE colour down its whole line (F1 and F2 match) — it was
        // not split by aaa claiming F1.
        assert_eq!(
            color("F1 on feature"),
            color("F2 on feature"),
            "feature's colour must not be split by the interior branch"
        );
        // The checked-out branch (main) owns the trunk colour.
        assert_eq!(color("D on main"), 0, "main owns the trunk colour");
        assert_ne!(
            color("F2 on feature"),
            0,
            "feature is distinct from the trunk"
        );
    }

    // ---------------------------------------------------------------------
    // M1.10 (#63) Task 4 Step 3 — shallow-aware Topo `DateOrder` traversal.
    //
    // All three of these run against **real** repositories: a synthetic
    // membership fixture cannot show what gix does when an object genuinely
    // isn't in the store, which is the entire question here.
    // ---------------------------------------------------------------------

    /// A depth-one clone is the real shape of the problem: git keeps the tip's
    /// commit object byte-for-byte, so it still carries a `parent` header naming
    /// an object that was never fetched, and records that tip in `.git/shallow`.
    ///
    /// The walk must stop there because the boundary is *recorded*, and must
    /// still fail loudly when it isn't — shallow state is never inferred from a
    /// lookup that didn't work out.
    #[test]
    fn walk_history_topo_stops_at_recorded_shallow_boundary() {
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        git(s, &["init", "-q", "-b", "main"]);
        commit(s, "root", 1);
        commit(s, "middle", 2);
        commit(s, "tip", 3);

        // `--depth` is only honoured over a transport, hence the file:// URL.
        let home = tempfile::tempdir().unwrap();
        let url = format!("file://{}", s.display());
        git(
            home.path(),
            &["clone", "-q", "--depth", "1", url.as_str(), "shallow"],
        );
        let repo = home.path().join("shallow");

        let materials = crate::read_history_materials(&repo).unwrap();
        let boundaries = canonical_boundaries(materials.shallow.clone());
        assert_eq!(
            boundaries.len(),
            1,
            "a depth-one clone records exactly one boundary"
        );
        let boundary = boundaries[0].clone();

        // The fixture's premise: the boundary commit still names a parent, and
        // that parent genuinely is not in this repository's object database.
        let raw = git_out(&repo, &["cat-file", "-p", &boundary.0]);
        let absent = raw
            .lines()
            .find_map(|line| line.strip_prefix("parent "))
            .expect("the boundary commit still carries a parent header")
            .to_string();
        assert!(
            !Command::new("git")
                .args(["cat-file", "-e", &absent])
                .current_dir(&repo)
                .status()
                .expect("git should be runnable")
                .success(),
            "the named parent must genuinely be absent"
        );

        let tips = canonical_tips(enumerated_tips(&repo));
        let walked = topo_walk(&repo, &tips, &boundaries)
            .expect("the recorded boundary makes the traversal finite");

        assert_eq!(
            walked.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            vec![boundary.clone()],
            "only the boundary commit itself is emitted"
        );
        assert!(
            walked[0].parents.is_empty(),
            "a recorded boundary is delivered parentless, so it lays out as a root"
        );

        // Same repository, same tips, no recorded boundary: a hard error naming
        // the object that could not be read.
        let err = topo_walk(&repo, &tips, &[]).unwrap_err();
        let text = err.to_string();
        assert!(
            matches!(err, RepoError::Walk(_)),
            "an unreadable parent is a read error, got {err:?}"
        );
        assert!(
            text.contains(&absent),
            "the error must name the absent parent, got {text:?}"
        );
    }

    /// An object simply missing from the store, with nothing recording it as a
    /// boundary, must propagate. Recording the *missing object itself* does not
    /// excuse it either; only a commit whose own id is recorded loses parents.
    #[test]
    fn walk_history_topo_rejects_unrecorded_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        commit(repo, "root", 1);
        commit(repo, "middle", 2);
        commit(repo, "tip", 3);

        let oid = |spec: &str| Oid(git_out(repo, &["rev-parse", spec]));
        let tip = oid("HEAD");
        let middle = oid("HEAD~1");
        let root = oid("HEAD~2");
        let tips = canonical_tips(enumerated_tips(repo));

        std::fs::remove_file(loose_object_path(repo, &root))
            .expect("the root should be a loose object");

        let err = topo_walk(repo, &tips, &[]).unwrap_err();
        assert!(
            matches!(err, RepoError::Walk(_)),
            "a missing parent is a read error, got {err:?}"
        );
        assert!(
            err.to_string().contains(&root.0),
            "the error must name the missing object, got {err}"
        );

        // A recorded boundary whose *object* is gone is still an error: the
        // boundary commit has to be loaded and validated before it can be cut.
        let err = topo_walk(repo, &tips, std::slice::from_ref(&root)).unwrap_err();
        assert!(
            err.to_string().contains(&root.0),
            "a missing boundary object is still an error, got {err}"
        );

        // Recording the *child* is what cuts the edge, and it cuts only its own
        // parents: the tip above it keeps the real parent it names.
        let walked = topo_walk(repo, &tips, std::slice::from_ref(&middle))
            .expect("recording the commit whose parent is absent makes the walk finite");
        assert_eq!(
            walked.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            vec![tip.clone(), middle.clone()]
        );
        assert!(
            walked[1].parents.is_empty(),
            "the recorded boundary lost its parents"
        );
        assert_eq!(
            walked[0].parents,
            vec![middle],
            "a non-boundary commit keeps every parent it names"
        );
    }

    /// Two independent malformed-store cases that must never be mistaken for a
    /// shallow cut: a parent header pointing at a blob, and a parent object whose
    /// bytes on disk are corrupt.
    #[test]
    fn walk_history_topo_rejects_wrong_kind_or_corrupt_parent() {
        // --- wrong kind: the parent header names a blob -------------------
        {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            git(repo, &["init", "-q", "-b", "main"]);

            let blob = git_stdin(repo, &["hash-object", "-w", "--stdin"], b"not a commit");
            let tree = git_out(repo, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
            // `--literally` is the only way to write a commit git would refuse to
            // build, which is exactly the corrupt repository we need to survive.
            let bytes = format!(
                "tree {tree}\nparent {blob}\n\
                 author Ada Lovelace <ada@example.com> 5 +0000\n\
                 committer Ada Lovelace <ada@example.com> 5 +0000\n\n\
                 wrong-kind parent\n"
            );
            let head = git_stdin(
                repo,
                &["hash-object", "-t", "commit", "-w", "--stdin", "--literally"],
                bytes.as_bytes(),
            );
            git(repo, &["update-ref", "refs/heads/main", &head]);

            let tips = canonical_tips(enumerated_tips(repo));
            let err = topo_walk(repo, &tips, &[]).unwrap_err();
            assert!(
                matches!(err, RepoError::Walk(_)),
                "a wrong-kind parent is a read error, got {err:?}"
            );
            assert!(
                err.to_string().contains(&blob),
                "the error must name the offending object, got {err}"
            );
        }

        // --- corrupt: the parent object's bytes are garbage ---------------
        {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            git(repo, &["init", "-q", "-b", "main"]);
            commit(repo, "root", 1);
            commit(repo, "child", 2);

            let child = Oid(git_out(repo, &["rev-parse", "HEAD"]));
            let root = Oid(git_out(repo, &["rev-parse", "HEAD~1"]));
            let tips = canonical_tips(enumerated_tips(repo));

            // Loose objects are written read-only, so replace rather than patch.
            let path = loose_object_path(repo, &root);
            std::fs::remove_file(&path).expect("the root should be a loose object");
            std::fs::write(&path, b"this is not a zlib stream").expect("planting garbage");

            let err = topo_walk(repo, &tips, &[]).unwrap_err();
            assert!(
                matches!(err, RepoError::Walk(_)),
                "a corrupt parent is a read error, got {err:?}"
            );

            // The contrast that matters: a *declared* boundary at the child is
            // what stops the walk, not the corruption below it.
            let walked = topo_walk(repo, &tips, std::slice::from_ref(&child))
                .expect("a recorded boundary never loads the parent below it");
            assert_eq!(
                walked.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                vec![child]
            );
        }
    }

    // ---------------------------------------------------------------------
    // M1.10 (#63) Task 4 Step 5 — replay ordering at every page boundary.
    // ---------------------------------------------------------------------

    /// The plan's normative reconvergence graph:
    ///
    /// ```text
    ///        M(110)
    ///       /      \
    ///    A(90)    B(10)
    ///       \      /
    ///        C(100)
    /// ```
    ///
    /// extended with two equal-timestamp side tips (`alpha` → L(105), `zulu` →
    /// Z(105), both off C) and several distinct full ref names that resolve to
    /// the same object, so tip de-duplication is exercised too.
    fn reconvergence_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        commit(repo, "C base", 100);
        let c = git_out(repo, &["rev-parse", "HEAD"]);

        git(repo, &["checkout", "-q", "-b", "topic-a"]);
        commit(repo, "A older than its own parent", 90);
        git(repo, &["checkout", "-q", "-b", "topic-b", &c]);
        commit(repo, "B much older still", 10);

        git(repo, &["checkout", "-q", "topic-a"]);
        merge(repo, "M reconvergence", 110, "topic-b");
        let m = git_out(repo, &["rev-parse", "HEAD"]);

        // Three more full ref names at the very same object.
        git(repo, &["update-ref", "refs/heads/main", &m]);
        git(repo, &["update-ref", "refs/remotes/origin/main", &m]);
        git(repo, &["tag", "v1", &m]);

        // Two tips whose commit times are exactly equal.
        git(repo, &["checkout", "-q", "-b", "alpha", &c]);
        commit(repo, "L equal time", 105);
        git(repo, &["checkout", "-q", "-b", "zulu", &c]);
        commit(repo, "Z equal time", 105);
        git(repo, &["checkout", "-q", "topic-a"]);
        dir
    }

    /// Choice A: a page starting at row `n` re-runs the whole traversal and
    /// discards the first `n` rows. That is only sound if the traversal is a
    /// pure function of (repository, canonical tips, canonical boundaries), so
    /// this pins it at **every** boundary, in two page sizes, against one
    /// uninterrupted oracle — and pins the oracle itself to `DateOrder`
    /// semantics, which a plain newest-first walk does not satisfy.
    #[test]
    fn topo_date_order_replay_matches_uninterrupted_at_every_boundary() {
        let dir = reconvergence_fixture();
        let repo = dir.path();

        let enumerated = enumerated_tips(repo);
        let tips = canonical_tips(enumerated.clone());
        let oracle: Vec<Oid> = topo_walk(repo, &tips, &[])
            .expect("uninterrupted walk")
            .into_iter()
            .map(|c| c.id)
            .collect();

        // --- the oracle really is a topological date order ----------------
        let summary_of = |id: &Oid| git_out(repo, &["log", "-1", "--format=%s", &id.0]);
        let names: Vec<String> = oracle.iter().map(summary_of).collect();
        assert_eq!(names.len(), 6, "six commits: C, A, B, M, L, Z");
        assert_eq!(names[0], "M reconvergence", "the newest tip comes first");
        assert_eq!(
            names[names.len() - 1],
            "C base",
            "C is held back until all four of its children are out, even though \
             a newest-first sort would place it fourth of six"
        );
        let position = |summary: &str| {
            names
                .iter()
                .position(|n| n == summary)
                .unwrap_or_else(|| panic!("{summary:?} missing from {names:?}"))
        };
        assert!(
            position("B much older still") < position("C base"),
            "topology outranks the clock: B(10) precedes its own parent C(100)"
        );
        assert!(
            position("A older than its own parent") < position("C base"),
            "A(90) precedes its own parent C(100)"
        );
        assert_eq!(
            oracle.iter().collect::<HashSet<_>>().len(),
            oracle.len(),
            "no commit is emitted twice"
        );

        // --- one page: re-run, discard `skip`, take at most `size` ---------
        let page = |skip: usize, size: usize| -> Vec<Oid> {
            let mut discarded = 0usize;
            let mut out: Vec<Oid> = Vec::new();
            walk_history_topo(repo, &tips, &[], |commit| {
                if discarded < skip {
                    discarded += 1;
                    return ControlFlow::Continue(());
                }
                out.push(commit.id);
                if out.len() == size {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .expect("replayed walk");
            out
        };

        let len = oracle.len();
        for boundary in 0..=len {
            for size in [1usize, 7] {
                let mut collected: Vec<Oid> = Vec::new();
                let mut next = boundary;
                loop {
                    let rows = page(next, size);
                    if rows.is_empty() {
                        break;
                    }
                    assert!(rows.len() <= size, "a page never exceeds its size");
                    next += rows.len();
                    collected.extend(rows);
                }

                assert_eq!(
                    collected.iter().collect::<HashSet<_>>().len(),
                    collected.len(),
                    "no duplicate row scrolling from {boundary} at page size {size}"
                );
                assert_eq!(
                    collected.as_slice(),
                    &oracle[boundary..],
                    "no gap scrolling from {boundary} at page size {size}"
                );

                let mut rejoined = oracle[..boundary].to_vec();
                rejoined.extend(collected);
                assert_eq!(
                    rejoined, oracle,
                    "prefix + paged remainder must rebuild the uninterrupted order \
                     (boundary {boundary}, page size {size})"
                );
            }
        }

        // --- independent of how the ref store happened to enumerate -------
        let mut orders: Vec<Vec<(String, Oid)>> = vec![enumerated.clone()];
        let mut reversed = enumerated.clone();
        reversed.reverse();
        orders.push(reversed);
        for rotation in 1..enumerated.len() {
            let mut rotated = enumerated.clone();
            rotated.rotate_left(rotation);
            orders.push(rotated);
        }
        for order in orders {
            let canonical = canonical_tips(order);
            assert_eq!(
                canonical, tips,
                "canonicalisation erases enumeration order"
            );
            let replayed: Vec<Oid> = topo_walk(repo, &canonical, &[])
                .expect("walk from a re-enumerated ref set")
                .into_iter()
                .map(|c| c.id)
                .collect();
            assert_eq!(replayed, oracle, "the order is enumeration-independent");
        }
    }
}
