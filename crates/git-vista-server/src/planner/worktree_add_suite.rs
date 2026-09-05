//! M11.04 (#549), ADR 0118: `AddWorktree` — where a new desk is allowed to go,
//! and who decides.
//!
//! # What this suite is actually about
//!
//! One sentence from the issue: *"every created path is inside the fence, and
//! a test proves a path outside it is refused **by the server**, not by the
//! picker."* Every test here drives the wire or the planner directly. No
//! frontend exists for this operation yet and none is needed to prove this.
//!
//! # The two halves, and why the second is a source read
//!
//! **Containment is a type, not a check.** `WorktreeName` cannot hold a
//! separator, a `..`, a leading dot or an absolute path, so
//! `worktrees_root().join(name)` can only ever be a direct child. That half is
//! testable by running things, and is.
//!
//! **The fence is not widened, and that is an omission.** `exec_add_worktree`
//! must not call `allow_repo_root` for the directory it creates. Adding that
//! call would make the app serve *more*, never less — so every behavioural
//! test in this repository stays green, the feature keeps working, and the
//! allowlist grows one permanent entry per worktree created. There is no
//! runtime signature to assert on. The only place it can be caught is the
//! source, and ADR 0117 earned that lesson one slice earlier.
//!
//! The source assertion here is an **exact-body comparison**, not a list of
//! forbidden names. A denylist is a list somebody has to remember to extend,
//! and grok found exactly that hole in #654's version of this test: it forbade
//! the substring `allow_root` while `allow_repo_root` — the public wrapper
//! that does the widening — sailed straight through, because the shorter
//! string is not a substring of the longer one. An exact body has nothing to
//! keep complete.

use super::*;
use git_vista_fixtures::seeded as seeded_repo;
use git_vista_protocol::WorktreeName;
use std::path::Path;

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("test-repo").unwrap(),
        WorktreeToken::new("test-worktree").unwrap(),
    )
}

fn name(s: &str) -> WorktreeName {
    WorktreeName::new(s).expect("a valid worktree name")
}

