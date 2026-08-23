//! M4.31e (#431): conflict resolution survives a reconnect and a crash.
//!
//! # What "reconnect" means here, and why these tests can prove it
//!
//! A reconnect is a client that shares **nothing** with the one that started
//! the work: no cached plan, no remembered verb, no in-memory conflict set. On
//! this server that is not a scenario to simulate — it is the ordinary case.
//! `conflicts::scan` holds no module state of any kind, and
//! `conflicts::continuation` re-runs that scan on every call, so every answer
//! is derived from git's own on-disk state at the moment it is asked.
//!
//! The precedent is #81's `a_sequence_resumes_after_a_reconnect`, which proved
//! the same property for *sequences* by observing that `sequence_in_progress`
//! reads `CHERRY_PICK_HEAD`/`REVERT_HEAD` off disk and remembers nothing.
//! #431 is the equivalent for **partially resolved conflict sets**.
//!
//! # Why this is a proving slice, not a building one
//!
//! Nothing here adds a mechanism. The statelessness that satisfies #431's
//! acceptance criteria is a consequence of decisions already made — ADR 0063's
//! scan-derived model, and ADR 0064 decision 6 ("`shape` records no
//! `Precondition`; the executor re-reads instead"). What was missing was any
//! test that would notice if that stopped being true.
//!
//! That gap is exactly the shape this repository keeps finding: a property
//! everyone believes, held by nothing. A future change that cached the
//! conflict set for speed would satisfy every other test in the tree and break
//! only these.

use super::*;
use std::path::PathBuf;

fn run(repo: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed in {repo:?}"
    );
}

fn out(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed in {repo:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A fresh repository on `main` with two committed files.
fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);
    (dir, repo)
}

