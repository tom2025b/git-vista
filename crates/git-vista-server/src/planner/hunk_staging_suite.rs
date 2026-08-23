//! #214 (M2.17c): line-level and hunk-level staging/unstaging — a selected
//! hunk stages alone, an entire-file selection, a crossing line selection
//! reordering content exactly as `git apply` does, refusals when the
//! underlying diff has moved out from under a plan, CRLF byte-exactness,
//! and unstaging a content hunk of a renamed file without disturbing the
//! rename itself.

use super::staging_exec::exec_stage_selection;
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

/// A fresh repository on branch `main` with one committed file and a
/// clean working tree.
fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);
    (dir, repo)
}

/// Capture one git command's stdout in a fixture repo.
fn run_out(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed in {repo:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// A committed 20-line file plus edits at both ends — far enough apart
/// that `git diff` emits two hunks.
fn repo_with_two_hunks() -> (tempfile::TempDir, PathBuf) {
    let (dir, repo) = seeded_repo();
    let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    std::fs::write(repo.join("a.txt"), &body).unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "twenty lines"]);
    let edited = body
        .replace("line 2\n", "line 2 changed\n")
        .replace("line 18\n", "line 18 changed\n");
    std::fs::write(repo.join("a.txt"), edited).unwrap();
    (dir, repo)
}

/// The wire plan for "hunk `index` of `path`", anchored from the parsed
/// diff itself (the same way a client copies anchors out of the served
/// diff).
fn plan_for_hunk_at(
    parsed: &git_vista_protocol::ParsedPatch,
    path: &str,
    index: u32,
    direction: git_vista_protocol::StageDirection,
) -> git_vista_protocol::PatchPlan {
    let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
        panic!("expected a hunks-shaped file");
    };
    let h = &hunks[index as usize];
    git_vista_protocol::PatchPlan {
        repository: RepositoryToken::new("test-repo").unwrap(),
        worktree: WorktreeToken::new("test-worktree").unwrap(),
        generation: GenerationToken::new("diff-v1:test").unwrap(),
        direction,
        files: vec![git_vista_protocol::FileSelection {
            path: path.to_string(),
            selection: git_vista_protocol::SelectionShape::Hunks {
                hunks: vec![git_vista_protocol::HunkRef {
                    index,
                    old_start: h.old_start,
                    new_start: h.new_start,
                }],
            },
        }],
    }
}

/// [`plan_for_hunk_at`] for the common case, `a.txt`.
fn plan_for_hunk(
    parsed: &git_vista_protocol::ParsedPatch,
    index: u32,
    direction: git_vista_protocol::StageDirection,
) -> git_vista_protocol::PatchPlan {
    plan_for_hunk_at(parsed, "a.txt", index, direction)
}

/// The wire plan for "these specific `lines` of hunk `index` of a.txt"
/// (#214) — the line-level sibling of [`plan_for_hunk`].
fn plan_for_lines(
    parsed: &git_vista_protocol::ParsedPatch,
    index: u32,
    lines: Vec<u32>,
    direction: git_vista_protocol::StageDirection,
) -> git_vista_protocol::PatchPlan {
    let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
        panic!("expected a hunks-shaped file");
    };
    let h = &hunks[index as usize];
    git_vista_protocol::PatchPlan {
        repository: RepositoryToken::new("test-repo").unwrap(),
        worktree: WorktreeToken::new("test-worktree").unwrap(),
        generation: GenerationToken::new("diff-v1:test").unwrap(),
        direction,
        files: vec![git_vista_protocol::FileSelection {
            path: "a.txt".to_string(),
            selection: git_vista_protocol::SelectionShape::Lines {
                hunks: vec![git_vista_protocol::HunkLines {
                    hunk: git_vista_protocol::HunkRef {
                        index,
                        old_start: h.old_start,
                        new_start: h.new_start,
                    },
                    lines,
                }],
            },
        }],
    }
}

