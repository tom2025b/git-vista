//! Golden-fixture test for the [`Plan`] wire contract (M1.06a, #142).
//!
//! `tests/fixtures/plan_v1.json` is the **committed** wire form of twenty-five
//! plans — one per [`GitOperation`] variant, together exercising every
//! [`RiskLevel`], [`Precondition`], [`RefState`] and [`RecoveryStrategy`]
//! variant. The test proves the contract is lossless in both directions:
//!
//! 1. the fixture deserializes into exactly the plans built here in code, and
//! 2. re-serializing those plans reproduces the fixture **byte for byte**.
//!
//! So any accidental rename, retag, or field change breaks this test loudly —
//! a wire change must be deliberate: update the fixture by running
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test plan_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).

use git_vista_protocol::{
    BranchName, CommitMessage, CommitOid, ForcePublish, GenerationToken, GitOperation,
    MergeStrategy, OperationHash, Plan, Precondition, RecoveryStrategy, RefChange, RefName,
    RefState, RemoteName, RepositoryToken, RiskLevel, SignatureStatus, StageDirection,
    TagAnnotation, TagDetail, TagKind, TagMessage, TagName, UnixSeconds, WorktreePath,
    WorktreeToken,
};

const FIXTURE: &str = include_str!("fixtures/plan_v1.json");
const FIXTURE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/plan_v1.json");

fn oid(byte: char) -> CommitOid {
    CommitOid::new(byte.to_string().repeat(40)).unwrap()
}

fn branch(name: &str) -> BranchName {
    BranchName::new(name).unwrap()
}

fn rname(name: &str) -> RefName {
    RefName::new(name).unwrap()
}

fn wpath(path: &str) -> WorktreePath {
    WorktreePath::new(path).unwrap()
}

fn tag(name: &str) -> TagName {
    TagName::new(name).unwrap()
}

/// One plan, with the boilerplate identity/window fields filled in and the
/// hash derived (recognisably, not cryptographically — the real SHA-256
/// derivation is #145's job; the *shape* is contract now) from a seed char.
#[allow(clippy::too_many_arguments)]
fn plan(
    seed: char,
    operation: GitOperation,
    risk: RiskLevel,
    preconditions: Vec<Precondition>,
    expected_ref_changes: Vec<RefChange>,
    recovery: RecoveryStrategy,
) -> Plan {
    Plan {
        repository: RepositoryToken::new("11111111-1111-5111-8111-111111111111").unwrap(),
        worktree: WorktreeToken::new("22222222-2222-5222-8222-222222222222").unwrap(),
        generation: GenerationToken::new("12345678901234567890").unwrap(),
        operation,
        operation_hash: OperationHash::new(seed.to_string().repeat(64)).unwrap(),
        issued_at: UnixSeconds(1_753_300_000),
        expires_at: UnixSeconds(1_753_300_300),
        risk,
        preconditions,
        expected_ref_changes,
        recovery,
    }
}

