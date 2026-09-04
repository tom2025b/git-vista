//! The `plan_<operation>` MCP tool surface (M2.23d, #248; ADR 0046): one tool
//! per exposed [`GitOperation`] variant, each of which **builds a reviewable
//! [`Plan`] and returns it, executing nothing**.
//!
//! Every tool in this file makes exactly one request — `POST /api/plan` — and
//! that endpoint reaches only `planner::build_plan_only`. There is no code
//! path from here to a write endpoint, and
//! [`tests::every_plan_tool_posts_only_to_api_plan`] proves it by capturing
//! the path each of the 23 tools actually sends. Executing an approved plan
//! is #249's `execute_plan`, a separate tool on a separate endpoint; keeping
//! the two apart is the whole point of the funnel — an agent submits a
//! reviewable plan, never argv.
//!
//! # The closed vocabulary, and how a 26th variant is caught
//!
//! [`exposure_of`] matches **exhaustively over [`GitOperation`], with no
//! wildcard arm**, mapping every variant either to its tool name or to a
//! stated reason it has none. A new variant therefore fails *this crate's*
//! build until somebody classifies it — the same guard
//! `contract_suite::covered_by` gives the server, applied one crate over.
//! Compile-time exhaustiveness is only half of it: [`tests`] below censuses
//! the classification against `git-vista-protocol`'s **own** golden fixture
//! (`tests/fixtures/plan_v1.json`, one plan per variant, written for an
//! unrelated purpose), so the vocabulary this file believes in and the
//! vocabulary the wire contract pins are checked against each other in both
//! directions rather than either being trusted.
//!
//! # Two variants are deliberately not exposed
//!
//! - **[`GitOperation::ResetTestRepo`]** — #153's explicit instruction. It
//!   restores a `gv --seed`ed fixture repository *and wipes the app journal*,
//!   which is a test-harness affordance, not a git operation a reviewer would
//!   ever approve. There is no `plan_reset_test_repo`, and
//!   [`tests::the_unexposed_variants_have_no_tool_and_no_dispatch_arm`]
//!   asserts the dispatcher refuses that name outright.
//! - **[`GitOperation::StageSelection`]** — its `patch: String` and
//!   `whole_files: Vec<String>` fields are **not** protocol newtypes and are
//!   not client-supplied: the server builds them from a
//!   [`PatchPlan`](git_vista_protocol::PatchPlan) via
//!   `patch_build::build_selected_patch`, and the operation hash binds those
//!   exact bytes. Exposing it would mean an MCP tool taking a free-form
//!   patch — precisely the free-form-string input #248 forbids — and the
//!   bytes would not be the ones the gate verified. Partial staging over MCP
//!   needs a `PatchPlan`-shaped surface of its own, which is a different
//!   issue; see ADR 0046 §"Alternatives considered".
//!
//! That exclusion rule is not case-by-case taste. It is the same rule stated
//! once: **a variant is exposable exactly when every one of its fields is a
//! validating protocol newtype, a `bool`, or a closed enum** — the things a
//! client can legitimately author and the wire boundary can legitimately
//! refuse. `StageSelection` is the only variant that fails it today.

use git_vista_protocol::{
    BranchName, CommitMessage, CommitOid, ForcePublish, GitOperation, MergeStrategy, Plan,
    Precondition, RecoveryStrategy, RefChange, RefName, RefState, RemoteName, RiskLevel,
    TagAnnotation, TagMessage, TagName, WorktreePath,
};

use git_vista_session::auth::{self, Session};
use git_vista_session::http;

use crate::tools::{PostFn, ToolError};

/// The one endpoint every tool in this module talks to. A constant, not a
/// literal at 23 call sites, so "these tools only ever reach the build stage"
/// is one thing to read and one thing for a test to pin.
pub(crate) const PLAN_ENDPOINT: &str = "/api/plan";

// ---------------------------------------------------------------------------
// The closed classification
// ---------------------------------------------------------------------------