/// A committed multi-line file (`a.txt`) with an uncommitted edit
/// spanning two adjacent single-line replacements — far enough apart in
/// *content* but close enough in *position* that `git diff` emits one
/// hunk with more than one added/removed line, so a line-level selection
/// can pick a genuine subset of it (#214). `b.txt` is a second tracked,
/// unmodified file a drift test can mutate on its own.
fn repo_with_multiline_hunk() -> (tempfile::TempDir, PathBuf) {
    let (dir, repo) = seeded_repo();
    let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    std::fs::write(repo.join("a.txt"), &body).unwrap();
    std::fs::write(repo.join("b.txt"), "unrelated\n").unwrap();
    run(&repo, &["add", "a.txt", "b.txt"]);
    run(&repo, &["commit", "-q", "-m", "ten lines plus b.txt"]);
    let edited = body
        .replace("line 4\n", "line 4 changed\n")
        .replace("line 5\n", "line 5 changed\n");
    std::fs::write(repo.join("a.txt"), edited).unwrap();
    (dir, repo)
}

/// A file renamed with further content edits, fully staged (`git add
/// -A` of a filesystem `mv` plus an edit) — the only way this server's
/// staging surface can ever actually present a `FileDiff::Hunks` entry
/// whose `old_path != new_path` (see
/// `unstaging_a_content_hunk_of_a_renamed_file_reverses_only_the_content`'s
/// doc for why).
fn repo_with_staged_rename_and_edit() -> (tempfile::TempDir, PathBuf) {
    let (dir, repo) = seeded_repo();
    let body: String = (1..=6).map(|i| format!("line {i}\n")).collect();
    std::fs::write(repo.join("a.txt"), &body).unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "six lines"]);
    std::fs::rename(repo.join("a.txt"), repo.join("renamed.txt")).unwrap();
    let edited = body.replace("line 3\n", "line 3 changed\n");
    std::fs::write(repo.join("renamed.txt"), edited).unwrap();
    run(&repo, &["add", "-A"]);
    (dir, repo)
}

/// M2.17b acceptance, the mechanism end to end on a real repository:
/// building the selected patch from git's own diff and applying it
/// `--cached` stages exactly the selected hunk — the other hunk stays a
/// worktree-only edit.
#[tokio::test]
async fn a_selected_hunk_stages_alone_and_the_rest_stays_unstaged() {
    let (_dir, repo) = repo_with_two_hunks();
    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    let plan = plan_for_hunk(&parsed, 0, git_vista_protocol::StageDirection::Stage);
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
    assert!(built.patch.contains("line 2 changed"));
    assert!(!built.patch.contains("line 18 changed"));

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    let cached = run_out(&repo, &["diff", "--cached", "--no-color"]);
    assert!(cached.contains("line 2 changed"), "{cached}");
    assert!(!cached.contains("line 18 changed"), "{cached}");
    let worktree = run_out(&repo, &["diff", "--no-color"]);
    assert!(worktree.contains("line 18 changed"), "{worktree}");
    assert!(!worktree.contains("line 2 changed"), "{worktree}");
}

/// The reverse leg: with both hunks staged, unstaging one (built from
/// the index-vs-HEAD base per the direction contract) moves exactly it
/// back to worktree-only.
#[tokio::test]
async fn unstaging_a_selected_hunk_reverses_only_it() {
    let (_dir, repo) = repo_with_two_hunks();
    run(&repo, &["add", "a.txt"]);
    let diff = run_out(&repo, &["diff", "--cached", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    let plan = plan_for_hunk(&parsed, 0, git_vista_protocol::StageDirection::Unstage);
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Unstage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Unstage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    let cached = run_out(&repo, &["diff", "--cached", "--no-color"]);
    assert!(!cached.contains("line 2 changed"), "{cached}");
    assert!(cached.contains("line 18 changed"), "{cached}");
    let worktree = run_out(&repo, &["diff", "--no-color"]);
    assert!(worktree.contains("line 2 changed"), "{worktree}");
}

/// The pathspec leg: an entire-file selection stages its file whole and
/// leaves other modified files untouched.
#[tokio::test]
async fn an_entire_file_selection_stages_only_its_pathspec() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("c.txt"), "c\n").unwrap();
    run(&repo, &["add", "c.txt"]);
    run(&repo, &["commit", "-q", "-m", "second file"]);
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
    std::fs::write(repo.join("c.txt"), "c changed\n").unwrap();

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &live.generation,
        "",
        &["c.txt".to_string()],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    let cached = run_out(&repo, &["diff", "--cached", "--name-only"]);
    assert_eq!(cached.trim(), "c.txt");
    let worktree = run_out(&repo, &["diff", "--name-only"]);
    assert_eq!(worktree.trim(), "a.txt");
}

