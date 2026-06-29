//! Native git-history reading for git-vista.
//!
//! Uses `gix` (pure-Rust gitoxide) to open a repository, seed a revision walk
//! from HEAD plus every ref tip, and traverse newest-first, mapping each commit
//! to a [`CommitSummary`]. This is deliberately a **separate, native-only crate**
//! rather than a module in `git-vista-core`: gix reads a filesystem repo and
//! can't compile for wasm, so keeping it out of `core` lets the browser frontend
//! depend on a clean, wasm-compatible core without any `#[cfg]` gating. The
//! native backend (the Tauri shell today) depends on this crate; the frontend
//! never does.
//!
//! It's UI-independent and headlessly unit-testable against fixture repositories.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use thiserror::Error;

use gix::refs::Category;
use gix::revision::walk::Sorting;
use gix::traverse::commit::simple::CommitTimeOrder;

use git_vista_core::model::{CommitSummary, GitRef, Oid, RefKind};

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("could not open a git repository at {path}: {message}")]
    Open { path: PathBuf, message: String },
    #[error("failed to read history: {0}")]
    Walk(String),
}

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
    let repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
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
    if let Ok(platform) = repo.references() {
        if let Ok(refs) = platform.all() {
            for reference in refs.filter_map(Result::ok) {
                if let Ok(id) = reference.into_fully_peeled_id() {
                    add_tip(id.detach(), &mut tips);
                }
            }
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
    let repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut seen = HashSet::new();
    let mut tips: Vec<gix::ObjectId> = Vec::new();
    if let Ok(platform) = repo.references() {
        if let Ok(all) = platform.all() {
            for reference in all.filter_map(Result::ok) {
                // Remote-tracking refs only (`refs/remotes/<remote>/…`). The
                // remote's symbolic `…/HEAD` is harmless here — it just mirrors a
                // branch tip we already seed from.
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

/// Read the repository's refs — HEAD, local & remote branches, and tags — each
/// peeled to the commit it ultimately points at, for badging and per-branch
/// colouring in the UI.
///
/// HEAD is always emitted (as [`RefKind::Head`], named `"HEAD"`) when it resolves
/// to a commit, whether it's on a branch or detached; when it's on a branch the
/// branch is emitted too, so a tip shows both. Refs that don't resolve to a
/// commit (an unborn HEAD, a broken ref) are skipped. Notes and worktree-private
/// refs are ignored.
pub fn read_refs(path: &Path) -> Result<Vec<GitRef>, RepoError> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut refs = Vec::new();

    // HEAD first, so it's the leading badge on its commit.
    if let Ok(head) = repo.head() {
        if let Some(id) = head.id() {
            refs.push(GitRef {
                name: "HEAD".to_string(),
                kind: RefKind::Head,
                target: Oid(id.detach().to_string()),
            });
        }
    }

    if let Ok(platform) = repo.references() {
        if let Ok(all) = platform.all() {
            for mut reference in all.filter_map(Result::ok) {
                // Classify by ref category, keeping only branches and tags. The
                // short name (owned now, before we consume the reference) is the
                // badge text: "main", "origin/main", "v1.0.0".
                let (kind, name) = match reference.name().category_and_short_name() {
                    Some((Category::LocalBranch, short)) => (RefKind::Branch, short.to_string()),
                    Some((Category::RemoteBranch, short)) => {
                        let name = short.to_string();
                        // Skip the remote's symbolic default-branch pointer
                        // (`refs/remotes/<remote>/HEAD`): it just mirrors another
                        // branch and isn't a branch tip worth badging.
                        if name.ends_with("/HEAD") {
                            continue;
                        }
                        (RefKind::RemoteBranch, name)
                    }
                    Some((Category::Tag, short)) => (RefKind::Tag, short.to_string()),
                    _ => continue, // HEAD pseudo-ref, notes, worktree-private, …
                };
                // Peel through tag objects to the commit the ref resolves to.
                if let Ok(id) = reference.peel_to_id() {
                    refs.push(GitRef {
                        name,
                        kind,
                        target: Oid(id.detach().to_string()),
                    });
                }
            }
        }
    }

    Ok(refs)
}

/// The GitHub web base URL for a repository's `origin` remote, e.g.
/// `"https://github.com/owner/repo"`, or `None` when there's no `origin`, the URL
/// can't be parsed, or the host isn't github.com. The UI turns this into per-commit
/// and per-ref links; `None` means it leaves the labels as plain text.
pub fn github_web_base(path: &Path) -> Option<String> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).ok()?;
    let url = repo.config_snapshot().string("remote.origin.url")?;
    web_base_from_remote(&url.to_string())
}

/// Parse a git remote URL into its GitHub web base (`https://github.com/owner/repo`),
/// or `None` if it isn't a github.com remote. Handles the common forms:
/// `git@github.com:owner/repo.git`, `https://github.com/owner/repo(.git)`, and
/// `ssh://git@github.com/owner/repo.git`. Pure (no I/O) so it's unit-testable.
fn web_base_from_remote(remote: &str) -> Option<String> {
    let s = remote.trim();
    // Reduce every form to "host/owner/repo…" by stripping scheme and any user@.
    let host_and_path = if let Some(idx) = s.find("://") {
        // scheme://[user@]host/path
        let after = &s[idx + 3..];
        after.split_once('@').map_or(after, |(_, h)| h).to_string()
    } else if let Some((user_host, path)) = s.split_once(':') {
        // scp-like: [user@]host:path
        let host = user_host.split_once('@').map_or(user_host, |(_, h)| h);
        format!("{host}/{path}")
    } else {
        return None;
    };

    let (host, path) = host_and_path.split_once('/')?;
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    // Strip a trailing "/" and the ".git" suffix, then require owner + repo.
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.splitn(3, '/');
    let owner = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    Some(format!("https://github.com/{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Run a git command in `dir`, failing the test loudly if git errors.
    fn git(dir: &Path, args: &[&str]) {
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
    fn commit(dir: &Path, message: &str, ts: i64) {
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

    /// Build a small fixture repo:
    ///
    /// ```text
    /// A(1) - B(2) - C(3) ---- E(6)   (main, E is a merge)
    ///          \            /
    ///           D(4) ------/         (feature)
    /// ```
    fn fixture() -> tempfile::TempDir {
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
                assert!(ids.contains(p.0.as_str()), "parent {} should be walked", p.0);
            }
        }
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

    #[test]
    fn read_refs_sees_head_branches_and_tags() {
        let dir = fixture();
        let p = dir.path();
        // Tag the root commit so there's a tag to find.
        git(p, &["tag", "v1.0", "HEAD~2"]);
        let refs = read_refs(p).unwrap();

        let names = |k: RefKind| {
            let mut v: Vec<String> = refs
                .iter()
                .filter(|r| r.kind == k)
                .map(|r| r.name.clone())
                .collect();
            v.sort();
            v
        };

        // HEAD is emitted exactly once, both branches and the tag are seen.
        assert_eq!(names(RefKind::Head), vec!["HEAD"]);
        assert_eq!(names(RefKind::Branch), vec!["feature", "main"]);
        assert_eq!(names(RefKind::Tag), vec!["v1.0"]);

        // On `main`, so HEAD resolves to the same commit as the `main` branch.
        let head = refs.iter().find(|r| r.kind == RefKind::Head).unwrap();
        let main = refs.iter().find(|r| r.name == "main").unwrap();
        assert_eq!(head.target, main.target);
    }

    #[test]
    fn parses_github_remote_urls_to_a_web_base() {
        let want = Some("https://github.com/owner/repo".to_string());
        // SSH (scp-like), with and without .git
        assert_eq!(web_base_from_remote("git@github.com:owner/repo.git"), want);
        assert_eq!(web_base_from_remote("git@github.com:owner/repo"), want);
        // HTTPS, with .git / trailing slash
        assert_eq!(web_base_from_remote("https://github.com/owner/repo.git"), want);
        assert_eq!(web_base_from_remote("https://github.com/owner/repo/"), want);
        // ssh:// URL form
        assert_eq!(web_base_from_remote("ssh://git@github.com/owner/repo.git"), want);
        // Case-insensitive host.
        assert_eq!(web_base_from_remote("git@GitHub.com:owner/repo.git"), want);
    }

    #[test]
    fn rejects_non_github_or_malformed_remotes() {
        assert_eq!(web_base_from_remote("git@gitlab.com:owner/repo.git"), None);
        assert_eq!(web_base_from_remote("https://example.com/owner/repo.git"), None);
        assert_eq!(web_base_from_remote("/local/path/repo.git"), None);
        assert_eq!(web_base_from_remote("git@github.com:owner.git"), None); // no repo
        assert_eq!(web_base_from_remote(""), None);
    }
}
