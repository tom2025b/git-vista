//! Reading reflogs — every recorded movement of HEAD, the local branches and
//! the remote-tracking branches — for the activity feed.
//!
//! Reflogs are the only first-party record of *how* a ref moved (commit,
//! merge, rebase, reset, checkout, "update by push", …): history itself only
//! says where refs point now. Read natively via gix like every other read in
//! this crate; the messages are parsed into event kinds downstream, in
//! `git_vista_core::activity` (pure, unit-tested there).
//!
//! One caveat inherited from git itself: **deleting a branch deletes its
//! reflog**, so deletions never appear here — that's what the server's ref
//! snapshots exist for.

use std::path::Path;

use gix::refs::Category;

use git_vista_core::activity::ReflogEntry;

use crate::RepoError;

/// Read the reflogs of HEAD, every local branch and every remote-tracking
/// branch, newest entry first *per ref*, capped at `per_ref_limit` entries
/// each (a rebase can write dozens of lines; the feed only shows so much).
///
/// Refs without a reflog (none written yet, or `core.logAllRefUpdates` off)
/// simply contribute nothing — a sparse feed, not an error. The remote's
/// symbolic `…/HEAD` pointer is skipped like everywhere else.
pub fn read_reflogs(path: &Path, per_ref_limit: usize) -> Result<Vec<ReflogEntry>, RepoError> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut entries = Vec::new();

    // HEAD's own log: checkouts live only here, and commits/merges made on a
    // checked-out branch are mirrored here (the feed assembly de-duplicates).
    if let Ok(head) = repo.find_reference("HEAD") {
        collect_ref_log(&head, "HEAD", per_ref_limit, &mut entries);
    }

    // Same error posture as `read_refs`/`walk_history`: failing to open or
    // list the ref store is a real error; one unreadable ref is skipped loudly.
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
                eprintln!("git-vista: skipping an unreadable ref while reading reflogs: {e}");
                continue;
            }
        };
        let short = match reference.name().category_and_short_name() {
            Some((Category::LocalBranch, short)) => short.to_string(),
            Some((Category::RemoteBranch, short)) => {
                let name = short.to_string();
                if name.ends_with("/HEAD") {
                    continue; // the remote's symbolic default-branch pointer
                }
                name
            }
            _ => continue, // tags and the rest move rarely and aren't "activity"
        };
        collect_ref_log(&reference, &short, per_ref_limit, &mut entries);
    }

    Ok(entries)
}

/// Append up to `limit` of `reference`'s log lines (newest first) to `out`,
/// under the short display name `ref_name`. A ref with no log yields nothing;
/// a malformed line is skipped loudly rather than failing the whole feed.
fn collect_ref_log(
    reference: &gix::Reference<'_>,
    ref_name: &str,
    limit: usize,
    out: &mut Vec<ReflogEntry>,
) {
    // `log_iter().rev()` walks the on-disk log newest-first without loading
    // it whole. `Ok(None)` = the ref has no reflog — fine, nothing to add.
    let mut platform = reference.log_iter();
    let iter = match platform.rev() {
        Ok(Some(iter)) => iter,
        Ok(None) => return,
        Err(e) => {
            eprintln!("git-vista: couldn't open the reflog of {ref_name:?}: {e}");
            return;
        }
    };
    for line in iter.take(limit) {
        match line {
            Ok(line) => out.push(ReflogEntry {
                ref_name: ref_name.to_string(),
                time: line.signature.time.seconds,
                old_oid: line.previous_oid.to_string(),
                new_oid: line.new_oid.to_string(),
                message: line.message.to_string(),
            }),
            Err(e) => {
                eprintln!("git-vista: skipping a malformed reflog line of {ref_name:?}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::tests::{commit, fixture, git};
    use git_vista_core::activity::{parse_reflog_message, ActivityKind};

    /// Entries for one ref, newest first, as (kind, message) for easy asserts.
    fn kinds_of<'a>(entries: &'a [ReflogEntry], ref_name: &str) -> Vec<(ActivityKind, &'a str)> {
        entries
            .iter()
            .filter(|e| e.ref_name == ref_name)
            .map(|e| (parse_reflog_message(&e.message).0, e.message.as_str()))
            .collect()
    }

    #[test]
    fn fixture_reflogs_carry_the_expected_events() {
        let dir = fixture(); // A-B-C on main, D on feature, E merges feature
        let entries = read_reflogs(dir.path(), 100).unwrap();

        // main, newest first: the merge E, then commits C, B, A(initial).
        let main = kinds_of(&entries, "main");
        assert_eq!(main.len(), 4, "main: {main:?}");
        assert_eq!(main[0].0, ActivityKind::Merge);
        assert!(main[1..].iter().all(|(k, _)| *k == ActivityKind::Commit));

        // feature: its creation from B, then commit D — creation is the
        // oldest entry, D the newest.
        let feature = kinds_of(&entries, "feature");
        assert_eq!(feature.len(), 2, "feature: {feature:?}");
        assert_eq!(feature[0].0, ActivityKind::Commit);
        assert_eq!(feature[1].0, ActivityKind::BranchCreated);

        // HEAD saw the two checkouts (to feature and back to main).
        let head = kinds_of(&entries, "HEAD");
        let checkouts = head.iter().filter(|(k, _)| *k == ActivityKind::Checkout).count();
        assert_eq!(checkouts, 2, "HEAD: {head:?}");
    }

    #[test]
    fn entries_chain_old_to_new_and_are_newest_first() {
        let dir = fixture();
        let entries = read_reflogs(dir.path(), 100).unwrap();
        let main: Vec<_> = entries.iter().filter(|e| e.ref_name == "main").collect();
        // Newest-first per ref: each entry's old oid is the next entry's new oid.
        for pair in main.windows(2) {
            assert_eq!(pair[0].old_oid, pair[1].new_oid, "reflog chain broken");
        }
        // The oldest entry (the initial commit) starts from the null oid.
        assert!(main.last().unwrap().old_oid.bytes().all(|b| b == b'0'));
    }

    #[test]
    fn per_ref_limit_caps_each_log() {
        let dir = fixture();
        let entries = read_reflogs(dir.path(), 1).unwrap();
        let main_count = entries.iter().filter(|e| e.ref_name == "main").count();
        assert_eq!(main_count, 1, "one newest entry per ref");
        // And it's the newest (the merge), not the root commit.
        let newest = entries.iter().find(|e| e.ref_name == "main").unwrap();
        assert!(newest.message.starts_with("merge"));
    }

    #[test]
    fn remote_tracking_push_updates_are_read() {
        let dir = fixture();
        let p = dir.path();
        // Simulate a push: update-ref writes the remote-tracking ref with a
        // reflog line when logging is forced on for it.
        git(p, &["config", "core.logAllRefUpdates", "always"]);
        git(p, &["update-ref", "-m", "update by push", "refs/remotes/origin/main", "main"]);
        commit(p, "F after push", 7);

        let entries = read_reflogs(p, 100).unwrap();
        let origin = kinds_of(&entries, "origin/main");
        assert_eq!(origin.len(), 1, "origin/main: {origin:?}");
        assert_eq!(origin[0].0, ActivityKind::Push);
    }

    #[test]
    fn a_repo_without_reflogs_yields_an_empty_feed_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "core.logAllRefUpdates", "false"]);
        commit(p, "A quiet", 1);
        let entries = read_reflogs(p, 100).unwrap();
        assert!(entries.is_empty(), "no reflogs configured: {entries:?}");
    }
}