/// The golden set: one plan per operation variant of the closed vocabulary.
fn golden_plans() -> Vec<Plan> {
    vec![
        plan(
            'a',
            GitOperation::CreateBranch {
                name: branch("feature/idea"),
                at: oid('1'),
            },
            RiskLevel::Reversible,
            vec![Precondition::RefAbsent {
                ref_name: rname("refs/heads/feature/idea"),
            }],
            vec![RefChange {
                ref_name: rname("refs/heads/feature/idea"),
                before: RefState::Absent,
                after: RefState::At(oid('1')),
            }],
            RecoveryStrategy::DeleteCreatedBranch {
                name: branch("feature/idea"),
            },
        ),
        plan(
            'b',
            GitOperation::CommitOnHead {
                message: CommitMessage::new("feat: land the thing").unwrap(),
                allow_empty: false,
            },
            RiskLevel::Reversible,
            vec![Precondition::BranchCheckedOut {
                branch: branch("main"),
            }],
            vec![RefChange {
                ref_name: rname("refs/heads/main"),
                before: RefState::At(oid('2')),
                after: RefState::Computed,
            }],
            RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/main"),
                to: oid('2'),
            },
        ),
        plan(
            'c',
            GitOperation::EmptyCommitOnBranch {
                branch: branch("feature/stub"),
                message: CommitMessage::new("chore: first (empty) commit").unwrap(),
                expected_tip: oid('3'),
            },
            RiskLevel::Reversible,
            vec![
                Precondition::BranchNotCheckedOut {
                    branch: branch("feature/stub"),
                },
                Precondition::RefAt {
                    ref_name: rname("refs/heads/feature/stub"),
                    oid: oid('3'),
                },
            ],
            vec![RefChange {
                ref_name: rname("refs/heads/feature/stub"),
                before: RefState::At(oid('3')),
                after: RefState::Computed,
            }],
            RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/feature/stub"),
                to: oid('3'),
            },
        ),
        plan(
            'd',
            GitOperation::StageAll,
            RiskLevel::Safe,
            vec![],
            vec![],
            RecoveryStrategy::NotNeeded,
        ),
        plan(
            'e',
            GitOperation::UnstageAll,
            RiskLevel::Safe,
            vec![],
            vec![],
            RecoveryStrategy::NotNeeded,
        ),
        plan(
            'f',
            GitOperation::CheckoutBranch {
                branch: branch("feature/idea"),
            },
            RiskLevel::Safe,
            vec![Precondition::RefExists {
                ref_name: rname("refs/heads/feature/idea"),
            }],
            vec![RefChange {
                ref_name: rname("HEAD"),
                before: RefState::Symbolic(rname("refs/heads/main")),
                after: RefState::Symbolic(rname("refs/heads/feature/idea")),
            }],
            RecoveryStrategy::CheckoutPrevious {
                branch: branch("main"),
            },
        ),
        plan(
            'a',
            GitOperation::MergeBranch {
                branch: branch("feature/idea"),
            },
            RiskLevel::Reversible,
            vec![
                Precondition::BranchCheckedOut {
                    branch: branch("main"),
                },
                Precondition::RefAt {
                    ref_name: rname("refs/heads/main"),
                    oid: oid('2'),
                },
            ],
            vec![RefChange {
                ref_name: rname("refs/heads/main"),
                before: RefState::At(oid('2')),
                after: RefState::Computed,
            }],
            RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/main"),
                to: oid('2'),
            },
        ),
        plan(
            'b',
            // The push shape production actually emits: fast-forward, no
            // upstream write. M2.20a's other combinations are pinned by
            // `a_lease_force_push_pins_its_own_wire_shape` below rather than
            // here, because `golden_set_covers_every_operation_variant`
            // allows only one plan per `op` tag.
            GitOperation::PushBranch {
                branch: branch("main"),
                remote: RemoteName::new("origin").unwrap(),
                set_upstream: false,
                force: ForcePublish::None,
            },
            RiskLevel::Remote,
            vec![
                Precondition::RemoteConfigured {
                    remote: RemoteName::new("origin").unwrap(),
                },
                Precondition::RefExists {
                    ref_name: rname("refs/heads/main"),
                },
            ],
            vec![RefChange {
                ref_name: rname("refs/remotes/origin/main"),
                before: RefState::At(oid('4')),
                after: RefState::At(oid('2')),
            }],
            RecoveryStrategy::Irrecoverable,
        ),
        plan(
            'c',
            GitOperation::DeleteBranch {
                branch: branch("feature/done"),
            },
            RiskLevel::Reversible,
            vec![Precondition::RefAt {
                ref_name: rname("refs/heads/feature/done"),
                oid: oid('5'),
            }],
            vec![RefChange {
                ref_name: rname("refs/heads/feature/done"),
                before: RefState::At(oid('5')),
                after: RefState::Absent,
            }],
            RecoveryStrategy::RecreateBranch {
                name: branch("feature/done"),
                at: oid('5'),
            },
        ),
        plan(
            'd',
            GitOperation::ForceDeleteBranch {
                branch: branch("feature/abandoned"),
            },
            RiskLevel::Destructive,
            vec![Precondition::RefAt {
                ref_name: rname("refs/heads/feature/abandoned"),
                oid: oid('6'),
            }],
            vec![RefChange {
                ref_name: rname("refs/heads/feature/abandoned"),
                before: RefState::At(oid('6')),
                after: RefState::Absent,
            }],
            RecoveryStrategy::RecreateBranch {
                name: branch("feature/abandoned"),
                at: oid('6'),
            },
        ),
        plan(
            'e',
            GitOperation::RebaseOntoBase {
                base: rname("origin/main"),
            },
            RiskLevel::Reversible,
            vec![
                Precondition::BranchCheckedOut {
                    branch: branch("feature/idea"),
                },
                Precondition::RefExists {
                    ref_name: rname("refs/remotes/origin/main"),
                },
            ],
            vec![RefChange {
                ref_name: rname("refs/heads/feature/idea"),
                before: RefState::At(oid('1')),
                after: RefState::Computed,
            }],
            RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/feature/idea"),
                to: oid('1'),
            },
        ),
        plan(
            'f',
            GitOperation::RestoreBranch {
                name: branch("feature/done"),
                tip: oid('5'),
            },
            RiskLevel::Reversible,
            vec![Precondition::RefAbsent {
                ref_name: rname("refs/heads/feature/done"),
            }],
            vec![RefChange {
                ref_name: rname("refs/heads/feature/done"),
                before: RefState::Absent,
                after: RefState::At(oid('5')),
            }],
            RecoveryStrategy::DeleteCreatedBranch {
                name: branch("feature/done"),
            },
        ),
        plan(
            'a',
            GitOperation::ResetBranch {
                branch: branch("main"),
                to: oid('2'),
                expected_tip: oid('7'),
            },
            RiskLevel::Destructive,
            vec![
                Precondition::RefAt {
                    ref_name: rname("refs/heads/main"),
                    oid: oid('7'),
                },
                Precondition::CleanWorktree,
            ],
            vec![RefChange {
                ref_name: rname("refs/heads/main"),
                before: RefState::At(oid('7')),
                after: RefState::At(oid('2')),
            }],
            RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/main"),
                to: oid('7'),
            },
        ),
        plan(
            'b',
            GitOperation::RevertCommit { commit: oid('8') },
            RiskLevel::Reversible,
            vec![Precondition::BranchCheckedOut {
                branch: branch("main"),
            }],
            vec![RefChange {
                ref_name: rname("refs/heads/main"),
                before: RefState::At(oid('2')),
                after: RefState::Computed,
            }],
            RecoveryStrategy::RevertCommit { commit: oid('8') },
        ),
        plan(
            'c',
            GitOperation::ResetTestRepo,
            RiskLevel::Destructive,
            vec![Precondition::SeedRecorded],
            vec![RefChange {
                ref_name: rname("refs/heads/main"),
                before: RefState::At(oid('7')),
                after: RefState::At(oid('9')),
            }],
            RecoveryStrategy::Irrecoverable,
        ),
        plan(
            'd',
            GitOperation::StageSelection {
                direction: StageDirection::Stage,
                expected_diff_generation: GenerationToken::new("diff-v1:12345678901234567890")
                    .unwrap(),
                patch: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n context\n+added\n"
                    .to_string(),
                whole_files: vec!["assets/logo.png".to_string()],
            },
            RiskLevel::Safe,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::NotNeeded,
        ),
        plan(
            'e',
            GitOperation::DiscardTrackedPaths {
                paths: vec![wpath("src/lib.rs"), wpath("dir/edited.txt")],
            },
            RiskLevel::Destructive,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::RecoverableIfStaged,
        ),
        plan(
            'f',
            GitOperation::DeleteUntrackedPaths {
                paths: vec![wpath("scratch/tmp.log")],
            },
            RiskLevel::Destructive,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
        // #222 (M2.19a): contract only — no handler builds this variant yet
        // and no execution is wired (see `GitOperation::AmendCommit`'s doc
        // comment); the golden plan still pins the wire shape today so #223
        // cannot silently change it while wiring execution in.
        plan(
            'a',
            GitOperation::AmendCommit {
                message: CommitMessage::new("fix: correct the typo").unwrap(),
                expected_tip: oid('2'),
                allow_empty: false,
            },
            RiskLevel::Destructive,
            vec![
                Precondition::BranchCheckedOut {
                    branch: branch("main"),
                },
                Precondition::RefAt {
                    ref_name: rname("refs/heads/main"),
                    oid: oid('2'),
                },
            ],
            vec![RefChange {
                ref_name: rname("refs/heads/main"),
                before: RefState::At(oid('2')),
                after: RefState::Computed,
            }],
            RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/main"),
                to: oid('2'),
            },
        ),
        // #227 (M2.20a): contract only, like `amend_commit` above — the
        // vocabulary and its network classification land before #229/#230
        // wire any socket. The golden plans pin the wire shape now so those
        // slices cannot quietly change it while adding execution.
        //
        // Fetch is `Safe`/`NotNeeded` with no ref change listed: which
        // `refs/remotes/*` move is unknowable until git has spoken to the
        // remote, so there is nothing honest to claim (see the variant doc).
        plan(
            'b',
            GitOperation::FetchRemote {
                remote: RemoteName::new("origin").unwrap(),
            },
            RiskLevel::Safe,
            vec![Precondition::RemoteConfigured {
                remote: RemoteName::new("origin").unwrap(),
            }],
            Vec::new(),
            RecoveryStrategy::NotNeeded,
        ),
        plan(
            'c',
            GitOperation::PullBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: branch("main"),
                strategy: MergeStrategy::Rebase,
            },
            RiskLevel::Reversible,
            vec![
                Precondition::BranchCheckedOut {
                    branch: branch("main"),
                },
                Precondition::RemoteConfigured {
                    remote: RemoteName::new("origin").unwrap(),
                },
                Precondition::RefAt {
                    ref_name: rname("refs/heads/main"),
                    oid: oid('2'),
                },
            ],
            vec![RefChange {
                ref_name: rname("refs/heads/main"),
                before: RefState::At(oid('2')),
                after: RefState::Computed,
            }],
            RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/main"),
                to: oid('2'),
            },
        ),
        // #235 (M2.21a, ADR 0041): contract only, the same staging as
        // `amend_commit` / `fetch_remote` above — the four tag operations'
        // wire shapes land and are pinned before any handler can build one
        // or any execution exists (`planner::execute` refuses all four).
        //
        // The golden `create_tag` is the *annotated* form so the fixture
        // carries a `TagAnnotation`; the lightweight form (annotation null)
        // is pinned by `plan.rs`'s wire-name unit test and by the paired
        // positives in `no_wire_body_can_request_a_signed_lightweight_tag`
        // below, since this set allows only one plan per `op` tag. The ref
        // change's `after` is `Computed`: an annotated tag ref points at a
        // tag *object* the operation itself creates, so the value is
        // unknowable at review time — exactly `commit_on_head`'s posture.
        plan(
            'd',
            GitOperation::CreateTag {
                name: tag("v1.0.0"),
                target: oid('2'),
                annotation: Some(TagAnnotation {
                    message: TagMessage::new("v1.0.0 — first stable release").unwrap(),
                    sign: false,
                }),
            },
            RiskLevel::Reversible,
            vec![Precondition::RefAbsent {
                ref_name: rname("refs/tags/v1.0.0"),
            }],
            vec![RefChange {
                ref_name: rname("refs/tags/v1.0.0"),
                before: RefState::Absent,
                after: RefState::Computed,
            }],
            RecoveryStrategy::DeleteCreatedTag {
                name: tag("v1.0.0"),
            },
        ),
        // `delete_local_tag`'s pinned oids are the decision under test: the
        // precondition and the recovery both carry the **unpeeled** ref value
        // ('8'…, standing in for an annotated tag's tag-object oid), so the
        // recovery restores the original tag object — signature and all —
        // rather than minting a look-alike. See `RecreateTag`'s doc.
        plan(
            'e',
            GitOperation::DeleteLocalTag {
                name: tag("v1.0.0"),
            },
            RiskLevel::Destructive,
            vec![Precondition::RefAt {
                ref_name: rname("refs/tags/v1.0.0"),
                oid: oid('8'),
            }],
            vec![RefChange {
                ref_name: rname("refs/tags/v1.0.0"),
                before: RefState::At(oid('8')),
                after: RefState::Absent,
            }],
            RecoveryStrategy::RecreateTag {
                name: tag("v1.0.0"),
                at: oid('8'),
            },
        ),
        // `delete_remote_tag` and `push_tag` list no ref change: a remote tag
        // has no local remote-tracking ref (tags fetch straight into
        // `refs/tags/`), so there is no honest local `RefChange` to show a
        // reviewer — the same D5 posture as `fetch_remote` above.
        plan(
            'f',
            GitOperation::DeleteRemoteTag {
                name: tag("v1.0.0"),
                remote: RemoteName::new("origin").unwrap(),
            },
            RiskLevel::Destructive,
            vec![Precondition::RemoteConfigured {
                remote: RemoteName::new("origin").unwrap(),
            }],
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
        plan(
            'a',
            GitOperation::PushTag {
                name: tag("v1.0.0"),
                remote: RemoteName::new("origin").unwrap(),
            },
            RiskLevel::Remote,
            vec![
                Precondition::RemoteConfigured {
                    remote: RemoteName::new("origin").unwrap(),
                },
                Precondition::RefExists {
                    ref_name: rname("refs/tags/v1.0.0"),
                },
            ],
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
    ]
}

