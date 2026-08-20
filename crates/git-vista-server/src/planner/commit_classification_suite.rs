//! M2.19a (#222) / M2.19b (#223) / #72 (M2.19): `GitOperation::AmendCommit`'s
//! `shape` (risk, CAS precondition, recovery), and the pure failure
//! classifiers `classify_amend_failure` / `classify_commit_failure` —
//! every branch, paired negatives, and the guarantee that the unknown arm
//! never swallows git's own stderr.

use super::*;
use std::path::PathBuf;

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("test-repo").unwrap(),
        WorktreeToken::new("test-worktree").unwrap(),
    )
}

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

/// `git rev-parse HEAD` in `repo`, trimmed — for tests that need a real
/// oid to build a compare-and-swap `GitOperation` against (#222).
async fn git_rev_parse_head(repo: &Path) -> String {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
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

// -----------------------------------------------------------------------
// M2.19a (#222): `GitOperation::AmendCommit`'s `shape` — contract only,
// no execution (see the variant's own doc comment and planner::execute's
// stub arm). These pin the plan-building side: risk, the CAS
// precondition, the expected ref change, and the recovery strategy.
// -----------------------------------------------------------------------

/// The happy-path shape: `Destructive` risk, a `BranchCheckedOut` +
/// `RefAt(expected_tip)` precondition pair on the checked-out branch, a
/// `Computed` ref change from `expected_tip`, and `ResetRef` recovery
/// back to `expected_tip` — exactly the design the variant's doc comment
/// argues for, pinned so a later edit that quietly reached for
/// `RecoverableIfStaged` or `Irrecoverable` instead fails here.
#[tokio::test]
async fn amend_commit_shape_is_destructive_with_cas_precondition_and_reset_recovery() {
    let (_dir, repo) = seeded_repo();
    let head = git_rev_parse_head(&repo).await;
    let head_oid = CommitOid::new(head.clone()).unwrap();

    let op = GitOperation::AmendCommit {
        message: CommitMessage::new("fix: typo").unwrap(),
        expected_tip: head_oid.clone(),
        allow_empty: false,
    };
    let (plan, observed) = build_plan(&repo, op, tokens()).await;

    assert_eq!(plan.risk, RiskLevel::Destructive);
    assert_eq!(
        plan.preconditions,
        vec![
            Precondition::BranchCheckedOut {
                branch: BranchName::new("main").unwrap(),
            },
            Precondition::RefAt {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                oid: head_oid.clone(),
            },
        ]
    );
    assert_eq!(
        plan.expected_ref_changes,
        vec![RefChange {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            before: RefState::At(head_oid.clone()),
            after: RefState::Computed,
        }]
    );
    assert_eq!(
        plan.recovery,
        RecoveryStrategy::ResetRef {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            to: head_oid,
        }
    );
    // Both preconditions genuinely hold against the freshly seeded repo —
    // proves the shape isn't vacuously satisfied by an always-true check.
    assert!(observed.held_at_build.iter().all(|&h| h));
}

/// `expected_tip` is a *live* check, not a value the plan merely carries:
/// build a plan whose `expected_tip` matches HEAD, then let another
/// commit land before execution. `refs/heads/main` moving trips
/// `enforce_fresh`'s generation check before its per-precondition loop
/// ever runs — the same layering every other tip-moved race in this
/// codebase goes through (`a_generation_move_refuses_execution`;
/// `EmptyCommitOnBranch` and `ResetBranch`'s own `RefAt` preconditions
/// are shadowed by it too, for the identical reason: any ref move is by
/// construction also a generation move). The named `RefAt` precondition
/// still earns its place — it is what the reviewer/UI sees named and
/// individually reviewable in `Plan::preconditions`, and it is the
/// backstop `verify_precondition` would use should a future generation
/// algorithm ever narrow which refs it digests. What matters here, and
/// what this test actually proves, is the end-to-end guarantee: a plan
/// built against one tip is refused, not silently honoured, once that
/// tip has moved.
#[tokio::test]
async fn amend_commit_refuses_when_the_tip_moved_after_the_plan_was_built() {
    let (_dir, repo) = seeded_repo();
    let head = git_rev_parse_head(&repo).await;

    let op = GitOperation::AmendCommit {
        message: CommitMessage::new("fix: typo").unwrap(),
        expected_tip: CommitOid::new(head).unwrap(),
        allow_empty: false,
    };
    let (plan, observed) = build_plan(&repo, op, tokens()).await;
    assert!(
        observed.held_at_build.iter().all(|&h| h),
        "both preconditions should hold at build time"
    );
    assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

    // The race: another commit lands on main before this plan executes.
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "raced ahead"]);

    let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("repository changed"), "{why}");
}

