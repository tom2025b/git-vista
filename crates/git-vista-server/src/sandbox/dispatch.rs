//! M1.13b (#66) Task 8: tests for the tier-dispatch classifier.
//!
//! The dispatch is the "must not be wrong" decision — which operation runs with
//! no sandbox — so it is tested for the failure directions, not just the happy
//! path.

use super::*;

#[test]
fn remote_subcommands_need_the_network() {
    for sub in [
        "push",
        "fetch",
        "clone",
        "ls-remote",
        "pull",
        // plumbing / helpers added after the C10 audit
        "fetch-pack",
        "send-pack",
        "http-fetch",
        "http-push",
    ] {
        assert_eq!(
            network_need(&[sub, "origin"]),
            NetworkNeed::Remote,
            "`git {sub}` reaches a remote"
        );
    }
}

/// The C10 audit's list of network-capable commands the argv classifier still
/// misses. These fail closed to `Local`/Strict, which *breaks* the network
/// attempt rather than granting it — the safe direction. This test documents
/// the known gap so the day someone moves classification to the typed operation
/// model, these are the cases to cover. It asserts the *current* fail-closed
/// behaviour, not that the gap is fixed.
#[test]
fn known_network_gaps_fail_closed_to_local_not_unsandboxed() {
    for args in [
        vec!["remote", "update"],
        vec!["submodule", "update", "--remote"],
        vec!["maintenance", "run", "--task=prefetch"],
        vec!["credential", "fill"],
    ] {
        let need = network_need(&args);
        assert_eq!(
            need,
            NetworkNeed::Local,
            "documented fail-closed gap: {args:?}"
        );
        assert_ne!(
            tier_for(need, false),
            Tier::Unsandboxed,
            "even a misclassified network command must never be unsandboxed: {args:?}"
        );
    }
}

#[test]
fn local_subcommands_do_not_need_the_network() {
    for sub in [
        "status",
        "commit",
        "add",
        "reset",
        "checkout",
        "merge",
        "branch",
        "rev-parse",
        "merge-base",
        "diff",
        "log",
        "cat-file",
        "config",
        "update-ref",
        "commit-tree",
        "bundle",
        "stash",
        "reflog",
    ] {
        assert_eq!(
            network_need(&[sub, "--whatever"]),
            NetworkNeed::Local,
            "`git {sub}` is local"
        );
    }
}

/// `git remote get-url` looks network-adjacent but only reads `.git/config`.
/// Misclassifying it as `Remote` would be harmless, but proving it is `Local`
/// documents the distinction the code comment claims.
#[test]
fn remote_config_subcommands_are_local_not_networked() {
    assert_eq!(
        network_need(&["remote", "get-url", "origin"]),
        NetworkNeed::Local
    );
    assert_eq!(
        network_need(&["remote", "add", "origin", "url"]),
        NetworkNeed::Local
    );
    assert_eq!(network_need(&["remote", "-v"]), NetworkNeed::Local);
}

/// Leading global flags (`-C <path>`, `-c k=v`) must not be mistaken for the
/// subcommand. A hostile *repository* cannot inject these onto the server's
/// argv, but the classifier is robust to them regardless.
#[test]
fn leading_global_flags_are_skipped_to_find_the_subcommand() {
    assert_eq!(
        network_need(&["-C", "/srv/repo", "push", "origin"]),
        NetworkNeed::Remote
    );
    assert_eq!(
        network_need(&["-c", "http.proxy=x", "-C", "/srv/repo", "status"]),
        NetworkNeed::Local
    );
    // A `-c` that tries to look like `push` as its *value* must not be read as
    // the subcommand.
    assert_eq!(
        network_need(&["-c", "alias.x=push", "status"]),
        NetworkNeed::Local
    );
}

#[test]
fn an_empty_or_flags_only_argv_is_local() {
    assert_eq!(network_need(&[]), NetworkNeed::Local);
    assert_eq!(network_need(&["--version"]), NetworkNeed::Local);
    assert_eq!(network_need(&["--help"]), NetworkNeed::Local);
}

/// An unknown subcommand fails **closed** to `Local`/`Strict`. A network op
/// wrongly given Strict breaks loudly; a local op wrongly given Network merely
/// over-permits. The dangerous direction — silently gaining access — is the one
/// this default forecloses.
#[test]
fn an_unknown_subcommand_fails_closed_to_local() {
    assert_eq!(network_need(&["some-new-porcelain"]), NetworkNeed::Local);
}

/// C10's strongest (failed) escalation attempt, kept as a regression guard.
/// `git -c alias.x=push x origin` expands `x` to `push` and runs it, so the
/// classifier's name-based view (`x` is unknown → `Local`) disagrees with what
/// git executes. The security property that matters survives regardless: with
/// `trusted=false` this can never reach `Unsandboxed`. The *availability*
/// consequence — the hidden push runs under Strict and fails — is the
/// intended fail-closed direction, not a hole.
#[test]
fn an_injected_alias_can_never_reach_unsandboxed() {
    let args = ["-c", "alias.x=push", "x", "origin"];
    let need = network_need(&args);
    // The name-based classifier sees `x`, an unknown subcommand → Local. That is
    // the documented fail-closed behaviour, asserted so a future change to it is
    // deliberate.
    assert_eq!(
        need,
        NetworkNeed::Local,
        "an unknown alias name classifies Local"
    );
    // The property that must hold no matter how classification lands:
    assert_ne!(
        tier_for(need, false),
        Tier::Unsandboxed,
        "an injected alias must never escalate an untrusted repo to no-sandbox"
    );
}

// -------------------------------------------------------------------------
// tier_for — the accidental-Unsandboxed guard
// -------------------------------------------------------------------------