/// The lease-force push's wire form, pinned against a literal rather than
/// against a round trip.
///
/// A round-trip test (serialize, deserialize, compare) passes for *any*
/// self-consistent encoding — including one where `ForcePublish` grew a
/// `#[serde(untagged)]` attribute and `{"mode": "with_lease"}` silently
/// became something else. Comparing against bytes written out by hand is what
/// makes the encoding itself the thing under test, which is the whole reason
/// this file exists.
#[test]
fn a_lease_force_push_pins_its_own_wire_shape() {
    let op = GitOperation::PushBranch {
        branch: branch("main"),
        remote: RemoteName::new("origin").unwrap(),
        set_upstream: true,
        force: ForcePublish::WithLease {
            expected_remote_tip: oid('4'),
        },
    };
    let expected = serde_json::json!({
        "op": "push_branch",
        "branch": "main",
        "remote": "origin",
        "set_upstream": true,
        "force": {
            "mode": "with_lease",
            "expected_remote_tip": "4444444444444444444444444444444444444444",
        },
    });
    assert_eq!(serde_json::to_value(&op).unwrap(), expected);
    // …and back, so the pin is bidirectional.
    assert_eq!(
        serde_json::from_value::<GitOperation>(expected).unwrap(),
        op
    );
}

/// The pre-M2.20a `push_branch` body must now be **rejected**, not silently
/// completed with defaults.
///
/// This is the paired negative for the fixture change above: without it, a
/// `#[serde(default)]` slipped onto `set_upstream` or `force` would leave
/// every other test in this file green while an omitted `force` quietly
/// became `ForcePublish::None`. That is a live risk rather than a
/// hypothetical — the old shape is exactly what a stale client, a replayed
/// request body, or a copy-pasted fixture would send.
#[test]
fn the_pre_m2_20a_push_body_no_longer_deserializes() {
    let old = serde_json::json!({
        "op": "push_branch",
        "branch": "main",
        "remote": "origin",
    });
    let err = serde_json::from_value::<GitOperation>(old)
        .expect_err("a push body without set_upstream/force must be a hard error");
    let msg = err.to_string();
    assert!(
        msg.contains("set_upstream") || msg.contains("force"),
        "the error must name the missing field, got: {msg}"
    );

    // The paired positive, proving the rejection above is about the missing
    // fields and not about some unrelated breakage in this JSON: the same
    // body with both fields supplied deserializes fine.
    let complete = serde_json::json!({
        "op": "push_branch",
        "branch": "main",
        "remote": "origin",
        "set_upstream": false,
        "force": { "mode": "none" },
    });
    assert_eq!(
        serde_json::from_value::<GitOperation>(complete).unwrap(),
        GitOperation::PushBranch {
            branch: branch("main"),
            remote: RemoteName::new("origin").unwrap(),
            set_upstream: false,
            force: ForcePublish::None,
        }
    );
}

