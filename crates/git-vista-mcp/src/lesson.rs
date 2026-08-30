//! `get_lesson` — a read tool that turns a plan into structured teaching data
//! (M12/CLOUD-4, #450; ADR 0093).
//!
//! # Structured lesson DATA, not HTML
//!
//! This is the placement decision the issue settles: rendering taste (how a
//! precondition becomes a sentence, which language, which markup) belongs to
//! a viewer — the artifact/board pipeline, `teacher-thing`, `decksmith`,
//! whatever renders next. It does not belong in a Rust MCP server whose every
//! other tool on this surface returns a typed DTO. [`Lesson`] below carries
//! the plan's own typed values, one-to-one with
//! [`git_vista_protocol::ExplanationFact`] — no English, no markup, no
//! rendering choice made on its behalf.
//!
//! # One source with the app (#92 composition)
//!
//! [`git_vista_protocol::explain`] is the single function that turns a
//! [`git_vista_protocol::Plan`] into an [`git_vista_protocol::Explanation`] —
//! the exact function `git-vista`'s Explain Mode panel calls. This tool calls
//! nothing else: [`to_lesson`] is a structural 1:1 mapping from
//! [`git_vista_protocol::ExplanationFact`] to [`LessonFact`], never inventing
//! or dropping a fact, so the lesson a caller of this tool sees and the
//! explanation the app's panel shows cannot drift — they are two renderings
//! of one call.
//!
//! # No network call
//!
//! Every other tool in this crate reaches the running `git-vista-server` over
//! HTTP. This one does not: a `plan` object already reflects live repository
//! state (it was built by a prior `plan_*` tool's `POST /api/plan`, which
//! evaluated every precondition against the live repository at build time),
//! so re-deriving its lesson is a pure, local, offline computation over the
//! bytes the caller already holds — exactly the same computation
//! `execute_tool`'s `execute_plan` performs on the same `plan` object before
//! it ever sends the HTTP request. That is also why this tool composes with
//! a `git-vista-fixtures` broken repository exactly as readily as a real one
//! (acceptance criterion 4): nothing here depends on how the `Plan` value was
//! produced, only on what it contains.

use git_vista_protocol::{
    Advisory, Explanation, ExplanationFact, IndexEffect, NetworkNeed, Plan, Precondition,
    RecoveryStrategy, RefChange, RiskLevel, Topic, WorktreeEffect,
};
use serde::Serialize;

use crate::plan_tools::{exposure_of, Exposure};
use crate::tools::ToolError;

/// The `get_lesson` half of `tools/list`.
pub(crate) fn lesson_tool_catalog() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "name": "get_lesson",
        "description": "Explain a plan as structured teaching data: the same typed facts \
                        (preconditions, ref moves, worktree/index effects, remote need, \
                        recovery strategy, risk and advisories) the app's own Explain Mode \
                        panel renders, computed by the identical git_vista_protocol::explain \
                        function so the two surfaces cannot drift. Returns data, never prose \
                        or HTML — a renderer turns each fact into a sentence, this tool is not \
                        that renderer. Pass the exact `plan` object a plan_* tool call \
                        returned; this tool is read-only and makes no network call — it \
                        explains the plan you already have, it does not build or submit one.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "plan": {
                    "type": "object",
                    "description": "The exact `plan` object returned by a prior plan_* tool \
                                    call."
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        }
    })]
}

/// One heading in the lesson — the wire mirror of
/// [`git_vista_protocol::Topic`]. A plain re-listing, not a reinterpretation:
/// six variants there (`explain.rs`'s `pub enum Topic`), six names here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LessonTopic {
    MustBeTrueFirst,
    WhatMoves,
    IndexAndWorktree,
    Remote,
    HowToUndo,
    WorthKnowing,
}

/// One statement in the lesson — the wire mirror of
/// [`git_vista_protocol::ExplanationFact`]. Every variant carries the exact
/// typed value [`ExplanationFact`] carries; `value` is never a `String`
/// pulled from anywhere else, so there is nothing here for the mapping in
/// [`to_lesson`] to paraphrase.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum LessonFact {
    Precondition(Precondition),
    RefMoves(RefChange),
    Worktree(WorktreeEffect),
    Index(IndexEffect),
    Remote(NetworkNeed),
    Recovery(RecoveryStrategy),
    Advisory(Advisory),
    Risk(RiskLevel),
}

/// One collapsible section — the wire mirror of
/// [`git_vista_protocol::Section`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LessonSection {
    pub topic: LessonTopic,
    pub facts: Vec<LessonFact>,
}

/// A plan's lesson: the wire mirror of [`Explanation`] — six sections,
/// always in the same order, always all present (see [`git_vista_protocol::explain`]'s
/// doc for why an empty section is emitted rather than hidden).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Lesson {
    pub sections: Vec<LessonSection>,
}

/// [`Topic`] to [`LessonTopic`]. Exhaustive, no wildcard: a topic added to
/// the protocol's fixed six fails this match until it is given a wire name.
fn lesson_topic(topic: Topic) -> LessonTopic {
    match topic {
        Topic::MustBeTrueFirst => LessonTopic::MustBeTrueFirst,
        Topic::WhatMoves => LessonTopic::WhatMoves,
        Topic::IndexAndWorktree => LessonTopic::IndexAndWorktree,
        Topic::Remote => LessonTopic::Remote,
        Topic::HowToUndo => LessonTopic::HowToUndo,
        Topic::WorthKnowing => LessonTopic::WorthKnowing,
    }
}

