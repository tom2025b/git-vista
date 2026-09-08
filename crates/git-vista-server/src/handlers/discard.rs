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

use git_vista_core::identity::WorktreeId;
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

/// Validate the whole body: the repository selector the list was derived
/// from, and the paths themselves (#721).
///
/// **One function, called by both handlers, rather than two steps each
/// handler remembers to take.** The lesson is `ResolveConflictRequest`'s,
/// learned by mutation on #429: a validation step that sits beside a handler
/// is a step someone can delete, and a test written against the validator
/// will not notice. Neither endpoint here can reach the planner without
/// having produced a `WorktreeId` first, because the planner requires it.
/// A malformed selector is a `400`, using the read endpoints' vocabulary.
/// Omission is rejected by JSON extraction before either handler runs.
fn validate_body(
    req: WorktreePathsRequest,
) -> Result<(WorktreeId, Vec<WorktreePath>), (StatusCode, String)> {
    let expected = req
        .repo
        .parse::<WorktreeId>()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()))?;
    Ok((expected, validate_paths(req)?))
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
    let (expected, paths) = match validate_body(req) {
        Ok(validated) => validated,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute_matching(GitOperation::DiscardTrackedPaths { paths }, expected).await
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
    let (expected, paths) = match validate_body(req) {
        Ok(validated) => validated,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute_matching(GitOperation::DeleteUntrackedPaths { paths }, expected).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(paths: &[&str]) -> WorktreePathsRequest {
        scoped_req(&WorktreeId::from_git_dir("/x/.git").to_string(), paths)
    }

    fn scoped_req(repo: &str, paths: &[&str]) -> WorktreePathsRequest {
        WorktreePathsRequest {
            repo: repo.to_string(),
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

    // -----------------------------------------------------------------------
    // #721: the repository selector
    // -----------------------------------------------------------------------

    /// The wire lets `repo` be any string (it is a `String` on the DTO, like
    /// [`ResolveConflictRequest`](git_vista_protocol::ResolveConflictRequest)'s),
    /// so the endpoint is where "this is not an id" gets decided — and a
    /// *path* is the spelling that matters, because the whole identity scheme
    /// exists so a request can never name the server's filesystem.
    #[test]
    fn a_repository_selector_that_is_a_path_is_a_wire_error() {
        for bad in ["/etc", "..", "../../elsewhere", "", "not-a-uuid"] {
            let (status, why) = validate_body(scoped_req(bad, &["a.txt"]))
                .expect_err("{bad:?} must not be accepted as a repository id");
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}: {why}");
            assert!(why.contains("repository id"), "{bad:?}: {why}");
        }
        // The positive control, without which every assertion above would
        // hold on a `validate_body` that refused every selector outright.
        let (expected, paths) = validate_body(scoped_req(
            &WorktreeId::from_git_dir("/x/.git").to_string(),
            &["a.txt"],
        ))
        .expect("a real worktree id is accepted");
        assert_eq!(expected, WorktreeId::from_git_dir("/x/.git"));
        assert_eq!(strs(&paths), ["a.txt"]);
    }

    /// A repository whose `a.txt` is tracked-and-dirty and whose
    /// `scratch.txt` is untracked — the two shapes the two endpoints act on,
    /// deliberately spelled the same way in every fixture below so a path
    /// name carried from one to another is *valid* in the other.
    fn dirty_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, repo) = git_vista_fixtures::seeded_files(&[("a.txt", "a\n")], "seed");
        dirty_worktree(&repo);
        (dir, repo)
    }

    fn dirty_worktree(worktree: &std::path::Path) {
        std::fs::write(worktree.join("a.txt"), "edited\n").unwrap();
        std::fs::write(worktree.join("scratch.txt"), "junk\n").unwrap();
    }

    /// A repository plus a **linked worktree of the same repository**, both
    /// dirty in the same two paths.
    ///
    /// Two sibling worktrees rather than two unrelated repositories, on
    /// purpose. They share a [`RepositoryId`](git_vista_core::identity::RepositoryId)
    /// and differ only in [`WorktreeId`] — which is the case this app creates
    /// by design and switches between, and the one a coarser check (compare
    /// the *repository*, not the *worktree*) would wave straight through. A
    /// fixture built from two unrelated repositories cannot express that
    /// difference at all, so it would call a broken check correct.
    fn repo_with_a_dirty_sibling() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let (dir, repo) = dirty_repo();
        git_vista_fixtures::git::run(&repo, &["branch", "desk-branch"]);
        let desks = tempfile::tempdir().unwrap();
        let desk = desks.path().join("desk");
        git_vista_fixtures::git::run(
            &repo,
            &["worktree", "add", desk.to_str().unwrap(), "desk-branch"],
        );
        dirty_worktree(&desk);
        (dir, desks, repo, desk)
    }

    fn porcelain(worktree: &std::path::Path) -> String {
        git_vista_fixtures::git::out(worktree, &["status", "--porcelain"])
    }

    async fn keyed(
        name: &str,
        future: impl std::future::Future<Output = (StatusCode, String)>,
    ) -> (StatusCode, String) {
        crate::operations::with_key(
            git_vista_protocol::IdempotencyKey::new(name).unwrap(),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            future,
        )
        .await
    }

    /// **The test #721 exists for.** A path list built against one worktree,
    /// sent while a *sibling* worktree of the same repository is selected,
    /// naming a file that is dirty in both.
    ///
    /// This is the case `planner::verify_path_states` structurally cannot
    /// refuse: it asks "is `a.txt` tracked-dirty here?", the answer is yes,
    /// and the batch runs. So a test that let the second worktree be clean —
    /// or that used two unrelated repositories with different files — would
    /// pass on the *path-state* recheck alone and prove nothing about the
    /// selector. The fixture makes both worktrees dirty in the same two
    /// names, which is why the only thing that can refuse this is identity.
    ///
    /// Drives the real handler, so the whole chain is under test: the body's
    /// `repo`, `validate_body`'s parse, the entry point the handler chooses,
    /// and the planner's comparison against the selection it will act on.
    #[tokio::test]
    async fn a_colliding_path_name_from_a_sibling_worktree_is_refused() {
        crate::state::with_isolated_test_current(async {
            let (_dir, _desks, repo, desk) = repo_with_a_dirty_sibling();
            let main = crate::state::set_current(&repo, git_vista_protocol::RepoMode::Active)
                .expect("the main worktree registers");
            let sibling = crate::state::set_current(&desk, git_vista_protocol::RepoMode::Active)
                .expect("the linked worktree registers and becomes the selection");

            // The premise, asserted rather than assumed: one repository, two
            // worktrees. If these ever stopped holding, the test below would
            // be measuring something else entirely.
            assert_eq!(
                main.repository, sibling.repository,
                "the fixture must be two worktrees of ONE repository"
            );
            assert_ne!(main.worktree, sibling.worktree);

            // And the collision itself: the same two path names are dirty in
            // both, so the path-state recheck cannot tell them apart.
            assert_eq!(porcelain(&repo), porcelain(&desk));
            assert!(porcelain(&repo).contains("a.txt"));

            let before_main = std::fs::read_to_string(repo.join("a.txt")).unwrap();
            let before_desk = std::fs::read_to_string(desk.join("a.txt")).unwrap();

            let (status, body) = keyed(
                "issue-721-discard-from-a-sibling",
                discard_tracked_paths(Json(scoped_req(&main.worktree.to_string(), &["a.txt"]))),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::PRECONDITION_FAILED,
                "a batch aimed at another worktree must be refused: {body}"
            );
            assert_ne!(
                status,
                StatusCode::CONFLICT,
                "and not as the path-state 409 — that code means the file moved"
            );
            assert!(
                body.contains("different repository"),
                "the refusal must say which kind of wrong this is: {body}"
            );

            // Nothing ran, in either worktree. The refusal is the point, but
            // a refusal that had already destroyed something would be worse
            // than no refusal at all.
            assert_eq!(
                std::fs::read_to_string(repo.join("a.txt")).unwrap(),
                before_main
            );
            assert_eq!(
                std::fs::read_to_string(desk.join("a.txt")).unwrap(),
                before_desk
            );

            // The delete twin, on the untracked name that also collides.
            let (status, body) = keyed(
                "issue-721-delete-from-a-sibling",
                delete_untracked_paths(Json(scoped_req(
                    &main.worktree.to_string(),
                    &["scratch.txt"],
                ))),
            )
            .await;
            assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
            assert!(
                repo.join("scratch.txt").exists(),
                "the named worktree lost a file"
            );
            assert!(
                desk.join("scratch.txt").exists(),
                "the selected worktree lost a file"
            );
        })
        .await;
    }

    /// The positive control for the test above, and it is not optional: every
    /// assertion there would still hold on a server that refused these two
    /// endpoints unconditionally, which would be a worse bug than the one
    /// #721 fixes.
    ///
    /// The *matching* selector — the one naming the worktree that really is
    /// selected — must run the operation, on that worktree and no other.
    #[tokio::test]
    async fn a_matching_selector_runs_and_touches_only_the_selected_worktree() {
        crate::state::with_isolated_test_current(async {
            let (_dir, _desks, repo, desk) = repo_with_a_dirty_sibling();
            let _main = crate::state::set_current(&repo, git_vista_protocol::RepoMode::Active)
                .expect("the main worktree registers");
            let sibling = crate::state::set_current(&desk, git_vista_protocol::RepoMode::Active)
                .expect("the linked worktree becomes the selection");

            let (status, body) = keyed(
                "issue-721-discard-matching",
                discard_tracked_paths(Json(scoped_req(&sibling.worktree.to_string(), &["a.txt"]))),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                std::fs::read_to_string(desk.join("a.txt")).unwrap(),
                "a\n",
                "the selected worktree's file must have been discarded"
            );
            assert_eq!(
                std::fs::read_to_string(repo.join("a.txt")).unwrap(),
                "edited\n",
                "the sibling must be untouched — the selector is a precondition, not an address"
            );
        })
        .await;
    }

    /// Opposite of the old gap-pinning test: extraction refuses omission for
    /// BOTH real endpoints, before any path can reach git.
    #[tokio::test]
    async fn an_omitted_selector_is_refused_without_touching_the_selection() {
        use axum::{body::Body, http::Request, routing::post, Router};
        use tower::ServiceExt;
        crate::state::with_isolated_test_current(async {
            let (_dir, repo) = dirty_repo();
            crate::state::set_current(&repo, git_vista_protocol::RepoMode::Active).unwrap();
            let app = Router::new()
                .route("/api/discard-tracked-paths", post(discard_tracked_paths))
                .route("/api/delete-untracked-paths", post(delete_untracked_paths));
            let before = porcelain(&repo);
            for (route, path) in [
                ("discard-tracked-paths", "a.txt"),
                ("delete-untracked-paths", "scratch.txt"),
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::post(format!("/api/{route}"))
                            .header("content-type", "application/json")
                            .body(Body::from(serde_json::json!({"paths": [path]}).to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(porcelain(&repo), before);
            }
        })
        .await;
    }

    /// Entry point for `failure-atlas mutation_check` over the wasm client.
    ///
    /// The ordinary host suite cannot compile the menu/confirmation/dispatch
    /// path. This ignored harness builds that path and runs only #733's browser
    /// spec, so mutations in the code under test are compiled rather than
    /// judged by a source census. `mutation_check` already owns a separate
    /// atlas lock; removing its inherited lock name lets these nested buildlock
    /// calls use the normal build slot instead of waiting on their parent.
    #[test]
    #[ignore = "failure-atlas compiles and drives the browser client through this harness"]
    fn captured_selector_browser_contract_for_mutation_check() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target");
        let run = |args: &[&str]| {
            std::process::Command::new("buildlock")
                .args(args)
                .current_dir(&root)
                .env_remove("BUILDLOCK_FILE")
                .env_remove("NO_COLOR")
                .env("CARGO_TARGET_DIR", &target)
                .status()
                .unwrap_or_else(|e| panic!("could not run buildlock {args:?}: {e}"))
        };

        assert!(
            run(&["trunk", "build", "--config", "crates/git-vista/Trunk.toml"]).success(),
            "the mutated wasm client must compile before its browser test runs"
        );
        assert!(
            run(&["./dev", "browser", "destructive-selector.spec.mjs"]).success(),
            "the captured-selector browser contract failed"
        );
    }
}