/// No wire body can ask for an unguarded force push (#227 acceptance).
///
/// `ForcePublish` has no such variant, so this is really a test that no
/// *serde* attribute has quietly made one reachable — `untagged`, an alias,
/// or a `From<bool>`-style shim would each re-open the hole the type was
/// shaped to close. Every spelling a caller might reach for must be a hard
/// deserialize error.
#[test]
fn no_wire_body_can_request_an_unguarded_force_push() {
    for force in [
        serde_json::json!({ "mode": "force" }),
        serde_json::json!({ "mode": "forced" }),
        serde_json::json!({ "mode": "with_lease" }), // a lease with no oid
        // A stray key alongside the lease: `deny_unknown_fields` catches this
        // on the struct variant, so a misspelled `expected_remote_tip` cannot
        // become a lease that pins nothing.
        serde_json::json!({
            "mode": "with_lease",
            "expected_remote_tip": "4444444444444444444444444444444444444444",
            "also_force": true,
        }),
        serde_json::json!("force"),
        serde_json::json!(true),
        serde_json::json!(null),
    ] {
        let body = serde_json::json!({
            "op": "push_branch",
            "branch": "main",
            "remote": "origin",
            "set_upstream": false,
            "force": force,
        });
        assert!(
            serde_json::from_value::<GitOperation>(body.clone()).is_err(),
            "a force mode of {force} must not deserialize"
        );
    }

    // The one stray-key case serde does *not* reject, recorded here rather
    // than left as a surprise: `deny_unknown_fields` has no effect on a
    // **unit** variant of an internally-tagged enum, so a tip supplied
    // alongside `"mode": "none"` is ignored. That degrades toward the safe
    // variant — the result is `ForcePublish::None`, a plain fast-forward
    // push, which the plan then shows as `RiskLevel::Remote` with no lease
    // precondition for the user to approve. It is pinned so that a future
    // encoding change which made this parse as a *lease* (or, worse, as
    // anything forceful) fails here instead of shipping.
    let stray = serde_json::json!({
        "op": "push_branch",
        "branch": "main",
        "remote": "origin",
        "set_upstream": false,
        "force": { "mode": "none", "expected_remote_tip": "4444444444444444444444444444444444444444" },
    });
    assert_eq!(
        serde_json::from_value::<GitOperation>(stray).unwrap(),
        GitOperation::PushBranch {
            branch: branch("main"),
            remote: RemoteName::new("origin").unwrap(),
            set_upstream: false,
            force: ForcePublish::None,
        },
        "an ignored stray key must still land on the *safe* force mode"
    );
}