/// [`ExplanationFact`] to [`LessonFact`]. Exhaustive, no wildcard, and every
/// arm carries its source value through unchanged — the mechanical half of
/// "a lesson never contains a fact the repository did not carry": there is
/// no arm here that could invent one.
fn lesson_fact(fact: &ExplanationFact) -> LessonFact {
    match fact {
        ExplanationFact::Precondition(p) => LessonFact::Precondition(p.clone()),
        ExplanationFact::RefMoves(r) => LessonFact::RefMoves(r.clone()),
        ExplanationFact::Worktree(w) => LessonFact::Worktree(*w),
        ExplanationFact::Index(i) => LessonFact::Index(*i),
        ExplanationFact::Remote(n) => LessonFact::Remote(*n),
        ExplanationFact::Recovery(r) => LessonFact::Recovery(r.clone()),
        ExplanationFact::Advisory(a) => LessonFact::Advisory(a.clone()),
        ExplanationFact::Risk(l) => LessonFact::Risk(*l),
    }
}

/// [`Explanation`] to [`Lesson`] — the whole of this tool's domain logic.
/// Every section and every fact travels across unchanged; only the shape
/// changes from a Rust value with no `Serialize` (deliberately, per
/// [`git_vista_protocol::explain`]'s module doc: nothing derived from a live
/// `Plan` should cross a wire the plan itself did not) to one this crate can
/// hand back as a `tools/call` result.
pub(crate) fn to_lesson(explanation: &Explanation) -> Lesson {
    Lesson {
        sections: explanation
            .sections
            .iter()
            .map(|s| LessonSection {
                topic: lesson_topic(s.topic),
                facts: s.facts.iter().map(lesson_fact).collect(),
            })
            .collect(),
    }
}

/// The exclusion list, enforced on this surface too.
///
/// # Why this is not a guard for an impossible state
///
/// `get_lesson`'s `plan` argument is **caller-supplied JSON**, not a value
/// this process built. Nothing upstream inspects it: `tools::call_tool`
/// dispatches `"get_lesson"` straight to [`get_lesson`], and
/// `tools::reject_undeclared_arguments` only compares argument *names*
/// against the schema — this tool's `plan` property is declared
/// `{"type": "object"}` with no `properties` block, so that walk returns
/// early and never reads the operation. And unlike `plan_tools`, this tool
/// makes no request, so the server-side re-validation `execute_plan` relies
/// on never happens either.
///
/// That was measured, not argued: before this check existed, a hand-built
/// `Plan` carrying `GitOperation::ResolveConflict` returned a full
/// six-section lesson from [`get_lesson`] — the #84 conflict-resolution
/// exclusion, explained by the very surface that refuses to plan it.
///
/// So the same classification `plan_tools::check_exposure` applies when
/// BUILDING a plan is applied here when EXPLAINING one. It is the identical
/// [`exposure_of`] table — one source, not a second copy that could drift —
/// and it covers #84 (conflict resolution), #77 (the stash drawer), #153
/// ([`git_vista_protocol::GitOperation::ResetTestRepo`]) and the sequence
/// controls without re-listing any of them here.
fn refuse_unexposed_operation(plan: &Plan) -> Result<(), ToolError> {
    match exposure_of(&plan.operation) {
        Exposure::Tool(_) => Ok(()),
        Exposure::Excluded(reason) => Err(ToolError::Execution(format!(
            "`get_lesson` will not explain an operation that is deliberately not \
             available through MCP: {reason}"
        ))),
    }
}