/// Whether a [`GitOperation`] variant is reachable through an MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exposure {
    /// Exposed as this `plan_*` tool.
    Tool(&'static str),
    /// Deliberately not exposed; the payload is the reason, in words.
    Excluded(&'static str),
}

/// The compile-time coverage guard for this crate: every [`GitOperation`]
/// variant is classified. **No wildcard arm, on purpose** — a 26th variant
/// fails to compile here until somebody decides whether an agent may plan it.
///
/// Grouped-arm style (`A | B | C => …`) is avoided for the `Tool` arms: each
/// variant names its own tool, one line, so the mapping reads as a table.
pub(crate) fn exposure_of(op: &GitOperation) -> Exposure {
    use Exposure::{Excluded, Tool};
    match op {
        // M3.24 (#77) — all three excluded for now, and the reason is the same
        // one for each: an agent cannot choose a stash entry it cannot see.
        //
        // Every stash operation is addressed by a POSITIONAL selector
        // (`stash@{n}`) that renumbers on every drop, and this surface has no
        // stash-listing tool yet. Exposing a planner without the reader would
        // be exposing a tool whose only safe use requires information the
        // agent has no way to obtain — it would have to guess `stash@{0}` and
        // hope. The tools land with the read tool, in their own slice.
        GitOperation::PushStash { .. } => Excluded(
            "the stash tool surface lands with its stash-listing reader; a planner \
             without one would invite guessing at entries",
        ),
        GitOperation::ApplyStash { .. } => Excluded(
            "addressed by a positional selector this surface cannot yet list, so an \
             agent could only guess which entry it is applying",
        ),
        GitOperation::BranchFromStash { .. } => Excluded(
            "same positional-selector problem as the rest of the drawer, and this one \
             also creates a branch and moves HEAD — three effects an agent would be \
             choosing blind",
        ),
        GitOperation::DropStash { .. } => Excluded(
            "destructive, and its safety rests on a compare-and-swap against a reflog \
             position an agent cannot see or re-derive between planning and submitting",
        ),
        // M4.31 (#84): not exposed as an MCP tool. Resolving a conflict is a
        // judgement about file content made one path at a time by someone
        // looking at three versions of it; an agent picking a side from a tool
        // description has not seen any of them.
        GitOperation::ResolveConflict { .. } => Excluded(
            "resolving a conflict is a judgement about file content, made one path \
             at a time by someone looking at all three versions of it; an agent \
             picking a side from a tool description has seen none of them",
        ),
        // M4.31c (#432), ADR 0069 decision 7: excluded a fortiori. Whole-side
        // resolution is already excluded because choosing requires having
        // SEEN the sides; this variant carries arbitrary authored bytes, so
        // an agent exposing it has not only seen none of the sides, it would
        // be authoring file content from a tool description — strictly less
        // information than the case already excluded above.
        GitOperation::ResolveConflictContent { .. } => Excluded(
            "carries arbitrary authored file content chosen by looking at three \
             versions of a conflict; an agent has seen none of them and would be \
             authoring bytes from a tool description — excluded more strongly than \
             whole-side resolution above, which this inherits from and extends",
        ),
        GitOperation::CreateBranch { .. } => Tool("plan_create_branch"),
        GitOperation::CommitOnHead { .. } => Tool("plan_commit_on_head"),
        GitOperation::EmptyCommitOnBranch { .. } => Tool("plan_empty_commit_on_branch"),
        GitOperation::StageAll => Tool("plan_stage_all"),
        GitOperation::UnstageAll => Tool("plan_unstage_all"),
        GitOperation::CheckoutBranch { .. } => Tool("plan_checkout_branch"),
        GitOperation::MergeBranch { .. } => Tool("plan_merge_branch"),
        GitOperation::PushBranch { .. } => Tool("plan_push_branch"),
        GitOperation::DeleteBranch { .. } => Tool("plan_delete_branch"),
        GitOperation::ForceDeleteBranch { .. } => Tool("plan_force_delete_branch"),
        GitOperation::RebaseOntoBase { .. } => Tool("plan_rebase_onto_base"),
        GitOperation::RestoreBranch { .. } => Tool("plan_restore_branch"),
        GitOperation::ResetBranch { .. } => Tool("plan_reset_branch"),
        GitOperation::RevertCommit { .. } => Tool("plan_revert_commit"),
        // Unexposed for now, and for a DIFFERENT reason than the stash verbs
        // and conflict resolution. Those are excluded on principle: the agent
        // cannot see what it would be choosing — a positional selector that
        // renumbers, or three versions of a file. A merge revert has neither
        // problem; the commit is named by oid and the mainline is a small
        // integer the tool description could explain.
        //
        // So this one is excluded only because the tool is not BUILT yet, not
        // because it should not be. Recorded that way so a later session
        // adding it knows the exposure argument is already made and does not
        // have to re-litigate it. Caught by the catalog census, which refused
        // to accept a Tool() naming something that does not exist.
        // Same position as RevertMerge: exposure is defensible (a commit named
        // by oid, plus a small integer) and simply not built yet. Recorded as
        // pending rather than refused.
        // The sequence controls are the one place in this family where an
        // agent has genuinely LESS information than a human. Continue means
        // "the conflicts are resolved correctly", which is a judgement about
        // file content nobody can make from a tool description — the same
        // reason conflict resolution itself is excluded. Abort discards
        // hand-made resolutions outright.
        GitOperation::SequenceContinue => Excluded(
            "continue asserts the conflicts were resolved correctly, which is a \
             judgement about file content an agent has not seen",
        ),
        GitOperation::SequenceSkip => Excluded(
            "skip drops a commit from the sequence; deciding that requires knowing why \
             it conflicted, which is the same unseen judgement",
        ),
        GitOperation::SequenceAbort => Excluded(
            "abort discards every conflict resolution made during the sequence — \
             hand-made decisions that exist nowhere else",
        ),
        GitOperation::CherryPick { .. } => Excluded(
            "the plan_cherry_pick tool is not built yet; exposure is defensible and \
             pending, not refused",
        ),
        GitOperation::CherryPickMerge { .. } => Excluded(
            "the plan_cherry_pick_merge tool is not built yet; exposure is defensible \
             and pending, not refused",
        ),
        GitOperation::RevertMerge { .. } => Excluded(
            "the plan_revert_merge tool is not built yet; exposure is defensible \
             (oid plus a small integer, nothing an agent cannot see) and is simply \
             pending, not refused",
        ),
        GitOperation::DiscardTrackedPaths { .. } => Tool("plan_discard_tracked_paths"),
        GitOperation::DeleteUntrackedPaths { .. } => Tool("plan_delete_untracked_paths"),
        GitOperation::AmendCommit { .. } => Tool("plan_amend_commit"),
        GitOperation::FetchRemote { .. } => Tool("plan_fetch_remote"),
        GitOperation::PullBranch { .. } => Tool("plan_pull_branch"),
        GitOperation::CreateTag { .. } => Tool("plan_create_tag"),
        GitOperation::DeleteLocalTag { .. } => Tool("plan_delete_local_tag"),
        GitOperation::DeleteRemoteTag { .. } => Tool("plan_delete_remote_tag"),
        GitOperation::PushTag { .. } => Tool("plan_push_tag"),
        // See the module doc for both reasons; they are different reasons.
        GitOperation::ResetTestRepo => Excluded(
            "a `gv --seed` test-fixture restore that also wipes the app journal — \
             a harness affordance, not an operation a reviewer would approve (#153)",
        ),
        GitOperation::StageSelection { .. } => Excluded(
            "its patch bytes and pathspecs are built server-side from a PatchPlan \
             and bound by the operation hash; an MCP tool could only take them as \
             a free-form patch string, which #248 forbids",
        ),
    }
}

// ---------------------------------------------------------------------------
// The tool catalog
// ---------------------------------------------------------------------------

/// The `plan_*` half of `tools/list`. Appended to the read-only six by
/// [`crate::tools::tool_catalog`].
///
/// Every schema is closed (`additionalProperties: false`) and every field is
/// **required unless the protocol itself models it as optional**. That is not
/// pedantry: `git-vista-protocol` deliberately gives `allow_empty`,
/// `set_upstream`, `force`, `strategy` and `sign` no `#[serde(default)]`, so
/// that a plan always *says* what it will do rather than inheriting an
/// answer from a config file the reviewer never saw. A schema that made them
/// optional would reintroduce exactly the silent default those types exist to
/// forbid.
pub(crate) fn plan_tool_catalog() -> Vec<serde_json::Value> {
    let branch = |what: &str| {
        serde_json::json!({
            "type": "string",
            "description": format!("Branch name, e.g. \"main\" or \"feature/x\" — {what}.")
        })
    };
    let oid = |what: &str| {
        serde_json::json!({
            "type": "string",
            "description": format!("Full 40- or 64-character lowercase hex commit id — {what}.")
        })
    };
    let remote = serde_json::json!({
        "type": "string",
        "description": "Configured remote name, e.g. \"origin\"."
    });
    let paths = serde_json::json!({
        "type": "array",
        "minItems": 1,
        "items": { "type": "string" },
        "description": "Worktree-relative paths (no leading '/', no '..' segment, \
                        never absolute). Each is validated as a WorktreePath."
    });
    let tag_name = serde_json::json!({
        "type": "string",
        "description": "Tag name without the refs/tags/ prefix, e.g. \"v1.0.0\"."
    });

    vec![
        tool(
            "plan_create_branch",
            "Plan `git branch <name> <at>` — create a branch at a commit. Builds the \
             plan only; nothing is created until the plan is submitted.",
            serde_json::json!({
                "name": branch("the new branch to create"),
                "at": oid("the commit the new branch will point at"),
            }),
            &["name", "at"],
        ),
        tool(
            "plan_commit_on_head",
            "Plan `git commit -m <message>` on the checked-out branch. Commits \
             whatever is already staged — call plan_stage_all (or stage in the app) \
             first if nothing is.",
            serde_json::json!({
                "message": { "type": "string", "description": "The commit message." },
                "allow_empty": {
                    "type": "boolean",
                    "description": "Pass --allow-empty, committing even with nothing staged. \
                                    Required, with no default: the plan must say which."
                },
            }),
            &["message", "allow_empty"],
        ),
        tool(
            "plan_empty_commit_on_branch",
            "Plan an empty commit on a branch that is NOT checked out (git commit-tree \
             on that branch's own tree, then a compare-and-swap update-ref). HEAD, the \
             index and the working tree are untouched.",
            serde_json::json!({
                "branch": branch("the branch to commit onto; must not be checked out"),
                "message": { "type": "string", "description": "The commit message." },
                "expected_tip": oid(
                    "the tip you reviewed; the operation is refused if the branch moved",
                ),
            }),
            &["branch", "message", "expected_tip"],
        ),
        tool(
            "plan_stage_all",
            "Plan `git add -A` — stage every working-tree change.",
            serde_json::json!({}),
            &[],
        ),
        tool(
            "plan_unstage_all",
            "Plan `git reset -q HEAD` — unstage everything, keeping every edit in the \
             working tree.",
            serde_json::json!({}),
            &[],
        ),
        tool(
            "plan_checkout_branch",
            "Plan `git checkout <branch>` — move HEAD and the working tree. Git itself \
             refuses if local changes would be overwritten.",
            serde_json::json!({ "branch": branch("the branch to check out") }),
            &["branch"],
        ),
        tool(
            "plan_merge_branch",
            "Plan `git merge --no-edit <branch>` into the checked-out branch. The \
             destination is always whatever is checked out; this never switches branches.",
            serde_json::json!({ "branch": branch("the branch to merge in") }),
            &["branch"],
        ),
        tool(
            "plan_push_branch",
            "Plan `git push [--set-upstream] [--force-with-lease] <remote> <branch>`. \
             A push leaves this machine: read the plan's risk and recovery before \
             submitting. There is no unguarded force — the only force available is \
             with-lease, which carries the remote tip you reviewed.",
            serde_json::json!({
                "branch": branch("the local branch to push"),
                "remote": remote,
                "set_upstream": {
                    "type": "boolean",
                    "description": "Also record <remote>/<branch> as this branch's upstream \
                                    (--set-upstream). A config write, not a history change."
                },
                "force": {
                    "type": "object",
                    "description": "Whether this push may overwrite the remote branch, and \
                                    under what guard. Required, with no default.",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["none", "with_lease"],
                            "description": "\"none\" is fast-forward-only (git refuses a \
                                            non-fast-forward). \"with_lease\" force-pushes \
                                            only while the remote branch still points where \
                                            you saw it."
                        },
                        "expected_remote_tip": {
                            "type": "string",
                            "description": "Required for \"with_lease\", forbidden for \
                                            \"none\": the remote-tracking tip you reviewed."
                        }
                    },
                    "required": ["mode"],
                    "additionalProperties": false
                },
            }),
            &["branch", "remote", "set_upstream", "force"],
        ),
        tool(
            "plan_delete_branch",
            "Plan `git branch -d <branch>` — the SAFE delete; git refuses a branch \
             whose commits are not merged.",
            serde_json::json!({ "branch": branch("the branch to delete") }),
            &["branch"],
        ),
        tool(
            "plan_force_delete_branch",
            "Plan `git branch -D <branch>` — delete even when unmerged, discarding any \
             commits only that branch holds. Use plan_delete_branch unless git has \
             already refused it.",
            serde_json::json!({ "branch": branch("the branch to force-delete") }),
            &["branch"],
        ),
        tool(
            "plan_rebase_onto_base",
            "Plan `git rebase <base>` of the checked-out branch. A failed rebase is \
             --abort'ed, restoring the pre-rebase state.",
            serde_json::json!({
                "base": {
                    "type": "string",
                    "description": "The base ref to rebase onto, e.g. \"origin/main\" or \
                                    \"main\" or a full refs/... name."
                },
            }),
            &["base"],
        ),
        tool(
            "plan_restore_branch",
            "Plan `git branch <name> <tip>` — re-create a deleted branch at its \
             journaled tip. The safe undo for a deletion.",
            serde_json::json!({
                "name": branch("the deleted branch to re-create"),
                "tip": oid("the tip it had before deletion"),
            }),
            &["name", "tip"],
        ),
        tool(
            "plan_reset_branch",
            "Plan moving a branch back to an earlier commit — `git reset --hard <to>` \
             when it is checked out with a clean worktree, else `git branch -f`. \
             Compare-and-swapped on expected_tip: refused if the branch moved.",
            serde_json::json!({
                "branch": branch("the branch to move"),
                "to": oid("the commit to move it back to"),
                "expected_tip": oid("the tip you reviewed; a mismatch refuses the operation"),
            }),
            &["branch", "to", "expected_tip"],
        ),
        tool(
            "plan_revert_commit",
            "Plan `git revert --no-edit <commit>` — the history-preserving undo for a \
             commit that is already shared. Aborted on conflict.",
            serde_json::json!({ "commit": oid("the commit to revert") }),
            &["commit"],
        ),
        tool(
            "plan_discard_tracked_paths",
            "Plan `git checkout -- <paths>` — discard uncommitted changes to \
             already-tracked paths. A worktree-only edit has NO other copy; read the \
             plan's recovery before submitting.",
            serde_json::json!({ "paths": paths }),
            &["paths"],
        ),
        tool(
            "plan_delete_untracked_paths",
            "Plan `git clean -f -- <paths>` — delete untracked paths outright. Nothing \
             in the repository can restore them: an untracked path was never written \
             to git's object database.",
            serde_json::json!({ "paths": paths }),
            &["paths"],
        ),
        tool(
            "plan_amend_commit",
            "Plan `git commit --amend -m <message>` — rewrite the checked-out branch's \
             tip commit in place instead of adding one on top. Compare-and-swapped on \
             expected_tip. Rewriting an already-published commit is history rewriting; \
             the plan says so.",
            serde_json::json!({
                "message": { "type": "string", "description": "The replacement commit message." },
                "expected_tip": oid("the tip commit you reviewed and intend to rewrite"),
                "allow_empty": {
                    "type": "boolean",
                    "description": "Pass --allow-empty so an amend with nothing staged still \
                                    rewrites the message. Required, with no default."
                },
            }),
            &["message", "expected_tip", "allow_empty"],
        ),
        tool(
            "plan_fetch_remote",
            "Plan `git fetch <remote>` — download the remote's objects and update its \
             remote-tracking refs. Touches no local branch, no index, no working tree.",
            serde_json::json!({ "remote": remote }),
            &["remote"],
        ),
        tool(
            "plan_pull_branch",
            "Plan `git pull --no-rebase|--rebase <remote> <branch>` — fetch, then \
             integrate into the CHECKED-OUT branch. `branch` is the remote's branch \
             (the refspec), not the destination.",
            serde_json::json!({
                "remote": remote,
                "branch": branch("the remote's branch to pull from, e.g. \"main\""),
                "strategy": {
                    "type": "string",
                    "enum": ["merge", "rebase"],
                    "description": "How to integrate. Required, with no default: leaving it \
                                    open would let the repository's pull.rebase config decide, \
                                    a value nobody reviewed."
                },
            }),
            &["remote", "branch", "strategy"],
        ),
        tool(
            "plan_create_tag",
            "Plan `git tag <name> <target>` (lightweight) or `git tag -a|-s -m <message> \
             <name> <target>` (annotated). Omit `annotation` for lightweight; supplying \
             it makes a real tag object.",
            serde_json::json!({
                "name": tag_name,
                "target": oid("the commit the tag speaks for"),
                "annotation": {
                    "type": "object",
                    "description": "Present ⇒ annotated tag (a tag object with message, tagger \
                                    and date). Absent ⇒ lightweight tag (a bare ref).",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "The tag object's message body."
                        },
                        "sign": {
                            "type": "boolean",
                            "description": "GPG-sign the tag object (git tag -s). Required \
                                            whenever an annotation is given: an annotated tag \
                                            request always states whether it is signed."
                        }
                    },
                    "required": ["message", "sign"],
                    "additionalProperties": false
                },
            }),
            &["name", "target"],
        ),
        tool(
            "plan_delete_local_tag",
            "Plan `git tag -d <name>` — delete a local tag. Unlike `git branch -d` this \
             has NO merged-work guard: a tag that was the only ref keeping a commit \
             alive takes that commit with it.",
            serde_json::json!({ "name": tag_name }),
            &["name"],
        ),
        tool(
            "plan_delete_remote_tag",
            "Plan `git push <remote> --delete refs/tags/<name>` — delete a tag from a \
             remote. The effect leaves this machine; other clones keep what they \
             already fetched.",
            serde_json::json!({ "name": tag_name, "remote": remote }),
            &["name", "remote"],
        ),
        tool(
            "plan_push_tag",
            "Plan `git push <remote> refs/tags/<name>` — publish exactly this one tag. \
             Never --tags, never --force.",
            serde_json::json!({ "name": tag_name, "remote": remote }),
            &["name", "remote"],
        ),
    ]
}

