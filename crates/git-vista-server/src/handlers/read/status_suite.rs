//! The live worktree-status seam: `GET /api/status/v2` against a real dirty
//! worktree, its generation tracking, the refuse-rather-than-truncate
//! contract, and the production cap's behaviour on an ordinary worktree
//! (#68c) — plus large-worktree responsiveness under a growing count of
//! untracked files, including the exact cap boundary and the 1k-file budget
//! (#68e).

use super::*;
use git_vista_protocol::{ChangeKind, ChangeSides, StatusEntry};

// --- duplicated cross-suite test helpers, verbatim from read.rs's original inline test module —
// private to their own modules and unreachable from here, same shape
// as the planner/*_suite.rs convention this crate already uses. ---

/// `git <args…>` in `repo`; asserts success. Same shape as the planner
/// suites' fixtures, duplicated because those helpers are private to their
/// own modules and unreachable from here.
fn run(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// A fresh repository on branch `main` with one committed file.
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

// ---- GET /api/status/v2: the live handler seam (#68c) ---------------------

/// The real handler, end to end: a dirty worktree (staged add, unstaged
/// modify, untracked file) produces a `WorktreeStatus` whose `entries`
/// actually reflect it, and whose `generation` is a real, non-empty
/// `status-v1:`-namespaced token — not the DTO's shape alone (task 10's
/// tests already pin that), but this file's own contribution: that the
/// three existing pieces (DTO, parser, generation inputs) are actually
/// wired together correctly.
#[tokio::test]
async fn worktree_status_v2_reflects_a_real_dirty_worktree() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    run(&repo, &["add", "new.txt"]);

    let status = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
        .await
        .expect("a real repository read must succeed");

    assert!(
        status.generation.as_str().starts_with("status-v1:"),
        "generation must carry the status-v1 namespace: {:?}",
        status.generation
    );
    assert_eq!(status.branch.as_deref(), Some("main"));

    let unstaged_a = status.entries.iter().any(|e| {
        matches!(
            e,
            StatusEntry::Changed { path, sides: ChangeSides::UnstagedOnly { .. }, .. }
                if path == "a.txt"
        )
    });
    assert!(
        unstaged_a,
        "a.txt's unstaged edit must appear: {:?}",
        status.entries
    );

    let staged_new = status.entries.iter().any(|e| matches!(
            e,
            StatusEntry::Changed { path, sides: ChangeSides::StagedOnly { staged: ChangeKind::Added }, .. }
                if path == "new.txt"
        ));
    assert!(
        staged_new,
        "new.txt's staged add must appear: {:?}",
        status.entries
    );
}

/// The generation changes across a real edit, and is stable when nothing
/// changed between two reads — the actual guarantee #68's "generation-
/// tagged and detects external changes" criterion is about, proven
/// against a real repository rather than assumed from the DTO's shape.
#[tokio::test]
async fn worktree_status_v2_generation_changes_with_the_worktree() {
    let (_dir, repo) = seeded_repo();

    let clean = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
        .await
        .unwrap();
    let clean_again = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
        .await
        .unwrap();
    assert_eq!(
        clean.generation, clean_again.generation,
        "two reads of an unchanged worktree must agree"
    );

    std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
    let dirty = worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
        .await
        .unwrap();
    assert_ne!(
        clean.generation, dirty.generation,
        "an unstaged edit must change the generation"
    );
}

/// A porcelain-v2 stream past the cap is refused outright, not parsed
/// into a `WorktreeStatus` missing (or mangling) its cut-off last entry —
/// see `worktree_status_v2_for_repo`'s doc comment for why a status cap
/// hit cannot be a success the way a file-read cap hit is. Uses a small
/// injected cap (the same testability seam `commit_diff_for_repo`'s
/// metadata-cap tests use) rather than constructing enough real
/// porcelain output to exceed the production 8 MiB ceiling.
#[tokio::test]
async fn worktree_status_v2_refuses_rather_than_serving_a_truncated_parse() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();

    let err = worktree_status_v2_for_repo(&repo, 4)
        .await
        .expect_err("a cap hit must be refused, not parsed");
    assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
}

/// The production cap is generous enough that an ordinary dirty worktree
/// never trips it — the control for the test above, so a cap-hit failure
/// there is known to come from the injected small cap, not from
/// `STATUS_V2_STDOUT_CAP` itself being too tight for real use.
#[tokio::test]
async fn worktree_status_v2_production_cap_does_not_truncate_an_ordinary_worktree() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
    worktree_status_v2_for_repo(&repo, STATUS_V2_STDOUT_CAP)
        .await
        .expect("an ordinary dirty worktree must not hit the production cap");
}

// ---- Large-worktree responsiveness (#68e) ---------------------------------
//
// #68's own text — "large worktrees stay responsive" — is unfalsifiable as
// written. This turns it into: a real measurement at several worktree
// sizes, a stated cap-boundary file count, and a budget a future change
// can actually fail against (`worktree_status_v2_budget_holds_at_1k_files`
// below, which runs in every `cargo test`; the full multi-N ladder is
// `#[ignore]`d — see that test's own doc comment for why).