// --- #214 (M2.17c): line-level staging ---------------------------------

/// M2.17c acceptance, line-level mechanism end to end on a real
/// repository: within a single-line replacement (`repo_with_two_hunks`'s
/// first hunk — `line 2` → `line 2 changed`, its second hunk at `line
/// 18` untouched throughout), selecting only the ADDED line reclassifies
/// the removed line to context (so the old content stays present) and
/// adds the new content alongside it. The clean, non-crossing case; see
/// `a_crossing_line_selection_reorders_content_exactly_as_git_apply_does`
/// below for the case where positional reconstruction does something
/// more surprising.
#[tokio::test]
async fn a_line_selection_stages_only_the_selected_replacement() {
    let (_dir, repo) = repo_with_two_hunks();
    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    // Hunk 0's lines: 0 context "line 1", 1 removed "line 2", 2 added
    // "line 2 changed", 3-5 context (verified against real `git diff`
    // output). Select only the added line.
    let plan = plan_for_lines(
        &parsed,
        0,
        vec![2],
        git_vista_protocol::StageDirection::Stage,
    );
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
    assert_eq!(
        built.patch,
        "--- a/a.txt\n\
             +++ b/a.txt\n\
             @@ -1,5 +1,6 @@\n\
             \x20line 1\n\
             \x20line 2\n\
             +line 2 changed\n\
             \x20line 3\n\
             \x20line 4\n\
             \x20line 5\n"
    );
    assert!(!built.patch.contains("line 18"));

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    // The index now holds BOTH lines: the untouched original and the
    // freshly staged addition, in that order.
    let staged_content = run_out(&repo, &["show", ":a.txt"]);
    let staged_lines: Vec<&str> = staged_content.lines().collect();
    assert_eq!(staged_lines[0], "line 1");
    assert_eq!(staged_lines[1], "line 2");
    assert_eq!(staged_lines[2], "line 2 changed");
    // The other hunk was never touched.
    assert!(!run_out(&repo, &["diff", "--cached", "--no-color"]).contains("line 18"));
}