/// A `pull_branch` body that omits `strategy` is a deserialize error — #227's
/// headline acceptance criterion, and the reason [`MergeStrategy`] has no
/// `Default`.
///
/// The paired positives matter as much as the negative: both real strategies
/// must deserialize, so this cannot pass by `pull_branch` being broken
/// outright. And an invented third value must fail, so it cannot pass by the
/// field accepting anything at all.
#[test]
fn a_pull_without_a_strategy_is_a_deserialize_error() {
    let without = serde_json::json!({
        "op": "pull_branch",
        "remote": "origin",
        "branch": "main",
    });
    let err = serde_json::from_value::<GitOperation>(without)
        .expect_err("an omitted pull strategy must be an error, never a default");
    assert!(
        err.to_string().contains("strategy"),
        "the error must name the missing field, got: {err}"
    );

    for (wire, expected) in [
        ("merge", MergeStrategy::Merge),
        ("rebase", MergeStrategy::Rebase),
    ] {
        let body = serde_json::json!({
            "op": "pull_branch",
            "remote": "origin",
            "branch": "main",
            "strategy": wire,
        });
        assert_eq!(
            serde_json::from_value::<GitOperation>(body).unwrap(),
            GitOperation::PullBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: branch("main"),
                strategy: expected,
            },
            "‘{wire}’ must be the wire name for {expected:?}"
        );
    }

    for invented in ["auto", "default", "ff_only", ""] {
        let body = serde_json::json!({
            "op": "pull_branch",
            "remote": "origin",
            "branch": "main",
            "strategy": invented,
        });
        assert!(
            serde_json::from_value::<GitOperation>(body).is_err(),
            "‘{invented}’ must not be an accepted strategy"
        );
    }
}

