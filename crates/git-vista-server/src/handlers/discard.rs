//! Discard/delete endpoints for uncommitted working-tree changes (#219,
//! M2.18a): `POST /api/discard-tracked-paths` (`git checkout -- <paths>`) and
//! `POST /api/delete-untracked-paths` (`git clean -f -- <paths>`) — two
//! separate, typed operations (#71), never one endpoint parameterised by a
//! bool.
//!
//! `DeleteUntrackedPaths` is the first operation in this codebase with **no
//! journal-backed undo at all**: an untracked path was never written to
//! git's object database, so once it is gone there is nothing anywhere in
//! this repository to recover it from. Every guard between this handler and
//! the executor — the [`WorktreePath`] newtype's wire-boundary validation
//! here, the race re-verification, the symlink-containment check (both in
//! `crate::planner`, beside the two `exec_*` functions this endpoint
//! reaches) — exists because of that fact, not despite it.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{GitOperation, WorktreePath, WorktreePathsRequest};

use crate::planner;
use crate::state::reject_if_read_only;

/// Validate a [`WorktreePathsRequest`] into `Vec<WorktreePath>`: at least one
/// path, every one passes the newtype's own wire-boundary gate (non-empty,
/// not option-shaped, not absolute, no `..` component — see
/// [`WorktreePath`]'s doc comment for the full rule and why it is necessary
/// but not sufficient on its own), and no path repeated.
///
/// **Why deduplicate here (#284).** The executors count `paths.len()` to tell
/// the user how much they just destroyed. `git clean -f -- a.txt a.txt`
/// deletes and reports one file (verified against real git 2.43.0), and the
/// post-run survivor check finds nothing left behind, so a request naming
/// `a.txt` twice used to answer "Deleted 2 untracked paths permanently" for
/// one file — an overstated blast radius in the one operation where that
/// count is the user's only record of what is gone for good. The same
/// `paths.len()` count backs the discard response, so both endpoints get the
/// fix by sharing this function.
///
/// A duplicate is a client sloppiness, not an attack, so it is dropped rather
/// than refused — a 400 would break a caller that sent a harmless repeat.
/// Order is preserved (first occurrence wins) because the response and the
/// journal entry list paths back in the order they were asked for.
///
/// Exact-string equality is the whole equivalence, which the newtype makes
/// sufficient rather than merely convenient: it already rejects `.` and `..`
/// components and leading `/`, so the usual spellings that name one file two
/// ways (`./a.txt`, `dir/../a.txt`, `/abs/a.txt`) cannot reach here at all.
pub(crate) fn validate_paths(
    req: WorktreePathsRequest,
) -> Result<Vec<WorktreePath>, (StatusCode, String)> {
    if req.paths.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Name at least one path.".to_string(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::with_capacity(req.paths.len());
    for raw in req.paths {
        // Validate every entry, including a repeat of one already seen: a
        // malformed path is still a wire error however many times it arrives.
        let path = WorktreePath::new(raw).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// `git checkout -- <paths>` (#219): discard uncommitted changes to
/// already-tracked paths, restoring each to its checked-out (index, else
/// HEAD) version via [`GitOperation::DiscardTrackedPaths`]. Destructive, and
/// only *sometimes* undoable outside git-vista — see that variant's own doc
/// comment for the exact, qualified recovery story.
pub(crate) async fn discard_tracked_paths(
    Json(req): Json<WorktreePathsRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let paths = match validate_paths(req) {
        Ok(paths) => paths,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute(GitOperation::DiscardTrackedPaths { paths }).await
}

/// `git clean -f -- <paths>` (#219): delete untracked paths from the working
/// tree outright via [`GitOperation::DeleteUntrackedPaths`]. **Irrecoverable**
/// — see that variant's own doc comment.
pub(crate) async fn delete_untracked_paths(
    Json(req): Json<WorktreePathsRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let paths = match validate_paths(req) {
        Ok(paths) => paths,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute(GitOperation::DeleteUntrackedPaths { paths }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(paths: &[&str]) -> WorktreePathsRequest {
        WorktreePathsRequest {
            paths: paths.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn strs(paths: &[WorktreePath]) -> Vec<&str> {
        paths.iter().map(WorktreePath::as_str).collect()
    }

    /// #284 defect 2: a repeat collapses to one entry, and the entries that
    /// remain keep the order they arrived in — the response and the journal
    /// list paths back in request order.
    #[test]
    fn a_repeated_path_collapses_to_one_entry_in_request_order() {
        let paths = validate_paths(req(&["b.txt", "a.txt", "b.txt", "c.txt", "a.txt"])).unwrap();
        assert_eq!(strs(&paths), ["b.txt", "a.txt", "c.txt"]);
        // Non-adjacent repeats too, not merely consecutive ones: a
        // compare-with-previous dedupe would have left the second "b.txt"
        // and the second "a.txt" in place and passed a length-3 list off as
        // deduplicated only because the input happened to be sorted.
        let paths = validate_paths(req(&["a.txt", "a.txt"])).unwrap();
        assert_eq!(strs(&paths), ["a.txt"]);
    }

    /// A list with no repeats is untouched — the dedupe must not silently
    /// eat distinct paths that merely share a directory or a prefix.
    #[test]
    fn distinct_paths_all_survive_deduplication() {
        let paths = validate_paths(req(&["dir/a.txt", "dir/b.txt", "dir/a.txt.bak", "a.txt"]))
            .expect("none of these are duplicates of each other");
        assert_eq!(
            strs(&paths),
            ["dir/a.txt", "dir/b.txt", "dir/a.txt.bak", "a.txt"]
        );
    }

    /// The dedupe must not become a way to smuggle a malformed path past the
    /// newtype: every entry is validated, including one that repeats an entry
    /// already accepted.
    #[test]
    fn a_malformed_repeat_is_still_a_wire_error() {
        let (status, _why) = validate_paths(req(&["a.txt", "../escape.txt", "a.txt"]))
            .expect_err("a `..` component is refused wherever it sits in the list");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // Also when the malformed entry is itself the repeated one.
        assert!(validate_paths(req(&["/etc/passwd", "/etc/passwd"])).is_err());
    }

    /// Deduplication must not turn a request into an empty one: the
    /// at-least-one-path gate is about what the client asked for, and a
    /// repeat still names a real path.
    #[test]
    fn an_empty_request_is_refused_and_an_all_duplicates_request_is_not() {
        let (status, why) = validate_paths(req(&[])).expect_err("no paths named");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(why.contains("at least one"), "{why}");
        assert_eq!(
            strs(&validate_paths(req(&["a.txt", "a.txt"])).unwrap()),
            ["a.txt"]
        );
    }
}
