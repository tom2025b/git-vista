//! Explain Mode (M6.39, #92) — a plan's own explanation, derived from the
//! plan.
//!
//! Git-Vista already refuses to run a write until it has built a [`Plan`] and
//! the user has approved it. That plan is not a string of shell: it is a typed
//! object saying what must be true first, which refs move, how risky it is,
//! and how to get back. This module renders the *same typed facts the server
//! is about to enforce* into a structure a viewer can read out loud.
//!
//! ## The one rule the whole design serves
//!
//! **If the plan says a precondition exists, the explanation says so. If it
//! does not, the explanation cannot invent one.**
//!
//! Everything below follows from that:
//!
//! - [`explain`] is a pure function of `&Plan`. No endpoint, no argv, no
//!   `String` input — the shape acceptance criterion 1 forbids never reaches
//!   here.
//! - An [`ExplanationFact`] carries the plan's **own typed value**. It does
//!   not restate it in prose and it does not summarise it. That is what turns
//!   criterion 5 from a judgement call into a mechanical check, and it is what
//!   makes translation a rendering concern rather than a rewrite: no English
//!   exists below the viewer.
//!
//! ## Why the protocol crate rather than the viewer
//!
//! `cargo test` never compiles the wasm viewer. Deciding what a plan *means*
//! inside `viewer.rs` would pin criterion 5 with nothing but a green gate —
//! the lesson #432 paid for, and the reason `features/conflicts/markers.rs` is
//! a framework-free core with host tests rather than logic in the view. `Plan`
//! and [`GitOperation`](crate::GitOperation) already live here, and both
//! server and client can compute the same explanation from the same input, so
//! there is nothing to keep in sync.
//!
//! ## Nothing new crosses the wire
//!
//! [`Explanation`] deliberately does **not** derive `Serialize`. The viewer
//! already holds the `Plan` and calls [`explain`] locally; a serialized
//! explanation would be a second copy of facts the plan already carries, and
//! the first thing to drift from it.

use crate::effects::{network_need_for_operation, IndexEffect, NetworkNeed, WorktreeEffect};
use crate::plan::{Advisory, Plan, Precondition, RecoveryStrategy, RefChange, RiskLevel};

/// One heading in the explanation. The six are fixed and always present, in
/// this order — see [`explain`] for why an empty section is emitted rather
/// than hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    /// The plan's preconditions: what the repository must look like for this
    /// to be admitted at all.
    MustBeTrueFirst,
    /// The plan's expected ref changes: which pointers move, and from where
    /// to where.
    WhatMoves,
    /// Derived from the operation: what happens to the working tree and the
    /// index.
    IndexAndWorktree,
    /// Derived from the operation: whether this reaches a remote.
    Remote,
    /// The plan's recovery strategy.
    HowToUndo,
    /// The plan's risk level and any advisories.
    WorthKnowing,
}

/// A single statement in an explanation, carrying the plan's own typed value.
///
/// Never a `String`, never a paraphrase. A renderer turns one of these into a
/// sentence; a different renderer turns it into a different language's
/// sentence. Neither can change what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplanationFact {
    /// Straight from [`Plan::preconditions`].
    Precondition(Precondition),
    /// Straight from [`Plan::expected_ref_changes`]. Names a ref the graph
    /// already draws, which is how criterion 3 is satisfied without a new
    /// glossary subsystem.
    RefMoves(RefChange),
    /// Derived: [`crate::GitOperation::worktree_effect`].
    Worktree(WorktreeEffect),
    /// Derived: [`crate::GitOperation::index_effect`].
    Index(IndexEffect),
    /// Derived: [`network_need_for_operation`].
    Remote(NetworkNeed),
    /// Straight from [`Plan::recovery`].
    Recovery(RecoveryStrategy),
    /// Straight from [`Plan::advisories`].
    Advisory(Advisory),
    /// Straight from [`Plan::risk`].
    Risk(RiskLevel),
}

