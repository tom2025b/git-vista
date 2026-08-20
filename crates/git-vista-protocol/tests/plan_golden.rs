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
    Advisory, BranchName, CommitMessage, CommitOid, ForcePublish, GenerationToken, GitOperation,
    MergeStrategy, OperationHash, Plan, Precondition, RecoveryStrategy, RefChange, RefName,
    RefState, RemoteName, RepositoryToken, RiskLevel, SignatureStatus, StageDirection,
    StashMessage, StashSelector, TagAnnotation, TagDetail, TagKind, TagMessage, TagName,
    UnixSeconds, WorktreePath, WorktreeToken,
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
        // Most operations earn none. The force-with-lease case that does is
        // built explicitly below, so the advisory wire shape is pinned by a
        // case a reader can see rather than by a defaulted argument.
        advisories: Vec::new(),
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
            '1',
            // M3.24 (#77): pop is its own variant, not apply with a flag —
            // Destructive where apply is Reversible, and it earns a recovery
            // because it removes the entry.
            GitOperation::PopStash {
                entry: StashSelector::new("stash@{0}").unwrap(),
                expected_oid: oid('a'),
            },
            RiskLevel::Destructive,
            vec![Precondition::CleanWorktree],
            vec![],
            RecoveryStrategy::RecreateStashEntry {
                at: oid('a'),
                message: None,
            },
        ),
        plan(
            '0',
            // M4.31 (#84): one path, one whole-side choice, no content.
            GitOperation::ResolveConflict {
                path: WorktreePath::new("src/a.rs").unwrap(),
                resolution: git_vista_protocol::conflict::Resolution::TakeTheirs,
            },
            RiskLevel::Reversible,
            vec![],
            vec![],
            RecoveryStrategy::ConflictRecreatableWhileInProgress,
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
        // #227 (M2.20a): the vocabulary and its network classification landed
        // before #229/#230 wired any socket, and these golden plans pinned the
        // wire shape so those slices could not quietly change it while adding
        // execution. It held: M2.20c (#229) wired `exec_fetch` against exactly
        // the plan below — same risk class, same single precondition, same
        // empty ref-change list — and this fixture needed no regeneration.
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
        // The three stash operations (M3.24, #77). `PopStash` is absent from
        // the enum on purpose, so it is absent here too — see GitOperation's
        // comment for why a single row cannot tell the truth about a half-done
        // pop.
        //
        // The pinned shape under test is the SELECTOR/OID SPLIT: `entry` is a
        // positional `stash@{n}` and is what reaches git's argv; `expected_oid`
        // is the single oid authority and rides in the precondition. A codex
        // pre-write review proved `git stash drop <oid>` is not a command and
        // that one oid can occupy two slots at once, so a fixture that let the
        // oid become the entry would be pinning a design that cannot run.
        plan(
            'a',
            GitOperation::PushStash {
                message: Some(StashMessage::new("wip: half-done refactor").unwrap()),
                keep_index: false,
                include_untracked: true,
            },
            RiskLevel::Reversible,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::NotNeeded,
        ),
        // `apply_stash` keeps the entry, so its recovery is NotNeeded — the
        // stash is still in the drawer to apply again. `CleanWorktree` is the
        // load-bearing precondition: it is what makes `reset --hard` + `clean`
        // a provably safe abort, because a clean tree has nothing of the
        // user's to destroy.
        plan(
            'b',
            GitOperation::ApplyStash {
                entry: StashSelector::new("stash@{0}").unwrap(),
                expected_oid: oid('3'),
            },
            RiskLevel::Reversible,
            vec![Precondition::CleanWorktree],
            Vec::new(),
            RecoveryStrategy::NotNeeded,
        ),
        // `drop_stash` is Destructive on the same reasoning ForceDeleteBranch
        // is: commits become unreachable. RiskLevel is about what can be lost,
        // not about whether an undo exists — and the undo here restores the
        // CONTENT at a new stash@{0}, never the original position.
        plan(
            'c',
            GitOperation::DropStash {
                entry: StashSelector::new("stash@{2}").unwrap(),
                expected_oid: oid('4'),
            },
            RiskLevel::Destructive,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::RecreateStashEntry {
                at: oid('4'),
                message: Some(StashMessage::new("wip: half-done refactor").unwrap()),
            },
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

/// The brace-matched body that follows `marker`, exclusive of the braces.
/// `None` on a missing marker, no block, or an unbalanced one — every caller
/// reads that as a failed extraction rather than as "nothing to check".
fn braced_body<'a>(src: &'a str, marker: &str) -> Option<&'a str> {
    let start = src.find(marker)?;
    let rest = &src[start..];
    let open = rest.find('{')?;
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Everything from `//` to end of line removed.
///
/// Runs **before** [`braced_body`], not after: doc-comment prose contains
/// unmatched braces (`{name}` in a formatted example, `${…}`), and a brace
/// matcher that counted those would run the enum body off its own end and swallow
/// whatever followed. Safe to do line-wise here because the region scanned holds
/// no string literals.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The variant names declared at the top level of an enum body — the
/// identifier that opens each variant, ignoring its fields and any nesting
/// inside them. Expects comment-free input (see [`strip_line_comments`]).
fn top_level_variant_names(code: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut depth = 0i32;
    let mut expecting = true;
    let bytes: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            '{' | '(' | '[' => {
                depth += 1;
                i += 1;
            }
            '}' | ')' | ']' => {
                depth -= 1;
                i += 1;
            }
            ',' if depth == 0 => {
                expecting = true;
                i += 1;
            }
            c if depth == 0 && expecting && (c.is_alphabetic() || c == '_') => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_alphanumeric() || bytes[i] == '_') {
                    i += 1;
                }
                names.push(bytes[start..i].iter().collect::<String>());
                expecting = false;
            }
            _ => i += 1,
        }
    }
    names
}