/// The property the whole design rests on: an **untrusted** repository can
/// never reach `Unsandboxed`, for any operation. If this ever fails, a hostile
/// repository is one classification bug away from running with no sandbox.
#[test]
fn an_untrusted_repo_is_never_unsandboxed_for_any_operation() {
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_ne!(
            tier_for(need, false),
            Tier::Unsandboxed,
            "untrusted repos must never be unsandboxed (need={need:?})"
        );
    }
}

/// Unsandboxed is reachable *only* through the trust flag — and then for every
/// operation, because trust is a property of the repository, not the operation.
#[test]
fn unsandboxed_is_reachable_only_through_the_trust_flag() {
    assert_eq!(tier_for(NetworkNeed::Local, true), Tier::Unsandboxed);
    assert_eq!(tier_for(NetworkNeed::Remote, true), Tier::Unsandboxed);
}

#[test]
fn untrusted_dispatch_is_strict_for_local_and_network_for_remote() {
    assert_eq!(tier_for(NetworkNeed::Local, false), Tier::Strict);
    assert_eq!(tier_for(NetworkNeed::Remote, false), Tier::Network);
}

/// Pin the actual production tier, not a local `let trusted = false` (which
/// would pass even if the real caller used `true` — the C10 audit flagged the
/// earlier version of this test as vacuous for exactly that reason). This
/// exercises the real `policy_for_repo` and asserts the tier it hands out is
/// never `Unsandboxed`.
///
/// Task 8 wired the Strict/Network split in; the additional coverage that asked
/// for lives below (`a_local_operation_gets_the_strict_tier_with_no_ports`,
/// `a_remote_operation_gets_the_network_tier_with_the_git_ports`,
/// `an_untrusted_repository_can_never_be_unsandboxed`). This test is kept as
/// written because it now pins a second thing: `policy_for_repo` is the entry
/// point `escape_contract::policy_for_case` calls, and the ten Network-tier
/// battery cases depend on it staying non-`Unsandboxed`.
#[test]
fn the_production_policy_is_never_unsandboxed_today() {
    let repo = tempfile::tempdir().expect("tempdir");
    let policy = super::policy_for_repo(repo.path())
        .expect("policy builds (shim present via tests/forces_shim_build.rs)");
    assert_ne!(
        policy.tier,
        Tier::Unsandboxed,
        "no repository may be unsandboxed until an explicit persisted trust flag exists"
    );
}

// ---------------------------------------------------------------------------
// Task 8 / D3: the declared-intent dispatch, wired to production
// ---------------------------------------------------------------------------
//
// Everything above this line tests the two pure classifiers in isolation.
// Everything below tests the thing Task 8 actually changed: that the classifier
// answers now *reach* `policy_for`, that trust is consulted, and that the
// failure directions fail closed. The negative cases are the point — a green
// "local gets Strict" proves very little on its own, since a `policy_for` that
// returned Strict unconditionally would also pass it.

use git_vista_protocol::{
    BranchName, CommitMessage, CommitOid, ForcePublish, GenerationToken, GitOperation,
    MergeStrategy, RefName, RemoteName, StageDirection, TagAnnotation, TagMessage, TagName,
    WorktreePath,
};

fn branch(s: &str) -> BranchName {
    BranchName::new(s).expect("valid branch name")
}
fn oid(s: &str) -> CommitOid {
    CommitOid::new(s).expect("valid oid")
}
fn wpath(s: &str) -> WorktreePath {
    WorktreePath::new(s).expect("valid worktree path")
}

/// Compile-time coverage guard, the same pattern
/// `planner::contract_suite::covered_by` uses for the identical problem: every
/// `GitOperation` variant is named here with **no wildcard arm**, so adding a
/// variant fails this match at compile time until an arm exists for it.
///
/// # What this match does *not* prove
///
/// Being honest about the limit, because an earlier version of this comment
/// was not: forcing an arm here is **presence enforcement only**. It makes a
/// contributor write *some* name for a new variant; it cannot make them add
/// that variant to [`every_operation`]'s hand-written `Vec`, and it cannot
/// make them add the name to the hand-written `expected` set in
/// [`every_operation_declares_every_variant`]. Those two are ordinary data,
/// not compile-checked. A contributor who adds a variant, writes its arm
/// here, and forgets both of those leaves all three sources *mutually
/// self-consistent and all three wrong* — the census silently shrinks and
/// every test in this file stays green while the new variant's
/// `NetworkNeed` has zero coverage.
///
/// That is exactly what happened between M2.17b (#213), M2.18a (#219) and
/// M2.19a (#222), when `every_operation()` went four variants
/// (`StageSelection`, `DiscardTrackedPaths`, `DeleteUntrackedPaths`,
/// `AmendCommit`) stale against the enum while its count literal quietly
/// stayed self-consistent at the old number, and `AmendCommit` shipped with a
/// zero-coverage classification.
///
/// The guard that actually closes that hole is
/// [`every_operation_covers_every_variant_the_enum_declares`], which compares
/// the census against the variant list **serde's derive macro generates from
/// the enum definition itself** — the one source in this file that no human
/// maintains and that therefore cannot drift stale in step with the others.
/// This match remains worth keeping for what it *does* do: it gives typed,
/// rename-safe provenance for the names the set assertions compare, so a
/// variant renamed in `plan.rs` cannot quietly keep matching a stale string.
fn variant_name(op: &GitOperation) -> &'static str {
    match op {
        GitOperation::PushStash { .. } => "PushStash",
        GitOperation::ApplyStash { .. } => "ApplyStash",
        GitOperation::BranchFromStash { .. } => "BranchFromStash",
        GitOperation::DropStash { .. } => "DropStash",
        GitOperation::ResolveConflict { .. } => "ResolveConflict",
        GitOperation::ResolveConflictContent { .. } => "ResolveConflictContent",
        GitOperation::CreateBranch { .. } => "CreateBranch",
        GitOperation::CommitOnHead { .. } => "CommitOnHead",
        GitOperation::EmptyCommitOnBranch { .. } => "EmptyCommitOnBranch",
        GitOperation::StageAll => "StageAll",
        GitOperation::UnstageAll => "UnstageAll",
        GitOperation::CheckoutBranch { .. } => "CheckoutBranch",
        GitOperation::AddWorktree { .. } => "AddWorktree",
        GitOperation::MergeBranch { .. } => "MergeBranch",
        GitOperation::PushBranch { .. } => "PushBranch",
        GitOperation::DeleteBranch { .. } => "DeleteBranch",
        GitOperation::ForceDeleteBranch { .. } => "ForceDeleteBranch",
        GitOperation::RebaseOntoBase { .. } => "RebaseOntoBase",
        GitOperation::RestoreBranch { .. } => "RestoreBranch",
        GitOperation::ResetBranch { .. } => "ResetBranch",
        GitOperation::RevertCommit { .. } => "RevertCommit",
        GitOperation::RevertMerge { .. } => "RevertMerge",
        GitOperation::CherryPick { .. } => "CherryPick",
        GitOperation::SequenceContinue => "SequenceContinue",
        GitOperation::SequenceSkip => "SequenceSkip",
        GitOperation::SequenceAbort => "SequenceAbort",
        GitOperation::CherryPickMerge { .. } => "CherryPickMerge",
        GitOperation::ResetTestRepo => "ResetTestRepo",
        GitOperation::StageSelection { .. } => "StageSelection",
        GitOperation::DiscardTrackedPaths { .. } => "DiscardTrackedPaths",
        GitOperation::DeleteUntrackedPaths { .. } => "DeleteUntrackedPaths",
        GitOperation::AmendCommit { .. } => "AmendCommit",
        GitOperation::FetchRemote { .. } => "FetchRemote",
        GitOperation::PullBranch { .. } => "PullBranch",
        GitOperation::CreateTag { .. } => "CreateTag",
        GitOperation::DeleteLocalTag { .. } => "DeleteLocalTag",
        GitOperation::DeleteRemoteTag { .. } => "DeleteRemoteTag",
        GitOperation::PushTag { .. } => "PushTag",
    }
}