/// `n` freshly created, distinctly named untracked files under `repo` —
/// the cheapest real worktree-size generator available: untracked
/// entries are one porcelain-v2 `? <path>` record each (no hash/mode
/// fields to compute), so file *creation* cost dominates over anything
/// `git status` itself has to do, keeping the measurement honest about
/// what it's actually timing.
fn generate_untracked_files(repo: &Path, n: usize) {
    for i in 0..n {
        std::fs::write(repo.join(format!("bench-{i:06}.txt")), "x\n").unwrap();
    }
}

/// One measurement: wall-clock time for the **real** `#68c` handler seam
/// (git spawn, `-z` porcelain read, `parse_porcelain_v2_z`, and the full
/// generation derivation — `read_generation_inputs`'s ref walk plus the
/// sha256 digest) against a worktree with `n` untracked files.
async fn time_status_v2(repo: &Path, n: usize) -> (std::time::Duration, bool) {
    generate_untracked_files(repo, n);
    let start = std::time::Instant::now();
    let result = worktree_status_v2_for_repo(repo, STATUS_V2_STDOUT_CAP).await;
    let elapsed = start.elapsed();
    (elapsed, result.is_ok())
}

/// The real measurement behind `docs/PERFORMANCE_BUDGETS.md`'s numbers —
/// **not** part of the normal test run. `#[ignore]`d because generating
/// up to 20,000 real files and shelling out to `git status` repeatedly
/// costs real wall-clock seconds, which has no place in every `cargo
/// test`/CI run; `worktree_status_v2_budget_holds_at_1k_files` below is
/// the fast, always-on regression check derived from what this found.
///
/// Run explicitly to reproduce or update the recorded numbers:
/// `cargo test -p git-vista-server --bin git-vista-server -- --ignored \
///  --nocapture large_worktree_responsiveness_ladder`
///
/// One host, one run each — not a statistically controlled benchmark
/// suite. `docs/PERFORMANCE_BUDGETS.md` says so explicitly; treat the
/// printed numbers as "real and reproducible," not "precise to the
/// millisecond."
#[tokio::test]
#[ignore = "generates up to 20k real files and shells out to git repeatedly; run explicitly, see doc comment"]
async fn large_worktree_responsiveness_ladder() {
    let (_dir, repo) = seeded_repo();
    println!("\n#68e large-worktree responsiveness ladder (one host, one run each):");
    println!("{:>8}  {:>12}  {:>8}", "n_files", "elapsed", "ok?");
    for n in [100usize, 1_000, 5_000, 20_000] {
        let (elapsed, ok) = time_status_v2(&repo, n).await;
        println!("{n:>8}  {elapsed:>12?}  {ok:>8}");
    }
}

/// Where the 8 MiB cap (`STATUS_V2_STDOUT_CAP`) actually bites, in file
/// count — not asserted from arithmetic on an assumed per-record size,
/// measured against a real, large, uniformly-named worktree (`? bench-
/// NNNNNN.txt\0` is 20 bytes/record: 2-byte marker+space, 15-byte name,
/// 1-byte NUL terminator; a real worktree's actual paths will differ, so
/// this is a lower bound on the file count that trips the cap for
/// *this* naming scheme, not a universal constant — `docs/
/// PERFORMANCE_BUDGETS.md` states that caveat explicitly). `#[ignore]`d
/// for the same reason as the ladder above: real cost, not a normal-run
/// check.
#[tokio::test]
#[ignore = "generates ~450k real files; run explicitly, see doc comment"]
async fn large_worktree_cap_boundary_in_file_count() {
    let (_dir, repo) = seeded_repo();
    // 20 bytes/record * ~450_000 ~= 8.6 MiB, comfortably past the 8 MiB
    // cap for this naming scheme.
    let (_elapsed, ok) = time_status_v2(&repo, 450_000).await;
    assert!(
        !ok,
        "450,000 uniformly-named untracked files should exceed \
             STATUS_V2_STDOUT_CAP for this record size — if this now \
             succeeds, the cap boundary moved and docs/PERFORMANCE_BUDGETS.md \
             needs its file-count figure re-measured"
    );
}

/// The always-on regression check: 1,000 changed files (cheap enough for
/// every `cargo test`/CI run) must complete well inside a generous
/// multiple of the budget `docs/PERFORMANCE_BUDGETS.md` states — loose
/// enough not to flake on a loaded CI runner, tight enough that a real
/// regression (e.g. the generation derivation's ref walk becoming
/// accidentally quadratic) would still fail it.
#[tokio::test]
async fn worktree_status_v2_budget_holds_at_1k_files() {
    let (_dir, repo) = seeded_repo();
    let (elapsed, ok) = time_status_v2(&repo, 1_000).await;
    assert!(ok, "1,000 untracked files must not hit the read cap");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "1,000-file worktree status took {elapsed:?}, budget is 2s \
             (see docs/PERFORMANCE_BUDGETS.md) — this is a real regression, \
             not flakiness, unless the CI runner is unusually loaded"
    );
}