/// serde's `rename_all = "snake_case"` applied to a PascalCase variant name.
fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The wire tags [`GitOperation`] *declares*, read back out of the enum's own
/// source — not a list copied beside it.
///
/// This is the `route_authz::registered_routes` trick: the check is only worth
/// anything if the "expected" side is re-derived from the real thing rather
/// than hand-maintained. A hand-copied literal here would stay in sync only by
/// somebody remembering, and the failure it is supposed to catch — a variant
/// added with dispatch arms (which the compiler *does* force) but no golden
/// plan — is exactly the case where nobody thought about the fixture.
fn declared_operation_tags() -> std::collections::BTreeSet<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plan.rs");
    let src = strip_line_comments(&std::fs::read_to_string(&path).expect("readable plan.rs"));
    let body = braced_body(&src, "pub enum GitOperation ")
        .expect("plan.rs still declares `pub enum GitOperation { .. }`");
    let names = top_level_variant_names(body);
    assert!(
        !names.is_empty(),
        "extracted no variants from GitOperation — the scanner no longer \
         recognises the enum's shape, which would let this whole test pass \
         vacuously"
    );
    names.iter().map(|n| snake_case(n)).collect()
}

/// Both answers for the extractor the census below rests on.
///
/// Without this, an extractor that returned the empty set — or that stopped at
/// the first variant — would make `golden_set_covers_every_operation_variant`
/// agree with itself and prove nothing.
#[test]
fn the_variant_extractor_reads_every_shape_and_only_variants() {
    let source = "pub enum Thing {\n\
                  /// A doc comment with a stray { brace and an Ident.\n\
                  Unit,\n\
                  Tuple(SomeType, Other),\n\
                  Struct { field: Map<String, Vec<u8>>, other: bool },\n\
                  Last,\n\
                  }\nfn after() { NotAVariant }\n";
    let stripped = strip_line_comments(source);
    let body = braced_body(&stripped, "pub enum Thing ").expect("body");
    assert_eq!(
        top_level_variant_names(body),
        vec!["Unit", "Tuple", "Struct", "Last"],
        "field types, doc-comment prose and code after the enum must not be \
         mistaken for variants"
    );
    // What that stray `{` in the doc comment is for: run the matcher over the
    // *unstripped* source and it never gets back to depth zero, so the body is
    // not found at all. `declared_operation_tags` would then panic on its
    // `expect` — loud, not silent — but only because the strip happens first
    // is the right body found. Ordering, pinned.
    assert!(
        braced_body(source, "pub enum Thing ").is_none(),
        "an unmatched brace in a comment no longer confuses the matcher, so \
         `strip_line_comments` running first has stopped being load-bearing — \
         check the claim in its doc comment before trusting it"
    );

    // And the balanced version, which is the quieter half of the same problem:
    // a comment whose braces match shifts the boundary without ever failing.
    let balanced = "pub enum Thing {\n\
                    /// prose with {a balanced} pair\n\
                    Only,\n\
                    }\nfn after() { NotAVariant }\n";
    assert_eq!(
        top_level_variant_names(
            braced_body(&strip_line_comments(balanced), "pub enum Thing ").expect("body")
        ),
        vec!["Only"]
    );
    // The quiet failure: braces that *match* leave the boundary alone, so
    // nothing errors — the prose inside the comment is simply counted as a
    // variant and the real one is missed. No exception, no empty set, just a
    // wrong answer that `declared_operation_tags` would compare against.
    assert_eq!(
        top_level_variant_names(braced_body(balanced, "pub enum Thing ").expect("body")),
        vec!["prose"],
        "if comment prose has stopped being mistaken for variants, \
         `strip_line_comments` may no longer be doing anything"
    );

    assert_eq!(snake_case("CreateBranch"), "create_branch");
    assert_eq!(snake_case("EmptyCommitOnBranch"), "empty_commit_on_branch");
    assert_eq!(snake_case("StageAll"), "stage_all");

    // Fail-closed on a shape it cannot read.
    assert!(braced_body("pub enum Thing;", "pub enum Thing ").is_none());
    assert!(braced_body("pub enum Thing { unterminated", "pub enum Thing ").is_none());
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
    // The expected side is re-read from `GitOperation`'s own source, not
    // copied beside it. The compiler already forces a new variant to grow a
    // dispatch arm in `planner.rs`; nothing forces it to grow a golden plan,
    // and a literal list here would have kept quiet about that at the old
    // count. Derived, the same omission fails with the missing tag named.
    let expected = declared_operation_tags();
    assert_eq!(
        tags, expected,
        "the golden plans and the GitOperation vocabulary disagree — a variant \
         in the enum with no golden plan (add one to `golden_plans()` and \
         regenerate the fixture with REGEN_GOLDEN=1), a golden plan for a \
         variant that no longer exists, or a deliberate wire rename that has \
         not reached the fixture"
    );
}