/// A merge left conflicted on **two** paths.
///
/// Two, not one, and that is load-bearing: a partially resolved set is only
/// observable when something can remain unresolved after something else is
/// resolved. With a single conflicted path "partially resolved" and "fully
/// resolved" are the same state, and the criterion this file exists for could
/// not be distinguished.
fn two_path_conflict() -> (tempfile::TempDir, PathBuf) {
    let (dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(repo.join("a.txt"), "theirs a\n").unwrap();
    std::fs::write(repo.join("b.txt"), "theirs b\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "theirs"]);
    run(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "ours a\n").unwrap();
    std::fs::write(repo.join("b.txt"), "ours b\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "ours"]);
    // Expected to fail — that is the fixture.
    let _ = std::process::Command::new("git")
        .args(["merge", "theirs"])
        .current_dir(&repo)
        .status();
    assert_eq!(
        out(&repo, &["ls-files", "-u", "--", "a.txt", "b.txt"])
            .lines()
            .filter(|l| !l.is_empty())
            .count(),
        6,
        "fixture: both paths conflicted, three stages each"
    );
    (dir, repo)
}

#[tokio::test]
async fn a_partially_resolved_conflict_set_is_recoverable_by_a_client_that_shares_no_state() {
    // #431's FIRST acceptance criterion.
    //
    // Resolve ONE of two conflicted paths, then ask the server what remains —
    // through a call that shares nothing with whatever resolved it. The second
    // path must still be there, and the first must be gone, both derived from
    // git rather than from anything remembered.
    //
    // MUTATION: have `conflicts::scan` cache its result, or have
    // `continuation` reuse a previous answer. Either makes the post-resolution
    // question return the pre-resolution set, and the assertions below fail.
    let (_dir, repo) = two_path_conflict();

    // Resolve a.txt by hand, exactly as a user would — no server involved.
    run(&repo, &["checkout", "--ours", "--", "a.txt"]);
    run(&repo, &["add", "--", "a.txt"]);

    // A completely fresh read. This is the "reconnected client": it holds no
    // reference to anything that produced the state above.
    let remaining = crate::conflicts::scan(&repo)
        .await
        .expect("the scan must succeed");

    assert_eq!(
        remaining.len(),
        1,
        "exactly one path should remain conflicted, got {:?}",
        remaining.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
    assert_eq!(
        remaining[0].path, "b.txt",
        "the UNRESOLVED path must be the one that survives"
    );
    assert!(
        remaining[0].ours.is_text() && remaining[0].theirs.is_text(),
        "and it must arrive fully described, not as a bare path — a reconnected \
         client has no cached stages to fall back on"
    );
}

#[tokio::test]
async fn unresolved_paths_still_block_continuation_after_a_reconnect() {
    // #431's SECOND acceptance criterion.
    //
    // The dangerous failure is the optimistic one: a reconnected client asks
    // "may I continue?" and is told yes because nothing remembered the
    // outstanding conflict. `Continuation::from_files`' own doc comment states
    // the rule this pins — an empty input means Clear, and that is only safe
    // because the caller actually looked.
    //
    // MUTATION: make `continuation` return Clear when it has no cached set, or
    // treat a scan error as an empty list. Both turn "I did not check" into a
    // green light to continue over an unresolved file.
    let (_dir, repo) = two_path_conflict();

    // Resolve one. The set is now partial — the exact state where an
    // optimistic answer would be most tempting and most wrong.
    run(&repo, &["checkout", "--theirs", "--", "a.txt"]);
    run(&repo, &["add", "--", "a.txt"]);

    let verdict = crate::conflicts::continuation(&repo)
        .await
        .expect("the continuation read must succeed");

    match verdict {
        git_vista_protocol::Continuation::Blocked {
            unresolved,
            unreadable,
        } => {
            assert_eq!(
                unresolved,
                vec!["b.txt".to_string()],
                "the outstanding path must be named, not merely counted"
            );
            assert!(
                unreadable.is_empty(),
                "nothing here is unreadable; a fault must not be invented"
            );
        }
        git_vista_protocol::Continuation::Clear => panic!(
            "a partially resolved set must BLOCK continuation — reporting Clear \
             here would let an operation proceed over an unresolved file"
        ),
    }

    // And the positive control: once the second path is resolved too, the same
    // stateless read must say Clear. Without this, every assertion above would
    // pass on an implementation that returned Blocked unconditionally.
    run(&repo, &["checkout", "--ours", "--", "b.txt"]);
    run(&repo, &["add", "--", "b.txt"]);
    assert_eq!(
        crate::conflicts::continuation(&repo).await.unwrap(),
        git_vista_protocol::Continuation::Clear,
        "with everything resolved the same read must clear the way"
    );
}

#[tokio::test]
async fn a_resolution_applied_but_not_committed_survives_and_is_visible() {
    // #431's THIRD acceptance criterion: "a resolution applied but not
    // committed is not lost, or is reported as lost — never silently
    // discarded."
    //
    // This is the crash case. A resolution stages content into the index and
    // writes the worktree; the commit that would seal it may never happen —
    // the process dies, the browser closes, the session ends. Both of those
    // writes are on disk, so the work survives by construction; what matters
    // is that a later reader can still SEE it rather than reporting the file
    // as untouched.
    //
    // MUTATION: have the resolution write only the worktree and skip the index
    // stage. The file would look resolved on disk while git still reported it
    // conflicted — the resolution silently lost at the next scan.
    let (_dir, repo) = two_path_conflict();

    // Resolve a.txt through the real production path, then simulate the crash
    // by simply never continuing the merge. Nothing cleans up; the process
    // "died" here.
    let (status, body) = crate::planner::plan_and_execute_in(
        &repo,
        None,
        (
            git_vista_protocol::RepositoryToken::new("11111111-1111-5111-8111-111111111111")
                .unwrap(),
            git_vista_protocol::WorktreeToken::new("22222222-2222-5222-8222-222222222222").unwrap(),
        ),
        git_vista_protocol::GitOperation::ResolveConflict {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            resolution: git_vista_protocol::conflict::Resolution::TakeOurs,
        },
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");

    // --- everything past this point is the "after the crash" reader ---

    // The worktree write survived.
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "ours a\n",
        "the resolved content must still be on disk after the crash"
    );

    // The index write survived too, and this is the half that would be
    // silently lost: a checkout without the `git add` leaves the path
    // conflicted, so the resolution would evaporate at the next scan.
    let staged = out(&repo, &["ls-files", "-s", "--", "a.txt"]);
    assert!(
        staged.contains(" 0\t"),
        "the resolution must be STAGED (stage 0), not merely written to the \
         worktree — got: {staged}"
    );

    // And a fresh, stateless read agrees: a.txt is done, b.txt is not.
    let remaining = crate::conflicts::scan(&repo).await.unwrap();
    assert_eq!(
        remaining
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        vec!["b.txt"],
        "after the crash the reader must see exactly the work that is left"
    );

    // The merge is still in progress — the resolution did not silently
    // complete something the user never asked to complete.
    assert!(
        repo.join(".git/MERGE_HEAD").exists(),
        "resolving one path must not have ended the merge"
    );
}
