//! The seam #590 depends on: the plan export prints the argv the executor
//! runs, because there is only one of them.
//!
//! [`git_vista_protocol::plan_export`] can be tested on its own — it is a pure
//! function and its own suite covers what it produces. What that suite cannot
//! see is whether this crate's executors still build their argv themselves. If
//! they do, the export is a *second* opinion about what git will be asked to
//! do, which is precisely the drift the issue was filed to avoid: the printed
//! command would be a plausible reconstruction rather than the command, and it
//! would go silently wrong the first time someone changed an executor and not
//! the export.
//!
//! That is a property of *this* crate's source, so the check is a source scan —
//! the same layer-1 tripwire shape `argv_boundary` already uses for spawn
//! sites, and it reuses that module's comment stripper so a doc comment
//! naming a builder cannot satisfy the assertion.

use std::path::Path;

use crate::argv_boundary::code_only;

/// Every executor that implements an operation the export prints, and the
/// shared builders it must construct its argv with.
///
/// Read this as the export's dependency list. An entry here says: "the plan
/// export claims to print what this file runs, and this is the shared function
/// that makes the claim true." Removing a builder call from an executor —
/// re-inlining the argv, which is the regression this exists to catch — drops
/// the name from the file and fails the scan.
///
/// `pull.rs` earns its entry for one builder only. Its two halves are
/// `fetch::run_fetch` and `branch_exec`'s `exec_merge`/`exec_rebase`, which are
/// covered by their own rows; what pull owns alone is the remote-tracking name
/// the integration runs against, and *that* is the string the export has to
/// reproduce to print the second line. It is the one place a local `format!`
/// would have been most tempting and most invisible.
const SHARED_ARGV_BUILDERS: &[(&str, &[&str])] = &[
    (
        "src/planner/branch_exec.rs",
        &[
            "create_branch_argv",
            "checkout_argv",
            "merge_argv",
            "delete_branch_argv",
            "rebase_argv",
            "rebase_abort_argv",
            "reset_hard_argv",
            "move_branch_argv",
        ],
    ),
    (
        "src/planner/commit_exec.rs",
        &["commit_on_head_argv", "amend_commit_argv"],
    ),
    (
        "src/planner/staging_exec.rs",
        &["stage_all_argv", "unstage_all_argv"],
    ),
    (
        "src/planner/sequence_exec.rs",
        &[
            "cherry_pick_argv",
            "revert_compute_argv",
            "revert_commit_argv",
            "revert_abort_argv",
            "sequence_argv",
        ],
    ),
    ("src/planner/fetch.rs", &["fetch_argv"]),
    ("src/planner/pull.rs", &["tracking_ref"]),
    ("src/planner/push.rs", &["push_argv"]),
    (
        "src/planner/tag_exec.rs",
        &["create_tag_argv", "delete_local_tag_argv"],
    ),
    (
        "src/planner/remote_tags.rs",
        &["push_tag_argv", "delete_remote_tag_argv"],
    ),
    (
        "src/planner/stash.rs",
        &[
            "push_stash_argv",
            "apply_stash_argv",
            "branch_from_stash_argv",
            "drop_stash_argv",
        ],
    ),
    (
        "src/planner/conflict_exec.rs",
        &["resolve_conflict_argv", "stage_resolved_path_argv"],
    ),
    (
        "src/planner/worktree_exec.rs",
        &["discard_tracked_argv", "delete_untracked_argv"],
    ),
];

/// INVARIANT: every printable operation's argv is built by the shared builder
/// the export reads, not by the executor itself.
///
/// # What would make this pass while the mechanism was broken
///
/// Two things, and both are closed here rather than assumed:
///
/// 1. **A mention in a comment.** `code_only` blanks comments before the
///    search, so a doc comment saying "see `checkout_argv`" cannot satisfy the
///    row. This is the failure mode that makes naive source scans worthless,
///    and this file would have had it: the executors are heavily documented and
///    several doc comments name the builders on purpose.
/// 2. **A same-named local function.** The needle is the *call* through the
///    module path (`plan_export::checkout_argv`), not the bare name, so an
///    executor that grew its own `checkout_argv` would not be mistaken for one
///    calling the shared one.
///
/// What it deliberately does NOT claim: that the executor passes the builder's
/// result *to git* rather than ignoring it. A source scan cannot see that, and
/// pretending otherwise would be the "I did not look, reported as a fact"
/// failure this crate is organised against. That half is held by the
/// behavioural suites which run each executor against a real repository — the
/// scan's job is only to keep the argv from being written twice.
#[test]
fn every_printable_operation_builds_its_argv_through_the_shared_builder() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    for (rel, builders) in SHARED_ARGV_BUILDERS {
        let path = root.join(rel);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{rel} must be readable to be scanned: {e}"));
        let code = code_only(&source);
        for builder in *builders {
            let needle = format!("plan_export::{builder}");
            assert!(
                code.contains(&needle),
                "{rel} does not call {needle}.\n\n\
                 The plan export (#590) prints `{builder}`'s output as the command a \
                 user will type. If this executor builds that argv itself instead, the \
                 printout is a second opinion about what git will be asked to do, and \
                 the two will drift the first time one of them changes.\n\n\
                 Build the argv with the shared builder and pass it to the spawn — do \
                 not add a local copy, and do not weaken this row."
            );
        }
    }
}

/// INVARIANT: the scan above is looking at real files.
///
/// A table of paths that silently stopped existing — a module renamed, a file
/// split — would leave the scan passing over nothing at all while reading like
/// coverage. The `read_to_string` above already panics on a missing file, but
/// it panics with an IO error; this says what the reader needs to know.
#[test]
fn every_scanned_executor_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    for (rel, builders) in SHARED_ARGV_BUILDERS {
        assert!(
            root.join(rel).is_file(),
            "{rel} is listed in SHARED_ARGV_BUILDERS but does not exist — if the module \
             moved, move the row with it rather than deleting the coverage"
        );
        assert!(
            !builders.is_empty(),
            "{rel} is listed with no builders, which asserts nothing"
        );
    }
}
