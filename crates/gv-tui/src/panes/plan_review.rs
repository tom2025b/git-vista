//! The review surface between a planned write and its execution (#460).
//!
//! The server already owns the meaning of a [`Plan`], and Explain Mode already
//! turns that plan into typed [`ExplanationFact`]s. This module is only the
//! terminal projection of those facts. [`project`] is deliberately a pure
//! function of `&Plan`; terminal state, HTTP responses, and the clock cannot
//! change what a received plan says.
//!
//! Approval has a second, separate invariant: the bytes received from
//! `POST /api/plan` are the bytes sent to `POST /api/execute-plan`. The parsed
//! plan is discarded after projection and key derivation, while
//! [`PlanApproval`] can only be minted by [`PlanReviewPane::approve`]. The
//! pane therefore contains no editable `Plan` at all.

use std::sync::Arc;

use git_vista_protocol::{
    explain, Advisory, ExplanationFact, IndexEffect, NetworkNeed, Plan, Precondition,
    RecoveryStrategy, RefState, RiskLevel, Topic, WorktreeEffect,
};

/// Visual emphasis for one projected row. Ratatui chooses the actual colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTone {
    Plain,
    Heading,
    Muted,
    Risk,
    Advisory,
    Error,
}

/// One logical terminal row produced from a plan fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRow {
    pub text: String,
    pub tone: RowTone,
}

/// Everything the terminal shows for a plan, before transient submission state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanProjection {
    pub rows: Vec<ReviewRow>,
}

/// Purely project one plan into terminal rows.
///
/// The six sections and their facts come from Explain Mode. This renderer does
/// not re-derive effects from [`git_vista_protocol::GitOperation`] and does not
/// invent a second explanation model.
pub fn project(plan: &Plan) -> PlanProjection {
    let mut rows = vec![
        row(RowTone::Muted, format!("generation: {}", plan.generation)),
        row(
            RowTone::Muted,
            format!(
                "issued: {}  expires: {} (server Unix seconds)",
                plan.issued_at.0, plan.expires_at.0
            ),
        ),
        row(
            RowTone::Muted,
            format!("operation hash: {}", plan.operation_hash),
        ),
    ];

    for section in explain(plan).sections {
        rows.push(row(RowTone::Plain, ""));
        rows.push(row(RowTone::Heading, heading(section.topic)));
        if section.facts.is_empty() {
            rows.push(row(RowTone::Muted, empty(section.topic)));
        } else {
            rows.extend(section.facts.iter().map(fact_row));
        }
    }

    PlanProjection { rows }
}

fn row(tone: RowTone, text: impl Into<String>) -> ReviewRow {
    ReviewRow {
        text: text.into(),
        tone,
    }
}

fn heading(topic: Topic) -> &'static str {
    match topic {
        Topic::MustBeTrueFirst => "Preconditions",
        Topic::WhatMoves => "Expected ref changes (before → after)",
        Topic::IndexAndWorktree => "Files and staging area",
        Topic::Remote => "Network",
        Topic::HowToUndo => "Recovery strategy",
        Topic::WorthKnowing => "Risk and advisories",
    }
}

fn empty(topic: Topic) -> &'static str {
    match topic {
        Topic::MustBeTrueFirst => "  none",
        Topic::WhatMoves => "  no refs move",
        Topic::IndexAndWorktree => "  no file or staging effects reported",
        Topic::Remote => "  no network effect reported",
        Topic::HowToUndo => "  no recovery fact reported",
        Topic::WorthKnowing => "  no risk or advisory fact reported",
    }
}

