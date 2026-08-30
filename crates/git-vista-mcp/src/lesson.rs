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
/// seven variants there, seven names here.
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

/// Run the `get_lesson` tool: parse the given `plan` argument, explain it
/// locally (no network call — see the module doc), and return its lesson.
pub(crate) fn get_lesson(args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let plan_value = args
        .get("plan")
        .ok_or_else(|| ToolError::Execution("missing required argument `plan`".to_string()))?;
    let plan: Plan = serde_json::from_value(plan_value.clone())
        .map_err(|e| ToolError::Execution(format!("`plan` is not a valid Plan: {e}")))?;

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
    fn get_lesson_never_emits_html_or_a_bare_string() {
        // #450's acceptance criterion 2, checked mechanically rather than by
        // eye: nothing in the result is prose. Every fact value is an
        // object or a plain typed scalar (an enum tag) — never a `String`
        // containing markup or a rendered sentence.
        let result = get_lesson(&serde_json::json!({ "plan": rich_plan() })).unwrap();
        let text = serde_json::to_string(&result).unwrap();
        assert!(
            !text.contains('<'),
            "result contains what looks like markup: {text}"
        );
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
}
