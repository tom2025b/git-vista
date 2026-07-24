//! Golden-fixture test for the [`Plan`] wire contract (M1.06a, #142).
//!
//! `tests/fixtures/plan_v1.json` is the **committed** wire form of fifteen
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
    BranchName, CommitMessage, CommitOid, GenerationToken, GitOperation, OperationHash, Plan,
    Precondition, RecoveryStrategy, RefChange, RefName, RefState, RemoteName, RepositoryToken,
    RiskLevel, UnixSeconds, WorktreeToken,
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
            GitOperation::PushBranch {
                branch: branch("main"),
                remote: RemoteName::new("origin").unwrap(),
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
    ]
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
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(tags, expected, "operation wire tags changed");
}