fn fact_row(fact: &ExplanationFact) -> ReviewRow {
    match fact {
        ExplanationFact::Precondition(value) => {
            row(RowTone::Plain, format!("  • {}", precondition(value)))
        }
        ExplanationFact::RefMoves(value) => row(
            RowTone::Plain,
            format!(
                "  • {}: {} → {}",
                value.ref_name,
                ref_state(&value.before),
                ref_state(&value.after)
            ),
        ),
        ExplanationFact::Worktree(value) => {
            row(RowTone::Plain, format!("  • {}", worktree(*value)))
        }
        ExplanationFact::Index(value) => row(RowTone::Plain, format!("  • {}", index(*value))),
        ExplanationFact::Remote(value) => row(RowTone::Plain, format!("  • {}", network(*value))),
        ExplanationFact::Recovery(value) => row(RowTone::Plain, format!("  • {}", recovery(value))),
        ExplanationFact::Advisory(value) => {
            row(RowTone::Advisory, format!("  ! {}", advisory(value)))
        }
        ExplanationFact::Risk(value) => row(RowTone::Risk, format!("  ! {}", risk(*value))),
    }
}

fn short(value: impl ToString) -> String {
    value.to_string().chars().take(7).collect()
}

fn precondition(value: &Precondition) -> String {
    match value {
        Precondition::RefAt { ref_name, oid } => {
            format!("{ref_name} is still at {}", short(oid))
        }
        Precondition::RefExists { ref_name } => format!("{ref_name} exists"),
        Precondition::RefAbsent { ref_name } => format!("{ref_name} does not exist"),
        Precondition::BranchCheckedOut { branch } => format!("{branch} is checked out"),
        Precondition::BranchNotCheckedOut { branch } => {
            format!("{branch} is not checked out")
        }
        Precondition::CleanWorktree => "working tree and staging area are clean".to_string(),
        Precondition::RemoteConfigured { remote } => format!("remote {remote} is configured"),
        Precondition::SeedRecorded => "the demo repository seed is recorded".to_string(),
    }
}

fn ref_state(value: &RefState) -> String {
    match value {
        RefState::Absent => "absent".to_string(),
        RefState::At(oid) => short(oid),
        RefState::Symbolic(name) => format!("symbolic {name}"),
        RefState::Computed => "new commit (computed on execution)".to_string(),
    }
}

fn worktree(value: WorktreeEffect) -> &'static str {
    match value {
        WorktreeEffect::Untouched => "working-tree files are untouched",
        WorktreeEffect::FilesRewritten => "tracked files are rewritten",
        WorktreeEffect::FilesRemoved => "files are removed from the working tree",
        WorktreeEffect::MayConflict => "tracked files may be left with conflicts",
        WorktreeEffect::RewrittenIfCheckedOut => {
            "working-tree files are rewritten only if the branch is checked out"
        }
    }
}

fn index(value: IndexEffect) -> &'static str {
    match value {
        IndexEffect::Untouched => "staging area is untouched",
        IndexEffect::EntriesStaged => "entries are staged",
        IndexEffect::EntriesUnstaged => "entries are unstaged",
        IndexEffect::StagesResolved => "conflict stages become one resolved entry",
        IndexEffect::Rebuilt => "staging area is rebuilt from the result",
        IndexEffect::MayGainConflictStages => "staging area may gain conflict stages",
        IndexEffect::RebuiltIfCheckedOut => {
            "staging area is rebuilt only if the branch is checked out"
        }
    }
}

fn network(value: NetworkNeed) -> &'static str {
    match value {
        NetworkNeed::Remote => "operation reaches a remote over the network",
        NetworkNeed::Local => "operation stays inside this local repository",
    }
}