/// #214: when a line-level selection "crosses" a diff's own grouping —
/// here the hunk emits both removed lines before both added lines
/// (`-line4 -line5 +line4changed +line5changed`, not interleaved
/// remove/add pairs), which is exactly what git's diff algorithm does
/// for two adjacent single-line replacements — selecting a subset that
/// spans the boundary reorders content on the new side.
///
/// **Confirmed against real `git apply`, not assumed.** Outside this
/// suite: hand-built the exact sub-hunk text `append_sub_hunk` produces
/// for this selection, ran `git apply --cached --whitespace=nowarn
/// --recount` against a real repository. The resulting index content is
/// `line 5` immediately followed by `line 4 changed` — reordered
/// relative to the file's original line order. This is an inherent
/// property of the unified-diff format itself (new-side content order
/// is exactly the top-to-bottom order of context+added lines in the
/// hunk body — module doc) applied to a diff that happened to group its
/// removes and adds separately; it is not a defect in `append_sub_hunk`.
/// A real user hand-editing this same hunk in `git add -p`'s `e` (edit)
/// mode would produce byte-identical text and hit the identical
/// reordering — confirmed too: `git add -p`'s `s` (split) refuses this
/// hunk outright ("Sorry, cannot split this hunk"), so `e` is the only
/// real-git path to a partial selection here, and it edits the same raw
/// bytes positionally, with no realignment logic of its own.
///
/// This test pins that `append_sub_hunk` reproduces the reordering
/// exactly, so a future "smarter" rewrite that tries to realign
/// crossing pairs doesn't silently diverge from what `git apply` itself
/// does with the bytes this server emits.
#[tokio::test]
async fn a_crossing_line_selection_reorders_content_exactly_as_git_apply_does() {
    let (_dir, repo) = repo_with_multiline_hunk();
    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    // Hunk 0's lines: 0-2 context, 3 removed "line 4", 4 removed
    // "line 5", 5 added "line 4 changed", 6 added "line 5 changed",
    // 7-9 context. Select "-line 4" (3, stays removed) and "+line 4
    // changed" (5, stays added); "-line 5" (4) is left unselected and
    // reclassifies to context, landing BEFORE the selected addition in
    // the sub-hunk body because index 4 precedes index 5 in the
    // original hunk.
    let plan = plan_for_lines(
        &parsed,
        0,
        vec![3, 5],
        git_vista_protocol::StageDirection::Stage,
    );
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    let staged = run_out(&repo, &["show", ":a.txt"]);
    let lines: Vec<&str> = staged.lines().collect();
    // line 4 is gone (its removal was selected); line 5 survives as
    // context, but now sits BEFORE line 4's replacement — the
    // reordering, confirmed to match real `git apply` bit for bit.
    let idx_line5 = lines.iter().position(|l| *l == "line 5").unwrap();
    let idx_line4changed = lines.iter().position(|l| *l == "line 4 changed").unwrap();
    assert!(
        idx_line5 < idx_line4changed,
        "expected the known reordering (line 5 before line 4 changed): {lines:?}"
    );
    assert!(!lines.contains(&"line 4"), "{lines:?}");
}

/// #214, Task 4 (the issue's own acceptance bar: "explicit test
/// coverage, not just staleness rejection") — flavor one: a line-level
/// selection built against one worktree state is refused once the
/// *same* file picks up a further, unrelated edit, and stages nothing
/// as a side effect of the refusal.
///
/// **Honest framing (review finding):** the gate this drives is
/// `diff-v1:`, a SHA-256 of the entire staging-base diff's bytes
/// (`handlers/read.rs::staging_diff_for_repo`) — shape-agnostic to
/// `Hunks` vs `Lines`, the exact same mechanism a whole-file or
/// whole-hunk selection already relies on. This test proves that
/// mechanism protects a `Lines` selection too (real coverage the issue
/// asks for), not that line-level reconstruction has any staleness
/// exposure of its own — it doesn't; `append_sub_hunk` never reads live
/// repository state, only the pinned diff already in hand.
#[tokio::test]
async fn a_line_level_selection_refuses_after_the_same_file_changes_further() {
    let (_dir, repo) = repo_with_multiline_hunk();
    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    let plan = plan_for_lines(
        &parsed,
        0,
        vec![3, 5],
        git_vista_protocol::StageDirection::Stage,
    );
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

    let stale = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();

    // a.txt picks up a further, unrelated edit (line 8) after the
    // selection was built against the diff above.
    let current = std::fs::read_to_string(repo.join("a.txt")).unwrap();
    std::fs::write(
        repo.join("a.txt"),
        current.replace("line 8\n", "line 8 changed\n"),
    )
    .unwrap();

    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &stale.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    assert!(
        msg.contains("changed while this selection was pending"),
        "{msg}"
    );

    let cached = run_out(&repo, &["diff", "--cached", "--name-only"]);
    assert!(
        cached.trim().is_empty(),
        "expected nothing staged after a refused selection, got {cached:?}"
    );
}