/// No wire body can ask for a **signed lightweight tag** (#235, ADR 0041) —
/// the state [`TagAnnotation`]'s nesting exists to make unrepresentable.
///
/// Like `no_wire_body_can_request_an_unguarded_force_push` above, this is
/// really a test that no serde attribute has quietly made the impossible
/// state reachable: `sign` lives only inside the annotation, so every
/// spelling that tries to sign without a message must be a hard deserialize
/// error. The paired positives prove the rejections are about the shape, not
/// about `create_tag` being broken outright.
#[test]
fn no_wire_body_can_request_a_signed_lightweight_tag() {
    let target = "2".repeat(40);
    for (what, annotation) in [
        // The flat shape #235 sketched: sign with no message to carry it.
        (
            "a sign-only annotation",
            serde_json::json!({ "sign": true }),
        ),
        // A message-less annotation, unsigned — still not an annotated tag.
        ("an empty annotation", serde_json::json!({})),
        // `sign` omitted: no silent unsigned default (no #[serde(default)]).
        (
            "an annotation that never says whether to sign",
            serde_json::json!({ "message": "v1" }),
        ),
        // deny_unknown_fields: a misspelled key cannot be silently dropped.
        (
            "an annotation with a stray key",
            serde_json::json!({ "message": "v1", "sign": true, "force": true }),
        ),
        // A top-level `sign` beside a null annotation — the flat spelling.
        (
            "a boolean where the annotation goes",
            serde_json::json!(true),
        ),
    ] {
        let body = serde_json::json!({
            "op": "create_tag",
            "name": "v1.0.0",
            "target": target,
            "annotation": annotation,
        });
        assert!(
            serde_json::from_value::<GitOperation>(body).is_err(),
            "{what} must not deserialize into a CreateTag"
        );
    }

    // Paired positives: both real kinds deserialize, and to the right values.
    let lightweight = serde_json::json!({
        "op": "create_tag",
        "name": "v1.0.0",
        "target": target,
        "annotation": null,
    });
    assert_eq!(
        serde_json::from_value::<GitOperation>(lightweight).unwrap(),
        GitOperation::CreateTag {
            name: tag("v1.0.0"),
            target: oid('2'),
            annotation: None,
        }
    );
    // An *omitted* annotation is also lightweight (serde's Option-field
    // convention) — pinned so a future `default`-related change cannot turn
    // absence into an error or, worse, into some annotated default.
    let omitted = serde_json::json!({
        "op": "create_tag",
        "name": "v1.0.0",
        "target": target,
    });
    assert_eq!(
        serde_json::from_value::<GitOperation>(omitted).unwrap(),
        GitOperation::CreateTag {
            name: tag("v1.0.0"),
            target: oid('2'),
            annotation: None,
        }
    );
    let signed = serde_json::json!({
        "op": "create_tag",
        "name": "v1.0.0",
        "target": target,
        "annotation": { "message": "v1.0.0", "sign": true },
    });
    assert_eq!(
        serde_json::from_value::<GitOperation>(signed).unwrap(),
        GitOperation::CreateTag {
            name: tag("v1.0.0"),
            target: oid('2'),
            annotation: Some(TagAnnotation {
                message: TagMessage::new("v1.0.0").unwrap(),
                sign: true,
            }),
        }
    );
}