// -----------------------------------------------------------------------
// M2.19b (#223): `classify_amend_failure` — the pure classification the
// wire's `AmendFailureKind` rests on. Driven branch by branch with the
// stderr shapes captured from a real git 2.43 (see the function's doc
// comment), plus the paired negatives that keep each leg from going
// vacuous. The end-to-end versions (real hooks, real failed signers,
// through the full pipeline) live in `contract_suite`.
// -----------------------------------------------------------------------

/// Every classification branch, with its paired negative on the same
/// row: the input that must NOT take that branch differs from the
/// matching one by exactly the load-bearing fact.
#[test]
fn classify_amend_failure_covers_every_branch_with_paired_negatives() {
    use AmendFailureKind::*;
    // Captured verbatim from git 2.43 (scratch experiments, 2026-08-02).
    let gpg = "error: gpg failed to sign the data:\n(no gpg output)\nfatal: failed to write commit object";
    let ssh = "error: Couldn't load public key /k: No such file or directory?\n\nfatal: failed to write commit object";
    let empty_amend = "You asked to amend the most recent commit, but doing so would make\nit empty. You can repeat your command with --allow-empty, or you can\nremove the commit entirely with \"git reset HEAD^\".";
    let merge_fatal = "fatal: You are in the middle of a merge -- cannot amend.";

    // (stderr, signing_requested, hook_present) → expected kind, and why.
    let cases: &[(&str, bool, bool, AmendFailureKind, &str)] = &[
        // -- signing, gpg format: the canonical line decides alone --
        (gpg, true, false, SigningFailed, "gpg line, signing on"),
        (
            gpg,
            false,
            false,
            SigningFailed,
            "the canonical gpg line is decisive even unprobed",
        ),
        (
            gpg,
            true,
            true,
            SigningFailed,
            "signing outranks a present hook",
        ),
        // -- signing, ssh format: needs the config probe --
        (
            ssh,
            true,
            false,
            SigningFailed,
            "ssh-format signer failure with signing configured",
        ),
        (
            ssh,
            false,
            false,
            Other,
            "paired negative: the identical stderr WITHOUT signing configured is a \
              plain object-write failure — blaming the signer would hide disk trouble",
        ),
        // -- hook rejection: silence plus a hook, and nothing fatal --
        (
            "",
            false,
            true,
            HookRejected,
            "the real shape: silent hook, empty stderr",
        ),
        (
            "nope: bad message",
            false,
            true,
            HookRejected,
            "a chatty hook is still a hook",
        ),
        (
            "",
            false,
            false,
            Other,
            "paired negative: the identical silence with NO hook present must not \
              invent a hook to blame",
        ),
        (
            merge_fatal,
            false,
            true,
            Other,
            "paired negative: git's own fatal refusals never classify as a hook, \
              hook present or not — the fatal: prefix is die()'s, unlocalized",
        ),
        (
            empty_amend,
            false,
            true,
            Other,
            "paired negative: the would-become-empty advice is git's, not the \
              hook's, even though it is non-fatal and a hook is present",
        ),
        // -- everything else --
        (
            merge_fatal,
            false,
            false,
            Other,
            "an ordinary fatal is Other",
        ),
        (
            empty_amend,
            false,
            false,
            Other,
            "the empty-amend advice is Other",
        ),
    ];
    for (stderr, signing, hook, expected, why) in cases {
        assert_eq!(
            classify_amend_failure(stderr, *signing, *hook),
            *expected,
            "{why} (stderr={stderr:?}, signing={signing}, hook={hook})"
        );
    }
}

// -----------------------------------------------------------------------
// #72 (M2.19): `classify_commit_failure` — the pure classification the
// wire's `CommitFailureKind` rests on. Every stderr/stdout fixture below
// was captured verbatim from a real git 2.43 (scratch repos, 2026-08-18),
// not invented — see `classify_commit_failure`'s own doc comment for the
// exact commands. Driven branch by branch with paired negatives, mirroring
// `classify_amend_failure_covers_every_branch_with_paired_negatives`
// above; the end-to-end version (a real hook, a real signing config,
// through the full pipeline) lives in `hook_timeout_suite` /
// `contract_suite`.
// -----------------------------------------------------------------------