/// The [`Advisory`] wire shape (M4.32, #85), pinned per variant.
///
/// Not in `plan_v1.json`: the golden set allows one plan per `op` tag, and the
/// push slot is spoken for. But the shape still has to be pinned somewhere — a
/// retagged variant would change what a client reads without breaking any
/// round-trip test, and the advisory a client fails to recognise is exactly
/// the "you are force-pushing the default branch" one.
#[test]
fn every_advisory_variant_pins_its_own_wire_shape() {
    let cases = [
        (
            Advisory::DefaultBranchPush {
                branch: branch("main"),
                remote: RemoteName::new("origin").unwrap(),
            },
            serde_json::json!({
                "kind": "default_branch_push",
                "branch": "main",
                "remote": "origin",
            }),
        ),
        (
            Advisory::DefaultBranchUnknown {
                reason: "no refs/remotes/origin/HEAD".into(),
            },
            serde_json::json!({
                "kind": "default_branch_unknown",
                "reason": "no refs/remotes/origin/HEAD",
            }),
        ),
        (
            Advisory::RemoteHistoryReplaced {
                branch: branch("main"),
                remote: RemoteName::new("origin").unwrap(),
            },
            serde_json::json!({
                "kind": "remote_history_replaced",
                "branch": "main",
                "remote": "origin",
            }),
        ),
    ];

    for (advisory, expected) in cases {
        assert_eq!(
            serde_json::to_value(&advisory).unwrap(),
            expected,
            "advisory wire shape changed: {advisory:?}"
        );
        assert_eq!(
            serde_json::from_value::<Advisory>(expected).unwrap(),
            advisory,
            "advisory did not round-trip: {advisory:?}"
        );
    }
}

/// A stray key inside an advisory is a hard error, matching every other body
/// in this contract. An advisory that silently absorbed an unknown field would
/// let a newer server think it had warned a client that never saw the warning.
#[test]
fn an_advisory_with_an_unknown_field_is_refused() {
    let stray = serde_json::json!({
        "kind": "default_branch_push",
        "branch": "main",
        "remote": "origin",
        "severity": "high",
    });
    assert!(serde_json::from_value::<Advisory>(stray).is_err());
}
