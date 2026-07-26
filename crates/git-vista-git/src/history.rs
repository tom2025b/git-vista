//! Walking commit history and finding which commits are on a remote.
//!
//! Both walks open the repository in isolated mode, seed a revision walk from a
//! set of tips, and traverse newest-first — [`walk_history`] from HEAD and every
//! ref tip, [`read_remote_commits`] from remote-tracking refs alone.

use std::collections::HashSet;
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
pub fn remote_membership(
    path: &Path,
    requested: &HashSet<Oid>,
) -> Result<HashSet<Oid>, RepoError> {
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
}