fn recovery(value: &RecoveryStrategy) -> String {
    match value {
        RecoveryStrategy::NotNeeded => "no recovery is needed".to_string(),
        RecoveryStrategy::ResetRef { ref_name, to } => {
            format!("move {ref_name} back to {}", short(to))
        }
        RecoveryStrategy::RecreateBranch { name, at } => {
            format!("recreate branch {name} at {}", short(at))
        }
        RecoveryStrategy::DeleteCreatedBranch { name } => {
            format!("delete the newly created branch {name}")
        }
        RecoveryStrategy::RecreateTag { name, at } => {
            format!("recreate tag {name} at {}", short(at))
        }
        RecoveryStrategy::RecreateStashEntry { at, message } => match message {
            Some(message) => format!("recreate stash {} ({message})", short(at)),
            None => format!("recreate stash {}", short(at)),
        },
        RecoveryStrategy::DeleteCreatedTag { name } => {
            format!("delete the newly created tag {name}")
        }
        RecoveryStrategy::CheckoutPrevious { branch } => {
            format!("check branch {branch} back out")
        }
        RecoveryStrategy::RevertCommit { commit } => {
            format!("revert commit {} with a new commit", short(commit))
        }
        RecoveryStrategy::RecoverableIfStaged => {
            "content may be recoverable only if it was staged before".to_string()
        }
        RecoveryStrategy::ConflictRecreatableWhileInProgress => {
            "recreate the conflict while its operation remains in progress".to_string()
        }
        RecoveryStrategy::Irrecoverable => "Git-Vista offers no recovery".to_string(),
    }
}

fn advisory(value: &Advisory) -> String {
    match value {
        Advisory::DefaultBranchPush { branch, remote } => {
            format!("{branch} is {remote}'s default branch")
        }
        Advisory::DefaultBranchUnknown { reason } => {
            format!("default-branch status could not be determined: {reason}")
        }
        Advisory::RemoteHistoryReplaced { branch, remote } => {
            format!("{branch} on {remote} is replaced and cannot be restored remotely here")
        }
    }
}

fn risk(value: RiskLevel) -> &'static str {
    match value {
        RiskLevel::Safe => "SAFE — local state is not lost",
        RiskLevel::Reversible => "REVERSIBLE — the recovery strategy above provides a way back",
        RiskLevel::Destructive => "DESTRUCTIVE — state can become unreachable",
        RiskLevel::Remote => "REMOTE — effects leave this machine",
    }
}

/// The immutable approval body and its retry-safe idempotency key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanApproval {
    wire: Arc<[u8]>,
    key: String,
}

impl PlanApproval {
    pub(crate) fn body(&self) -> &[u8] {
        &self.wire
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

/// What the server established about an approval attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmissionOutcome {
    Executed(String),
    /// The server proved only that the reviewed plan can no longer execute.
    /// `/api/execute-plan` carries no typed reason that can distinguish an
    /// expired plan from a generation or precondition conflict.
    Stale,
    Refused {
        status: u16,
        message: String,
    },
    TransportFailed(String),
}

impl SubmissionOutcome {
    /// Interpret an HTTP response without turning its prose into guessed facts.
    pub fn from_response(status: u16, body: &[u8]) -> SubmissionOutcome {
        let message = String::from_utf8_lossy(body).trim().to_string();
        match status {
            200..=299 => SubmissionOutcome::Executed(message),
            // A generation mismatch, expiry, a failed live precondition, or
            // any other conflict means the reviewed plan is no longer
            // executable. The status proves that fact; the untyped body does
            // not prove which cause produced it.
            409 => SubmissionOutcome::Stale,
            _ => SubmissionOutcome::Refused { status, message },
        }
    }