/// One catalog entry, with the two things every schema here must have:
/// `type: object` and `additionalProperties: false`, so a misspelled argument
/// is refused rather than silently ignored.
///
/// That last clause is a *behavioural* promise, not just advertised text:
/// `tools.rs`'s `every_tool_schema_is_a_closed_object` pins the declaration
/// across the whole surface, and `tools::reject_undeclared_arguments` — run at
/// the top of every `call_tool`, nested objects included — is what makes it
/// true at call time. Without that enforcement the declaration was decorative,
/// and a misspelled *optional* field (`anotation`, `curser`) was dropped
/// silently while the call proceeded as if it had never been given.
fn tool(
    name: &str,
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Build the [`GitOperation`] a `plan_*` tool call names, or `None` if `name`
/// is not a plan tool at all (so the caller can fall through to its own
/// unknown-tool handling).
///
/// Pure: every argument is validated and every protocol newtype constructed
/// **here**, before any network call exists — a malformed branch name or a
/// non-hex oid is refused inside this process, never forwarded for the server
/// to reject. That is the boundary #248 asks for, and it is why this function
/// takes no session.
pub(crate) fn operation_for(
    name: &str,
    args: &serde_json::Value,
) -> Option<Result<GitOperation, ToolError>> {
    Some(match name {
        "plan_create_branch" => (|| {
            Ok(GitOperation::CreateBranch {
                name: branch_arg(args, "name")?,
                at: oid_arg(args, "at")?,
            })
        })(),
        "plan_commit_on_head" => (|| {
            Ok(GitOperation::CommitOnHead {
                message: message_arg(args, "message")?,
                allow_empty: bool_arg(args, "allow_empty")?,
            })
        })(),
        "plan_empty_commit_on_branch" => (|| {
            Ok(GitOperation::EmptyCommitOnBranch {
                branch: branch_arg(args, "branch")?,
                message: message_arg(args, "message")?,
                expected_tip: oid_arg(args, "expected_tip")?,
            })
        })(),
        "plan_stage_all" => Ok(GitOperation::StageAll),
        "plan_unstage_all" => Ok(GitOperation::UnstageAll),
        "plan_checkout_branch" => {
            branch_arg(args, "branch").map(|branch| GitOperation::CheckoutBranch { branch })
        }
        "plan_merge_branch" => {
            branch_arg(args, "branch").map(|branch| GitOperation::MergeBranch { branch })
        }
        "plan_push_branch" => (|| {
            Ok(GitOperation::PushBranch {
                branch: branch_arg(args, "branch")?,
                remote: remote_arg(args, "remote")?,
                set_upstream: bool_arg(args, "set_upstream")?,
                force: force_arg(args, "force")?,
            })
        })(),
        "plan_delete_branch" => {
            branch_arg(args, "branch").map(|branch| GitOperation::DeleteBranch { branch })
        }
        "plan_force_delete_branch" => {
            branch_arg(args, "branch").map(|branch| GitOperation::ForceDeleteBranch { branch })
        }
        "plan_rebase_onto_base" => {
            ref_arg(args, "base").map(|base| GitOperation::RebaseOntoBase { base })
        }
        "plan_restore_branch" => (|| {
            Ok(GitOperation::RestoreBranch {
                name: branch_arg(args, "name")?,
                tip: oid_arg(args, "tip")?,
            })
        })(),
        "plan_reset_branch" => (|| {
            Ok(GitOperation::ResetBranch {
                branch: branch_arg(args, "branch")?,
                to: oid_arg(args, "to")?,
                expected_tip: oid_arg(args, "expected_tip")?,
            })
        })(),
        "plan_revert_commit" => {
            oid_arg(args, "commit").map(|commit| GitOperation::RevertCommit { commit })
        }
        "plan_discard_tracked_paths" => {
            paths_arg(args, "paths").map(|paths| GitOperation::DiscardTrackedPaths { paths })
        }
        "plan_delete_untracked_paths" => {
            paths_arg(args, "paths").map(|paths| GitOperation::DeleteUntrackedPaths { paths })
        }
        "plan_amend_commit" => (|| {
            Ok(GitOperation::AmendCommit {
                message: message_arg(args, "message")?,
                expected_tip: oid_arg(args, "expected_tip")?,
                allow_empty: bool_arg(args, "allow_empty")?,
            })
        })(),
        "plan_fetch_remote" => {
            remote_arg(args, "remote").map(|remote| GitOperation::FetchRemote { remote })
        }
        "plan_pull_branch" => (|| {
            Ok(GitOperation::PullBranch {
                remote: remote_arg(args, "remote")?,
                branch: branch_arg(args, "branch")?,
                strategy: strategy_arg(args, "strategy")?,
            })
        })(),
        "plan_create_tag" => (|| {
            Ok(GitOperation::CreateTag {
                name: tag_arg(args, "name")?,
                target: oid_arg(args, "target")?,
                annotation: annotation_arg(args, "annotation")?,
            })
        })(),
        "plan_delete_local_tag" => {
            tag_arg(args, "name").map(|name| GitOperation::DeleteLocalTag { name })
        }
        "plan_delete_remote_tag" => (|| {
            Ok(GitOperation::DeleteRemoteTag {
                name: tag_arg(args, "name")?,
                remote: remote_arg(args, "remote")?,
            })
        })(),
        "plan_push_tag" => (|| {
            Ok(GitOperation::PushTag {
                name: tag_arg(args, "name")?,
                remote: remote_arg(args, "remote")?,
            })
        })(),
        _ => return None,
    })
}

/// Run one `plan_*` tool: build the operation, `POST` it to
/// [`PLAN_ENDPOINT`], and answer the built plan plus its agent-readable
/// review digest. `None` when `name` is not a plan tool.
///
/// Production passes `http::post_json`; tests inject a capturing closure,
/// which is how [`tests::every_plan_tool_posts_only_to_api_plan`] can assert
/// the path and body of all 23 tools without a server.
pub(crate) fn call_plan_tool(
    name: &str,
    args: &serde_json::Value,
    session: &mut Option<Session>,
    post: &mut PostFn<'_>,
    authenticate: &mut dyn FnMut() -> Result<Session, String>,
) -> Option<Result<serde_json::Value, ToolError>> {
    let op = match operation_for(name, args)? {
        Ok(op) => op,
        Err(e) => return Some(Err(e)),
    };
    if let Err(refused) = check_exposure(name, &op) {
        return Some(Err(refused));
    }
    Some(build_plan(op, session, post, authenticate))
}

/// The second lock on the exclusion list, checked in production on every
/// call: the operation [`operation_for`]'s dispatch arm just built must be
/// the one [`exposure_of`] says this tool name exposes.
///
/// The dispatch arms and the classification are written independently — one
/// keyed by tool name, the other by variant — so this is not a tautology.
/// A future edit that gave `ResetTestRepo` a dispatch arm (the thing #153
/// forbids) would satisfy `operation_for` and be refused *here*, before any
/// request exists, because `exposure_of` still classifies it `Excluded`. A
/// dispatch arm wired to the wrong variant is caught the same way.
fn check_exposure(name: &str, op: &GitOperation) -> Result<(), ToolError> {
    match exposure_of(op) {
        Exposure::Tool(expected) if expected == name => Ok(()),
        Exposure::Tool(other) => Err(ToolError::Execution(format!(
            "`{name}` built the operation the tool table exposes as `{other}` — \
             refusing rather than planning an operation under the wrong name"
        ))),
        Exposure::Excluded(reason) => Err(ToolError::Execution(format!(
            "`{name}` names an operation that is deliberately not available \
             through MCP: {reason}"
        ))),
    }
}

/// Production's [`call_plan_tool`], with the real HTTP client and
/// authenticator wired in. Kept separate so the injectable form above has no
/// production caller passing anything unusual.
pub(crate) fn call_plan_tool_live(
    name: &str,
    args: &serde_json::Value,
    session: &mut Option<Session>,
) -> Option<Result<serde_json::Value, ToolError>> {
    call_plan_tool(
        name,
        args,
        session,
        &mut |path, body, cookie, csrf| http::post_json(path, body, Some(cookie), Some(csrf)),
        &mut auth::authenticate,
    )
}

fn build_plan(
    op: GitOperation,
    session: &mut Option<Session>,
    post: &mut PostFn<'_>,
    authenticate: &mut dyn FnMut() -> Result<Session, String>,
) -> Result<serde_json::Value, ToolError> {
    let body = serde_json::to_vec(&op)
        .map_err(|e| ToolError::Execution(format!("could not encode the operation: {e}")))?;
    let raw = crate::tools::authed_post(PLAN_ENDPOINT, &body, session, post, authenticate)
        .map_err(ToolError::Execution)?;
    let plan: Plan = serde_json::from_slice(&raw).map_err(|e| {
        ToolError::Execution(format!("{PLAN_ENDPOINT} did not return a valid Plan: {e}"))
    })?;
    let plan_json = serde_json::to_value(&plan)
        .map_err(|e| ToolError::Execution(format!("could not re-encode the plan: {e}")))?;
    Ok(serde_json::json!({
        "plan": plan_json,
        "review": review_of(&plan),
    }))
}

// ---------------------------------------------------------------------------
// The review digest
// ---------------------------------------------------------------------------

/// The agent-readable half of a `plan_*` result: risk and recovery as named
/// values *with their meaning spelled out*, plus every precondition and
/// expected ref change as a sentence.
///
/// #248's acceptance criterion is that these are readable by an agent
/// deciding whether to approve, "not just embedded JSON it has to parse
/// blind". The full [`Plan`] DTO still travels verbatim beside this (under
/// `plan`) — unchanged, so #249 can submit exactly the bytes the server
/// issued and the operation hash still binds. This digest never replaces it.
pub(crate) fn review_of(plan: &Plan) -> serde_json::Value {
    serde_json::json!({
        "risk": risk_name(plan.risk),
        "risk_means": risk_meaning(plan.risk),
        "recovery": recovery_name(&plan.recovery),
        "recovery_means": recovery_meaning(&plan.recovery),
        "preconditions": plan
            .preconditions
            .iter()
            .map(precondition_sentence)
            .collect::<Vec<_>>(),
        "expected_ref_changes": plan
            .expected_ref_changes
            .iter()
            .map(ref_change_sentence)
            .collect::<Vec<_>>(),
        "operation_hash": plan.operation_hash.as_str(),
        "expires_at_unix": plan.expires_at,
        "nothing_has_run_yet":
            "This tool only built the plan. The repository is unchanged; submitting \
             the plan is a separate, explicit step.",
    })
}

fn risk_name(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Safe => "safe",
        RiskLevel::Reversible => "reversible",
        RiskLevel::Destructive => "destructive",
        RiskLevel::Remote => "remote",
    }
}