/// #214, Task 4, flavor two: an edit to a completely unrelated tracked
/// file (a.txt itself untouched) also moves the diff-v1 token and
/// refuses the selection just as hard, staging nothing.
#[tokio::test]
async fn a_line_level_selection_refuses_after_an_unrelated_file_changes() {
    let (_dir, repo) = repo_with_multiline_hunk();
    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    let plan = plan_for_lines(
        &parsed,
        0,
        vec![3, 5],
        git_vista_protocol::StageDirection::Stage,
    );
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

    let stale = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();

    // b.txt changes; a.txt (and thus the selection's own file) is
    // untouched.
    std::fs::write(repo.join("b.txt"), "unrelated changed\n").unwrap();

    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &stale.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    assert!(
        msg.contains("changed while this selection was pending"),
        "{msg}"
    );

    let cached = run_out(&repo, &["diff", "--cached", "--name-only"]);
    assert!(
        cached.trim().is_empty(),
        "expected nothing staged after a refused selection, got {cached:?}"
    );
}

/// #214 review finding (blocker, `append_sub_hunk`): reclassifying an
/// unselected Removed line to context copied its `no_newline_at_eof`
/// flag verbatim, even when a later selected Added line still followed
/// it in the reconstructed body — a self-contradictory patch (`\ No
/// newline at end of file` attached to a non-final line) that real `git
/// apply --cached --recount` accepted anyway, silently concatenating the
/// two lines with no separating newline and corrupting the staged blob.
/// Confirmed against real git 2.43.0 with the exact production argv
/// before the fix; this test pins the corrected behavior through the
/// same real path. A file's committed last line lacks a trailing
/// newline (`oldlast`); the edit replaces it with `newlast` (also no
/// trailing newline). Selecting only the Added half (leaving the
/// Removed half unstaged) must stage BOTH lines, properly separated, not
/// a merged `oldlastnewlast`.
#[tokio::test]
async fn a_reclassified_eof_line_does_not_merge_with_what_follows_it() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "context\noldlast").unwrap(); // no trailing \n
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "no trailing newline"]);
    std::fs::write(repo.join("a.txt"), "context\nnewlast").unwrap(); // no trailing \n

    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    // Hunk 0's lines: 0 context "context", 1 removed "oldlast" (eof), 2
    // added "newlast" (eof). Select only the added line.
    let plan = plan_for_lines(
        &parsed,
        0,
        vec![2],
        git_vista_protocol::StageDirection::Stage,
    );
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
    assert_eq!(
        built.patch,
        "--- a/a.txt\n\
             +++ b/a.txt\n\
             @@ -1,2 +1,3 @@\n\
             \x20context\n\
             -oldlast\n\
             \\ No newline at end of file\n\
             +oldlast\n\
             +newlast\n\
             \\ No newline at end of file\n"
    );

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    // Byte-exact: three properly newline-separated lines, no merge, no
    // trailing newline after the true final line.
    let staged = std::process::Command::new("git")
        .args(["show", ":a.txt"])
        .current_dir(&repo)
        .output()
        .unwrap()
        .stdout;
    assert_eq!(staged, b"context\noldlast\nnewlast");
}

/// #214 review finding (should-fix): every existing line-level test
/// selects lines from a single `HunkLines` entry. `PatchPlan::validate`
/// already requires `Vec<HunkLines>` support (`well_ordered` checks
/// ordinals strictly ascend across it) and `append_file_patch_lines`
/// already loops over every entry — but nothing drove more than one.
/// This selects the added line of `repo_with_two_hunks`'s *first* hunk
/// (index 2 of hunk 0: `line 2` -> `line 2 changed`) AND the added line
/// of its *second*, unrelated hunk (index 4 of hunk 1: `line 18` ->
/// `line 18 changed`) in one `PatchPlan`, and proves both land in the
/// index from a single apply.
#[tokio::test]
async fn a_multi_hunk_line_level_selection_stages_both_hunks_lines() {
    let (_dir, repo) = repo_with_two_hunks();
    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
        panic!("expected a hunks-shaped file");
    };
    assert_eq!(hunks.len(), 2, "fixture drift: expected two hunks");
    let plan = git_vista_protocol::PatchPlan {
        repository: RepositoryToken::new("test-repo").unwrap(),
        worktree: WorktreeToken::new("test-worktree").unwrap(),
        generation: GenerationToken::new("diff-v1:test").unwrap(),
        direction: git_vista_protocol::StageDirection::Stage,
        files: vec![git_vista_protocol::FileSelection {
            path: "a.txt".to_string(),
            selection: git_vista_protocol::SelectionShape::Lines {
                hunks: vec![
                    git_vista_protocol::HunkLines {
                        hunk: git_vista_protocol::HunkRef {
                            index: 0,
                            old_start: hunks[0].old_start,
                            new_start: hunks[0].new_start,
                        },
                        lines: vec![2],
                    },
                    git_vista_protocol::HunkLines {
                        hunk: git_vista_protocol::HunkRef {
                            index: 1,
                            old_start: hunks[1].old_start,
                            new_start: hunks[1].new_start,
                        },
                        lines: vec![4],
                    },
                ],
            },
        }],
    };
    assert_eq!(plan.validate(), Ok(()));
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
    assert!(built.patch.contains("line 2 changed"));
    assert!(built.patch.contains("line 18 changed"));

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    let staged = run_out(&repo, &["show", ":a.txt"]);
    assert!(staged.contains("line 2\nline 2 changed"), "{staged}");
    assert!(staged.contains("line 18\nline 18 changed"), "{staged}");
}