/// One collapsible section of the explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub topic: Topic,
    pub facts: Vec<ExplanationFact>,
}

/// A plan's explanation: six sections, always in the same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    pub sections: Vec<Section>,
}

impl Explanation {
    /// The facts under one topic. `None` is impossible for a value built by
    /// [`explain`] — every topic is always present — so this returns a slice
    /// and an absent topic reads as no facts.
    pub fn facts_for(&self, topic: Topic) -> &[ExplanationFact] {
        self.sections
            .iter()
            .find(|s| s.topic == topic)
            .map(|s| s.facts.as_slice())
            .unwrap_or(&[])
    }

    /// Every fact, in section order. The flat form the parity test walks.
    pub fn all_facts(&self) -> impl Iterator<Item = &ExplanationFact> {
        self.sections.iter().flat_map(|s| s.facts.iter())
    }
}

/// Explain a plan.
///
/// # Why every section is emitted, even when empty
///
/// An operation with no preconditions gets a `MustBeTrueFirst` section with no
/// facts, not a missing section. Two reasons, in order of weight:
///
/// 1. **"Nothing must be true first" is itself the teaching sentence.** A
///    section that vanishes says nothing; an empty one says the check was made
///    and came back empty — the same `Obs`/`Observed` distinction this crate
///    draws everywhere else, and the reason
///    [`Advisory::DefaultBranchUnknown`] exists at all.
/// 2. The panel keeps one shape across all 37 operations. A layout that
///    changes between operations makes the reader re-find each heading, which
///    is a poor trade for hiding one blank line.
///
/// `WorthKnowing` can never be empty in practice — [`Plan::risk`] is a plain
/// field, not an `Option`, so [`ExplanationFact::Risk`] is always there. The
/// design note that asked whether it should start collapsed for that reason
/// therefore does not apply: it starts expanded like the rest.
///
/// # Why recovery is always emitted, including `NotNeeded`
///
/// Skipping [`RecoveryStrategy::NotNeeded`] would force the parity test to
/// carry a carve-out ("recovery may be absent if and only if it is
/// `NotNeeded`"), and a carve-out is somewhere a real omission can hide.
/// Emitting it keeps recovery a 1:1 mapping with nothing to check
/// conditionally — and "nothing to undo, because nothing here needs undoing"
/// is a better sentence than a missing heading.
pub fn explain(plan: &Plan) -> Explanation {
    let op = &plan.operation;

    Explanation {
        sections: vec![
            Section {
                topic: Topic::MustBeTrueFirst,
                facts: plan
                    .preconditions
                    .iter()
                    .cloned()
                    .map(ExplanationFact::Precondition)
                    .collect(),
            },
            Section {
                topic: Topic::WhatMoves,
                facts: plan
                    .expected_ref_changes
                    .iter()
                    .cloned()
                    .map(ExplanationFact::RefMoves)
                    .collect(),
            },
            Section {
                topic: Topic::IndexAndWorktree,
                // Worktree before index: the files are what the reader can
                // see, and the index is the part that needs explaining in
                // terms of them.
                facts: vec![
                    ExplanationFact::Worktree(op.worktree_effect()),
                    ExplanationFact::Index(op.index_effect()),
                ],
            },
            Section {
                topic: Topic::Remote,
                facts: vec![ExplanationFact::Remote(network_need_for_operation(op))],
            },
            Section {
                topic: Topic::HowToUndo,
                facts: vec![ExplanationFact::Recovery(plan.recovery.clone())],
            },
            Section {
                topic: Topic::WorthKnowing,
                // Risk first: it is the one fact present for every operation,
                // so leading with it keeps the section from ever opening on a
                // blank.
                facts: std::iter::once(ExplanationFact::Risk(plan.risk))
                    .chain(
                        plan.advisories
                            .iter()
                            .cloned()
                            .map(ExplanationFact::Advisory),
                    )
                    .collect(),
            },
        ],
    }
}