fn risk_meaning(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Safe => {
            "Nothing can be lost: this only adds or re-arranges state that stays reachable."
        }
        RiskLevel::Reversible => {
            "State moves, but a local undo exists — see recovery for exactly which."
        }
        RiskLevel::Destructive => {
            "Commits or working-tree state can become unreachable. Confirm with the \
             user before submitting."
        }
        RiskLevel::Remote => {
            "The effect leaves this machine. Once the remote and its other clients \
             have it, no local command can recall it."
        }
    }
}

fn recovery_name(recovery: &RecoveryStrategy) -> &'static str {
    match recovery {
        RecoveryStrategy::NotNeeded => "not_needed",
        RecoveryStrategy::ResetRef { .. } => "reset_ref",
        RecoveryStrategy::RecreateBranch { .. } => "recreate_branch",
        RecoveryStrategy::DeleteCreatedBranch { .. } => "delete_created_branch",
        RecoveryStrategy::RecreateTag { .. } => "recreate_tag",
        RecoveryStrategy::RecreateStashEntry { .. } => "recreate_stash_entry",
        RecoveryStrategy::DeleteCreatedTag { .. } => "delete_created_tag",
        RecoveryStrategy::CheckoutPrevious { .. } => "checkout_previous",
        RecoveryStrategy::RevertCommit { .. } => "revert_commit",
        RecoveryStrategy::RecoverableIfStaged => "recoverable_if_staged",
        RecoveryStrategy::ConflictRecreatableWhileInProgress => {
            "conflict_recreatable_while_in_progress"
        }
        RecoveryStrategy::Irrecoverable => "irrecoverable",
    }
}

fn recovery_meaning(recovery: &RecoveryStrategy) -> String {
    match recovery {
        RecoveryStrategy::ConflictRecreatableWhileInProgress => {
            "Undo by asking git to rebuild the conflict (git checkout --merge on the \
             path) — but only while the merge, rebase or cherry-pick that produced it \
             is still in progress. Once it is concluded or aborted, this stops being \
             possible."
                .to_string()
        }
        RecoveryStrategy::NotNeeded => {
            "Nothing is destroyed, so there is nothing to recover.".to_string()
        }
        RecoveryStrategy::RecreateStashEntry { at, .. } => format!(
            "Undo by re-creating the stash entry from {}. It comes back at the \
             top of the list (stash@{{0}}), not at its original position.",
            at.as_str()
        ),
        RecoveryStrategy::ResetRef { ref_name, to } => format!(
            "Undo by moving {} back to {}.",
            ref_name.as_str(),
            to.as_str()
        ),
        RecoveryStrategy::RecreateBranch { name, at } => format!(
            "Undo by re-creating branch {} at {} — but only until git gc prunes those commits.",
            name.as_str(),
            at.as_str()
        ),
        RecoveryStrategy::DeleteCreatedBranch { name } => format!(
            "Undo by deleting the branch {} this would create; nothing else changes.",
            name.as_str()
        ),
        RecoveryStrategy::RecreateTag { name, at } => format!(
            "Undo by pointing refs/tags/{} back at {}, which restores an annotated tag \
             byte-identically (message, tagger, signature) — until git gc prunes it.",
            name.as_str(),
            at.as_str()
        ),
        RecoveryStrategy::DeleteCreatedTag { name } => format!(
            "Undo by deleting the tag {} this would create; nothing else changes.",
            name.as_str()
        ),
        RecoveryStrategy::CheckoutPrevious { branch } => {
            format!("Undo by checking {} back out.", branch.as_str())
        }
        RecoveryStrategy::RevertCommit { commit } => format!(
            "Undo by reverting {} — history-preserving, since the result may already be shared.",
            commit.as_str()
        ),
        RecoveryStrategy::RecoverableIfStaged => {
            "git-vista offers no undo. Content that was staged before this runs may still \
             exist as a dangling blob until the next git gc; content that was only ever \
             edited in the working tree has no other copy at all."
                .to_string()
        }
        RecoveryStrategy::Irrecoverable => {
            "There is no undo, and none is possible: the effect either left this machine \
             or was never in git's object database."
                .to_string()
        }
    }
}

fn precondition_sentence(check: &Precondition) -> String {
    match check {
        Precondition::RefAt { ref_name, oid } => format!(
            "{} must still be exactly at {} when the plan runs.",
            ref_name.as_str(),
            oid.as_str()
        ),
        Precondition::RefExists { ref_name } => format!("{} must exist.", ref_name.as_str()),
        Precondition::RefAbsent { ref_name } => {
            format!("{} must not already exist.", ref_name.as_str())
        }
        Precondition::BranchCheckedOut { branch } => {
            format!("{} must be the checked-out branch.", branch.as_str())
        }
        Precondition::BranchNotCheckedOut { branch } => {
            format!("{} must NOT be the checked-out branch.", branch.as_str())
        }
        Precondition::CleanWorktree => "The working tree and index must be clean.".to_string(),
        Precondition::RemoteConfigured { remote } => {
            format!("A remote named {} must be configured.", remote.as_str())
        }
        Precondition::SeedRecorded => {
            "The repository must carry a recorded `gv --seed`.".to_string()
        }
        // M11.02 (#547). An agent reading this needs the rule and the way to
        // check it; which worktree actually holds the branch is in the
        // server's refusal, not in the precondition.
        Precondition::BranchFreeInEveryOtherWorktree { branch } => format!(
            "No other worktree of this repository may have `{}` checked out \
             (`git worktree list`).",
            branch.as_str()
        ),
    }
}

fn ref_change_sentence(change: &RefChange) -> String {
    format!(
        "{}: {} → {}",
        change.ref_name.as_str(),
        ref_state_words(&change.before),
        ref_state_words(&change.after)
    )
}