/// Every classification branch, with its paired negative on the same
/// row: the input that must NOT take that branch differs from the
/// matching one by exactly the load-bearing fact.
#[test]
fn classify_commit_failure_covers_every_branch_with_paired_negatives() {
    use CommitFailureKind::*;
    // Captured verbatim (scratch repos, 2026-08-18, git 2.43):
    //   git -c commit.gpgsign=true -c user.signingkey=DOESNOTEXIST \
    //       -c gpg.format=openpgp commit -m x
    let gpg_no_key = "error: gpg failed to sign the data:\ngpg: skipped \"DOESNOTEXIST\": \
                           No secret key\n[GNUPG:] INV_SGNR 9 DOESNOTEXIST\n\
                           [GNUPG:] FAILURE sign 17\ngpg: signing failed: No secret key\n\n\
                           fatal: failed to write commit object";
    // A synthetic FAILURE code carrying GPG_ERR_NO_AGENT (77) in the low
    // 16 bits, the same masking `classify_sign_failure`'s own fixture
    // documents — this server's sandbox denies the AF_UNIX socket
    // gpg-agent needs, which surfaces this way when gpg gets far enough
    // to try.
    let gpg_agent_unreachable = "[GNUPG:] FAILURE sign 67108941";
    // The bare `FAILURE sign 17` line with no preceding `INV_SGNR` —
    // `gpg_no_key` above always carries both (that's what a real gpg
    // invocation prints), so relying on it alone would let the
    // `Some(17)` arm's mapping drift without any row noticing: the
    // `INV_SGNR` branch, checked separately in the same loop, would
    // already have returned `SigningKeyMissing` before the loop ever
    // reached the `FAILURE` line. This fixture isolates the arm the
    // `INV_SGNR` line would otherwise shadow.
    let gpg_no_key_failure_line_only = "[GNUPG:] FAILURE sign 17";
    // git -c commit.gpgsign=true -c gpg.format=ssh \
    //     -c user.signingkey=/nonexistent/key commit -m x
    let ssh_bad_key = "error: Couldn't load public key /nonexistent/key: No such file or \
                            directory?\n\nfatal: failed to write commit object";
    // git commit -m x   (unstaged tracked change only)
    let no_changes_added = "On branch main\nChanges not staged for commit:\n\t\
                                 modified:   a.txt\n\nno changes added to commit (use \"git \
                                 add\" and/or \"git commit -a\")";
    // git commit -m x   (clean working tree)
    let nothing_to_commit = "On branch main\nnothing to commit, working tree clean";
    // git commit -m x   (only an untracked file present)
    let untracked_only = "On branch main\n\nUntracked files:\n\tuntracked.txt\n\n\
                               nothing added to commit but untracked files present (use \
                               \"git add\" to track)";
    let merge_fatal = "fatal: You are in the middle of a merge -- cannot amend.";

    // (stdout, stderr, signing_requested, hook_present) → expected kind, and why.
    let cases: &[(&str, &str, bool, bool, CommitFailureKind, &str)] = &[
        // -- nothing staged, checked ahead of everything else --
        (
            nothing_to_commit,
            "",
            true,
            true,
            NothingStaged,
            "an empty working tree is never a signing or hook problem, no matter \
                 what else is configured",
        ),
        (
            no_changes_added,
            "",
            false,
            false,
            NothingStaged,
            "unstaged-but-tracked changes are the same 'nothing staged' answer, \
                 different git wording",
        ),
        (
            untracked_only,
            "",
            false,
            false,
            NothingStaged,
            "untracked-only is the third 'nothing staged' shape git prints",
        ),
        // -- signing, gpg format: positive status-fd evidence outranks \
        //    a present hook, because a hook rejection can never produce \
        //    it (hooks run before signing in git's own sequence) --
        (
            "",
            gpg_no_key,
            true,
            true,
            SigningKeyMissing,
            "INV_SGNR / FAILURE sign 17 is positive proof of a signing attempt, \
                 decisive even with a hook present",
        ),
        (
            "",
            gpg_agent_unreachable,
            true,
            false,
            SigningAgentUnavailable,
            "FAILURE sign carrying GPG_ERR_NO_AGENT (77) in the low 16 bits",
        ),
        (
            "",
            gpg_no_key_failure_line_only,
            true,
            false,
            SigningKeyMissing,
            "the bare FAILURE sign 17 line, isolated from INV_SGNR, so the code-17 \
                 arm itself is exercised rather than shadowed by the earlier branch",
        ),
        (
            "",
            gpg_no_key,
            false,
            false,
            SigningKeyMissing,
            "paired negative: the GNUPG status line is decisive even unprobed — \
                 it cannot be produced by anything but a real signing attempt",
        ),
        // -- signing, ssh format: needs the config probe, same as amend --
        (
            "",
            ssh_bad_key,
            true,
            false,
            SigningAgentUnavailable,
            "ssh-format signer failure with signing configured",
        ),
        (
            "",
            ssh_bad_key,
            false,
            false,
            Other,
            "paired negative: the identical stderr WITHOUT signing configured is a \
                 plain object-write failure — blaming the signer would hide disk trouble",
        ),
        // -- hook rejection: silence plus a hook, and nothing fatal --
        (
            "",
            "",
            false,
            true,
            HookRejected,
            "the real shape: silent hook, empty stderr",
        ),
        (
            "",
            "nope: bad message",
            false,
            true,
            HookRejected,
            "a chatty hook is still a hook",
        ),
        (
            "",
            "",
            true,
            true,
            HookRejected,
            "the genuine ambiguity this function exists to resolve: signing \
                 requested AND a hook present, empty stderr either way — the hook, as \
                 the earlier stage in git's own sequence, wins over the signing-agent \
                 fallback below",
        ),
        (
            "",
            "",
            false,
            false,
            Other,
            "paired negative: the identical silence with NO hook present and no \
                 signing requested must not invent a hook to blame",
        ),
        (
            "",
            merge_fatal,
            false,
            true,
            Other,
            "paired negative: git's own fatal refusals never classify as a hook, \
                 hook present or not — the fatal: prefix is die()'s, unlocalized",
        ),
        // -- the sandboxed signing-agent fallback: only once nothing else --
        // -- explains the empty stderr --
        (
            "",
            "",
            true,
            false,
            SigningAgentUnavailable,
            "signing requested, empty stderr, no hook to blame instead — the \
                 production shape under this server's sandbox",
        ),
        (
            "",
            "   \n  ",
            true,
            false,
            SigningAgentUnavailable,
            "whitespace-only stderr is still empty for this purpose",
        ),
        // -- everything else --
        (
            "",
            merge_fatal,
            false,
            false,
            Other,
            "an ordinary fatal with nothing else configured is Other",
        ),
    ];
    for (stdout, stderr, signing, hook, expected, why) in cases {
        assert_eq!(
            classify_commit_failure(stdout, stderr, *signing, *hook),
            *expected,
            "{why} (stdout={stdout:?}, stderr={stderr:?}, signing={signing}, hook={hook})"
        );
    }
}