/// [`TagDetail`]'s exact wire bytes, pinned against literals in both
/// directions and for both kinds (#235) — the same posture as
/// `a_lease_force_push_pins_its_own_wire_shape` above: a round trip alone
/// would bless any self-consistent encoding, so the JSON is written by hand.
///
/// The DTO ships **before its producer** (the M2.21 read slice of #74), so
/// this pin is what stops that slice from quietly reshaping the contract
/// while wiring the endpoint in.
#[test]
fn tag_detail_pins_its_wire_shape_for_both_kinds() {
    let annotated = TagDetail {
        name: tag("v1.0.0"),
        kind: TagKind::Annotated,
        target: oid('2'),
        tag_object: Some(oid('8')),
        tagger: Some("Example Tagger <tagger@example.invalid> 1753300000 +0000".to_string()),
        message: Some(TagMessage::new("v1.0.0 — first stable release").unwrap()),
        signature: SignatureStatus::UnknownKey,
    };
    let annotated_wire = serde_json::json!({
        "name": "v1.0.0",
        "kind": "annotated",
        "target": "2".repeat(40),
        "tag_object": "8".repeat(40),
        "tagger": "Example Tagger <tagger@example.invalid> 1753300000 +0000",
        "message": "v1.0.0 — first stable release",
        "signature": "unknown_key",
    });
    assert_eq!(serde_json::to_value(&annotated).unwrap(), annotated_wire);
    assert_eq!(
        serde_json::from_value::<TagDetail>(annotated_wire).unwrap(),
        annotated
    );

    // A lightweight tag: no object, no tagger, no message, nothing signed —
    // `target` is the commit itself.
    let lightweight = TagDetail {
        name: tag("tip-marker"),
        kind: TagKind::Lightweight,
        target: oid('2'),
        tag_object: None,
        tagger: None,
        message: None,
        signature: SignatureStatus::Unsigned,
    };
    let lightweight_wire = serde_json::json!({
        "name": "tip-marker",
        "kind": "lightweight",
        "target": "2".repeat(40),
        "tag_object": null,
        "tagger": null,
        "message": null,
        "signature": "unsigned",
    });
    assert_eq!(
        serde_json::to_value(&lightweight).unwrap(),
        lightweight_wire
    );
    assert_eq!(
        serde_json::from_value::<TagDetail>(lightweight_wire).unwrap(),
        lightweight
    );

    // deny_unknown_fields is live (see TagDetail's doc for why a *read* DTO
    // starts strict): a stray key is a hard error, not an ignored field.
    let stray = serde_json::json!({
        "name": "v1.0.0",
        "kind": "lightweight",
        "target": "2".repeat(40),
        "tag_object": null,
        "tagger": null,
        "message": null,
        "signature": "unsigned",
        "verified": true,
    });
    assert!(serde_json::from_value::<TagDetail>(stray).is_err());

    // And the signature vocabulary is closed: every declared status has a
    // pinned wire name, and an invented one is refused — "unverifiable" and
    // "invalid" must never collapse into each other by rename.
    for (status, wire) in [
        (SignatureStatus::Unsigned, "unsigned"),
        (SignatureStatus::Valid, "valid"),
        (SignatureStatus::Invalid, "invalid"),
        (SignatureStatus::UnknownKey, "unknown_key"),
        (SignatureStatus::Unverifiable, "unverifiable"),
    ] {
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!(wire)
        );
    }
    assert!(serde_json::from_value::<SignatureStatus>(serde_json::json!("verified")).is_err());
}