fn ref_state_words(state: &RefState) -> String {
    match state {
        RefState::Absent => "absent".to_string(),
        RefState::At(oid) => oid.as_str().to_string(),
        RefState::Symbolic(name) => format!("symbolic → {}", name.as_str()),
        RefState::Computed => "a new commit this operation creates".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Typed argument extraction
// ---------------------------------------------------------------------------
//
// Each helper takes one JSON argument and returns the *protocol newtype*, so
// validation happens once, here, at the process boundary — never as a raw
// string forwarded to the server. A newtype's own constructor owns the rules
// (non-empty, not option-shaped, hex, worktree-relative); this layer only
// turns its error into a `ToolError` naming the field.

fn str_arg(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    match args.get(key) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        None | Some(serde_json::Value::Null) => Err(ToolError::Execution(format!(
            "missing required argument `{key}`"
        ))),
        Some(other) => Err(ToolError::Execution(format!(
            "`{key}` must be a string, got {other}"
        ))),
    }
}

fn bool_arg(args: &serde_json::Value, key: &str) -> Result<bool, ToolError> {
    match args.get(key) {
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        None | Some(serde_json::Value::Null) => Err(ToolError::Execution(format!(
            "missing required argument `{key}` — it has no default on purpose, \
             so the plan always says which behaviour was asked for"
        ))),
        Some(other) => Err(ToolError::Execution(format!(
            "`{key}` must be a boolean, got {other}"
        ))),
    }
}

/// Turn a newtype constructor's own refusal into a `ToolError` naming the
/// field. Every typed helper below funnels through this, so no validation
/// rule is ever re-implemented here.
fn typed<T, E: std::fmt::Display>(key: &str, built: Result<T, E>) -> Result<T, ToolError> {
    built.map_err(|e| ToolError::Execution(format!("`{key}`: {e}")))
}

fn branch_arg(args: &serde_json::Value, key: &str) -> Result<BranchName, ToolError> {
    typed(key, BranchName::new(str_arg(args, key)?))
}

fn ref_arg(args: &serde_json::Value, key: &str) -> Result<RefName, ToolError> {
    typed(key, RefName::new(str_arg(args, key)?))
}

fn oid_arg(args: &serde_json::Value, key: &str) -> Result<CommitOid, ToolError> {
    typed(key, CommitOid::new(str_arg(args, key)?))
}

fn message_arg(args: &serde_json::Value, key: &str) -> Result<CommitMessage, ToolError> {
    typed(key, CommitMessage::new(str_arg(args, key)?))
}

fn remote_arg(args: &serde_json::Value, key: &str) -> Result<RemoteName, ToolError> {
    typed(key, RemoteName::new(str_arg(args, key)?))
}

fn tag_arg(args: &serde_json::Value, key: &str) -> Result<TagName, ToolError> {
    typed(key, TagName::new(str_arg(args, key)?))
}

fn paths_arg(args: &serde_json::Value, key: &str) -> Result<Vec<WorktreePath>, ToolError> {
    let raw = match args.get(key) {
        Some(serde_json::Value::Array(a)) => a,
        None | Some(serde_json::Value::Null) => {
            return Err(ToolError::Execution(format!(
                "missing required argument `{key}`"
            )))
        }
        Some(other) => {
            return Err(ToolError::Execution(format!(
                "`{key}` must be an array of worktree-relative paths, got {other}"
            )))
        }
    };
    if raw.is_empty() {
        return Err(ToolError::Execution(format!(
            "`{key}` must name at least one path — an empty selection is not an operation"
        )));
    }
    raw.iter()
        .map(|v| {
            let s = v.as_str().ok_or_else(|| {
                ToolError::Execution(format!("`{key}` entries must be strings, got {v}"))
            })?;
            typed(key, WorktreePath::new(s))
        })
        .collect()
}

fn strategy_arg(args: &serde_json::Value, key: &str) -> Result<MergeStrategy, ToolError> {
    match str_arg(args, key)?.as_str() {
        "merge" => Ok(MergeStrategy::Merge),
        "rebase" => Ok(MergeStrategy::Rebase),
        other => Err(ToolError::Execution(format!(
            "`{key}` must be \"merge\" or \"rebase\", got {other:?}"
        ))),
    }
}

/// `force` on `plan_push_branch`. The two shapes are checked against each
/// other in both directions on purpose: `with_lease` **requires** the tip
/// (a lease that pins nothing is not a lease), and `none` **forbids** it (a
/// caller who supplied a tip and got a plain push would believe they were
/// protected by a compare-and-swap that never existed).
fn force_arg(args: &serde_json::Value, key: &str) -> Result<ForcePublish, ToolError> {
    let obj = match args.get(key) {
        Some(v @ serde_json::Value::Object(_)) => v,
        None | Some(serde_json::Value::Null) => {
            return Err(ToolError::Execution(format!(
                "missing required argument `{key}` — a push must state whether it may \
                 overwrite the remote branch; there is no default"
            )))
        }
        Some(other) => {
            return Err(ToolError::Execution(format!(
                "`{key}` must be an object like {{\"mode\": \"none\"}}, got {other}"
            )))
        }
    };
    let tip = obj.get("expected_remote_tip");
    match str_arg(obj, "mode")?.as_str() {
        "none" => {
            if tip.is_some_and(|v| !v.is_null()) {
                return Err(ToolError::Execution(format!(
                    "`{key}.expected_remote_tip` is only meaningful with mode \
                     \"with_lease\" — a fast-forward push pins nothing"
                )));
            }
            Ok(ForcePublish::None)
        }
        "with_lease" => Ok(ForcePublish::WithLease {
            expected_remote_tip: typed(
                "force.expected_remote_tip",
                CommitOid::new(str_arg(obj, "expected_remote_tip")?),
            )?,
        }),
        other => Err(ToolError::Execution(format!(
            "`{key}.mode` must be \"none\" or \"with_lease\", got {other:?}"
        ))),
    }
}

fn annotation_arg(args: &serde_json::Value, key: &str) -> Result<Option<TagAnnotation>, ToolError> {
    let obj = match args.get(key) {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(v @ serde_json::Value::Object(_)) => v,
        Some(other) => {
            return Err(ToolError::Execution(format!(
                "`{key}` must be an object with `message` and `sign`, got {other}"
            )))
        }
    };
    Ok(Some(TagAnnotation {
        message: typed(
            "annotation.message",
            TagMessage::new(str_arg(obj, "message")?),
        )?,
        sign: bool_arg(obj, "sign")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{
        GenerationToken, OperationHash, RepositoryToken, UnixSeconds, WorktreeToken,
    };
    use git_vista_session::http::HttpResponse;

    /// What the injected poster records: the path and body one tool sent.
    type CapturedPost = (String, Vec<u8>);

    fn ok_response(body: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    // -- the census oracle -------------------------------------------------
    //
    // `git-vista-protocol`'s golden fixture holds one Plan per GitOperation
    // variant, with its wire `op` tag — written for `plan_golden.rs`, not for
    // this crate, and pinned there by `golden_set_covers_every_operation_variant`.
    // Using it as the census input means the vocabulary this file classifies
    // and the vocabulary the wire contract commits to are checked against each
    // other, rather than this crate grading its own homework off a list it
    // also wrote.

    fn golden_op_tags() -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../git-vista-protocol/tests/fixtures/plan_v1.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading the protocol golden fixture {path:?}: {e}"));
        let plans: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let mut tags: Vec<String> = plans
            .as_array()
            .expect("the golden fixture is an array of plans")
            .iter()
            .map(|p| {
                p["operation"]["op"]
                    .as_str()
                    .expect("every golden plan's operation carries an `op` tag")
                    .to_string()
            })
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }

    /// The two wire tags this crate deliberately does not expose. Written out
    /// as tags (not as `Exposure::Excluded` results) so the census below can
    /// compare two independently-authored lists.
    // M3.24 (#77): the three stash operations are unexposed for one shared
    // reason — every stash entry is addressed by a positional selector that
    // renumbers on every drop, and this surface has no stash-listing tool yet.
    // A planner without the reader would be a tool whose only safe use needs
    // information the agent cannot obtain. They land together, in their own
    // slice. See `exposure_of` for the per-operation wording.
    // M4.31 (#84): resolve_conflict is unexposed for a different reason than
    // the stash three — picking a side is a judgement about file content made
    // by someone looking at all three versions of it, and an agent choosing
    // from a tool description has seen none of them. Listed here so the census
    // sees a considered exclusion rather than an omission.
    const UNEXPOSED_TAGS: &[&str] = &[
        "reset_test_repo",
        "resolve_conflict",
        "resolve_conflict_content",
        "stage_selection",
        "branch_from_stash",
        "cherry_pick",
        "sequence_abort",
        "sequence_continue",
        "sequence_skip",
        "cherry_pick_merge",
        "revert_merge",
        "push_stash",
        "apply_stash",
        "drop_stash",
    ];

    fn catalog_names() -> Vec<String> {
        plan_tool_catalog()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// The bidirectional census, against the protocol crate's own fixture:
    /// every operation in the wire vocabulary either has a `plan_<tag>` tool
    /// or is on the short unexposed list, and every tool in the catalog names
    /// an operation that actually exists in that vocabulary.
    ///
    /// This is the guard that a *new* variant cannot land half-covered.
    /// `exposure_of`'s wildcard-free match already refuses to compile without
    /// a classification; this refuses to pass without a tool or a stated
    /// exclusion, and refuses equally if a tool is invented for an operation
    /// the protocol does not have.
    #[test]
    fn the_plan_tool_surface_censuses_the_whole_wire_vocabulary() {
        let tags = golden_op_tags();
        assert!(
            tags.len() >= 25,
            "the golden fixture censused only {} operations — has it been trimmed?",
            tags.len()
        );

        let catalog = catalog_names();
        let mut expected: Vec<String> = tags
            .iter()
            .filter(|t| !UNEXPOSED_TAGS.contains(&t.as_str()))
            .map(|t| format!("plan_{t}"))
            .collect();
        expected.sort();
        let mut actual = catalog.clone();
        actual.sort();
        assert_eq!(
            actual, expected,
            "the plan_* tool surface and the protocol's operation vocabulary disagree"
        );

        // …and the unexposed tags really are in that vocabulary, so the
        // exclusion list can't quietly rot into naming operations that no
        // longer exist (which would silently stop excluding anything).
        for unexposed in UNEXPOSED_TAGS {
            assert!(
                tags.iter().any(|t| t == unexposed),
                "‘{unexposed}’ is on the unexposed list but is not an operation \
                 the protocol defines — the exclusion has rotted"
            );
        }
    }

    /// `exposure_of`'s classification agrees with the hand-written catalog —
    /// the third independent source. A variant classified `Tool("plan_x")`
    /// whose schema nobody wrote would pass the census above (which reads the
    /// fixture, not `exposure_of`) and fail here.
    #[test]
    fn exposure_of_agrees_with_the_hand_written_catalog() {
        let catalog = catalog_names();
        for op in samples() {
            let tag = serde_json::to_value(&op).unwrap()["op"]
                .as_str()
                .unwrap()
                .to_string();
            match exposure_of(&op) {
                Exposure::Tool(name) => {
                    assert_eq!(
                        name,
                        format!("plan_{tag}"),
                        "‘{tag}’ is exposed under a name that is not plan_<wire tag>"
                    );
                    assert!(
                        catalog.iter().any(|c| c == name),
                        "exposure_of claims ‘{name}’ but no catalog entry defines its schema"
                    );
                }
                Exposure::Excluded(reason) => {
                    assert!(
                        UNEXPOSED_TAGS.contains(&tag.as_str()),
                        "‘{tag}’ is excluded but is not on the reviewed unexposed list"
                    );
                    assert!(
                        reason.len() > 40,
                        "‘{tag}’ is excluded without a real stated reason: {reason:?}"
                    );
                }
            }
        }
    }

    /// One sample per variant. The list is only a *census input* — the
    /// compile-time coverage guard is `exposure_of`'s wildcard-free match, and
    /// this length assertion is what forces a new variant to get a sample too,
    /// so the runtime censuses above keep seeing the whole vocabulary.
    fn samples() -> Vec<GitOperation> {
        let zeros = "0".repeat(40);
        let oid = |s: &str| CommitOid::new(s.to_string()).unwrap();
        let branch = |s: &str| BranchName::new(s).unwrap();
        let message = |s: &str| CommitMessage::new(s).unwrap();
        let remote = || RemoteName::new("origin").unwrap();
        let tag = |s: &str| TagName::new(s).unwrap();
        let path = |s: &str| WorktreePath::new(s).unwrap();
        vec![
            // M3.24 (#77). DropStash is Excluded from the tool surface but
            // must still appear here: the census proves every protocol variant
            // has a considered exposure, and "deliberately excluded" is an
            // answer the census has to see.
            GitOperation::PushStash {
                message: None,
                keep_index: false,
                include_untracked: true,
            },
            GitOperation::ApplyStash {
                entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
                expected_oid: git_vista_protocol::CommitOid::new("1".repeat(40)).unwrap(),
            },
            GitOperation::BranchFromStash {
                name: git_vista_protocol::BranchName::new("from-stash").unwrap(),
                entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
                expected_oid: git_vista_protocol::CommitOid::new("1".repeat(40)).unwrap(),
            },
            GitOperation::DropStash {
                entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
                expected_oid: git_vista_protocol::CommitOid::new("1".repeat(40)).unwrap(),
            },
            // M4.31 (#84). Present in the census even though `exposure_of`
            // excludes it from the tool surface — the census is about the
            // *vocabulary*, and a variant missing here would mean nothing
            // checked that its exclusion was deliberate rather than forgotten.
            GitOperation::ResolveConflict {
                path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
                resolution: git_vista_protocol::conflict::Resolution::TakeOurs,
            },
            // M4.31c (#432), ADR 0069 decision 7. Same reasoning as
            // ResolveConflict above: present so the exclusion is checked as
            // deliberate, not absent by omission.
            GitOperation::ResolveConflictContent {
                path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
                expected_stages: [
                    Some(git_vista_protocol::CommitOid::new("1".repeat(40)).unwrap()),
                    Some(git_vista_protocol::CommitOid::new("2".repeat(40)).unwrap()),
                    Some(git_vista_protocol::CommitOid::new("3".repeat(40)).unwrap()),
                ],
                expected_source: git_vista_protocol::GenerationToken::new("conflict-v1:census")
                    .unwrap(),
                content: "resolved\n".to_string(),
            },
            GitOperation::CreateBranch {
                name: branch("b"),
                at: oid(&zeros),
            },
            GitOperation::CommitOnHead {
                message: message("m"),
                allow_empty: false,
            },
            GitOperation::EmptyCommitOnBranch {
                branch: branch("b"),
                message: message("m"),
                expected_tip: oid(&zeros),
            },
            GitOperation::StageAll,
            GitOperation::UnstageAll,
            GitOperation::CheckoutBranch {
                branch: branch("b"),
            },
            GitOperation::MergeBranch {
                branch: branch("b"),
            },
            GitOperation::PushBranch {
                branch: branch("b"),
                remote: remote(),
                set_upstream: false,
                force: ForcePublish::None,
            },
            GitOperation::DeleteBranch {
                branch: branch("b"),
            },
            GitOperation::ForceDeleteBranch {
                branch: branch("b"),
            },
            GitOperation::RebaseOntoBase {
                base: RefName::new("main").unwrap(),
            },
            GitOperation::RestoreBranch {
                name: branch("b"),
                tip: oid(&zeros),
            },
            GitOperation::ResetBranch {
                branch: branch("b"),
                to: oid(&zeros),
                expected_tip: oid(&zeros),
            },
            GitOperation::RevertMerge {
                commit: git_vista_protocol::CommitOid::new("1".repeat(40)).unwrap(),
                mainline: std::num::NonZeroU8::new(1).unwrap(),
            },
            GitOperation::CherryPick {
                commit: git_vista_protocol::CommitOid::new("1".repeat(40)).unwrap(),
            },
            GitOperation::SequenceContinue,
            GitOperation::SequenceSkip,
            GitOperation::SequenceAbort,
            GitOperation::CherryPickMerge {
                commit: git_vista_protocol::CommitOid::new("1".repeat(40)).unwrap(),
                mainline: std::num::NonZeroU8::new(1).unwrap(),
            },
            GitOperation::RevertCommit {
                commit: oid(&zeros),
            },
            GitOperation::ResetTestRepo,
            GitOperation::StageSelection {
                direction: git_vista_protocol::StageDirection::Stage,
                expected_diff_generation: GenerationToken::new("diff-v1:x").unwrap(),
                patch: String::new(),
                whole_files: vec!["a.txt".to_string()],
            },
            GitOperation::DiscardTrackedPaths {
                paths: vec![path("a.txt")],
            },
            GitOperation::DeleteUntrackedPaths {
                paths: vec![path("a.txt")],
            },
            GitOperation::AmendCommit {
                message: message("m"),
                expected_tip: oid(&zeros),
                allow_empty: false,
            },
            GitOperation::FetchRemote { remote: remote() },
            GitOperation::PullBranch {
                remote: remote(),
                branch: branch("b"),
                strategy: MergeStrategy::Merge,
            },
            GitOperation::CreateTag {
                name: tag("v1"),
                target: oid(&zeros),
                annotation: None,
            },
            GitOperation::DeleteLocalTag { name: tag("v1") },
            GitOperation::DeleteRemoteTag {
                name: tag("v1"),
                remote: remote(),
            },
            GitOperation::PushTag {
                name: tag("v1"),
                remote: remote(),
            },
        ]
    }

    #[test]
    fn the_sample_census_covers_every_operation_the_protocol_defines() {
        let mut sampled: Vec<String> = samples()
            .iter()
            .map(|op| {
                serde_json::to_value(op).unwrap()["op"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        sampled.sort();
        let deduped = {
            let mut d = sampled.clone();
            d.dedup();
            d
        };
        assert_eq!(sampled, deduped, "two samples share an operation kind");
        assert_eq!(
            sampled,
            golden_op_tags(),
            "samples() and the protocol's golden fixture census different vocabularies"
        );
    }

    // -- the arguments each tool actually sends ----------------------------

    /// A plan the injected poster can answer with, so a tool call completes
    /// end to end without a server. Deliberately *not* varied per tool: what
    /// these tests assert is the request, and a constant response makes a
    /// difference in the captured request unambiguous.
    fn stub_plan_json() -> Vec<u8> {
        let plan = Plan {
            repository: RepositoryToken::new("repo").unwrap(),
            worktree: WorktreeToken::new("wt").unwrap(),
            generation: GenerationToken::new("gen-1").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_700_000_000),
            expires_at: UnixSeconds(1_700_000_300),
            risk: RiskLevel::Safe,
            preconditions: vec![],
            expected_ref_changes: vec![],
            recovery: RecoveryStrategy::NotNeeded,
            advisories: Vec::new(),
        };
        serde_json::to_vec(&plan).unwrap()
    }

    /// Every `plan_*` tool in the catalog, with a minimal valid argument set.
    /// Hand-written beside the schemas on purpose: if a schema's required
    /// fields change and this list does not, the call fails and says so.
    fn valid_args() -> Vec<(&'static str, serde_json::Value)> {
        let z = "0".repeat(40);
        vec![
            (
                "plan_create_branch",
                serde_json::json!({ "name": "b", "at": z }),
            ),
            (
                "plan_commit_on_head",
                serde_json::json!({ "message": "m", "allow_empty": false }),
            ),
            (
                "plan_empty_commit_on_branch",
                serde_json::json!({ "branch": "b", "message": "m", "expected_tip": z }),
            ),
            ("plan_stage_all", serde_json::json!({})),
            ("plan_unstage_all", serde_json::json!({})),
            ("plan_checkout_branch", serde_json::json!({ "branch": "b" })),
            ("plan_merge_branch", serde_json::json!({ "branch": "b" })),
            (
                "plan_push_branch",
                serde_json::json!({
                    "branch": "b", "remote": "origin", "set_upstream": false,
                    "force": { "mode": "none" }
                }),
            ),
            ("plan_delete_branch", serde_json::json!({ "branch": "b" })),
            (
                "plan_force_delete_branch",
                serde_json::json!({ "branch": "b" }),
            ),
            (
                "plan_rebase_onto_base",
                serde_json::json!({ "base": "main" }),
            ),
            (
                "plan_restore_branch",
                serde_json::json!({ "name": "b", "tip": z }),
            ),
            (
                "plan_reset_branch",
                serde_json::json!({ "branch": "b", "to": z, "expected_tip": z }),
            ),
            ("plan_revert_commit", serde_json::json!({ "commit": z })),
            (
                "plan_discard_tracked_paths",
                serde_json::json!({ "paths": ["a.txt"] }),
            ),
            (
                "plan_delete_untracked_paths",
                serde_json::json!({ "paths": ["a.txt"] }),
            ),
            (
                "plan_amend_commit",
                serde_json::json!({ "message": "m", "expected_tip": z, "allow_empty": false }),
            ),
            (
                "plan_fetch_remote",
                serde_json::json!({ "remote": "origin" }),
            ),
            (
                "plan_pull_branch",
                serde_json::json!({ "remote": "origin", "branch": "b", "strategy": "merge" }),
            ),
            (
                "plan_create_tag",
                serde_json::json!({ "name": "v1", "target": z }),
            ),
            ("plan_delete_local_tag", serde_json::json!({ "name": "v1" })),
            (
                "plan_delete_remote_tag",
                serde_json::json!({ "name": "v1", "remote": "origin" }),
            ),
            (
                "plan_push_tag",
                serde_json::json!({ "name": "v1", "remote": "origin" }),
            ),
        ]
    }

    #[test]
    fn the_valid_argument_table_covers_the_whole_catalog() {
        let mut named: Vec<&str> = valid_args().iter().map(|(n, _)| *n).collect();
        named.sort_unstable();
        let mut catalog = catalog_names();
        catalog.sort();
        assert_eq!(
            named,
            catalog.iter().map(String::as_str).collect::<Vec<_>>(),
            "the exercised-arguments table and the catalog disagree"
        );
    }

    fn session() -> Session {
        Session {
            cookie: "gv_session=live".to_string(),
            csrf: "csrf".to_string(),
        }
    }

    /// #248's load-bearing claim, proven per tool: **a `plan_*` tool sends
    /// exactly one request, to `/api/plan`, and to nothing else.** No write
    /// endpoint is ever contacted, because the only path any of them can
    /// reach is the build endpoint.
    ///
    /// Not vacuous: the captured path is compared against the literal
    /// `"/api/plan"` (not against `PLAN_ENDPOINT`, which would pass even if
    /// the constant were changed to `/api/push`), the request count is pinned
    /// at exactly one, and the body is deserialized back into a
    /// `GitOperation` and compared to the variant the tool is supposed to
    /// build — so a tool wired to the wrong operation fails here too.
    #[test]
    fn every_plan_tool_posts_only_to_api_plan() {
        for (name, args) in valid_args() {
            let mut captured: Vec<CapturedPost> = Vec::new();
            let mut sess = Some(session());
            let result = call_plan_tool(
                name,
                &args,
                &mut sess,
                &mut |path, body, _cookie, _csrf| {
                    captured.push((path.to_string(), body.to_vec()));
                    Ok(ok_response(&stub_plan_json()))
                },
                &mut || panic!("{name} re-authenticated when it had a live session"),
            )
            .unwrap_or_else(|| panic!("{name} is in the catalog but has no dispatch arm"))
            .unwrap_or_else(|e| panic!("{name} failed with valid arguments: {e:?}"));

            assert_eq!(captured.len(), 1, "{name} made {} requests", captured.len());
            let (path, body) = &captured[0];
            assert_eq!(
                path, "/api/plan",
                "{name} contacted {path} — plan tools may only reach the build endpoint"
            );
            let sent: GitOperation = serde_json::from_slice(body)
                .unwrap_or_else(|e| panic!("{name} sent a body that is not a GitOperation: {e}"));
            assert_eq!(
                format!(
                    "plan_{}",
                    serde_json::to_value(&sent).unwrap()["op"].as_str().unwrap()
                ),
                name,
                "{name} built the wrong operation: {sent:?}"
            );
            assert!(
                result.get("plan").is_some() && result.get("review").is_some(),
                "{name}'s result must carry both the verbatim plan and its review digest"
            );
        }
    }

    /// The paired negative for the census: the two unexposed operations have
    /// no tool AND no dispatch arm. A catalog-only check would miss a
    /// dispatcher that still answered `plan_reset_test_repo` while the schema
    /// was gone — which is the reachable path, not the advertised one.
    #[test]
    fn the_unexposed_variants_have_no_tool_and_no_dispatch_arm() {
        for tag in UNEXPOSED_TAGS {
            let name = format!("plan_{tag}");
            assert!(
                !catalog_names().contains(&name),
                "‘{name}’ is advertised in the catalog"
            );
            assert!(
                operation_for(&name, &serde_json::json!({})).is_none(),
                "‘{name}’ has a dispatch arm — it is reachable even though unadvertised"
            );
        }
        // …and the surrounding positive, so "returns None" is not just what
        // this function does for everything.
        assert!(operation_for("plan_stage_all", &serde_json::json!({})).is_some());
    }

    /// The string arguments of every tool, split by what the protocol's own
    /// newtype for that field actually guarantees. Kept as one place so the
    /// two hostile-input tests below stay in step with the schemas.
    ///
    /// `message` fields carry [`CommitMessage`]/[`TagMessage`], which check
    /// non-empty (and, for tags, bounded) and nothing else — deliberately:
    /// they land in a `-m <value>` argv *value* position, where a leading `-`
    /// is a legitimate character, not a smuggled flag. Every other string
    /// field names a branch, ref, remote, tag, path or commit id, all of
    /// which reach a git argv *word* position and are gated accordingly.
    fn string_keys(args: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        let mut argv_words = Vec::new();
        let mut message_values = Vec::new();
        for (k, v) in args.as_object().unwrap() {
            if !v.is_string() {
                continue;
            }
            if k == "message" {
                message_values.push(k.clone());
            } else {
                argv_words.push(k.clone());
            }
        }
        (argv_words, message_values)
    }

    /// Every string argument of every tool refuses an empty value, and every
    /// argv-word argument additionally refuses an option-shaped one — both
    /// *before* any request exists and before any authentication happens.
    ///
    /// The split is deliberate rather than a weakening: asserting that a
    /// commit message rejects a leading `-` would be asserting a rule the
    /// protocol does not have (and should not — `git commit -m -- oops` is a
    /// legitimate message), and a test that demanded it would have to be
    /// "fixed" by making the production types wrong.
    #[test]
    fn hostile_string_arguments_are_refused_before_any_request() {
        // The two field-less operations are the only tools with nothing to
        // poison; pinned by name so a tool that silently *lost* its arguments
        // (and would therefore be exercised by nothing below) fails here.
        let mut argument_free: Vec<&str> = Vec::new();
        for (name, args) in valid_args() {
            let (argv_words, message_values) = string_keys(&args);
            if argv_words.is_empty() && message_values.is_empty() && !args["paths"].is_array() {
                argument_free.push(name);
                continue;
            }
            let cases = argv_words
                .iter()
                .flat_map(|k| ["", "--upload-pack=/tmp/evil"].map(|h| (k.clone(), h)))
                .chain(message_values.iter().map(|k| (k.clone(), "")));
            for (key, hostile) in cases {
                let mut poisoned = args.clone();
                poisoned[&key] = serde_json::json!(hostile);
                let mut sess = None;
                let outcome = call_plan_tool(
                    name,
                    &poisoned,
                    &mut sess,
                    &mut |path, _, _, _| panic!("{name} sent {key}={hostile:?} to {path}"),
                    &mut || panic!("{name} authenticated for a refused request"),
                )
                .unwrap();
                assert!(
                    outcome.is_err(),
                    "{name} accepted {key}={hostile:?} — a newtype should have refused it"
                );
                assert!(
                    sess.is_none(),
                    "{name} authenticated before validating {key}"
                );
            }
        }
        assert_eq!(
            argument_free,
            ["plan_stage_all", "plan_unstage_all"],
            "the set of argument-free plan tools changed — a tool with no \
             arguments is exercised by nothing in this test"
        );
    }

    /// The CRLF question, answered honestly for this surface.
    ///
    /// `tools.rs` refuses CR/LF in its arguments because the read tools splice
    /// them into a **URL path**, where this crate's hand-rolled HTTP client
    /// would put them on the wire verbatim — a request-splitting vector
    /// (CWE-93) that #246's review found. The plan tools have no such
    /// exposure by construction: the path is the constant `/api/plan` and
    /// every value travels in a JSON **body**, where `serde_json` escapes
    /// control characters into `\r`/`\n` two-character sequences.
    ///
    /// So rather than pretend the newtypes reject CRLF (they do not — a
    /// weird-but-nonempty branch name is git's to refuse at argv time, and it
    /// does), this pins the property that actually protects the wire: for
    /// **every** tool, with **every** string argument poisoned with a
    /// smuggled request, the call either fails outright or sends a body
    /// containing no raw CR or LF byte — to a path that is still exactly
    /// `/api/plan`.
    #[test]
    fn a_crlf_payload_cannot_reach_the_wire_through_a_plan_tool() {
        const SMUGGLED: &str =
            "x\r\nHost: evil\r\n\r\nPOST /api/push HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
        let mut ever_accepted = false;
        for (name, args) in valid_args() {
            let (mut keys, messages) = string_keys(&args);
            keys.extend(messages);
            for key in keys {
                let mut poisoned = args.clone();
                poisoned[&key] = serde_json::json!(SMUGGLED);
                let mut captured: Vec<CapturedPost> = Vec::new();
                let mut sess = Some(session());
                let outcome = call_plan_tool(
                    name,
                    &poisoned,
                    &mut sess,
                    &mut |path, body, _, _| {
                        captured.push((path.to_string(), body.to_vec()));
                        Ok(ok_response(&stub_plan_json()))
                    },
                    &mut || panic!("no re-authentication expected"),
                )
                .unwrap();
                if outcome.is_err() {
                    assert!(
                        captured.is_empty(),
                        "{name} refused {key} but had already sent a request"
                    );
                    continue;
                }
                ever_accepted = true;
                let (path, body) = &captured[0];
                assert_eq!(path, "/api/plan", "{name} built a path from {key}");
                assert!(
                    !body.contains(&b'\r') && !body.contains(&b'\n'),
                    "{name} put a raw CR/LF from {key} on the wire"
                );
            }
        }
        // Anti-vacuity: if every case had been refused, the escaping claim
        // above would never have been exercised at all.
        assert!(
            ever_accepted,
            "no tool accepted the smuggled value, so nothing proved the body escaping"
        );
    }

    #[test]
    fn a_lease_force_push_requires_the_tip_it_leases_on() {
        let base = serde_json::json!({
            "branch": "b", "remote": "origin", "set_upstream": false,
            "force": { "mode": "with_lease" }
        });
        match operation_for("plan_push_branch", &base).unwrap() {
            Err(ToolError::Execution(msg)) => {
                assert!(msg.contains("expected_remote_tip"), "{msg}")
            }
            other => panic!("a lease without a tip was accepted: {other:?}"),
        }

        // The inverse, which is the finding that makes this pair matter: a
        // tip supplied with mode "none" must NOT be silently dropped into a
        // plain push — the caller would believe a compare-and-swap protected
        // them when nothing did.
        let mislabelled = serde_json::json!({
            "branch": "b", "remote": "origin", "set_upstream": false,
            "force": { "mode": "none", "expected_remote_tip": "0".repeat(40) }
        });
        match operation_for("plan_push_branch", &mislabelled).unwrap() {
            Err(ToolError::Execution(msg)) => {
                assert!(msg.contains("expected_remote_tip"), "{msg}")
            }
            other => panic!("a tip beside mode \"none\" was silently ignored: {other:?}"),
        }

        // …and the well-formed lease still builds the leased variant.
        let good = serde_json::json!({
            "branch": "b", "remote": "origin", "set_upstream": false,
            "force": { "mode": "with_lease", "expected_remote_tip": "a".repeat(40) }
        });
        match operation_for("plan_push_branch", &good).unwrap().unwrap() {
            GitOperation::PushBranch {
                force:
                    ForcePublish::WithLease {
                        expected_remote_tip,
                    },
                ..
            } => assert_eq!(expected_remote_tip.as_str(), "a".repeat(40)),
            other => panic!("expected a leased push, got {other:?}"),
        }
    }

    #[test]
    fn an_omitted_no_default_flag_is_refused_rather_than_assumed() {
        // `allow_empty`, `set_upstream`, `sign` and `strategy` have no serde
        // default in the protocol precisely so a plan always states them.
        // A schema that made them optional here would undo that, so the
        // dispatcher refuses each one's absence.
        for (name, args, missing) in [
            (
                "plan_commit_on_head",
                serde_json::json!({ "message": "m" }),
                "allow_empty",
            ),
            (
                "plan_amend_commit",
                serde_json::json!({ "message": "m", "expected_tip": "0".repeat(40) }),
                "allow_empty",
            ),
            (
                "plan_pull_branch",
                serde_json::json!({ "remote": "origin", "branch": "b" }),
                "strategy",
            ),
            (
                "plan_push_branch",
                serde_json::json!({ "branch": "b", "remote": "origin", "force": {"mode": "none"} }),
                "set_upstream",
            ),
            (
                "plan_create_tag",
                serde_json::json!({
                    "name": "v1", "target": "0".repeat(40),
                    "annotation": { "message": "hi" }
                }),
                "sign",
            ),
        ] {
            match operation_for(name, &args).unwrap() {
                Err(ToolError::Execution(msg)) => assert!(
                    msg.contains(missing),
                    "{name} refused without naming `{missing}`: {msg}"
                ),
                other => panic!("{name} assumed a value for `{missing}`: {other:?}"),
            }
        }
    }

    #[test]
    fn an_empty_path_selection_is_refused() {
        for name in ["plan_discard_tracked_paths", "plan_delete_untracked_paths"] {
            let args = serde_json::json!({ "paths": [] });
            assert!(
                operation_for(name, &args).unwrap().is_err(),
                "{name} accepted an empty selection"
            );
            // Paired positive: a non-empty one still builds.
            let ok = serde_json::json!({ "paths": ["a.txt"] });
            assert!(operation_for(name, &ok).unwrap().is_ok());
        }
        // …and an escaping path is refused by WorktreePath itself.
        let escape = serde_json::json!({ "paths": ["../etc/passwd"] });
        assert!(operation_for("plan_discard_tracked_paths", &escape)
            .unwrap()
            .is_err());
    }

    // -- the review digest -------------------------------------------------

    #[test]
    fn the_review_digest_spells_out_risk_and_recovery_in_words() {
        // Literal expected text, not a second call to the function under
        // test: the point of the digest is that an agent can read it, so a
        // test that only checked "some string came back" would pass while it
        // said nothing.
        assert_eq!(risk_name(RiskLevel::Destructive), "destructive");
        assert!(risk_meaning(RiskLevel::Destructive).contains("unreachable"));
        assert!(risk_meaning(RiskLevel::Remote).contains("leaves this machine"));
        assert_eq!(
            recovery_name(&RecoveryStrategy::Irrecoverable),
            "irrecoverable"
        );
        assert!(recovery_meaning(&RecoveryStrategy::Irrecoverable).contains("no undo"));

        let reset = RecoveryStrategy::ResetRef {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            to: CommitOid::new("a".repeat(40)).unwrap(),
        };
        let words = recovery_meaning(&reset);
        assert!(words.contains("refs/heads/main"), "{words}");
        assert!(words.contains(&"a".repeat(40)), "{words}");
    }

    #[test]
    fn the_review_digest_keeps_the_plan_verbatim_beside_it() {
        // #249 submits the plan back and the operation hash binds it, so the
        // digest must never be a replacement for the DTO. Round-trip the
        // `plan` half and assert it deserializes into an equal Plan.
        let raw = stub_plan_json();
        let plan: Plan = serde_json::from_slice(&raw).unwrap();
        let mut sess = Some(session());
        let value = call_plan_tool(
            "plan_stage_all",
            &serde_json::json!({}),
            &mut sess,
            &mut |_, _, _, _| Ok(ok_response(&raw)),
            &mut || panic!("should not re-authenticate"),
        )
        .unwrap()
        .unwrap();
        let echoed: Plan = serde_json::from_value(value["plan"].clone()).unwrap();
        assert_eq!(echoed, plan, "the plan must survive the tool byte-for-byte");
        assert_eq!(value["review"]["risk"], "safe");
        assert_eq!(value["review"]["recovery"], "not_needed");
        assert_eq!(
            value["review"]["operation_hash"],
            plan.operation_hash.as_str()
        );
    }

    #[test]
    fn preconditions_and_ref_changes_read_as_sentences() {
        let at = Precondition::RefAt {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            oid: CommitOid::new("b".repeat(40)).unwrap(),
        };
        let words = precondition_sentence(&at);
        assert!(words.contains("refs/heads/main"), "{words}");
        assert!(words.contains(&"b".repeat(40)), "{words}");
        assert_eq!(
            precondition_sentence(&Precondition::CleanWorktree),
            "The working tree and index must be clean."
        );
        assert_eq!(
            ref_change_sentence(&RefChange {
                ref_name: RefName::new("refs/heads/feature").unwrap(),
                before: RefState::Absent,
                after: RefState::Computed,
            }),
            "refs/heads/feature: absent → a new commit this operation creates"
        );
    }

    /// The production second lock, both directions. This is the guard that
    /// would catch a future dispatch arm for an excluded variant — the
    /// failure mode a catalog-shaped test cannot see, because such an arm
    /// would never appear in the catalog.
    #[test]
    fn check_exposure_refuses_an_excluded_or_misnamed_operation() {
        // The positive first, so the refusals below are not just "this
        // function refuses everything".
        assert!(check_exposure("plan_stage_all", &GitOperation::StageAll).is_ok());

        // An excluded variant reaching a dispatch arm is refused by name, and
        // the refusal carries the stated reason so the agent learns why.
        match check_exposure("plan_stage_all", &GitOperation::ResetTestRepo) {
            Err(ToolError::Execution(msg)) => {
                assert!(msg.contains("not available"), "{msg}");
                assert!(msg.contains("harness affordance"), "{msg}");
            }
            other => panic!("ResetTestRepo was allowed through: {other:?}"),
        }
        let selection = GitOperation::StageSelection {
            direction: git_vista_protocol::StageDirection::Stage,
            expected_diff_generation: GenerationToken::new("diff-v1:x").unwrap(),
            patch: String::new(),
            whole_files: vec!["a.txt".to_string()],
        };
        assert!(check_exposure("plan_stage_selection", &selection).is_err());

        // A dispatch arm wired to the wrong variant is refused too.
        match check_exposure("plan_stage_all", &GitOperation::UnstageAll) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("plan_unstage_all"), "{msg}"),
            other => panic!("a misnamed operation was allowed through: {other:?}"),
        }
    }

    /// A non-200 from the plan endpoint must surface as a tool error, never
    /// as a "plan" the agent could then try to submit.
    #[test]
    fn a_refused_build_is_an_error_not_an_empty_plan() {
        let mut sess = Some(session());
        let outcome = call_plan_tool(
            "plan_stage_all",
            &serde_json::json!({}),
            &mut sess,
            &mut |_, _, _, _| {
                Ok(HttpResponse {
                    status: 403,
                    headers: Vec::new(),
                    body: b"This repository is open in Visualize mode.".to_vec(),
                })
            },
            &mut || panic!("403 is not 401 — no re-authentication"),
        )
        .unwrap();
        match outcome {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("Visualize"), "{msg}"),
            other => panic!("a refused build was not an error: {other:?}"),
        }
    }
}