/// The wire name serde's `Serialize` derive gives `op` — read off the real
/// serialization rather than recomputed here, so it is the enum's own
/// `#[serde(tag = "op", rename_all = "snake_case")]` contract talking and not
/// a second hand-written mapping that could disagree with it.
fn wire_name(op: &GitOperation) -> String {
    let json = serde_json::to_value(op).expect("GitOperation serializes");
    json.get("op")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("serialized GitOperation has no `op` tag: {json}"))
        .to_string()
}

/// **Every variant the `GitOperation` enum actually declares**, harvested from
/// serde's own derive output instead of maintained by hand.
///
/// Deserializing an `op` tag that matches nothing makes serde report
/// `unknown variant `…`, expected one of `create_branch`, `commit_on_head`, …`
/// — and that list is generated by the derive macro *from the enum
/// definition*, so it grows the moment a variant is added and there is no
/// edit anyone can forget. This is the only variant census in this file that
/// is not hand-written, which is precisely why the completeness guard below
/// is built on it: the failure mode being defended against is a human
/// updating some of the hand-written sources and not the others.
///
/// If serde ever changes this message's shape the parse yields a set that
/// cannot equal the sampled one, so the guard fails loudly rather than
/// quietly harvesting nothing and passing — the vacuous-green direction is
/// closed by construction, and
/// [`the_serde_variant_census_is_actually_harvesting_names`] pins it directly.
fn variant_names_the_enum_declares() -> std::collections::BTreeSet<String> {
    let err = serde_json::from_str::<GitOperation>(r#"{"op":"__no_such_variant__"}"#)
        .expect_err("a nonexistent op tag must not deserialize");
    let message = err.to_string();
    let list = message
        .split_once("expected one of ")
        .unwrap_or_else(|| {
            panic!("serde's unknown-variant message no longer has the expected shape: {message}")
        })
        .1;
    list.split(", ")
        .map(|token| {
            token
                .trim()
                .trim_end_matches(|c: char| !c.is_ascii_graphic())
        })
        .filter_map(|token| token.strip_prefix('`'))
        .filter_map(|token| token.split('`').next())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

/// One value of every `GitOperation` variant, so the classifier test below is
/// exhaustive in fact and not only in intent.
///
/// This `Vec` is hand-written and therefore *can* be forgotten — see
/// [`variant_name`]'s doc for why the compile-enforced match does not prevent
/// that. What prevents it is
/// [`every_operation_covers_every_variant_the_enum_declares`], which checks
/// this list against [`variant_names_the_enum_declares`]; adding a variant to
/// `plan.rs` and not to this `Vec` fails that test.
fn every_operation() -> Vec<GitOperation> {
    let tip = "1111111111111111111111111111111111111111";
    vec![
        // M3.24 (#77) — every stash verb is Local: refs/stash never leaves
        // the repo. There is no pop verb; see `plan.rs` and ADR 0078.
        GitOperation::PushStash {
            message: None,
            keep_index: false,
            include_untracked: true,
        },
        GitOperation::ApplyStash {
            entry: git_vista_protocol::StashSelector::new("stash@{0}").expect("valid selector"),
            expected_oid: oid(tip),
        },
        GitOperation::BranchFromStash {
            name: branch("from-stash"),
            entry: git_vista_protocol::StashSelector::new("stash@{0}").expect("valid selector"),
            expected_oid: oid(tip),
        },
        GitOperation::DropStash {
            entry: git_vista_protocol::StashSelector::new("stash@{0}").expect("valid selector"),
            expected_oid: oid(tip),
        },
        GitOperation::ResolveConflict {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            resolution: git_vista_protocol::conflict::Resolution::TakeOurs,
        },
        GitOperation::ResolveConflictContent {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            expected_stages: [Some(oid(tip)), Some(oid(tip)), Some(oid(tip))],
            expected_source: git_vista_protocol::GenerationToken::new("conflict-v1:census")
                .unwrap(),
            content: "resolved\n".to_string(),
        },
        GitOperation::CreateBranch {
            name: branch("feature"),
            at: oid(tip),
        },
        GitOperation::CommitOnHead {
            message: CommitMessage::new("msg").expect("valid message"),
            allow_empty: false,
        },
        GitOperation::EmptyCommitOnBranch {
            branch: branch("feature"),
            message: CommitMessage::new("msg").expect("valid message"),
            expected_tip: oid(tip),
        },
        GitOperation::StageAll,
        GitOperation::UnstageAll,
        GitOperation::CheckoutBranch {
            branch: branch("feature"),
        },
        // M11.04 (#549). Local: it writes a directory and a metadata file from
        // objects already on disk, and opens no socket. Its coverage here
        // matters more than most — it is the one operation whose spawn carries
        // an extra write grant (`git_cmd::sandboxed_with_grant`), and that
        // helper refuses to combine a grant with the Network tier, so a
        // reclassification to Remote would break it loudly rather than
        // quietly widening what a granted spawn can reach.
        GitOperation::AddWorktree {
            name: git_vista_protocol::WorktreeName::new("review-549").expect("valid name"),
            branch: branch("feature"),
        },
        GitOperation::MergeBranch {
            branch: branch("feature"),
        },
        GitOperation::PushBranch {
            branch: branch("feature"),
            remote: RemoteName::new("origin").expect("valid remote"),
            set_upstream: false,
            force: ForcePublish::None,
        },
        GitOperation::DeleteBranch {
            branch: branch("feature"),
        },
        GitOperation::ForceDeleteBranch {
            branch: branch("feature"),
        },
        GitOperation::RebaseOntoBase {
            base: RefName::new("refs/heads/main").expect("valid ref"),
        },
        GitOperation::RestoreBranch {
            name: branch("feature"),
            tip: oid(tip),
        },
        GitOperation::ResetBranch {
            branch: branch("feature"),
            to: oid(tip),
            expected_tip: oid(tip),
        },
        GitOperation::RevertCommit { commit: oid(tip) },
        GitOperation::RevertMerge {
            commit: oid(tip),
            mainline: std::num::NonZeroU8::new(1).unwrap(),
        },
        GitOperation::CherryPick { commit: oid(tip) },
        GitOperation::SequenceContinue,
        GitOperation::SequenceSkip,
        GitOperation::SequenceAbort,
        GitOperation::CherryPickMerge {
            commit: oid(tip),
            mainline: std::num::NonZeroU8::new(1).unwrap(),
        },
        GitOperation::ResetTestRepo,
        GitOperation::StageSelection {
            direction: StageDirection::Stage,
            expected_diff_generation: GenerationToken::new("diff-v1:x")
                .expect("valid generation token"),
            patch: String::new(),
            whole_files: vec!["a.txt".to_string()],
        },
        GitOperation::DiscardTrackedPaths {
            paths: vec![wpath("a.txt")],
        },
        GitOperation::DeleteUntrackedPaths {
            paths: vec![wpath("a.txt")],
        },
        GitOperation::AmendCommit {
            message: CommitMessage::new("msg").expect("valid message"),
            expected_tip: oid(tip),
            allow_empty: false,
        },
        GitOperation::FetchRemote {
            remote: RemoteName::new("origin").expect("valid remote"),
        },
        GitOperation::PullBranch {
            remote: RemoteName::new("origin").expect("valid remote"),
            branch: branch("main"),
            strategy: MergeStrategy::Merge,
        },
        // M2.21a (#235): the annotated sample, so the classification below is
        // exercised with an annotation present; `CreateTag`'s kind cannot
        // change the answer (both write only local objects and refs) and the
        // lightweight form is one field of this value set to `None`.
        GitOperation::CreateTag {
            name: TagName::new("v1.0.0").expect("valid tag name"),
            target: oid("1111111111111111111111111111111111111111"),
            annotation: Some(TagAnnotation {
                message: TagMessage::new("v1.0.0").expect("valid tag message"),
                sign: false,
            }),
        },
        GitOperation::DeleteLocalTag {
            name: TagName::new("v1.0.0").expect("valid tag name"),
        },
        GitOperation::DeleteRemoteTag {
            name: TagName::new("v1.0.0").expect("valid tag name"),
            remote: RemoteName::new("origin").expect("valid remote"),
        },
        GitOperation::PushTag {
            name: TagName::new("v1.0.0").expect("valid tag name"),
            remote: RemoteName::new("origin").expect("valid remote"),
        },
    ]
}

/// A lease-force push, for the tests below that must see *both* `ForcePublish`
/// modes. Kept out of [`every_operation`] on purpose: that list is
/// one-value-per-variant (its own guard asserts no variant appears twice), and
/// `PushBranch`'s two force modes are one variant. Their classification is
/// checked separately by
/// [`a_lease_force_push_declares_remote_like_every_other_push`].
fn lease_force_push() -> GitOperation {
    GitOperation::PushBranch {
        branch: branch("feature"),
        remote: RemoteName::new("origin").expect("valid remote"),
        set_upstream: true,
        force: ForcePublish::WithLease {
            expected_remote_tip: oid("2222222222222222222222222222222222222222"),
        },
    }
}

/// Proves [`every_operation`] is actually exhaustive, rather than trusting a
/// hand-maintained list to agree with a hand-maintained count (the gap that
/// let `AmendCommit` ship with a zero-coverage `NetworkNeed` classification in
/// M2.19a, #222): every value `every_operation()` returns is tagged through
/// [`variant_name`]'s compile-enforced match, and the resulting name set must
/// be the full 25 with none missing and none doubled.
#[test]
fn every_operation_declares_every_variant() {
    let names: std::collections::BTreeSet<&str> =
        every_operation().iter().map(variant_name).collect();
    assert_eq!(
        every_operation().len(),
        names.len(),
        "every_operation() lists the same GitOperation variant more than once"
    );
    let expected: std::collections::BTreeSet<&str> = [
        "ResolveConflict",
        "ResolveConflictContent",
        "CreateBranch",
        "CommitOnHead",
        "EmptyCommitOnBranch",
        "StageAll",
        "UnstageAll",
        "CheckoutBranch",
        "AddWorktree",
        "MergeBranch",
        "PushBranch",
        "DeleteBranch",
        "ForceDeleteBranch",
        "RebaseOntoBase",
        "RestoreBranch",
        "ResetBranch",
        "RevertCommit",
        "RevertMerge",
        "CherryPick",
        "SequenceContinue",
        "SequenceSkip",
        "SequenceAbort",
        "CherryPickMerge",
        "ResetTestRepo",
        "StageSelection",
        "DiscardTrackedPaths",
        "DeleteUntrackedPaths",
        "AmendCommit",
        "FetchRemote",
        "PullBranch",
        "CreateTag",
        "DeleteLocalTag",
        "DeleteRemoteTag",
        "PushTag",
        "PushStash",
        "ApplyStash",
        "BranchFromStash",
        "DropStash",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        names, expected,
        "every_operation() must list exactly the GitOperation variants \
         variant_name() knows about — if this fails after adding a variant, \
         add it to both variant_name() and every_operation()"
    );
}

/// Which variants the enum declares that `samples` never produced a value of.
///
/// Factored out of the guard below so the guard's ability to fail can itself
/// be tested — see
/// [`the_completeness_guard_catches_a_variant_dropped_from_the_census`].
fn variants_missing_from(samples: &[GitOperation]) -> std::collections::BTreeSet<String> {
    let sampled: std::collections::BTreeSet<String> = samples.iter().map(wire_name).collect();
    variant_names_the_enum_declares()
        .difference(&sampled)
        .cloned()
        .collect()
}

/// **The guard that actually closes the M2.19a (#222) drift hole.**
///
/// Every other census in this file is hand-written — [`every_operation`]'s
/// `Vec`, the `expected` name set above, the count literals below — so all of
/// them can be left stale *together*, staying mutually consistent while the
/// enum has moved on. That is not hypothetical: it is the recorded history of
/// #213/#219/#222, and it is reproducible today by deleting a variant from all
/// three at once, which leaves this file's other tests green even with that
/// variant's `NetworkNeed` classification wrong.
///
/// This test compares the census against [`variant_names_the_enum_declares`],
/// which serde's derive macro generates from the enum definition. Nobody
/// maintains that list, so it cannot be forgotten in step with the others: add
/// a variant to `plan.rs` and this test fails until [`every_operation`] carries
/// a value of it, which in turn is what forces the `NetworkNeed`
/// classification below to be exercised at all.
#[test]
fn every_operation_covers_every_variant_the_enum_declares() {
    let missing = variants_missing_from(&every_operation());
    assert!(
        missing.is_empty(),
        "every_operation() is stale against the GitOperation enum — it has no \
         value for {missing:?}, so those variants' NetworkNeed classification \
         has zero coverage in this file. Add a sample for each to \
         every_operation() (and its name to variant_name()/the expected set)."
    );
}

/// The paired negative for the guard above: drop one variant from the census —
/// exactly the shape of the #222 regression, where `AmendCommit` was absent
/// from `every_operation()` — and the mechanism must name it.
///
/// Without this, `every_operation_covers_every_variant_the_enum_declares`
/// would be a green assertion with nothing proving it can ever go red (for
/// instance if [`variant_names_the_enum_declares`] silently harvested an empty
/// set, its `difference` would be empty and the guard would pass forever).
#[test]
fn the_completeness_guard_catches_a_variant_dropped_from_the_census() {
    let truncated: Vec<GitOperation> = every_operation()
        .into_iter()
        .filter(|op| !matches!(op, GitOperation::AmendCommit { .. }))
        .collect();
    assert_eq!(
        truncated.len(),
        every_operation().len() - 1,
        "the negative control must actually remove AmendCommit"
    );
    let missing = variants_missing_from(&truncated);
    assert_eq!(
        missing,
        ["amend_commit".to_string()].into_iter().collect(),
        "dropping AmendCommit from the census must be reported as missing; \
         if this is empty the completeness guard cannot fail and is vacuous"
    );
}

/// [`variant_names_the_enum_declares`] must be harvesting real names, not
/// returning an empty or truncated set that would make the guard vacuous in a
/// way the negative control above cannot see.
///
/// Pinned against [`variant_name`]'s independent, compile-enforced match:
/// every name that match knows, snake_cased by serde, must appear in the
/// harvested set and the two must be the same size. Two independently-derived
/// lists agreeing is the point — this is not asserting the mapping by calling
/// the function that defines it.
#[test]
fn the_serde_variant_census_is_actually_harvesting_names() {
    let harvested = variant_names_the_enum_declares();
    assert!(
        harvested.len() >= 26,
        "serde's variant census came back implausibly short ({}): the \
         unknown-variant message parse has probably broken — {harvested:?}",
        harvested.len()
    );
    for op in every_operation() {
        assert!(
            harvested.contains(&wire_name(&op)),
            "serde reports variant names that do not include {:?}'s wire name \
             {:?} — the harvest is not parsing what it claims to",
            variant_name(&op),
            wire_name(&op)
        );
    }
    assert_eq!(
        harvested.len(),
        every_operation().len(),
        "the enum declares variants the census has no sample for, or vice \
         versa; declared: {harvested:?}"
    );
}

/// Exactly five operations in the enum reach a remote: `PushBranch`,
/// `FetchRemote`, `PullBranch` (M2.20a, #227) and the two tag operations
/// M2.21a (#235) added, `DeleteRemoteTag` and `PushTag` — both pushes under
/// the hood.
///
/// The **negative half is what matters**: the other twenty must be `Local`,
/// so a future edit that classified, say, `MergeBranch` as `Remote` to "be
/// safe" is caught here. Widening is not safe — it moves an operation from
/// the no-network Strict tier into a tier with outbound TCP on four ports.
/// The mirror-image negative matters just as much for #235: `CreateTag` and
/// `DeleteLocalTag` sitting in the Local set is what pins that the tag
/// *local/remote split across four variants* did not quietly give the local
/// pair a socket.
///
/// Asserting the whole *set* (rather than a count plus a spot-check on one
/// name) is deliberate: a count alone would let a swap through — reclassify
/// `FetchRemote` down to `Local` and `MergeBranch` up to `Remote` and the
/// total is still five. The names come from [`variant_name`]'s
/// compile-enforced match rather than from `format!("{op:?}")`, so the
/// comparison is over typed provenance and a variant renamed in `plan.rs`
/// cannot quietly keep matching a stale `Debug` prefix.
#[test]
fn exactly_the_five_remote_operations_declare_a_network_need() {
    let ops = every_operation();
    assert_eq!(
        ops.len(),
        38,
        "every_operation() must list every GitOperation variant; the enum has 38 \
         (this literal is a tripwire, not the enforcement — \
         every_operation_covers_every_variant_the_enum_declares is what checks \
         the census against the enum itself and cannot be left stale with it)"
    );
    let mut remote = std::collections::BTreeSet::new();
    let mut local = std::collections::BTreeSet::new();
    for op in &ops {
        match network_need_for_operation(op) {
            NetworkNeed::Remote => remote.insert(variant_name(op)),
            NetworkNeed::Local => local.insert(variant_name(op)),
        };
    }
    let expected: std::collections::BTreeSet<&str> = [
        "PushBranch",
        "FetchRemote",
        "PullBranch",
        "DeleteRemoteTag",
        "PushTag",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        remote, expected,
        "the set of network-reaching operations changed; declared Remote: {remote:?}"
    );
    assert_eq!(
        local.len(),
        33,
        "the other thirty-three operations must stay Local; declared Local: {local:?}"
    );
}

/// A `PushBranch` carrying [`ForcePublish::WithLease`] is still `Remote` —
/// the force mode changes the plan's [`RiskLevel`], never the tier.
///
/// This exists because `every_operation()` can only hold one value per
/// variant, so the lease-force shape would otherwise have **zero** coverage
/// in this file — precisely the gap that let `AmendCommit` ship unclassified
/// (see [`variant_name`]'s doc). The paired assertion below is the one with
/// teeth: it pins that the two force modes agree, so a future edit that made
/// the classification depend on `force` at all fails here.
#[test]
fn a_lease_force_push_declares_remote_like_every_other_push() {
    let plain = GitOperation::PushBranch {
        branch: branch("feature"),
        remote: RemoteName::new("origin").expect("valid remote"),
        set_upstream: false,
        force: ForcePublish::None,
    };
    assert_eq!(
        network_need_for_operation(&lease_force_push()),
        NetworkNeed::Remote,
        "a force-with-lease push reaches a remote like any other push"
    );
    assert_eq!(
        network_need_for_operation(&lease_force_push()),
        network_need_for_operation(&plain),
        "the tier must not depend on the force mode — risk and reach are \
         independent axes (see RiskLevel::Destructive's doc in plan.rs)"
    );
}

/// Both `MergeStrategy` values classify identically: the integration happens
/// after the objects arrive, so it cannot change whether a socket opens.
///
/// The negative this guards: a reader who thinks "rebase is the local half"
/// and splits the arm would silently route a pull's fetch through the Strict
/// tier, breaking it at runtime with `EACCES` on `connect()`.
#[test]
fn both_pull_strategies_declare_the_same_network_need() {
    let remote = RemoteName::new("origin").expect("valid remote");
    let needs: Vec<NetworkNeed> = [MergeStrategy::Merge, MergeStrategy::Rebase]
        .into_iter()
        .map(|strategy| {
            network_need_for_operation(&GitOperation::PullBranch {
                remote: remote.clone(),
                branch: branch("main"),
                strategy,
            })
        })
        .collect();
    assert_eq!(
        needs,
        vec![NetworkNeed::Remote, NetworkNeed::Remote],
        "both pull strategies must declare Remote; got {needs:?}"
    );
}

/// The declaration is what picks the tier, and the *stated* argv of each
/// remote operation must agree with it — this is the cross-check's own
/// premise, tested on the argvs the planner builds (push and, since M2.20c
/// #229, `fetch --progress`) or will build (pull, #230).
///
/// Why the not-yet-built argvs are worth pinning: the D3 cross-check
/// `debug_assert`s on a `Local` declaration meeting a `Remote`-looking argv.
/// The reverse — `Remote` declared, argv unrecognised — is tolerated
/// silently, which means a `pull` argv missing from `REMOTE_SUBCOMMANDS`
/// would produce *no* signal at all when #230 lands. Checking it here is the
/// only place that failure mode is visible before it matters — and it earned
/// its keep for fetch, whose real argv (`fetch --progress <remote>`) was
/// pinned here a slice before anything ran it.
#[test]
fn the_remote_declarations_and_their_argvs_agree() {
    for args in [
        vec!["push", "origin", "feature"],
        vec!["push", "--set-upstream", "origin", "feature"],
        vec![
            "push",
            "--force-with-lease=feature:abc",
            "origin",
            "feature",
        ],
        // The argv `planner::fetch::exec_fetch` actually builds (M2.20c,
        // #229) — `--progress` included, since a flag between the subcommand
        // and the remote is exactly the shape a first-token classifier could
        // have been written to miss.
        vec!["fetch", "--progress", "origin"],
        vec!["fetch", "origin"],
        vec!["pull", "--no-rebase", "origin", "main"],
        vec!["pull", "--rebase", "origin", "main"],
    ] {
        assert_eq!(
            network_need(&args),
            NetworkNeed::Remote,
            "the argv classifier must agree with the declaration for {args:?}, \
             or the D3 cross-check would be tripped (push) or silent (fetch/pull)"
        );
    }
}

// --- the cross-check (D3) --------------------------------------------------

/// The tolerated direction: an argv the incomplete `REMOTE_SUBCOMMANDS` list
/// does not recognise must never pull a `Remote` declaration down to `Local`.
/// Narrowing here would take the network away from an operation that declared
/// it needs the network, on the word of a list documented as incomplete.
#[test]
fn the_cross_check_never_narrows_a_remote_declaration() {
    for args in [
        vec!["remote", "update"],
        vec!["submodule", "update", "--remote"],
        vec!["status", "--porcelain"],
        vec![],
    ] {
        assert_eq!(
            reconcile_need(NetworkNeed::Remote, &args),
            NetworkNeed::Remote,
            "a declared Remote must survive argv {args:?}"
        );
    }
}

/// The empty-argv hazard D3 named explicitly: `network_need(&[])` is `Local`,
/// which routes to Strict. Before Task 8 nothing in production called it, so
/// that was latent; now that the tier is live, the guarantee is that an empty
/// argv cannot *decide* anything — the declaration does.
#[test]
fn an_empty_argv_cannot_move_the_tier() {
    assert_eq!(
        network_need(&[]),
        NetworkNeed::Local,
        "documented behaviour"
    );
    assert_eq!(
        reconcile_need(NetworkNeed::Remote, &[]),
        NetworkNeed::Remote
    );
    assert_eq!(reconcile_need(NetworkNeed::Local, &[]), NetworkNeed::Local);
}

/// Agreement is a no-op in both directions.
#[test]
fn the_cross_check_passes_agreeing_pairs_through() {
    assert_eq!(
        reconcile_need(NetworkNeed::Local, &["status", "--porcelain"]),
        NetworkNeed::Local
    );
    assert_eq!(
        reconcile_need(NetworkNeed::Remote, &["push", "origin"]),
        NetworkNeed::Remote
    );
}

/// The disagreement that is a server bug: declared `Local`, argv starts with a
/// known remote subcommand. In a debug build this must be *loud*, because a
/// developer meeting it has written a mismatch between
/// `network_need_for_operation` and the argv their `exec_*` builds.
///
/// `debug_assert!` compiles away in release, where the documented behaviour is
/// "log and keep the stricter tier". That half is asserted structurally by
/// `the_cross_check_keeps_the_stricter_tier_on_mismatch` below rather than by
/// running a release build from a debug test.
#[test]
#[should_panic(expected = "cross-check")]
fn a_local_declaration_with_a_remote_argv_panics_in_debug() {
    let _ = reconcile_need(NetworkNeed::Local, &["push", "origin", "main"]);
}

/// The release behaviour, stated as the property that makes it safe: the value
/// `reconcile_need` would return on a mismatch is the declared `Local`, and
/// `tier_for` maps that to `Strict` — the tier with **no** network at all,
/// which is stricter than the `Network` tier the argv argued for. So the
/// mismatch fails closed: a genuinely-remote command mislabelled `Local` gets
/// `EACCES` on `connect()` and says so, rather than silently gaining a socket.
#[test]
fn the_cross_check_keeps_the_stricter_tier_on_mismatch() {
    assert_eq!(
        tier_for(NetworkNeed::Local, false),
        Tier::Strict,
        "the value kept on a mismatch must route to the stricter tier"
    );
    assert_ne!(
        tier_for(NetworkNeed::Local, false),
        tier_for(NetworkNeed::Remote, false),
        "if these were the same tier the cross-check would be decorative"
    );
}

// --- INV-13 / ADR 0029: Strict is refused, never downgraded -----------------

fn caps(landlock_abi: i32, bwrap_present: bool, userns: bool) -> capabilities::Capabilities {
    capabilities::Capabilities {
        landlock_abi,
        bwrap_present,
        userns,
        seccomp_available: true,
    }
}

/// Every single missing capability refuses, and names itself. The assertion
/// that matters is `is_err()`: the alternatives ADR 0029 rejects — returning a
/// `Network` policy, or a `Strict` policy with hooks blocked — are both `Ok`,
/// so a regression to either fails here rather than shipping a quietly weaker
/// sandbox.
#[test]
fn strict_refuses_and_names_the_capability_when_the_host_cannot_supply_it() {
    let launcher = Some(PathBuf::from("/usr/bin/bwrap"));
    for (label, c, expect) in [
        ("no landlock", caps(-1, true, true), "landlock_abi>=6"),
        (
            "landlock below floor",
            caps(LANDLOCK_ABI_FLOOR as i32 - 1, true, true),
            "landlock_abi>=6",
        ),
        ("no userns", caps(8, true, false), "user_namespaces"),
        ("no bwrap", caps(8, false, true), "bwrap"),
    ] {
        let got = strict_launcher(&c, launcher.clone());
        match got {
            Err(shim::ShimError::StrictUnavailable { missing }) => {
                assert!(
                    missing.contains(&expect),
                    "{label}: the refusal must name `{expect}`, got {missing:?}"
                );
            }
            other => {
                panic!("{label}: INV-13 requires a named refusal, never a degrade — got {other:?}")
            }
        }
    }
}

/// A host with every capability but no launcher at a reviewed absolute path
/// still cannot run the tier, and the refusal must not be empty-handed.
#[test]
fn strict_refuses_with_a_named_reason_when_only_the_launcher_is_absent() {
    match strict_launcher(&caps(8, true, true), None) {
        Err(shim::ShimError::StrictUnavailable { missing }) => {
            assert!(
                !missing.is_empty(),
                "a refusal that names nothing tells the operator nothing"
            );
        }
        other => panic!("expected a named refusal, got {other:?}"),
    }
}

/// The refusal's own text must point at the decision, not just at the symptom —
/// an operator who sees it needs to know this is deliberate.
#[test]
fn the_strict_refusal_explains_itself() {
    let e = shim::ShimError::StrictUnavailable {
        missing: vec!["bwrap"],
    };
    let text = e.to_string();
    assert!(text.contains("bwrap"), "names what is missing: {text}");
    assert!(text.contains("ADR 0029"), "cites the decision: {text}");
    assert!(
        text.contains("refused"),
        "says the operation is refused, not degraded: {text}"
    );
}

/// A host that *can* supply the tier gets the launcher back unchanged.
/// Without this leg the refusal tests above would pass on a `strict_launcher`
/// that refused unconditionally.
#[test]
fn strict_is_granted_on_a_capable_host() {
    let launcher = PathBuf::from("/usr/bin/bwrap");
    assert_eq!(
        strict_launcher(&caps(8, true, true), Some(launcher.clone())),
        Ok(launcher)
    );
}

// --- the production policy, end to end -------------------------------------

/// A local operation on an untrusted repository is `Strict`, with the tier's
/// whole shape: a resolved bwrap launcher (there is no strict tier without the
/// namespaces) and **no** network ports (F3 — `--net-deny`).
#[test]
fn a_local_operation_gets_the_strict_tier_with_no_ports() {
    let repo = tempfile::tempdir().expect("tempdir");
    let policy = policy_for(repo.path(), false, NetworkNeed::Local)
        .expect("policy builds (shim present via tests/forces_shim_build.rs)");
    assert_eq!(policy.tier, Tier::Strict);
    assert!(
        policy.bwrap.is_some(),
        "a Strict policy without a launcher would panic in `shim_argv`"
    );
    assert!(
        policy.net_ports.is_empty(),
        "the strict tier denies the network outright; ports there would be an \
         argv that contradicts itself"
    );
}

/// A remote operation is `Network`: no bwrap (its namespace breaks push, F3),
/// and the git ports present.
#[test]
fn a_remote_operation_gets_the_network_tier_with_the_git_ports() {
    let repo = tempfile::tempdir().expect("tempdir");
    let policy = policy_for(repo.path(), false, NetworkNeed::Remote)
        .expect("policy builds (shim present via tests/forces_shim_build.rs)");
    assert_eq!(policy.tier, Tier::Network);
    assert_eq!(policy.bwrap, None);
    assert_eq!(policy.net_ports, DEFAULT_GIT_PORTS.to_vec());
}

/// The property the whole dispatch rests on, asserted against the *production*
/// constructor rather than against `tier_for` with a local `let trusted =
/// false` (the vacuity the C10 audit flagged): an untrusted repository cannot
/// reach `Unsandboxed` for any need, and the secret set is never empty in
/// whichever tier it does reach.
#[test]
fn an_untrusted_repository_can_never_be_unsandboxed() {
    let repo = tempfile::tempdir().expect("tempdir");
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        let policy = policy_for(repo.path(), false, need).expect("policy builds");
        assert_ne!(
            policy.tier,
            Tier::Unsandboxed,
            "an untrusted repository must never be unsandboxed (need={need:?})"
        );
        assert!(
            !policy.secret_excludes.is_empty(),
            "the secret set must never be silently empty (need={need:?})"
        );
    }
}

/// Revokes on drop, so a panicking assertion cannot leave a real trust marker
/// behind in `~/.local/state/git-vista/trusted-repos`.
struct TrustGuard(PathBuf);

impl Drop for TrustGuard {
    fn drop(&mut self) {
        let _ = trust::revoke(&self.0);
    }
}

/// `sandbox::trust`'s first production consumer, tested through the production
/// constructor: a granted repository reaches `Unsandboxed` for *every* need
/// (trust is a property of the repository, not the operation), and revoking
/// takes it straight back.
///
/// The before/after legs are both required. Without the "before" leg this would
/// pass on a `policy_for` that always returned `Unsandboxed`; without the
/// "after" leg it would pass on one that never consulted `revoke`.
#[test]
fn an_operator_granted_repository_is_unsandboxed_and_revoke_takes_it_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let canonical = dir.path().canonicalize().expect("canonicalize");

    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_ne!(
            policy_for(dir.path(), false, need)
                .expect("policy builds")
                .tier,
            Tier::Unsandboxed,
            "before the grant, nothing may be unsandboxed"
        );
    }

    let guard = TrustGuard(canonical.clone());
    trust::grant(&canonical).expect("grant writes a marker");
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_eq!(
            policy_for(dir.path(), false, need)
                .expect("policy builds")
                .tier,
            Tier::Unsandboxed,
            "an operator-trusted repository runs unsandboxed for every need"
        );
    }

    drop(guard);
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_ne!(
            policy_for(dir.path(), false, need)
                .expect("policy builds")
                .tier,
            Tier::Unsandboxed,
            "revoking trust must take the sandbox back immediately"
        );
    }
}

/// Trust is keyed by canonical path, and this is the failure direction that
/// matters: a marker granted for one repository must not trust a *different*
/// one. A hash-collision or a prefix-match implementation would fail here.
#[test]
fn a_grant_for_one_repository_does_not_trust_its_neighbour() {
    let granted = tempfile::tempdir().expect("tempdir");
    let other = tempfile::tempdir().expect("tempdir");
    let canonical = granted.path().canonicalize().expect("canonicalize");
    let guard = TrustGuard(canonical.clone());
    trust::grant(&canonical).expect("grant");

    assert_ne!(
        policy_for(other.path(), false, NetworkNeed::Local)
            .expect("policy builds")
            .tier,
        Tier::Unsandboxed,
        "a grant must not leak to a repository the operator never named"
    );
    drop(guard);
}

/// A path that does not exist cannot be canonicalised, and every uncertainty in
/// the trust chain means untrusted. Fail-closed, asserted rather than assumed.
#[test]
fn an_unresolvable_path_is_never_trusted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-repo");
    assert!(!repo_is_trusted(&missing));
}