/// #214, Task 3: byte-exact CRLF round trip through a REAL `git apply`
/// (not just an assertion on the built string, per the task brief).
#[tokio::test]
async fn a_hunk_of_a_crlf_file_applies_byte_exact() {
    let (_dir, repo) = seeded_repo();
    let body = "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n";
    std::fs::write(repo.join("crlf.txt"), body).unwrap();
    run(&repo, &["add", "crlf.txt"]);
    run(&repo, &["commit", "-q", "-m", "crlf file"]);
    let edited = "one\r\nTWO\r\nthree\r\nfour\r\nFIVE\r\n";
    std::fs::write(repo.join("crlf.txt"), edited).unwrap();

    let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
        panic!("expected Hunks");
    };
    // Confirm the \r survived parsing before trusting reconstruction.
    assert!(
        hunks[0].lines.iter().any(|l| l.text.ends_with('\r')),
        "{:?}",
        hunks[0].lines
    );

    let plan = plan_for_hunk_at(
        &parsed,
        "crlf.txt",
        0,
        git_vista_protocol::StageDirection::Stage,
    );
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Stage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    // Byte-exact: the staged blob still has \r\n line endings, matching
    // the worktree file exactly — not the LF-only bytes str::lines()
    // used to silently produce (diff.rs's split_diff_lines fix).
    let staged = run_out(&repo, &["show", ":crlf.txt"]);
    assert!(staged.contains("TWO\r\n"), "{staged:?}");
    assert!(staged.contains("FIVE\r\n"), "{staged:?}");
    assert!(staged.contains("one\r\n"), "{staged:?}");
}