#[test]
fn golden_fixture_round_trips_losslessly() {
    let plans = golden_plans();

    // Deliberate-regeneration path (see module docs): rewrite the fixture from
    // the plans above, then fall through and verify against what was written.
    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&plans).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    // 1. The committed wire form deserializes into exactly these plans…
    let parsed: Vec<Plan> = serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, plans, "fixture and in-code golden plans diverged");

    // 2. …and re-serializing reproduces the committed bytes exactly, so no
    //    field is dropped, defaulted, renamed, or reordered in flight.
    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized plans no longer match the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}

#[test]
fn golden_set_covers_every_operation_variant() {
    // One plan per variant of the closed vocabulary: count the distinct `op`
    // tags on the wire. A new GitOperation variant without a golden plan (or
    // a golden plan reusing a tag) fails here, keeping fixture and vocabulary
    // in lockstep.
    let plans = golden_plans();
    let tags: std::collections::BTreeSet<String> = plans
        .iter()
        .map(|p| {
            serde_json::to_value(&p.operation).unwrap()["op"]
                .as_str()
                .expect("every operation serializes with an op tag")
                .to_string()
        })
        .collect();
    assert_eq!(
        tags.len(),
        plans.len(),
        "duplicate operation kinds in the golden set"
    );
    let expected: std::collections::BTreeSet<String> = [
        "create_branch",
        "commit_on_head",
        "empty_commit_on_branch",
        "stage_all",
        "unstage_all",
        "checkout_branch",
        "merge_branch",
        "push_branch",
        "delete_branch",
        "force_delete_branch",
        "rebase_onto_base",
        "restore_branch",
        "reset_branch",
        "revert_commit",
        "reset_test_repo",
        "stage_selection",
        "discard_tracked_paths",
        "delete_untracked_paths",
        "amend_commit",
        "fetch_remote",
        "pull_branch",
        "create_tag",
        "delete_local_tag",
        "delete_remote_tag",
        "push_tag",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(tags, expected, "operation wire tags changed");
}