    pub fn message(&self) -> String {
        match self {
            SubmissionOutcome::Executed(message) => message.clone(),
            SubmissionOutcome::Stale => {
                "Plan is stale. It was not executed. Build and review a new plan.".to_string()
            }
            SubmissionOutcome::Refused { status, message } => {
                format!("Plan was not executed (HTTP {status}): {message}")
            }
            SubmissionOutcome::TransportFailed(message) => {
                format!("Approval could not be delivered; execution is unknown: {message}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReviewState {
    AwaitingDecision,
    Submitting,
    Refused(SubmissionOutcome),
}

/// State for the modal pane. The plan and its original bytes never change.
#[derive(Clone, Debug)]
pub struct PlanReviewPane {
    wire: Arc<[u8]>,
    key: String,
    projection: PlanProjection,
    state: ReviewState,
    scroll: usize,
}

impl PlanReviewPane {
    /// Parse the exact bytes returned by `/api/plan` and retain them for approval.
    pub fn from_wire(wire: Vec<u8>) -> Result<PlanReviewPane, String> {
        let plan: Plan = serde_json::from_slice(&wire)
            .map_err(|error| format!("/api/plan did not return a valid Plan: {error}"))?;
        let projection = project(&plan);
        let key = format!("tui-{}-{}", plan.operation_hash.as_str(), plan.issued_at.0);
        Ok(PlanReviewPane {
            wire: Arc::from(wire),
            key,
            projection,
            state: ReviewState::AwaitingDecision,
            scroll: 0,
        })
    }

    /// Mint one approval from the preserved wire bytes. Repeated key presses
    /// while a submission is in flight cannot mint a second request.
    pub fn approve(&mut self) -> Option<PlanApproval> {
        if self.state != ReviewState::AwaitingDecision {
            return None;
        }
        self.state = ReviewState::Submitting;
        Some(PlanApproval {
            wire: Arc::clone(&self.wire),
            key: self.key.clone(),
        })
    }

    pub fn receive(&mut self, outcome: SubmissionOutcome) {
        self.state = ReviewState::Refused(outcome);
        self.scroll = 0;
    }

    pub fn is_submitting(&self) -> bool {
        self.state == ReviewState::Submitting
    }

    pub fn scroll(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.rows().len().saturating_sub(1));
    }

    pub fn offset(&self) -> usize {
        self.scroll
    }

    pub fn rows(&self) -> Vec<ReviewRow> {
        let mut rows = Vec::new();
        match &self.state {
            ReviewState::AwaitingDecision => {}
            ReviewState::Submitting => rows.push(row(
                RowTone::Advisory,
                "Submitting the exact reviewed plan; the server is re-validating it…",
            )),
            ReviewState::Refused(outcome) => rows.push(row(RowTone::Error, outcome.message())),
        }
        rows.extend(self.projection.rows.clone());
        rows
    }

    pub fn help(&self) -> &'static str {
        match self.state {
            ReviewState::AwaitingDecision => "a approve · Esc refuse · j/k scroll",
            ReviewState::Submitting => "waiting for server re-validation",
            ReviewState::Refused(_) => "Esc close · j/k scroll",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{
        BranchName, CommitOid, GenerationToken, GitOperation, OperationHash, RefChange, RefName,
        RemoteName, RepositoryToken, StashMessage, TagName, UnixSeconds, WorktreeToken,
    };

    fn oid(c: char) -> CommitOid {
        CommitOid::new(c.to_string().repeat(40)).unwrap()
    }

    fn plan() -> Plan {
        Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("worktree-1").unwrap(),
            generation: GenerationToken::new("generation-reviewed").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_788_365_000),
            expires_at: UnixSeconds(1_788_365_300),
            risk: RiskLevel::Destructive,
            preconditions: vec![
                Precondition::CleanWorktree,
                Precondition::RefAt {
                    ref_name: RefName::new("refs/heads/main").unwrap(),
                    oid: oid('1'),
                },
            ],
            expected_ref_changes: vec![RefChange {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                before: RefState::At(oid('1')),
                after: RefState::Computed,
            }],
            recovery: RecoveryStrategy::ResetRef {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                to: oid('1'),
            },
            advisories: vec![Advisory::DefaultBranchUnknown {
                reason: "remote HEAD was not available".to_string(),
            }],
        }
    }

    fn text(projection: &PlanProjection) -> String {
        projection
            .rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn every_fact_variant() -> Vec<ExplanationFact> {
        use ExplanationFact as Fact;
        let ref_name = || RefName::new("refs/heads/main").unwrap();
        let branch = || BranchName::new("main").unwrap();
        let mut facts = vec![
            Fact::Precondition(Precondition::RefAt {
                ref_name: ref_name(),
                oid: oid('1'),
            }),
            Fact::Precondition(Precondition::RefExists {
                ref_name: ref_name(),
            }),
            Fact::Precondition(Precondition::RefAbsent {
                ref_name: ref_name(),
            }),
            Fact::Precondition(Precondition::BranchCheckedOut { branch: branch() }),
            Fact::Precondition(Precondition::BranchNotCheckedOut { branch: branch() }),
            Fact::Precondition(Precondition::CleanWorktree),
            Fact::Precondition(Precondition::RemoteConfigured {
                remote: RemoteName::new("origin").unwrap(),
            }),
            Fact::Precondition(Precondition::SeedRecorded),
            Fact::Recovery(RecoveryStrategy::NotNeeded),
            Fact::Recovery(RecoveryStrategy::ResetRef {
                ref_name: ref_name(),
                to: oid('2'),
            }),
            Fact::Recovery(RecoveryStrategy::RecreateBranch {
                name: branch(),
                at: oid('3'),
            }),
            Fact::Recovery(RecoveryStrategy::DeleteCreatedBranch { name: branch() }),
            Fact::Recovery(RecoveryStrategy::RecreateTag {
                name: TagName::new("v1").unwrap(),
                at: oid('4'),
            }),
            Fact::Recovery(RecoveryStrategy::RecreateStashEntry {
                at: oid('5'),
                message: Some(StashMessage::new("work").unwrap()),
            }),
            Fact::Recovery(RecoveryStrategy::RecreateStashEntry {
                at: oid('5'),
                message: None,
            }),
            Fact::Recovery(RecoveryStrategy::DeleteCreatedTag {
                name: TagName::new("v2").unwrap(),
            }),
            Fact::Recovery(RecoveryStrategy::CheckoutPrevious { branch: branch() }),
            Fact::Recovery(RecoveryStrategy::RevertCommit { commit: oid('6') }),
            Fact::Recovery(RecoveryStrategy::RecoverableIfStaged),
            Fact::Recovery(RecoveryStrategy::ConflictRecreatableWhileInProgress),
            Fact::Recovery(RecoveryStrategy::Irrecoverable),
            Fact::Advisory(Advisory::DefaultBranchPush {
                branch: branch(),
                remote: RemoteName::new("origin").unwrap(),
            }),
            Fact::Advisory(Advisory::DefaultBranchUnknown {
                reason: "not observed".to_string(),
            }),
            Fact::Advisory(Advisory::RemoteHistoryReplaced {
                branch: branch(),
                remote: RemoteName::new("origin").unwrap(),
            }),
        ];
        for before in [
            RefState::Absent,
            RefState::At(oid('7')),
            RefState::Symbolic(ref_name()),
            RefState::Computed,
        ] {
            facts.push(Fact::RefMoves(RefChange {
                ref_name: ref_name(),
                before,
                after: RefState::At(oid('8')),
            }));
        }
        for effect in [
            WorktreeEffect::Untouched,
            WorktreeEffect::FilesRewritten,
            WorktreeEffect::FilesRemoved,
            WorktreeEffect::MayConflict,
            WorktreeEffect::RewrittenIfCheckedOut,
        ] {
            facts.push(Fact::Worktree(effect));
        }
        for effect in [
            IndexEffect::Untouched,
            IndexEffect::EntriesStaged,
            IndexEffect::EntriesUnstaged,
            IndexEffect::StagesResolved,
            IndexEffect::Rebuilt,
            IndexEffect::MayGainConflictStages,
            IndexEffect::RebuiltIfCheckedOut,
        ] {
            facts.push(Fact::Index(effect));
        }
        for need in [NetworkNeed::Remote, NetworkNeed::Local] {
            facts.push(Fact::Remote(need));
        }
        for level in [
            RiskLevel::Safe,
            RiskLevel::Reversible,
            RiskLevel::Destructive,
            RiskLevel::Remote,
        ] {
            facts.push(Fact::Risk(level));
        }
        facts
    }

    #[test]
    fn projection_contains_every_review_fact_and_staleness_field() {
        let rendered = text(&project(&plan()));
        for expected in [
            "generation-reviewed",
            "expires: 1788365300",
            &"a".repeat(64),
            "Preconditions",
            "working tree and staging area are clean",
            "refs/heads/main is still at 1111111",
            "Expected ref changes (before → after)",
            "refs/heads/main: 1111111 → new commit",
            "DESTRUCTIVE",
            "remote HEAD was not available",
            "Recovery strategy",
            "move refs/heads/main back to 1111111",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
    }

    #[test]
    fn projection_uses_explain_modes_six_sections_in_its_fixed_order() {
        let rendered = text(&project(&plan()));
        let headings = [
            "Preconditions",
            "Expected ref changes",
            "Files and staging area",
            "Network",
            "Recovery strategy",
            "Risk and advisories",
        ];
        let positions: Vec<_> = headings
            .iter()
            .map(|heading| rendered.find(heading).expect("heading is present"))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn every_explain_fact_variant_projects_to_a_nonempty_terminal_row() {
        let facts = every_fact_variant();
        assert_eq!(facts.len(), 46, "the typed-fact corpus quietly thinned");
        for fact in facts {
            let rendered = fact_row(&fact);
            assert!(
                !rendered.text.trim().is_empty(),
                "{fact:?} projected to an empty row"
            );
            assert!(rendered.text.len() > 5, "{fact:?}: {rendered:?}");
        }
    }

    #[test]
    fn approval_submits_the_original_wire_not_a_reserialized_or_edited_plan() {
        let original = plan();
        let canonical = serde_json::to_string(&original).unwrap();
        // Whitespace makes byte identity stronger than value equality: a
        // serialize-after-render implementation cannot reproduce these bytes.
        let wire = format!("  {canonical}\n").into_bytes();
        let mut pane = PlanReviewPane::from_wire(wire.clone()).unwrap();

        // Even an independently parsed, edited Plan has no route back into the
        // pane: it stores the original bytes, projection, and key, not a Plan.
        let mut edited_clone: Plan = serde_json::from_slice(&wire).unwrap();
        edited_clone.operation = GitOperation::UnstageAll;
        assert_ne!(edited_clone, original);

        let approval = pane.approve().unwrap();
        assert_eq!(approval.body(), wire);
        assert!(approval.key().contains(&"a".repeat(64)));
        assert!(approval.key().contains("1788365000"));
        assert!(
            pane.approve().is_none(),
            "one decision minted two submissions"
        );
    }

    #[test]
    fn a_generation_conflict_says_only_that_the_plan_is_stale() {
        let outcome = SubmissionOutcome::from_response(
            409,
            b"The repository changed while this plan was pending -- refresh and try again.",
        );
        assert_eq!(outcome, SubmissionOutcome::Stale);
        let message = outcome.message();
        assert_eq!(
            message,
            "Plan is stale. It was not executed. Build and review a new plan."
        );
        for invented in [
            "repository changed",
            "branch moved",
            "working tree",
            "ref moved",
        ] {
            assert!(!message.to_lowercase().contains(invented), "{message}");
        }
    }

    #[test]
    fn every_409_is_stale_regardless_of_english_prose() {
        for body in [
            b"This plan has expired \xe2\x80\x94 refresh and try again.".as_slice(),
            b"refs/heads/main moved".as_slice(),
            b"This plan was built for a different worktree".as_slice(),
            b"some future conflict wording".as_slice(),
        ] {
            assert_eq!(
                SubmissionOutcome::from_response(409, body),
                SubmissionOutcome::Stale
            );
        }
    }

    #[test]
    fn every_success_status_is_execution_and_no_refusal_status_is() {
        for status in [200, 201, 202, 204, 299] {
            assert!(matches!(
                SubmissionOutcome::from_response(status, b"done"),
                SubmissionOutcome::Executed(_)
            ));
        }
        for status in [199, 300, 400, 401, 403, 500] {
            assert!(!matches!(
                SubmissionOutcome::from_response(status, b"not done"),
                SubmissionOutcome::Executed(_)
            ));
        }
    }
}