/// Mutation-shaped guard, mirroring
/// `classify_sign_failure_distinguishes_no_secret_key_from_agent_unreachable`:
/// swapping the `17` arm's target kind must be distinguishable from the
/// real thing rather than collapsing the two closed-set reasons together.
#[test]
fn classify_commit_failure_distinguishes_no_secret_key_from_agent_unreachable() {
    let no_key = classify_commit_failure("", "[GNUPG:] FAILURE sign 17", true, false);
    let no_agent = classify_commit_failure("", "[GNUPG:] FAILURE sign 67108941", true, false);
    assert_ne!(no_key, no_agent);
    assert_eq!(no_key, CommitFailureKind::SigningKeyMissing);
    assert_eq!(no_agent, CommitFailureKind::SigningAgentUnavailable);
}

/// #72's own explicit requirement: the unknown/passthrough arm must
/// forward git's real words, never a canned substitute — proven at the
/// wire-body level, not just the classification, since it is
/// `commit_refusal_body` that decides what `message` actually carries.
#[test]
fn commit_refusal_body_never_swallows_the_unknown_arms_stderr() {
    let (status, body) = commit_refusal_body(
        CommitFailureKind::Other,
        "fatal: some completely unrecognised git failure text (2026-08-18)",
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: CommitError = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!("commit_refusal_body must emit parseable CommitError ({e}): {body}")
    });
    assert_eq!(parsed.kind, CommitFailureKind::Other);
    assert_eq!(
        parsed.message, "fatal: some completely unrecognised git failure text (2026-08-18)",
        "the Other arm must carry git's own words byte-for-byte"
    );
}