fn add(n: &str, branch_name: &str) -> GitOperation {
    GitOperation::AddWorktree {
        name: name(n),
        branch: BranchName::new(branch_name).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// The wire boundary refuses a path before any path exists
// ---------------------------------------------------------------------------

/// **The acceptance criterion.** A name that would escape the managed root is
/// refused by the **server**, at the wire boundary, before a handler runs and
/// before any path is computed.
///
/// Driven through `serde_json` rather than through a picker, because the
/// picker is not what is being tested — and because this is the exact path a
/// hand-written `curl`, a stale client, or a hostile one takes.
#[test]
fn a_name_that_would_escape_the_managed_root_is_refused_at_the_wire() {
    // Every shape that would make `worktrees_root().join(name)` name something
    // other than a direct child of the root. The last is the one that matters
    // most and is the least obvious: `Path::join` given an ABSOLUTE argument
    // discards the base entirely, so an accepted absolute name would not
    // escape the root — it would replace it.
    for hostile in [
        "../escape",
        "..",
        "a/b",
        "a/../../b",
        "/etc",
        "/tmp/anywhere",
        ".hidden",
        ".git",
        "",
        "-oProxyCommand=x",
    ] {
        let body = serde_json::json!({ "name": hostile, "branch": "main" }).to_string();
        let parsed: Result<git_vista_protocol::AddWorktreeRequest, _> = serde_json::from_str(&body);
        assert!(
            parsed.is_err(),
            "‘{hostile}’ was accepted as a worktree name — the managed root's \
             containment is a property of this type and nothing downstream re-checks it"
        );
    }
}

/// The paired positive, and the reason the test above is not vacuous: a
/// validator that refused everything would satisfy it and break the feature.
#[test]
fn ordinary_desk_names_are_accepted() {
    for good in ["review-549", "desk_two", "spike.2", "m11", "A1"] {
        let body = serde_json::json!({ "name": good, "branch": "main" }).to_string();
        let parsed: Result<git_vista_protocol::AddWorktreeRequest, _> = serde_json::from_str(&body);
        assert!(
            parsed.is_ok(),
            "‘{good}’ is an ordinary name and was refused"
        );
    }
}

/// The request body cannot carry a location at all — `deny_unknown_fields`
/// plus the absence of a `path` field. A client that tries to name one is a
/// 400, rather than having its field silently ignored while the server picks
/// somewhere else and reports success.
#[test]
fn the_request_body_has_no_way_to_name_a_location() {
    let body = serde_json::json!({ "name": "ok", "branch": "main", "path": "/tmp/x" }).to_string();
    let parsed: Result<git_vista_protocol::AddWorktreeRequest, _> = serde_json::from_str(&body);
    assert!(
        parsed.is_err(),
        "a request naming a path was accepted; a silently-ignored `path` is worse \
         than a refused one, because the caller is told the operation succeeded"
    );
}

// ---------------------------------------------------------------------------
// The plan states git's actual rule
// ---------------------------------------------------------------------------

/// `RiskLevel::Safe` and `RecoveryStrategy::NotNeeded`, from the issue: it
/// creates a directory and destroys nothing.
#[tokio::test]
async fn the_plan_is_safe_and_needs_no_recovery() {
    let (_dir, repo) = seeded_repo();
    let (plan, _observed) = build_plan(&repo, add("review-549", "main"), tokens()).await;
    assert_eq!(plan.risk, RiskLevel::Safe);
    assert_eq!(plan.recovery, RecoveryStrategy::NotNeeded);
    assert!(
        plan.expected_ref_changes.is_empty(),
        "a new worktree moves no ref in this repository: {:?}",
        plan.expected_ref_changes
    );
}

/// **The precondition pair.** git refuses `worktree add` on a branch checked
/// out ANYWHERE, including here. `BranchFreeInEveryOtherWorktree` covers every
/// *other* worktree and by its own definition does not cover this one — so
/// alone it is a half-check that reads as complete, and the case it misses is
/// the most common mistake a user makes: asking for a second desk on the
/// branch they are looking at.
#[tokio::test]
async fn the_plan_states_both_halves_of_gits_rule() {
    let (_dir, repo) = seeded_repo();
    let (plan, _observed) = build_plan(&repo, add("review-549", "main"), tokens()).await;
    assert!(
        plan.preconditions.iter().any(|p| matches!(
            p,
            Precondition::BranchFreeInEveryOtherWorktree { branch } if branch.as_str() == "main"
        )),
        "missing the other-worktrees half: {:?}",
        plan.preconditions
    );
    assert!(
        plan.preconditions.iter().any(|p| matches!(
            p,
            Precondition::BranchNotCheckedOut { branch } if branch.as_str() == "main"
        )),
        "missing the this-worktree half — a second desk on the branch you are \
         standing on is the case the other precondition cannot see: {:?}",
        plan.preconditions
    );
}

/// The census must be *taken* for this operation, or the precondition it
/// carries reads `no_census_taken` — a `CensusFailed` — and every honest
/// `worktree add` is refused with "couldn't check". `needs_worktree_census` is
/// the one place both observation paths read, which is what keeps them from
/// drifting; this proves this operation is in it.
#[tokio::test]
async fn the_observation_actually_takes_a_census_for_this_operation() {
    let (_dir, repo) = seeded_repo();
    let (plan, observed) = build_plan(&repo, add("review-549", "main"), tokens()).await;
    let held = plan
        .preconditions
        .iter()
        .zip(&observed.held_at_build)
        .find(|(p, _)| matches!(p, Precondition::BranchFreeInEveryOtherWorktree { .. }))
        .map(|(_, held)| *held);
    assert_eq!(
        held,
        Some(true),
        "the collision precondition did not hold on a repository with no other \
         worktrees — the census was not taken, so it read as 'nobody looked'"
    );
}

// ---------------------------------------------------------------------------
// The omission that carries the fence
// ---------------------------------------------------------------------------

/// `exec_add_worktree`'s body, with `//` comment lines dropped and whitespace
/// collapsed — so the comparison is about what the function *does*.
fn exec_body_normalised() -> String {
    const BRANCH_EXEC: &str = include_str!("branch_exec.rs");
    let after = BRANCH_EXEC
        .split_once("pub(super) async fn exec_add_worktree(")
        .expect("branch_exec.rs no longer defines `exec_add_worktree`")
        .1;
    let end = after
        .find("\n}\n")
        .expect("`exec_add_worktree` is no longer a closed block");
    after[..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(|line| line.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

/// **The load-bearing omission, pinned as an exact body rather than a
/// denylist.**
///
/// Creating a worktree must never widen the allowed roots. The managed root is
/// admitted once, at startup; every child of it is therefore already servable,
/// and the new desk is discoverable and openable with no further grant. A
/// well-meaning `allow_repo_root(&dest)` here would look like a fix, would
/// "work", and would grow the allowlist by one permanent entry per creation —
/// each outliving the directory it was added for.
///
/// # Why exact, and not a list of forbidden calls
///
/// A denylist is a list someone has to keep complete. #654's version of this
/// test forbade the substring `allow_root` and grok found the hole: the public
/// wrapper is `allow_repo_root`, which does not *contain* `allow_root` as a
/// substring, so the widening call it was written to catch would have passed.
/// An exact body has nothing to keep complete — every edit to this function,
/// including one nobody thought to forbid, lands here as a diff.
///
/// This test failing is not necessarily a bug. It means this security-critical
/// function changed, and the change wants a human to look at it and update the
/// literal deliberately. That is the intended cost.
///
/// It has been paid once already: #656's fix routed all three failure arms
/// through `crate::state::withheld_detail`, so the literal below was reread and
/// updated on purpose. The check that mattered while doing so is the one the
/// paired positive below automates — no `allow_root`/`allow_repo_root` appeared,
/// and the destination is still computed from `worktrees_root()` joined to a
/// validated single segment.
#[test]
fn the_executor_body_is_exactly_what_it_should_be() {
    const EXPECTED: &str = concat!(
        "repo: &Path, name: &WorktreeName, branch: &BranchName, ) -> (StatusCode, String) { let ",
        "root = crate::state::worktrees_root(); if let Err(e) = std::fs::create_dir_all(&root) { ",
        "return ( StatusCode::INTERNAL_SERVER_ERROR, crate::state::withheld_detail( ",
        "\"/api/add-worktree\", \"Couldn't prepare the worktrees folder.\", &format!(\"{}: {e}\", ",
        "root.display()), ), ); } let dest = root.join(name.as_str()); if dest.exists() { return ",
        "( StatusCode::CONFLICT, format!( \"There is already a desk called ‘{name}’. Pick another ",
        "name, or open \\ the existing one from the worktree list.\" ), ); } let Some(dest_str) = ",
        "dest.to_str() else { return ( StatusCode::INTERNAL_SERVER_ERROR, \"The worktrees folder's ",
        "path is not valid UTF-8.\".to_string(), ); }; let args = [\"worktree\", \"add\", dest_str, ",
        "branch.as_str()]; let output = match crate::git_cmd::git_output_in_managed_root(repo, ",
        "&args, &root).await { Ok(output) => output, Err(e) => { return ( ",
        "StatusCode::INTERNAL_SERVER_ERROR, crate::state::withheld_detail( \"/api/add-worktree\", ",
        "\"Couldn't run git to open the desk.\", &e.to_string(), ), ) } }; if ",
        "!output.status.success() { let why = String::from_utf8_lossy(&output.stderr); return ( ",
        "StatusCode::CONFLICT, crate::state::withheld_detail( \"/api/add-worktree\", &format!( \"git ",
        "wouldn't open a desk called ‘{name}’ on ‘{branch}’. A branch \\ can only be checked out ",
        "at one desk at a time — the worktree list \\ says which one has it.\" ), &why, ), ); } ( ",
        "StatusCode::OK, format!(\"Opened a second desk called ‘{name}’ on ‘{branch}’.\"), )",
    );
    assert_eq!(
        exec_body_normalised(),
        EXPECTED,
        "\n`exec_add_worktree` changed. This is a security-critical function: it is \
         the one place that creates a directory for a worktree, and the guarantee it \
         carries is an OMISSION — it must not admit a new allowed root. Read the diff, \
         confirm the change does not widen the fence, then update this literal \
         deliberately.\n"
    );
}

/// The paired positive for the pin above: an exact-body assertion is only worth
/// having if the body it pins actually does the safe thing. This names the two
/// facts the literal is there to protect, so a future edit that updates the
/// literal without thinking still has to keep these true.
#[test]
fn the_pinned_body_computes_its_path_and_grants_only_the_managed_root() {
    let body = exec_body_normalised();
    assert!(
        body.contains("root.join(name.as_str())"),
        "the destination is no longer computed from the managed root and a validated \
         name: {body}"
    );
    assert!(
        body.contains("git_output_in_managed_root(repo, &args, &root)"),
        "the spawn's write grant is no longer the managed root itself — a grant \
         derived from anything else is a way to hand git a directory the fence \
         never admitted: {body}"
    );
}

// ---------------------------------------------------------------------------
// End to end, against a real git
// ---------------------------------------------------------------------------

/// The one owner of `GIT_VISTA_WORKTREES_ROOT` across this file's tests.
///
/// Same discipline, for the same measured reason, as `sandbox::argv`'s
/// `SSH_AUTH_SOCK_LOCK`: `set_var` mutates process-wide state and `cargo test`
/// runs this binary's tests on threads of one process, so two tests setting
/// this key concurrently would silently read each other's root. Scoped to this
/// file because nothing else in the tree touches this key.
static WORKTREES_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point the managed root at `root`, run `f` to completion on a private
/// runtime, then restore whatever the variable held before.
///
/// `f` returns a future, but this function is **synchronous** and drives it
/// with `block_on` rather than being `async` itself. That is deliberate: the
/// guard must be held for the whole critical section, and holding a
/// `MutexGuard` across an `.await` is both a clippy denial here
/// (`await_holding_lock`) and a real hazard — the task could be parked with
/// the lock held while another test waits on it. Blocking inside the section
/// has neither problem, because nothing else in this binary can be scheduled
/// onto this thread while it blocks.
fn with_worktrees_root<F, Fut, T>(root: &Path, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _guard = WORKTREES_ROOT_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var_os("GIT_VISTA_WORKTREES_ROOT");
    // SAFETY: `WORKTREES_ROOT_LOCK`, held across this whole function, is the
    // only synchronization this key needs — this file is its one writer.
    unsafe { std::env::set_var("GIT_VISTA_WORKTREES_ROOT", root) };
    let out = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
        .block_on(f());
    match prior {
        Some(v) => unsafe { std::env::set_var("GIT_VISTA_WORKTREES_ROOT", v) },
        None => unsafe { std::env::remove_var("GIT_VISTA_WORKTREES_ROOT") },
    }
    out
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// The pipeline test `contract_suite::covered_by` names for this operation:
/// a real `git worktree add`, and the created desk lands as a **direct child
/// of the managed root** — which is the containment claim, observed rather
/// than argued.
#[test]
fn add_worktree_opens_a_second_desk_under_the_managed_root() {
    let (_dir, repo) = seeded_repo();
    // A branch that is not checked out here, so both halves of git's rule are
    // satisfied and the operation can actually run.
    git_out(&repo, &["branch", "feature/desk"]);

    let managed = tempfile::tempdir().unwrap();
    let (status, body) = with_worktrees_root(managed.path(), || async {
        plan_and_execute_in(
            &repo,
            None,
            tokens(),
            add("review-549", "feature/desk"),
            crate::planner::DropProof::Nothing,
        )
        .await
    });

    assert_eq!(status, StatusCode::OK, "{body}");

    // It exists, it is where the app said it would be, and it is a DIRECT
    // child — not merely somewhere beneath the root, which a `..` in a name
    // could also have satisfied.
    let dest = managed.path().join("review-549");
    assert!(dest.is_dir(), "no desk at {}", dest.display());
    assert_eq!(
        dest.parent(),
        Some(managed.path()),
        "the desk is not a direct child of the managed root"
    );

    // And git agrees it is a worktree of this repository, on that branch.
    let listing = git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        listing.contains("refs/heads/feature/desk"),
        "git does not list the new desk on its branch:\n{listing}"
    );
}

/// The paired negative, and the half that matters: the SAME operation on a
/// branch that is already checked out here is refused rather than attempted.
/// Without this, the test above would pass on an executor that ignored every
/// precondition.
///
/// # The third assertion, and why the first two were not enough
///
/// As shipped in #654 this test asserted only `status != OK` and that the
/// destination is absent — and it stayed green while the refusal body carried
/// an absolute path, because this request skips a failed precondition, reaches
/// git, and git answers `fatal: 'main' is already used by worktree at
/// '<abs path>'`, which was relayed with only `.trim()` applied (found by
/// codex and grok independently, on PR #656).
///
/// So the body is now read. It is asserted against **this run's actual
/// temporary directories**, not against a pattern that could match by
/// accident: `managed.path()` is where the desk would have gone and `repo` is
/// where git would have named the collision, and neither may appear.
#[test]
fn a_desk_on_the_branch_you_are_standing_on_is_refused() {
    let (_dir, repo) = seeded_repo();
    let managed = tempfile::tempdir().unwrap();
    let (status, body) = with_worktrees_root(managed.path(), || async {
        // `main` is this repository's checked-out branch.
        plan_and_execute_in(
            &repo,
            None,
            tokens(),
            add("review-549", "main"),
            crate::planner::DropProof::Nothing,
        )
        .await
    });

    assert_ne!(
        status,
        StatusCode::OK,
        "a second desk on the checked-out branch must be refused: {body}"
    );
    assert!(
        !managed.path().join("review-549").exists(),
        "the desk was created despite the refusal"
    );

    for leaked in [managed.path(), repo.as_path()] {
        let leaked = leaked.to_string_lossy();
        assert!(
            !body.contains(leaked.as_ref()),
            "the refusal carried the absolute path `{leaked}` with \
             GIT_VISTA_EXPOSE_PATHS unset: {body}"
        );
    }
    // The paired positive for the redaction: withholding the path must not
    // have cost the user the reason. The name and the branch are their own
    // words, so both survive.
    assert!(
        body.contains("review-549") && body.contains("main"),
        "a refusal the user cannot act on is not an improvement: {body}"
    );
}

// ---------------------------------------------------------------------------
// The managed root is ADMITTED, not merely written to (#656 fix 1)
//
// ADR 0118's containment argument — "a new desk is inside the fence by
// construction, because the root is admitted once at startup" — was a sentence
// with nothing performing it. `worktrees_root()` resolved and
// `exec_add_worktree` wrote there, but nothing ever scanned the root, so it was
// never in the allowed roots and a desk the app had just created was not
// servable (grok, on PR #656).
//
// These two tests are what turn the sentence into a mechanism. They use the
// process-global catalog deliberately: the allowed roots ARE the global
// catalog, and a test that supplied its own fence would prove nothing about the
// one the server actually consults. Each admits only its own unique temporary
// directory, so nothing another test can see is widened.
// ---------------------------------------------------------------------------

/// The fresh-install case, which is the one that was broken: the managed root
/// does not exist yet, `scan_worktrees_root` creates **and admits** it, and the
/// desk `AddWorktree` then makes is inside the fence.
///
/// The `create_dir_all` is load-bearing rather than tidy, and this test is
/// where that shows: `scan_direct_children` returns early — before
/// `allow_root` — when `read_dir` fails, so on a missing root the admission
/// never happens at all.
#[test]
fn a_missing_managed_root_is_created_admitted_and_its_desks_are_servable() {
    let (_dir, repo) = seeded_repo();
    git_out(&repo, &["branch", "feature/desk"]);
    let parent = tempfile::tempdir().unwrap();
    // Deliberately does NOT exist: a fresh install has never made a desk.
    let managed = parent.path().join("never-created");
    assert!(!managed.exists(), "the fixture must start with no root");

    let dest = with_worktrees_root(&managed, || async {
        let dest = managed.join("review-656");
        assert!(
            !crate::state::path_is_allowed(&dest),
            "the fixture must start outside the fence, or this proves nothing"
        );

        crate::state::scan_worktrees_root();
        assert!(managed.is_dir(), "the scan must create the root it admits");

        let (status, body) = plan_and_execute_in(
            &repo,
            None,
            tokens(),
            add("review-656", "feature/desk"),
            crate::planner::DropProof::Nothing,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        dest
    });

    let canonical = std::fs::canonicalize(&dest).expect("the desk exists");
    assert!(
        crate::state::path_is_allowed(&canonical),
        "the desk the app just created is outside its own fence: {}",
        canonical.display()
    );
}

/// The user-visible half of the same claim, and the one grok's finding was
/// actually about: the drawer offers to open a desk only when the census marks
/// it `Serviceable::Yes`, and that verdict is computed from the very allowed
/// roots the scan populates. Without the scan the row comes back
/// `OutsideAllowedRoots` and the desk the app just made cannot be opened.
///
/// Asserting on `Serviceable` rather than on `path_is_allowed` alone is what
/// makes this a test of the outcome rather than of the mechanism.
#[test]
fn a_desk_the_app_just_made_is_serviceable_in_the_census() {
    let (_dir, repo) = seeded_repo();
    git_out(&repo, &["branch", "feature/desk"]);
    let managed = tempfile::tempdir().unwrap();

    let siblings = with_worktrees_root(managed.path(), || async {
        crate::state::scan_worktrees_root();
        let (status, body) = plan_and_execute_in(
            &repo,
            None,
            tokens(),
            add("review-656b", "feature/desk"),
            crate::planner::DropProof::Nothing,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        match crate::worktree_census::worktree_census(&repo, false, &crate::state::path_is_allowed)
            .await
        {
            git_vista_protocol::WorktreeCensus::Observed { siblings } => siblings,
            git_vista_protocol::WorktreeCensus::CensusFailed { reason } => {
                panic!("the census failed: {reason}")
            }
        }
    });

    let desk = siblings
        .iter()
        .find(|s| s.name == "review-656b")
        .expect("the census must list the desk that was just created");
    assert_eq!(
        desk.serviceable,
        git_vista_protocol::Serviceable::Yes,
        "a desk under the managed root must be openable, not fenced off"
    );
}
