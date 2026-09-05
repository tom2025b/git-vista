//! Explain Mode's parity test (M6.39, #92) — acceptance criterion 5.
//!
//! > *Tests compare explanation facts with plan facts.*
//!
//! Two halves, because the explanation has two kinds of fact and they need
//! different anchors.
//!
//! ## The plan half — anchored on the `Plan` itself
//!
//! [`ExplanationFact::Precondition`], `RefMoves`, `Recovery`, `Advisory` and
//! `Risk` all restate a field of the plan they came from, so the plan is the
//! anchor and the test runs in both directions:
//!
//! - **No fact without a plan field** — catches invention.
//! - **No plan field without a fact** — catches omission.
//!
//! ## The derived half — anchored on a hand-written table, deliberately
//!
//! `Worktree`, `Index` and `Remote` have **no plan field**. Computing the
//! expected value by calling `worktree_effect()` would assert `f(x) == f(x)`
//! and could not go red no matter what the 37 match arms said — the exact
//! shape of the standing caution *"never assert a mapping by calling the
//! function that defines it"*.
//!
//! So [`EFFECTS`] below is a second, independent statement of the same
//! mapping, written from what each git verb does rather than copied from the
//! match. Yes, that duplicates it. **The duplication is the test**; without it
//! the derived half is decoration.
//!
//! Its own vacuity is guarded too: the table must cover every operation
//! exactly once, name no operation that does not exist, and exercise every
//! variant of all three effect enums — a table of thirty-seven `Untouched`
//! rows would agree with an accessor stubbed the same way, and
//! [`effects_table_exercises_every_variant_of_every_effect_enum`] is what
//! stops that from passing for either of them.
//!
//! ## The fixture corpus
//!
//! `tests/fixtures/plan_v1.json` — the committed wire form already maintained
//! by `plan_golden.rs`, one plan per variant of the closed vocabulary. Reusing
//! it rather than hand-rolling 37 more plans means these tests read the same
//! shapes production emits, and a new operation lands in both tests at once.
//! Two synthetic plans are added for cases the corpus cannot carry: an
//! advisory-bearing plan (the fixture has none) and the second
//! `StageDirection`.

use git_vista_protocol::{
    Advisory, BranchName, ExplanationFact, GitOperation, IndexEffect, NetworkNeed, Plan,
    RemoteName, StageDirection, Topic, WorktreeEffect,
};

const FIXTURE: &str = include_str!("fixtures/plan_v1.json");

/// The index effect a row expects. Almost every operation determines it by
/// which variant it is; [`GitOperation::StageSelection`] determines it from
/// one of its own fields, and a row that had to pick one direction would be
/// wrong about the other.
#[derive(Debug, Clone, Copy)]
enum ExpectedIndex {
    Always(IndexEffect),
    ByStageDirection {
        stage: IndexEffect,
        unstage: IndexEffect,
    },
}

use ExpectedIndex::{Always, ByStageDirection};
use IndexEffect as I;
use NetworkNeed as N;
use WorktreeEffect as W;