/// #214, Task 2 (renames-with-content): empirically confirmed, not
/// assumed.
///
/// `append_file_patch`/`append_file_patch_lines` emit only `--- a/<old>`
/// / `+++ b/<new>` and hunks for a renamed-and-edited file — no `rename
/// from`/`rename to`/`similarity index` lines, because the parser
/// deliberately drops those (diff.rs's module doc: a rename with
/// content edits parses as plain `FileDiff::Hunks`, not `Renamed`).
///
/// **This shape is only ever reachable in the Unstage direction.**
/// Verified against a real repo: the Stage direction's base diff is a
/// bare `git diff` (worktree vs index); with the rename done via a plain
/// filesystem move (not staged), the old path shows as a plain deletion
/// and the new path, being untracked, is invisible to `git diff`
/// entirely (untracked files never appear in it — confirmed directly).
/// A `FileDiff::Hunks` entry with `old_path != new_path` can only come
/// from a diff that compares two *trees* where git's own rename
/// detection paired an old and a new path — `index-vs-HEAD` (`git diff
/// --cached` after `git add -A` of a rename+edit) or a commit/ref
/// comparison. Of those, only Unstage's `index-vs-HEAD` base is wired to
/// this server's staging surface today.
///
/// **`git apply --cached --reverse` handles the headerless form
/// correctly as-is** — verified directly outside this suite: staged a
/// rename+edit (`git add -A` of a `mv` + content edit), built the exact
/// `--- a/old +++ b/new` + hunk text this server emits, ran `git apply
/// --cached --reverse --recount` against the real repo. Result: the
/// rename stayed staged (still `R`, still 100% similarity — the
/// *content* hunk is what got reversed, not the rename), the reversed
/// content landed correctly at the *new* path in the worktree (never at
/// `old_path`, which no longer exists anywhere), and the index still
/// held the pre-edit content at the new path. No `rename from`/`rename
/// to` lines were needed. The forward (`--cached`, no `--reverse`)
/// direction, by contrast, does need them — attempted without, it fails
/// outright (`"<new path>: does not exist in index"`) — but the forward
/// direction is exactly the one this shape can never reach (previous
/// paragraph), so nothing here needs the headers added. This test
/// exercises the reachable (Unstage) leg end to end; the forward leg's
/// unreachability is the empirical finding, not something a passing
/// test can assert.
#[tokio::test]
async fn unstaging_a_content_hunk_of_a_renamed_file_reverses_only_the_content() {
    let (_dir, repo) = repo_with_staged_rename_and_edit();

    let staged_before = run_out(&repo, &["diff", "--cached", "--no-color"]);
    assert!(
        staged_before.contains("rename from a.txt"),
        "{staged_before}"
    );
    assert!(
        staged_before.contains("rename to renamed.txt"),
        "{staged_before}"
    );

    let diff = run_out(&repo, &["diff", "--cached", "--no-color", "--no-textconv"]);
    let parsed = git_vista_protocol::parse_unified_diff(&diff);
    let (old_path, new_path, hunk0) = match &parsed.files[0] {
        git_vista_protocol::FileDiff::Hunks {
            old_path,
            new_path,
            hunks,
        } => (old_path.clone(), new_path.clone(), hunks[0].clone()),
        other => panic!("expected a rename-with-edit to parse as Hunks, got {other:?}"),
    };
    assert_eq!(old_path.as_deref(), Some("a.txt"));
    assert_eq!(new_path.as_deref(), Some("renamed.txt"));

    let plan = git_vista_protocol::PatchPlan {
        repository: RepositoryToken::new("test-repo").unwrap(),
        worktree: WorktreeToken::new("test-worktree").unwrap(),
        generation: GenerationToken::new("diff-v1:test").unwrap(),
        direction: git_vista_protocol::StageDirection::Unstage,
        files: vec![git_vista_protocol::FileSelection {
            path: "renamed.txt".to_string(),
            selection: git_vista_protocol::SelectionShape::Hunks {
                hunks: vec![git_vista_protocol::HunkRef {
                    index: 0,
                    old_start: hunk0.old_start,
                    new_start: hunk0.new_start,
                }],
            },
        }],
    };
    let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
    assert!(built.patch.starts_with("--- a/a.txt\n+++ b/renamed.txt\n"));
    assert!(
        !built.patch.contains("rename from"),
        "no rename headers should be needed: {}",
        built.patch
    );

    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Unstage,
    )
    .await
    .unwrap();
    let (status, msg) = exec_stage_selection(
        &repo,
        NetworkNeed::Local,
        git_vista_protocol::StageDirection::Unstage,
        &live.generation,
        &built.patch,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    // The rename itself is still staged, untouched, still 100%
    // similarity — only the content hunk was reversed.
    let staged_after = run_out(&repo, &["diff", "--cached", "--no-color"]);
    assert!(staged_after.contains("rename from a.txt"), "{staged_after}");
    assert!(
        staged_after.contains("similarity index 100%"),
        "{staged_after}"
    );
    assert!(!staged_after.contains("line 3 changed"), "{staged_after}");

    // The content change is back in the worktree, at the NEW path —
    // never at old_path, which doesn't exist anywhere anymore.
    assert!(!repo.join("a.txt").exists());
    let worktree_content = std::fs::read_to_string(repo.join("renamed.txt")).unwrap();
    assert!(
        worktree_content.contains("line 3 changed"),
        "{worktree_content}"
    );
}