/// Run the `get_lesson` tool: parse the given `plan` argument, explain it
/// locally (no network call — see the module doc), and return its lesson.
pub(crate) fn get_lesson(args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let plan_value = args
        .get("plan")
        .ok_or_else(|| ToolError::Execution("missing required argument `plan`".to_string()))?;
    let plan: Plan = serde_json::from_value(plan_value.clone())
        .map_err(|e| ToolError::Execution(format!("`plan` is not a valid Plan: {e}")))?;
    refuse_unexposed_operation(&plan)?;

    let explanation = git_vista_protocol::explain(&plan);
    let lesson = to_lesson(&explanation);
    serde_json::to_value(&lesson)
        .map_err(|e| ToolError::Execution(format!("could not encode the lesson: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{
        BranchName, CommitOid, GenerationToken, GitOperation, OperationHash, RefName, RefState,
        RemoteName, RepositoryToken, UnixSeconds, WorktreePath, WorktreeToken,
    };

    fn oid(byte: u8) -> CommitOid {
        CommitOid::new(format!("{byte:02x}").repeat(20)).unwrap()
    }

    /// Deliberately populated on every axis: a precondition, a ref change, an
    /// advisory, and a non-default recovery — so every branch of
    /// [`lesson_fact`] is exercised by at least one plan below, not just the
    /// derived worktree/index/remote facts every plan always carries.
    fn rich_plan() -> Plan {
        Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("7").unwrap(),
            operation: GitOperation::PushBranch {
                branch: BranchName::new("main").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
                set_upstream: false,
                force: git_vista_protocol::ForcePublish::None,
            },
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_753_300_000),
            expires_at: UnixSeconds(1_753_300_300),
            risk: RiskLevel::Remote,
            preconditions: vec![Precondition::RefAt {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                oid: oid(1),
            }],
            expected_ref_changes: vec![RefChange {
                ref_name: RefName::new("refs/remotes/origin/main").unwrap(),
                before: RefState::At(oid(1)),
                after: RefState::At(oid(2)),
            }],
            recovery: RecoveryStrategy::NotNeeded,
            advisories: vec![Advisory::DefaultBranchPush {
                branch: BranchName::new("main").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
            }],
        }
    }

    fn stage_all_plan() -> Plan {
        Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("1").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("b".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1),
            expires_at: UnixSeconds(300),
            risk: RiskLevel::Safe,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
            advisories: Vec::new(),
        }
    }

    fn delete_untracked_plan() -> Plan {
        Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("1").unwrap(),
            operation: GitOperation::DeleteUntrackedPaths {
                paths: vec![WorktreePath::new("scratch.txt").unwrap()],
            },
            operation_hash: OperationHash::new("c".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1),
            expires_at: UnixSeconds(300),
            risk: RiskLevel::Destructive,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            recovery: RecoveryStrategy::Irrecoverable,
            advisories: Vec::new(),
        }
    }

    /// The independent half of the parity check. Written by hand from what
    /// each [`ExplanationFact`] variant IS, not by calling [`lesson_fact`] —
    /// the house rule against asserting a mapping by calling the function
    /// that defines it. Returns the JSON `(kind, value)` pair a faithful
    /// mapping must produce for a given fact.
    fn expected_kind_and_value(fact: &ExplanationFact) -> (&'static str, serde_json::Value) {
        match fact {
            ExplanationFact::Precondition(p) => ("precondition", serde_json::to_value(p).unwrap()),
            ExplanationFact::RefMoves(r) => ("ref_moves", serde_json::to_value(r).unwrap()),
            ExplanationFact::Worktree(w) => ("worktree", serde_json::to_value(w).unwrap()),
            ExplanationFact::Index(i) => ("index", serde_json::to_value(i).unwrap()),
            ExplanationFact::Remote(n) => ("remote", serde_json::to_value(n).unwrap()),
            ExplanationFact::Recovery(r) => ("recovery", serde_json::to_value(r).unwrap()),
            ExplanationFact::Advisory(a) => ("advisory", serde_json::to_value(a).unwrap()),
            ExplanationFact::Risk(l) => ("risk", serde_json::to_value(l).unwrap()),
        }
    }

    /// **The mutation-sensitive parity check.** For every plan and every fact
    /// `explain()` produces, the lesson's serialized JSON must carry exactly
    /// the tag and value [`expected_kind_and_value`] independently says it
    /// should — proving [`to_lesson`] neither invents nor drops a fact, and
    /// neither mislabels nor reshapes one, across every topic in order.
    #[test]
    fn every_lesson_fact_matches_the_explanation_it_was_built_from() {
        for plan in [rich_plan(), stage_all_plan(), delete_untracked_plan()] {
            let explanation = git_vista_protocol::explain(&plan);
            let lesson = to_lesson(&explanation);
            let lesson_json = serde_json::to_value(&lesson).unwrap();
            let sections = lesson_json["sections"].as_array().unwrap();

            assert_eq!(
                sections.len(),
                explanation.sections.len(),
                "section count drifted from explain()'s own six"
            );

            for (section_json, section) in sections.iter().zip(&explanation.sections) {
                let facts_json = section_json["facts"].as_array().unwrap();
                assert_eq!(
                    facts_json.len(),
                    section.facts.len(),
                    "{:?}: lesson fact count drifted from the explanation",
                    section.topic
                );
                for (fact_json, fact) in facts_json.iter().zip(&section.facts) {
                    let (want_kind, want_value) = expected_kind_and_value(fact);
                    assert_eq!(
                        fact_json["kind"], want_kind,
                        "{:?}: lesson fact tagged wrong: {fact:?}",
                        section.topic
                    );
                    assert_eq!(
                        fact_json["value"], want_value,
                        "{:?}: lesson fact value drifted from the plan's own: {fact:?}",
                        section.topic
                    );
                }
            }
        }
    }

    /// The anti-vacuity guard: the plans above must actually exercise every
    /// [`LessonFact`] variant, or the parity check above would pass by
    /// covering only a subset and prove nothing about the rest.
    #[test]
    fn the_parity_plans_exercise_every_lesson_fact_variant() {
        let mut kinds = std::collections::BTreeSet::new();
        for plan in [rich_plan(), stage_all_plan(), delete_untracked_plan()] {
            for fact in git_vista_protocol::explain(&plan).all_facts() {
                kinds.insert(expected_kind_and_value(fact).0);
            }
        }
        for want in [
            "precondition",
            "ref_moves",
            "worktree",
            "index",
            "remote",
            "recovery",
            "advisory",
            "risk",
        ] {
            assert!(
                kinds.contains(want),
                "no plan above ever produces a {want} fact"
            );
        }
    }

    #[test]
    fn topics_serialize_to_the_six_fixed_snake_case_names_in_order() {
        let explanation = git_vista_protocol::explain(&stage_all_plan());
        let lesson = to_lesson(&explanation);
        let names: Vec<serde_json::Value> = serde_json::to_value(&lesson).unwrap()["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["topic"].clone())
            .collect();
        assert_eq!(
            names,
            [
                "must_be_true_first",
                "what_moves",
                "index_and_worktree",
                "remote",
                "how_to_undo",
                "worth_knowing"
            ]
        );
    }

    #[test]
    fn get_lesson_returns_a_lesson_for_a_valid_plan() {
        let plan = rich_plan();
        let args = serde_json::json!({ "plan": plan });
        let result = get_lesson(&args).unwrap();
        assert_eq!(result["sections"].as_array().unwrap().len(), 6);
        // Spot-check one fact the rich plan is built to carry, all the way
        // through the tool's own entry point rather than just `to_lesson`.
        let worth_knowing = result["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["topic"] == "worth_knowing")
            .unwrap();
        let kinds: Vec<&str> = worth_knowing["facts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["risk", "advisory"]);
    }

    #[test]
    fn a_missing_plan_argument_is_a_local_execution_error() {
        match get_lesson(&serde_json::json!({})) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("plan")),
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_plan_argument_is_a_local_execution_error() {
        match get_lesson(&serde_json::json!({ "plan": { "not": "a real plan" } })) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("not a valid Plan")),
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // (a) The exclusion list, enforced here too — #84, #77, #153, ADR 0046.
    //
    // These are not hypothetical: the probe recorded in
    // `refuse_unexposed_operation`'s doc comment ran on this branch before
    // the gate existed and got a full six-section lesson back for a
    // `ResolveConflict` plan.
    // ---------------------------------------------------------------------

    /// One plan per *reason* the MCP surface excludes an operation, so the
    /// refusal is proven for each family rather than for one lucky variant.
    fn excluded_plans() -> Vec<(&'static str, Plan)> {
        let base = |op: GitOperation| Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("1").unwrap(),
            operation: op,
            operation_hash: OperationHash::new("d".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1),
            expires_at: UnixSeconds(300),
            risk: RiskLevel::Destructive,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
            advisories: Vec::new(),
        };
        vec![
            (
                // #84 / ADR 0064 d7: whole-side conflict resolution.
                "resolve_conflict",
                base(GitOperation::ResolveConflict {
                    path: WorktreePath::new("src/main.rs").unwrap(),
                    resolution: git_vista_protocol::conflict::Resolution::TakeOurs,
                }),
            ),
            (
                // #77: the stash drawer, addressed by a positional selector.
                "push_stash",
                base(GitOperation::PushStash {
                    message: None,
                    keep_index: false,
                    include_untracked: false,
                }),
            ),
            (
                // #153: the test-harness fixture restore.
                "reset_test_repo",
                base(GitOperation::ResetTestRepo),
            ),
            (
                // The sequence controls: the same unseen-content judgement.
                "sequence_abort",
                base(GitOperation::SequenceAbort),
            ),
        ]
    }

    #[test]
    fn get_lesson_refuses_an_operation_the_plan_surface_does_not_expose() {
        for (label, plan) in excluded_plans() {
            match get_lesson(&serde_json::json!({ "plan": plan })) {
                Err(ToolError::Execution(msg)) => {
                    assert!(
                        msg.contains("deliberately not available through MCP"),
                        "{label}: refused, but not as an exclusion: {msg}"
                    );
                    // The refusal carries `exposure_of`'s own stated reason
                    // through, rather than a generic "no". Checked against
                    // the classification's payload, not against a copy of
                    // the wording pasted into this test.
                    let Exposure::Excluded(reason) = exposure_of(&plan.operation) else {
                        panic!("{label} is not classified Excluded — fixture is stale");
                    };
                    assert!(
                        msg.contains(reason),
                        "{label}: refusal dropped exposure_of's reason\n  got: {msg}\n want: {reason}"
                    );
                }
                other => panic!("{label}: expected an exclusion refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_exclusion_gate_still_explains_every_exposed_operation() {
        // The other half, and the reason the test above cannot pass by
        // refusing everything: every representative plan names an operation
        // `exposure_of` classifies `Tool`, and every one still gets a
        // six-section lesson.
        for (label, plan) in representative_plans() {
            assert!(
                matches!(exposure_of(&plan.operation), Exposure::Tool(_)),
                "{label}: fixture is not an exposed operation"
            );
            let lesson = get_lesson(&serde_json::json!({ "plan": plan }))
                .unwrap_or_else(|e| panic!("{label}: exposed operation refused: {e:?}"));
            assert_eq!(lesson["sections"].as_array().unwrap().len(), 6, "{label}");
        }
    }

    // ---------------------------------------------------------------------
    // (b) Grafted from #560: the plan-anchored fidelity check.
    //
    // `every_lesson_fact_matches_the_explanation_it_was_built_from` above
    // anchors on `explain()`'s own output, so it proves "to_lesson mirrors
    // explain" — a real property, but not #450's invariant. THIS test
    // anchors on the `Plan`'s own serialized fields instead, in BOTH
    // directions: no lesson fact that the plan does not carry, and no plan
    // field that the lesson drops.
    // ---------------------------------------------------------------------

    fn base_plan(operation: GitOperation, risk: RiskLevel, recovery: RecoveryStrategy) -> Plan {
        Plan {
            repository: RepositoryToken::new("11111111-1111-5111-8111-111111111111").unwrap(),
            worktree: WorktreeToken::new("22222222-2222-5222-8222-222222222222").unwrap(),
            generation: GenerationToken::new("12345678901234567890").unwrap(),
            operation,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_753_300_000),
            expires_at: UnixSeconds(1_753_300_300),
            risk,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            advisories: Vec::new(),
            recovery,
        }
    }

    /// `create_branch`: a `RefAbsent` precondition, one ref change.
    fn create_branch_plan() -> Plan {
        let mut p = base_plan(
            GitOperation::CreateBranch {
                name: BranchName::new("feature/idea").unwrap(),
                at: oid(1),
            },
            RiskLevel::Reversible,
            RecoveryStrategy::DeleteCreatedBranch {
                name: BranchName::new("feature/idea").unwrap(),
            },
        );
        p.preconditions = vec![Precondition::RefAbsent {
            ref_name: RefName::new("refs/heads/feature/idea").unwrap(),
        }];
        p.expected_ref_changes = vec![RefChange {
            ref_name: RefName::new("refs/heads/feature/idea").unwrap(),
            before: RefState::Absent,
            after: RefState::At(oid(1)),
        }];
        p
    }

    /// `push_branch` carrying TWO advisories — content nothing about the
    /// operation alone implies, so this plan can only be explained correctly
    /// if the lesson comes from THIS `Plan` and not from a value
    /// reconstructed from the operation.
    fn push_branch_plan_with_advisories() -> Plan {
        let mut p = base_plan(
            GitOperation::PushBranch {
                branch: BranchName::new("main").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
                set_upstream: false,
                force: git_vista_protocol::ForcePublish::None,
            },
            RiskLevel::Remote,
            RecoveryStrategy::Irrecoverable,
        );
        p.preconditions = vec![
            Precondition::RemoteConfigured {
                remote: RemoteName::new("origin").unwrap(),
            },
            Precondition::RefExists {
                ref_name: RefName::new("refs/heads/main").unwrap(),
            },
        ];
        p.expected_ref_changes = vec![RefChange {
            ref_name: RefName::new("refs/remotes/origin/main").unwrap(),
            before: RefState::At(oid(4)),
            after: RefState::At(oid(2)),
        }];
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

    /// `force_delete_branch`: destructive, `RecreateBranch` recovery.
    fn force_delete_branch_plan() -> Plan {
        let mut p = base_plan(
            GitOperation::ForceDeleteBranch {
                branch: BranchName::new("feature/abandoned").unwrap(),
            },
            RiskLevel::Destructive,
            RecoveryStrategy::RecreateBranch {
                name: BranchName::new("feature/abandoned").unwrap(),
                at: oid(6),
            },
        );
        p.preconditions = vec![Precondition::RefAt {
            ref_name: RefName::new("refs/heads/feature/abandoned").unwrap(),
            oid: oid(6),
        }];
        p.expected_ref_changes = vec![RefChange {
            ref_name: RefName::new("refs/heads/feature/abandoned").unwrap(),
            before: RefState::At(oid(6)),
            after: RefState::Absent,
        }];
        p
    }

    /// `fetch_remote`: `Safe` risk but `NetworkNeed::Remote` — the case
    /// proving risk and reach are independent axes.
    fn fetch_remote_plan() -> Plan {
        let mut p = base_plan(
            GitOperation::FetchRemote {
                remote: RemoteName::new("origin").unwrap(),
            },
            RiskLevel::Safe,
            RecoveryStrategy::NotNeeded,
        );
        p.preconditions = vec![Precondition::RemoteConfigured {
            remote: RemoteName::new("origin").unwrap(),
        }];
        p
    }

    /// `merge_branch`: the only fixture with a non-`Untouched` worktree
    /// effect and a `Computed` ref change.
    fn merge_branch_plan() -> Plan {
        let mut p = base_plan(
            GitOperation::MergeBranch {
                branch: BranchName::new("feature/idea").unwrap(),
            },
            RiskLevel::Reversible,
            RecoveryStrategy::ResetRef {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                to: oid(2),
            },
        );
        p.preconditions = vec![
            Precondition::BranchCheckedOut {
                branch: BranchName::new("main").unwrap(),
            },
            Precondition::RefAt {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                oid: oid(2),
            },
        ];
        p.expected_ref_changes = vec![RefChange {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            before: RefState::At(oid(2)),
            after: RefState::Computed,
        }];
        p
    }

    fn unstage_all_plan() -> Plan {
        base_plan(
            GitOperation::UnstageAll,
            RiskLevel::Safe,
            RecoveryStrategy::NotNeeded,
        )
    }

    /// The fixture battery the plan-anchored checks below run over. Keyed by
    /// the same label [`DERIVED`] uses, so a fixture added without a table
    /// row fails loudly instead of being skipped.
    fn representative_plans() -> Vec<(&'static str, Plan)> {
        vec![
            ("create_branch", create_branch_plan()),
            ("push_branch", push_branch_plan_with_advisories()),
            ("force_delete_branch", force_delete_branch_plan()),
            ("fetch_remote", fetch_remote_plan()),
            ("merge_branch", merge_branch_plan()),
            ("stage_all", stage_all_plan()),
            ("unstage_all", unstage_all_plan()),
        ]
    }

    fn section<'a>(lesson: &'a serde_json::Value, topic: &str) -> &'a [serde_json::Value] {
        lesson["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["topic"] == topic)
            .unwrap_or_else(|| panic!("no {topic} section in {lesson}"))["facts"]
            .as_array()
            .unwrap()
    }

    /// The `value` payloads of every fact adjacently tagged `kind`.
    fn facts_of_kind<'a>(facts: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
        facts
            .iter()
            .filter(|f| f["kind"] == kind)
            .map(|f| &f["value"])
            .collect()
    }

    /// # Exactly which of `Plan`'s fields this covers — five of twelve
    ///
    /// `Plan` has **twelve** fields. This test checks the **five** that
    /// `explain` copies verbatim out of the plan — `preconditions`,
    /// `expected_ref_changes`, `advisories`, `recovery` and `risk` — in both
    /// directions.
    ///
    /// The other seven are out of scope here, and naming them is the point:
    /// a test called "no plan field lacks a lesson fact" would be claiming
    /// twelve while checking five, which is how a reader concludes a gap is
    /// closed when it is not.
    ///
    /// - `operation` reaches a lesson only *derived*, as the worktree, index
    ///   and remote facts. That is
    ///   [`derived_facts_match_the_independent_table`]'s job, against a
    ///   hand-written table, not this test's.
    /// - `repository`, `worktree`, `generation`, `operation_hash`,
    ///   `issued_at` and `expires_at` are the plan's envelope — identity,
    ///   integrity and expiry. `explain` never reads one, so no
    ///   `ExplanationFact` variant can carry one and there is nothing here
    ///   to compare them against.
    ///
    /// # Order is part of the contract, so these are sequences, not sets
    ///
    /// A lesson is teaching material read top to bottom, and the plan's own
    /// ordering is its reading order: `merge_branch` states
    /// `BranchCheckedOut` before `RefAt` because which branch you are on
    /// comes before where it points. `explain` already treats fact order as
    /// meaningful and says why in its own doc — risk leads `WorthKnowing`,
    /// worktree precedes index — so a mirror that preserved membership while
    /// shuffling sequence would be re-ordering a lesson's sentences behind
    /// the reader's back.
    ///
    /// A `contains` check cannot see that, and did not: adding `.rev()` to
    /// `plan.preconditions.iter()` inside `explain` left all thirteen lesson
    /// tests green. These are `assert_eq!` over sequences for that reason,
    /// and `.rev()` on either `plan.preconditions` or `plan.advisories` now
    /// turns this test red.
    ///
    /// **The ref-change leg of that claim is not yet falsifiable, and saying
    /// so is the point.** `.rev()` on `plan.expected_ref_changes.iter()`
    /// still passes, because no plan `git-vista-server`'s planner can build
    /// carries more than one ref change — every arm of `planner::shape`
    /// produces at most a single `RefChange`, so reversing that list is a
    /// no-op and no fixture here can have two. The assertion is written as a
    /// sequence anyway: it is the same contract, it costs nothing, and it
    /// starts biting the day an operation moves two refs. It is not
    /// evidence today.
    #[test]
    fn the_five_plan_authored_fields_reach_the_lesson_intact_and_in_the_plans_own_order() {
        for (label, plan) in representative_plans() {
            let lesson = get_lesson(&serde_json::json!({ "plan": plan })).unwrap();
            let plan_json = serde_json::to_value(&plan).unwrap();

            let must_be_true_first = section(&lesson, "must_be_true_first");
            let what_moves = section(&lesson, "what_moves");
            let how_to_undo = section(&lesson, "how_to_undo");
            let worth_knowing = section(&lesson, "worth_knowing");

            // Preconditions: both directions, in the plan's own order.
            let lesson_preconditions = facts_of_kind(must_be_true_first, "precondition");
            let plan_preconditions: Vec<&serde_json::Value> = plan_json["preconditions"]
                .as_array()
                .unwrap()
                .iter()
                .collect();
            assert_eq!(
                lesson_preconditions, plan_preconditions,
                "{label}: the lesson's preconditions are not the plan's own, in the \
                 plan's own order"
            );

            // Ref changes: both directions, in the plan's own order.
            let lesson_ref_moves = facts_of_kind(what_moves, "ref_moves");
            let plan_ref_changes: Vec<&serde_json::Value> = plan_json["expected_ref_changes"]
                .as_array()
                .unwrap()
                .iter()
                .collect();
            assert_eq!(
                lesson_ref_moves, plan_ref_changes,
                "{label}: the lesson's ref changes are not the plan's own, in the \
                 plan's own order"
            );

            // Advisories: both directions, in the plan's own order.
            let lesson_advisories = facts_of_kind(worth_knowing, "advisory");
            let plan_advisories: Vec<&serde_json::Value> =
                plan_json["advisories"].as_array().unwrap().iter().collect();
            assert_eq!(
                lesson_advisories, plan_advisories,
                "{label}: the lesson's advisories are not the plan's own, in the \
                 plan's own order"
            );

            // Recovery: exactly one fact, exactly the plan's own value.
            let lesson_recovery = facts_of_kind(how_to_undo, "recovery");
            assert_eq!(
                lesson_recovery.len(),
                1,
                "{label}: recovery must appear exactly once"
            );
            assert_eq!(
                lesson_recovery[0], &plan_json["recovery"],
                "{label}: lesson's recovery disagrees with the plan's"
            );

            // Risk: exactly one fact, exactly the plan's own value, leading
            // the section.
            let lesson_risk = facts_of_kind(worth_knowing, "risk");
            assert_eq!(
                lesson_risk.len(),
                1,
                "{label}: risk must appear exactly once"
            );
            assert_eq!(
                lesson_risk[0], &plan_json["risk"],
                "{label}: lesson's risk disagrees with the plan's"
            );
            assert!(
                worth_knowing[0]["kind"] == "risk",
                "{label}: worth_knowing must lead with risk"
            );
        }
    }

    #[test]
    fn the_plan_anchored_fidelity_check_is_not_vacuous() {
        // Every "both directions" comparison above is `[] == []` when no
        // fixture carries the field. If no representative plan has a
        // precondition, a ref change or an advisory, that test degenerates
        // into three assertions of `0 == 0` and proves nothing — including
        // nothing about order, since an empty sequence cannot be reversed.
        let plans: Vec<Plan> = representative_plans().into_iter().map(|(_, p)| p).collect();
        let preconditions: usize = plans.iter().map(|p| p.preconditions.len()).sum();
        let ref_changes: usize = plans.iter().map(|p| p.expected_ref_changes.len()).sum();
        let advisories: usize = plans.iter().map(|p| p.advisories.len()).sum();
        assert!(
            preconditions > 0,
            "no representative plan carries a precondition"
        );
        assert!(
            ref_changes > 0,
            "no representative plan carries a ref change"
        );
        assert!(advisories > 0, "no representative plan carries an advisory");
    }

    // ---------------------------------------------------------------------
    // (b) Grafted from #560: the derived third of the payload.
    //
    // `worktree`, `index` and `remote` are the three facts with NO plan
    // field to trace back to — `explain` derives them from the operation. So
    // they get a hand-written table instead: computing the expectation by
    // calling `worktree_effect()`/`index_effect()`/`network_need_for_operation`
    // would assert f(x) == f(x).
    // ---------------------------------------------------------------------

    /// `(label, worktree, index, remote)` — written by reading what each
    /// operation DOES, never by running the derivation.
    const DERIVED: &[(&str, &str, &str, &str)] = &[
        ("create_branch", "untouched", "untouched", "local"),
        ("push_branch", "untouched", "untouched", "remote"),
        ("force_delete_branch", "untouched", "untouched", "local"),
        ("fetch_remote", "untouched", "untouched", "remote"),
        ("merge_branch", "may_conflict", "rebuilt", "local"),
        ("stage_all", "untouched", "entries_staged", "local"),
        ("unstage_all", "untouched", "entries_unstaged", "local"),
    ];

    #[test]
    fn derived_facts_match_the_independent_table() {
        for (label, plan) in representative_plans() {
            let (_, want_worktree, want_index, want_remote) = DERIVED
                .iter()
                .find(|(t, ..)| *t == label)
                .unwrap_or_else(|| panic!("no DERIVED row for {label}"));
            let lesson = get_lesson(&serde_json::json!({ "plan": plan })).unwrap();
            let index_and_worktree = section(&lesson, "index_and_worktree");
            let remote = section(&lesson, "remote");

            assert_eq!(
                facts_of_kind(index_and_worktree, "worktree"),
                vec![&serde_json::json!(*want_worktree)],
                "{label}: worktree effect"
            );
            assert_eq!(
                facts_of_kind(index_and_worktree, "index"),
                vec![&serde_json::json!(*want_index)],
                "{label}: index effect"
            );
            assert_eq!(
                facts_of_kind(remote, "remote"),
                vec![&serde_json::json!(*want_remote)],
                "{label}: network need"
            );
        }
    }

    #[test]
    fn the_derived_table_exercises_every_variant_it_claims_to() {
        // Anti-vacuity: a table of seven identical rows would agree with a
        // wire mapping stubbed the same way. Every distinct label used above
        // must appear at least once, so a future edit dropping the last row
        // that exercises one fails loudly here rather than silently
        // narrowing the check.
        for want in ["untouched", "may_conflict"] {
            assert!(
                DERIVED.iter().any(|(_, w, ..)| *w == want),
                "no row has worktree {want}"
            );
        }
        for want in ["untouched", "rebuilt", "entries_staged", "entries_unstaged"] {
            assert!(
                DERIVED.iter().any(|(_, _, i, _)| *i == want),
                "no row has index {want}"
            );
        }
        for want in ["local", "remote"] {
            assert!(
                DERIVED.iter().any(|(_, _, _, n)| *n == want),
                "no row has remote {want}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // (c) The replacement for `get_lesson_never_emits_html_or_a_bare_string`.
    //
    // That test asserted only that the serialized result contains no `'<'`.
    // An English sentence passes it. An empty payload passes it. A payload
    // of nulls passes it. This one asserts over the actual mechanism.
    // ---------------------------------------------------------------------

    /// Fact kinds whose `value` is a tagged OBJECT, with the tag field that
    /// object must carry. Straight from each type's own `#[serde(tag = ...)]`
    /// in `git-vista-protocol` (`RefChange` is a plain struct, so its
    /// required key is a field name).
    const OBJECT_FACTS: &[(&str, &str)] = &[
        ("precondition", "check"),
        ("ref_moves", "ref_name"),
        ("recovery", "strategy"),
        ("advisory", "kind"),
    ];

    /// Fact kinds whose `value` is a bare string, and the CLOSED set of
    /// strings each may be. Transcribed from the enums themselves —
    /// `WorktreeEffect`/`IndexEffect`/`NetworkNeed` in `effects.rs`,
    /// `RiskLevel` in `plan.rs`, all `rename_all = "snake_case"`.
    const SCALAR_FACT_VOCABULARY: &[(&str, &[&str])] = &[
        (
            "worktree",
            &[
                "untouched",
                "files_rewritten",
                "files_removed",
                "may_conflict",
                "rewritten_if_checked_out",
            ],
        ),
        (
            "index",
            &[
                "untouched",
                "entries_staged",
                "entries_unstaged",
                "stages_resolved",
                "rebuilt",
                "may_gain_conflict_stages",
                "rebuilt_if_checked_out",
            ],
        ),
        ("remote", &["local", "remote"]),
        ("risk", &["safe", "reversible", "destructive", "remote"]),
    ];

    #[test]
    fn every_lesson_fact_value_is_typed_data_never_prose() {
        let mut seen_kinds = std::collections::BTreeSet::new();
        for (label, plan) in representative_plans() {
            let lesson = get_lesson(&serde_json::json!({ "plan": plan })).unwrap();
            for section in lesson["sections"].as_array().unwrap() {
                for fact in section["facts"].as_array().unwrap() {
                    let kind = fact["kind"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{label}: fact has no `kind` tag: {fact}"));
                    let value = &fact["value"];
                    seen_kinds.insert(kind.to_string());

                    if let Some((_, tag)) = OBJECT_FACTS.iter().find(|(k, _)| *k == kind) {
                        let object = value.as_object().unwrap_or_else(|| {
                            panic!("{label}: `{kind}` value is not an object: {value}")
                        });
                        assert!(
                            object.contains_key(*tag),
                            "{label}: `{kind}` object has no `{tag}` discriminator: {value}"
                        );
                        continue;
                    }

                    let (_, vocabulary) = SCALAR_FACT_VOCABULARY
                        .iter()
                        .find(|(k, _)| *k == kind)
                        .unwrap_or_else(|| {
                            panic!(
                                "{label}: `{kind}` is not a classified fact kind — a new \
                                 LessonFact variant must be added to one of the two tables \
                                 above before this test can vouch for it"
                            )
                        });
                    let text = value.as_str().unwrap_or_else(|| {
                        panic!("{label}: `{kind}` value is not a string: {value}")
                    });
                    assert!(
                        vocabulary.contains(&text),
                        "{label}: `{kind}` value `{text}` is outside its closed vocabulary \
                         {vocabulary:?} — a rendered sentence or an invented label"
                    );
                }
            }
        }

        // Anti-vacuity, two ways. Without these the whole test passes over a
        // lesson with zero sections, or over one that only ever emits the
        // three derived scalars.
        for want in [
            "precondition",
            "ref_moves",
            "worktree",
            "index",
            "remote",
            "recovery",
            "advisory",
            "risk",
        ] {
            assert!(
                seen_kinds.contains(want),
                "no representative plan ever produced a `{want}` fact, so this test \
                 never checked one"
            );
        }
        assert_eq!(
            seen_kinds.len(),
            8,
            "an unclassified fact kind reached the wire: {seen_kinds:?}"
        );
    }

    // ---------------------------------------------------------------------
    // (c2) The other leg of #450's "never emits HTML or a bare string".
    //
    // `every_lesson_fact_value_is_typed_data_never_prose` above proves each
    // fact `value` has the right SHAPE — a tagged object carrying its
    // discriminator, or a scalar drawn from a closed vocabulary. It never
    // looks INSIDE an object, and it never looks at a key it was not told
    // about. So the criterion survived two breaks intact:
    //
    //   * markup or a rendered sentence in a string nested inside any
    //     `OBJECT_FACTS` payload, and
    //   * a rendered string added BESIDE the keys every other test reads —
    //     a `"heading"` on each section, a `"sentence"` on each fact. Every
    //     other test in this file reads only `topic`, `kind` and `value`, so
    //     an extra sibling key is invisible to all of them.
    //
    // Replacing the old `'<'`-free assertion with a shape check alone would
    // have left this criterion covered by nothing while reading as covered.
    // This test walks EVERY string a lesson carries, wherever it sits.
    // ---------------------------------------------------------------------

    /// Substrings that make a string markup rather than a value. Applied to
    /// every string in the payload with no exception — the declared
    /// free-text field below included, because #450 says no HTML, and being
    /// free text is not permission to emit tags or entities.
    const MARKUP_NEEDLES: &[&str] = &["<", ">", "&#", "&lt", "&gt", "&amp", "`", "**"];

    /// The ONLY strings a lesson may spell with whitespace in them, as
    /// `(fact kind, dotted path inside the fact object)`.
    ///
    /// `Advisory::DefaultBranchUnknown`'s `reason` is human prose by design
    /// — its own doc in `plan.rs` says it "is for a human reading the plan,
    /// never for a caller to match on" — and it is the *plan's* prose,
    /// carried through verbatim, not a sentence this tool composed.
    ///
    /// Everything else a lesson can carry is a machine token: a ref name, a
    /// branch name, a remote name, an oid, a worktree path, or a
    /// `snake_case` enum name. None of those can contain a space, which is
    /// what makes "contains whitespace" a mechanical stand-in for "reads as
    /// a rendered sentence" rather than a guess.
    const FREE_TEXT: &[(&str, &str)] = &[("advisory", "value.reason")];

    /// Every string reachable in `value`, paired with its dotted path below
    /// `path`. Recursive on purpose: a fact's payload nests (`ref_moves`
    /// carries two `RefState` objects of its own), and a check that stopped
    /// at the top level would be the same blind spot again one layer down.
    fn strings_under(value: &serde_json::Value, path: &str, out: &mut Vec<(String, String)>) {
        match value {
            serde_json::Value::String(s) => out.push((path.to_string(), s.clone())),
            serde_json::Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    strings_under(item, &format!("{path}[{i}]"), out);
                }
            }
            serde_json::Value::Object(fields) => {
                for (key, field) in fields {
                    strings_under(field, &format!("{path}.{key}"), out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn no_string_a_lesson_carries_is_markup_or_a_rendered_sentence() {
        let assert_no_markup = |label: &str, at: &str, text: &str| {
            for needle in MARKUP_NEEDLES {
                assert!(
                    !text.contains(needle),
                    "{label}: {at} carries markup ({needle:?}): {text:?} — a lesson is \
                     data, and a renderer is the only thing entitled to produce markup"
                );
            }
        };
        let assert_no_prose = |label: &str, at: &str, text: &str| {
            assert!(
                !text.chars().any(char::is_whitespace),
                "{label}: {at} contains whitespace, so it reads as a rendered sentence \
                 rather than a value: {text:?} — if it is genuinely free text the plan \
                 itself carries, declare it in FREE_TEXT and say why there"
            );
        };

        let mut checked = 0usize;
        let mut free_text_checked = 0usize;

        for (label, plan) in representative_plans() {
            let lesson = get_lesson(&serde_json::json!({ "plan": plan })).unwrap();

            // Anything hung off the lesson root other than `sections`.
            for (key, value) in lesson.as_object().unwrap() {
                if key == "sections" {
                    continue;
                }
                let mut found = Vec::new();
                strings_under(value, key, &mut found);
                for (path, text) in found {
                    assert_no_markup(label, &path, &text);
                    assert_no_prose(label, &path, &text);
                    checked += 1;
                }
            }

            for section in lesson["sections"].as_array().unwrap() {
                // Anything hung off a section other than `facts` — its
                // `topic`, and a rendered `"heading"` if one is ever added.
                for (key, value) in section.as_object().unwrap() {
                    if key == "facts" {
                        continue;
                    }
                    let mut found = Vec::new();
                    strings_under(value, key, &mut found);
                    for (path, text) in found {
                        let at = format!("section.{path}");
                        assert_no_markup(label, &at, &text);
                        assert_no_prose(label, &at, &text);
                        checked += 1;
                    }
                }

                // Every key of every fact — `kind`, `value`, and anything
                // else that ever appears beside them.
                for fact in section["facts"].as_array().unwrap() {
                    let kind = fact["kind"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{label}: fact has no `kind` tag: {fact}"));
                    for (key, value) in fact.as_object().unwrap() {
                        let mut found = Vec::new();
                        strings_under(value, key, &mut found);
                        for (path, text) in found {
                            let at = format!("{kind}.{path}");
                            assert_no_markup(label, &at, &text);
                            checked += 1;
                            if FREE_TEXT.contains(&(kind, path.as_str())) {
                                free_text_checked += 1;
                                continue;
                            }
                            assert_no_prose(label, &at, &text);
                        }
                    }
                }
            }
        }

        // Anti-vacuity, two ways. Without the first, a lesson of zero
        // sections passes this and proves nothing; without the second, a
        // FREE_TEXT entry could name a field no fixture ever reaches, so the
        // exemption would sit unexercised and unfalsifiable.
        assert!(
            checked >= 100,
            "only {checked} strings were inspected — the fixtures no longer produce a \
             payload large enough for this check to mean anything"
        );
        assert_eq!(
            free_text_checked, 1,
            "expected exactly one declared free-text string across the fixtures \
             (push_branch's DefaultBranchUnknown advisory `reason`), found \
             {free_text_checked} — either FREE_TEXT names a field nothing reaches, or \
             a fixture changed"
        );
    }
}