/// **The independent mapping.** One row per operation, keyed by the `op` tag
/// serde writes — a handle that exists without asking any classifier
/// anything.
///
/// Written from what the git verb does, not read off the match arms. Where
/// the two disagree, that disagreement is the finding.
const EFFECTS: &[(&str, WorktreeEffect, ExpectedIndex, NetworkNeed)] = &[
    // Ref-only work: a pointer moves, no file is opened, nothing is staged.
    (
        "create_branch",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    (
        "delete_branch",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    (
        "force_delete_branch",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    (
        "restore_branch",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    ("create_tag", W::Untouched, Always(I::Untouched), N::Local),
    (
        "delete_local_tag",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    // Dropping a stash entry deletes a ref. It does not put anything back —
    // which is the whole substance of #514.
    ("drop_stash", W::Untouched, Always(I::Untouched), N::Local),
    // Committing writes the index's tree into an object and moves a ref. The
    // files on disk are already what is being committed.
    (
        "commit_on_head",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    (
        "empty_commit_on_branch",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    ("amend_commit", W::Untouched, Always(I::Untouched), N::Local),
    // Remote round trips. Objects and remote-tracking refs are written;
    // neither lives in the working tree, which is why a fetch is the safe
    // half of a pull.
    ("push_branch", W::Untouched, Always(I::Untouched), N::Remote),
    (
        "fetch_remote",
        W::Untouched,
        Always(I::Untouched),
        N::Remote,
    ),
    ("push_tag", W::Untouched, Always(I::Untouched), N::Remote),
    (
        "delete_remote_tag",
        W::Untouched,
        Always(I::Untouched),
        N::Remote,
    ),
    // Index-only verbs.
    (
        "stage_all",
        W::Untouched,
        Always(I::EntriesStaged),
        N::Local,
    ),
    (
        "unstage_all",
        W::Untouched,
        Always(I::EntriesUnstaged),
        N::Local,
    ),
    (
        "stage_selection",
        W::Untouched,
        ByStageDirection {
            stage: I::EntriesStaged,
            unstage: I::EntriesUnstaged,
        },
        N::Local,
    ),
    // Worktree writes that cannot conflict.
    (
        "checkout_branch",
        W::FilesRewritten,
        Always(I::Rebuilt),
        N::Local,
    ),
    (
        "sequence_abort",
        W::FilesRewritten,
        Always(I::Rebuilt),
        N::Local,
    ),
    (
        "reset_test_repo",
        W::FilesRewritten,
        Always(I::Rebuilt),
        N::Local,
    ),
    // `git stash push` saves the changes by taking them off disk, and resets
    // the index to HEAD in the same step.
    (
        "push_stash",
        W::FilesRewritten,
        Always(I::Rebuilt),
        N::Local,
    ),
    // Conflict resolution writes the chosen side and stages it.
    (
        "resolve_conflict",
        W::FilesRewritten,
        Always(I::StagesResolved),
        N::Local,
    ),
    (
        "resolve_conflict_content",
        W::FilesRewritten,
        Always(I::StagesResolved),
        N::Local,
    ),
    // `git checkout -- <paths>` overwrites each named path from the index;
    // the index itself is the source, not the target.
    (
        "discard_tracked_paths",
        W::FilesRewritten,
        Always(I::Untouched),
        N::Local,
    ),
    // The one operation that removes files rather than rewriting them.
    (
        "delete_untracked_paths",
        W::FilesRemoved,
        Always(I::Untouched),
        N::Local,
    ),
    // M11.05 (#550). Written from the operation's description, not from
    // reading `effects.rs`: `git worktree remove` deletes a whole working
    // tree — but a DIFFERENT one, a linked sibling this row is not asking
    // about. This repository's own tree and index are neither read nor
    // changed, and the two census reads plus the removal spawn never touch a
    // remote.
    (
        "remove_worktree",
        W::Untouched,
        Always(I::Untouched),
        N::Local,
    ),
    // Everything that runs a merge in git's sense: files are rewritten and
    // the operation can stop part-way with markers on disk.
    ("merge_branch", W::MayConflict, Always(I::Rebuilt), N::Local),
    (
        "rebase_onto_base",
        W::MayConflict,
        Always(I::Rebuilt),
        N::Local,
    ),
    ("cherry_pick", W::MayConflict, Always(I::Rebuilt), N::Local),
    (
        "cherry_pick_merge",
        W::MayConflict,
        Always(I::Rebuilt),
        N::Local,
    ),
    (
        "revert_commit",
        W::MayConflict,
        Always(I::Rebuilt),
        N::Local,
    ),
    ("revert_merge", W::MayConflict, Always(I::Rebuilt), N::Local),
    (
        "sequence_continue",
        W::MayConflict,
        Always(I::Rebuilt),
        N::Local,
    ),
    (
        "sequence_skip",
        W::MayConflict,
        Always(I::Rebuilt),
        N::Local,
    ),
    // A pull is a fetch plus an integration: the fetch half makes it remote,
    // the integration half makes it able to conflict.
    ("pull_branch", W::MayConflict, Always(I::Rebuilt), N::Remote),
    // `git stash apply` runs without `--index`, so a clean apply leaves the
    // index alone and the work arrives unstaged; only a conflicting apply
    // writes unmerged stages.
    (
        "apply_stash",
        W::MayConflict,
        Always(I::MayGainConflictStages),
        N::Local,
    ),
    // `git stash branch` restores the index as well as the worktree, and the
    // executor's own doc records that it can still conflict.
    (
        "branch_from_stash",
        W::MayConflict,
        Always(I::Rebuilt),
        N::Local,
    ),
    // The conditional pair: hard reset when the branch is checked out,
    // `git branch -f` when it is not.
    (
        "reset_branch",
        W::RewrittenIfCheckedOut,
        Always(I::RebuiltIfCheckedOut),
        N::Local,
    ),
];

/// The `op` tag serde writes for this operation. An identity handle obtained
/// without consulting any classifier — which is what lets the table below be
/// independent of the matches it checks.
fn op_name(op: &GitOperation) -> String {
    serde_json::to_value(op).expect("operation serializes")["op"]
        .as_str()
        .expect("operation is internally tagged on `op`")
        .to_string()
}

/// The committed golden corpus: one plan per operation variant.
fn corpus() -> Vec<Plan> {
    serde_json::from_str(FIXTURE).expect("golden plan fixture deserializes")
}

/// A plan carrying advisories. The golden fixture has none — it pins the push
/// shape production actually emits, which earns no advisory — so the omission
/// direction would never see an [`Advisory`] without this.
fn plan_with_advisories() -> Plan {
    let mut p = corpus()
        .into_iter()
        .find(|p| matches!(p.operation, GitOperation::PushBranch { .. }))
        .expect("corpus has a push");
    p.advisories = vec![
        Advisory::DefaultBranchPush {
            branch: BranchName::new("main").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
        },
        Advisory::DefaultBranchUnknown {
            reason: "no refs/remotes/origin/HEAD".to_string(),
        },
    ];
    p
}

/// The golden corpus carries one `StageSelection` and therefore one
/// direction. This is the other one.
fn plan_with_unstage_selection() -> Plan {
    let mut p = corpus()
        .into_iter()
        .find(|p| matches!(p.operation, GitOperation::StageSelection { .. }))
        .expect("corpus has a stage selection");
    match &mut p.operation {
        GitOperation::StageSelection { direction, .. } => {
            *direction = match direction {
                StageDirection::Stage => StageDirection::Unstage,
                StageDirection::Unstage => StageDirection::Stage,
            };
        }
        _ => unreachable!("just matched"),
    }
    p
}

fn every_plan() -> Vec<Plan> {
    let mut all = corpus();
    all.push(plan_with_advisories());
    all.push(plan_with_unstage_selection());
    all
}

// ---------------------------------------------------------------------------
// The table's own integrity, checked before it is trusted to check anything
// ---------------------------------------------------------------------------

#[test]
fn effects_table_covers_every_operation_exactly_once() {
    let corpus = corpus();
    let mut names: Vec<String> = corpus.iter().map(|p| op_name(&p.operation)).collect();
    names.sort();

    let mut rows: Vec<String> = EFFECTS.iter().map(|(n, ..)| n.to_string()).collect();
    rows.sort();

    let deduped = {
        let mut d = rows.clone();
        d.dedup();
        d
    };
    assert_eq!(rows, deduped, "the effects table names an operation twice");

    assert_eq!(
        rows, names,
        "the effects table and the operation vocabulary have drifted — a new \
         operation needs a row here as well as a match arm, and a row naming \
         no operation is a typo the compiler cannot see"
    );
}

#[test]
fn effects_table_exercises_every_variant_of_every_effect_enum() {
    // The anti-vacuity guard. A table of thirty-seven identical rows would
    // agree with an accessor stubbed the same way, and every assertion below
    // would pass while proving nothing. Naming each variant explicitly also
    // means adding one to either enum fails here until the table uses it.
    for want in [
        W::Untouched,
        W::FilesRewritten,
        W::FilesRemoved,
        W::MayConflict,
        W::RewrittenIfCheckedOut,
    ] {
        assert!(
            EFFECTS.iter().any(|(_, w, ..)| *w == want),
            "no row exercises WorktreeEffect::{want:?}"
        );
    }

    for want in [
        I::Untouched,
        I::EntriesStaged,
        I::EntriesUnstaged,
        I::StagesResolved,
        I::Rebuilt,
        I::MayGainConflictStages,
        I::RebuiltIfCheckedOut,
    ] {
        assert!(
            EFFECTS.iter().any(|(_, _, i, _)| match i {
                Always(v) => *v == want,
                ByStageDirection { stage, unstage } => *stage == want || *unstage == want,
            }),
            "no row exercises IndexEffect::{want:?}"
        );
    }

    for want in [N::Local, N::Remote] {
        assert!(
            EFFECTS.iter().any(|(.., n)| *n == want),
            "no row exercises NetworkNeed::{want:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The derived half
// ---------------------------------------------------------------------------

#[test]
fn every_derived_effect_matches_the_independent_table() {
    for plan in every_plan() {
        let name = op_name(&plan.operation);
        let (_, want_worktree, want_index, want_remote) = EFFECTS
            .iter()
            .find(|(n, ..)| *n == name)
            .unwrap_or_else(|| panic!("no table row for {name}"));

        let want_index = match want_index {
            Always(v) => *v,
            ByStageDirection { stage, unstage } => match &plan.operation {
                GitOperation::StageSelection { direction, .. } => match direction {
                    StageDirection::Stage => *stage,
                    StageDirection::Unstage => *unstage,
                },
                other => panic!("{other:?} has a direction-keyed row but no direction"),
            },
        };

        let explanation = git_vista_protocol::explain(&plan);
        assert_eq!(
            explanation.facts_for(Topic::IndexAndWorktree),
            [
                ExplanationFact::Worktree(*want_worktree),
                ExplanationFact::Index(want_index),
            ],
            "{name}: the explanation's worktree/index facts disagree with the \
             independent table"
        );

        assert_eq!(
            explanation.facts_for(Topic::Remote),
            [ExplanationFact::Remote(*want_remote)],
            "{name}: the explanation's remote fact disagrees with the \
             independent table"
        );
    }
}

// ---------------------------------------------------------------------------
// The plan half — both directions
// ---------------------------------------------------------------------------

#[test]
fn no_fact_without_a_plan_field() {
    for plan in every_plan() {
        let name = op_name(&plan.operation);
        let explanation = git_vista_protocol::explain(&plan);

        for fact in explanation.all_facts() {
            match fact {
                ExplanationFact::Precondition(p) => assert!(
                    plan.preconditions.contains(p),
                    "{name}: explanation invented precondition {p:?}"
                ),
                ExplanationFact::RefMoves(r) => assert!(
                    plan.expected_ref_changes.contains(r),
                    "{name}: explanation invented ref change {r:?}"
                ),
                ExplanationFact::Recovery(r) => assert_eq!(
                    *r, plan.recovery,
                    "{name}: explanation states a recovery the plan does not"
                ),
                ExplanationFact::Advisory(a) => assert!(
                    plan.advisories.contains(a),
                    "{name}: explanation invented advisory {a:?}"
                ),
                ExplanationFact::Risk(l) => assert_eq!(
                    *l, plan.risk,
                    "{name}: explanation states a risk the plan does not"
                ),
                // Derived facts have no plan field to trace to. They are
                // checked against the independent table above; asserting
                // anything about them here would only re-derive them.
                ExplanationFact::Worktree(_)
                | ExplanationFact::Index(_)
                | ExplanationFact::Remote(_) => {}
            }
        }
    }
}

#[test]
fn no_plan_field_without_a_fact() {
    for plan in every_plan() {
        let name = op_name(&plan.operation);
        let explanation = git_vista_protocol::explain(&plan);
        let facts: Vec<&ExplanationFact> = explanation.all_facts().collect();

        for p in &plan.preconditions {
            assert!(
                facts
                    .iter()
                    .any(|f| **f == ExplanationFact::Precondition(p.clone())),
                "{name}: explanation omits precondition {p:?}"
            );
        }
        for r in &plan.expected_ref_changes {
            assert!(
                facts
                    .iter()
                    .any(|f| **f == ExplanationFact::RefMoves(r.clone())),
                "{name}: explanation omits ref change {r:?}"
            );
        }
        for a in &plan.advisories {
            assert!(
                facts
                    .iter()
                    .any(|f| **f == ExplanationFact::Advisory(a.clone())),
                "{name}: explanation omits advisory {a:?}"
            );
        }
        assert!(
            facts
                .iter()
                .any(|f| **f == ExplanationFact::Recovery(plan.recovery.clone())),
            "{name}: explanation omits the recovery strategy — including \
             NotNeeded, which is emitted deliberately so this check needs no \
             carve-out"
        );
        assert!(
            facts
                .iter()
                .any(|f| **f == ExplanationFact::Risk(plan.risk)),
            "{name}: explanation omits the risk level"
        );
    }
}

#[test]
fn the_plan_half_is_not_vacuous() {
    // Every assertion in the two directions above is a loop over a
    // collection. If the corpus happened to carry no preconditions, no ref
    // changes and no advisories, both tests would pass over empty loops and
    // report nothing. Count what is actually being compared.
    let plans = every_plan();
    let preconditions: usize = plans.iter().map(|p| p.preconditions.len()).sum();
    let ref_changes: usize = plans.iter().map(|p| p.expected_ref_changes.len()).sum();
    let advisories: usize = plans.iter().map(|p| p.advisories.len()).sum();

    assert!(
        preconditions > 0,
        "no plan in the corpus carries a precondition — the parity loops are empty"
    );
    assert!(
        ref_changes > 0,
        "no plan in the corpus carries a ref change — the parity loops are empty"
    );
    assert!(
        advisories > 0,
        "no plan in the corpus carries an advisory — the parity loops are empty"
    );
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

#[test]
fn every_operation_gets_all_six_sections_in_order() {
    for plan in every_plan() {
        let name = op_name(&plan.operation);
        let topics: Vec<Topic> = git_vista_protocol::explain(&plan)
            .sections
            .iter()
            .map(|s| s.topic)
            .collect();
        assert_eq!(
            topics,
            [
                Topic::MustBeTrueFirst,
                Topic::WhatMoves,
                Topic::IndexAndWorktree,
                Topic::Remote,
                Topic::HowToUndo,
                Topic::WorthKnowing,
            ],
            "{name}: the explanation's shape changed between operations — an \
             empty section is emitted, never hidden"
        );
    }
}

#[test]
fn worth_knowing_is_never_empty() {
    // `Plan::risk` is a plain field rather than an Option, so this section
    // always carries at least the risk level. That is what settles the design
    // question about collapsing it by default: it can never open on a blank.
    for plan in every_plan() {
        let name = op_name(&plan.operation);
        let explanation = git_vista_protocol::explain(&plan);
        assert!(
            matches!(
                explanation.facts_for(Topic::WorthKnowing).first(),
                Some(ExplanationFact::Risk(_))
            ),
            "{name}: WorthKnowing does not lead with the risk level"
        );
    }
}
