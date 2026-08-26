//! `get_lesson` (#450): a read tool that builds a plan the exact way the
//! matching `plan_*` tool would — the same `POST /api/plan`, the same
//! server-side `planner::build_plan_only`, nothing executed — and turns the
//! plan the server actually returned into a structured teaching document, by
//! calling the SAME `git_vista_protocol::explain` function Explain Mode's
//! browser panel calls (M6.39, #92; ADR 0091). The two surfaces cannot drift
//! from each other because they share the facts, not because anyone kept two
//! copies of English in sync.
//!
//! # Why this reuses `plan_tools::operation_for`/`check_exposure`/a new
//! `build_plan_typed`, rather than a second vocabulary
//!
//! [`plan_tools::operation_for`] is already the closed, audited mapping from
//! a tool NAME to a [`GitOperation`] — pure, local, no network — and
//! [`plan_tools::check_exposure`] is the second lock that refuses an
//! operation `exposure_of` classifies `Excluded` even if a future dispatch
//! arm could build one. Reimplementing either here would be a second place
//! for the M4.31/#84 conflict exclusion, the M3.24/#77 stash exclusion, and
//! #153's `ResetTestRepo` exclusion to drift out of sync with the audited
//! one. So `get_lesson`'s own argument is not a `Plan`, and not a bespoke
//! operation description: it is the exact `(tool name, arguments)` pair a
//! caller would otherwise give a `plan_*` tool, forwarded through the same
//! two locks. `get_lesson` therefore reaches exactly the 23 operations
//! `plan_*` exposes today — no more, no less — and a future widening of one
//! surface widens the other automatically, by construction.
//!
//! One consequence worth stating rather than discovering: `resolve_conflict`,
//! `resolve_conflict_content`, and the sequence/cherry-pick/stash operations
//! are **not** in that set (see [`plan_tools`]'s own `UNEXPOSED_TAGS`
//! census). #450's own text imagines a lesson about "a conflict on disk, a
//! sequence mid-flight" — but no MCP tool can build a plan for either today,
//! for reasons #84 and #77 already gave (an agent picking a side has seen
//! none of the three versions; a stash entry's positional selector has no
//! reader yet). `get_lesson` inherits that boundary rather than punching a
//! second hole in it. Teaching those two scenarios through MCP is future
//! scope, gated on the same reader/selection work `plan_*` is waiting on —
//! not something this tool should attempt piecemeal.
//!
//! # Why the plan comes from the live server, not from the caller
//!
//! #450's acceptance criterion is a lesson built "from live repository
//! state," and its mutation target is "a lesson never contains a fact the
//! repository did not carry." An earlier draft of this tool took an
//! already-built `Plan` object as its argument and explained that verbatim —
//! zero network calls, trivially read-only. It was rejected on review: a
//! caller-supplied `Plan` can be fabricated, and a fabricated plan yields a
//! confident lesson about a repository state that never existed. Testing
//! that shape could only prove "no fact the *argument* did not carry," which
//! is a strictly weaker property already proven one crate over by
//! `git-vista-protocol`'s own `explain_parity.rs`. Grounding this tool in the
//! live `/api/plan` round trip — the exact one `plan_*` already makes — is
//! what makes the repository, rather than the caller, the source of truth.
//!
//! This does not weaken "read-only": `/api/plan` reaches only
//! `planner::build_plan_only` (no mutation guard, no executor, no argv), the
//! same endpoint every `plan_*` tool is proven to be confined to
//! (`plan_tools::tests::every_plan_tool_posts_only_to_api_plan`), and no new
//! server route is added. `get_lesson` is a second caller of that one
//! endpoint, not a new one.
//!
//! # Why two new mirror types (`LessonTopic`, `LessonNetworkNeed`)
//!
//! [`Explanation`](git_vista_protocol::Explanation) deliberately derives no
//! `Serialize` — see that module's doc: the browser viewer already holds the
//! `Plan` locally and calls `explain` itself, so serializing the result would
//! be a second copy of facts the plan already carries. [`Topic`] and
//! [`NetworkNeed`] inherit that: neither ever needed to cross a wire before.
//! This tool is the first thing that needs both on the wire — an MCP agent
//! is not running Rust or wasm and cannot call `explain` itself — and that
//! need is a **transport** fact, not a domain one. Per the issue's own
//! ruling ("transport is not domain, and rendering taste does not belong in
//! a Rust MCP server"), the encoding lives here, in the crate that owns the
//! wire, rather than adding `Serialize` to a protocol type whose module doc
//! explains why it deliberately has none. [`Precondition`], [`RefChange`],
//! [`WorktreeEffect`], [`IndexEffect`], [`RecoveryStrategy`], [`Advisory`]
//! and [`RiskLevel`] already derive `Serialize`/`Deserialize` (`Plan`'s own
//! fields carry them across the wire today), so [`LessonFact`] embeds those
//! five directly and needs no mirror for them.
//!
//! # Why `Lesson` embeds the built `plan` verbatim
//!
//! A caller who only wanted the teaching document would otherwise need a
//! second `plan_*` call to get the reviewable plan this lesson is *about* —
//! and #450's own "what it is" asks for "the real state, the typed facts
//! about it, and explanation blocks" as one document. Embedding it also
//! makes the mutation-proof invariant checkable directly from the tool's
//! output: every [`LessonFact`] in the returned document can be traced back
//! to a field of the returned `plan`, in the same document, with nothing
//! held aside.
//!
//! One thing worth being explicit about rather than leaving implicit: the
//! embedded `plan` carries a live `operation_hash`, `generation` and
//! `expires_at` — the same execution-binding fields `execute_plan` validates
//! (#145). A lesson stored for teaching (in `teacher-thing`, say) is
//! therefore a snapshot whose `expires_at` will pass; that is harmless
//! (`execute_plan` re-validates against the *live* repository and refuses a
//! stale plan outright), but a consumer rendering a stored lesson later
//! should not read an expired `operation_hash` as still submittable.

use git_vista_protocol::{
    Advisory, ExplanationFact, IndexEffect, NetworkNeed, Plan, Precondition, RecoveryStrategy,
    RefChange, RiskLevel, Topic, WorktreeEffect,
};

use crate::auth::{self, Session};
use crate::http;
use crate::plan_tools;
use crate::tools::{PostFn, ToolError};

// ---------------------------------------------------------------------------
// Wire mirrors for the two protocol types with no `Serialize` impl — see the
// module doc's "Why two new mirror types" section.
// ---------------------------------------------------------------------------

/// The wire form of [`Topic`] — same six values, same order, `snake_case`
/// like every other internally-named enum this crate re-exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonTopic {
    MustBeTrueFirst,
    WhatMoves,
    IndexAndWorktree,
    Remote,
    HowToUndo,
    WorthKnowing,
}

impl From<Topic> for LessonTopic {
    /// Exhaustive, no wildcard: a seventh `Topic` variant fails this build
    /// until it is given a wire name here, rather than silently falling back
    /// to some default heading.
    fn from(topic: Topic) -> Self {
        match topic {
            Topic::MustBeTrueFirst => LessonTopic::MustBeTrueFirst,
            Topic::WhatMoves => LessonTopic::WhatMoves,
            Topic::IndexAndWorktree => LessonTopic::IndexAndWorktree,
            Topic::Remote => LessonTopic::Remote,
            Topic::HowToUndo => LessonTopic::HowToUndo,
            Topic::WorthKnowing => LessonTopic::WorthKnowing,
        }
    }
}

/// The wire form of [`NetworkNeed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonNetworkNeed {
    Remote,
    Local,
}

impl From<NetworkNeed> for LessonNetworkNeed {
    /// Exhaustive, no wildcard — see [`From<Topic>`](#impl-From<Topic>-for-LessonTopic).
    fn from(need: NetworkNeed) -> Self {
        match need {
            NetworkNeed::Remote => LessonNetworkNeed::Remote,
            NetworkNeed::Local => LessonNetworkNeed::Local,
        }
    }
}

// ---------------------------------------------------------------------------
// The lesson document
// ---------------------------------------------------------------------------

/// One statement in a lesson, carrying the plan's own typed value — the wire
/// twin of [`ExplanationFact`]. Externally tagged (serde's default enum
/// representation) rather than internally tagged like `Precondition` et al.,
/// because a uniform representation has to hold for every payload shape this
/// wraps, including [`RiskLevel`] and the two effect enums, which serialize
/// to a bare string rather than an object — an internal tag requires a
/// map-shaped payload, which a bare string is not.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonFact {
    Precondition(Precondition),
    RefMoves(RefChange),
    Worktree(WorktreeEffect),
    Index(IndexEffect),
    Remote(LessonNetworkNeed),
    Recovery(RecoveryStrategy),
    Advisory(Advisory),
    Risk(RiskLevel),
}

impl From<&ExplanationFact> for LessonFact {
    /// Exhaustive, no wildcard, matching this whole file's posture: a ninth
    /// `ExplanationFact` variant fails this build until it is classified
    /// here, rather than silently vanishing from every lesson.
    fn from(fact: &ExplanationFact) -> Self {
        match fact {
            ExplanationFact::Precondition(p) => LessonFact::Precondition(p.clone()),
            ExplanationFact::RefMoves(r) => LessonFact::RefMoves(r.clone()),
            ExplanationFact::Worktree(w) => LessonFact::Worktree(*w),
            ExplanationFact::Index(i) => LessonFact::Index(*i),
            ExplanationFact::Remote(n) => LessonFact::Remote((*n).into()),
            ExplanationFact::Recovery(r) => LessonFact::Recovery(r.clone()),
            ExplanationFact::Advisory(a) => LessonFact::Advisory(a.clone()),
            ExplanationFact::Risk(l) => LessonFact::Risk(*l),
        }
    }
}

/// One collapsible section of the lesson — the wire twin of
/// [`git_vista_protocol::Section`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LessonSection {
    pub topic: LessonTopic,
    pub facts: Vec<LessonFact>,
}

/// The full lesson: the plan it explains, verbatim, and the six sections
/// [`git_vista_protocol::explain`] derived from it. See the module doc's
/// "Why `Lesson` embeds the built `plan` verbatim".
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Lesson {
    pub plan: Plan,
    pub sections: Vec<LessonSection>,
}

/// Explain `plan` and re-express the result on the wire. The one function
/// that touches [`git_vista_protocol::explain`] — everything above this line
/// is plumbing for its result, and everything below is plumbing to get a
/// `Plan` here in the first place.
pub fn lesson_for(plan: Plan) -> Lesson {
    let explanation = git_vista_protocol::explain(&plan);
    let sections = explanation
        .sections
        .iter()
        .map(|section| LessonSection {
            topic: section.topic.into(),
            facts: section.facts.iter().map(LessonFact::from).collect(),
        })
        .collect();
    Lesson { plan, sections }
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// `get_lesson`'s single catalog entry. Kept in its own function, matching
/// [`plan_tools::plan_tool_catalog`] and [`crate::execute_tool::execute_tool_catalog`],
/// so [`crate::tools::tool_catalog`] reads as "reads, lesson, plans,
/// execute" rather than one long literal.
pub fn lesson_tool_catalog() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "name": "get_lesson",
        "description": "Build a plan exactly the way the named plan_* tool would — the \
                        same POST /api/plan request, nothing executed — then turn the plan \
                        the server actually returned into a structured teaching document: \
                        its own typed facts, organised into the same six topics Explain \
                        Mode's browser panel shows (what must be true first, what moves, \
                        the index/worktree/network effects, how to undo it, what is worth \
                        knowing). Every fact in the lesson traces to a field of the \
                        returned plan, because both this tool and the browser panel derive \
                        their sentences from the same git_vista_protocol::explain function \
                        (#92, #450) — they cannot drift from each other. The plan itself \
                        travels alongside the lesson, unchanged, under `plan` (the same \
                        object a plan_* call alone would have returned), so one call \
                        answers both 'what would this do' and 'what does that mean'. \
                        Reaches exactly the operations plan_* itself exposes — conflict \
                        resolution and sequence continuation are not among them today (see \
                        plan_* 's own tool list), and get_lesson does not add a path around \
                        that.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "plan_tool": {
                    "type": "string",
                    "description": "The exact plan_* tool name to build a plan for, e.g. \
                                    \"plan_merge_branch\" — anything plan_* itself exposes. \
                                    A name plan_* does not expose (or does not recognise) \
                                    is refused the same way calling that name directly \
                                    would be."
                },
                "arguments": {
                    "type": "object",
                    "description": "The arguments you would pass to that plan_* tool, \
                                    unchanged. Omit for a tool that takes none (e.g. \
                                    \"plan_stage_all\")."
                }
            },
            "required": ["plan_tool"],
            "additionalProperties": false
        }
    })]
}

/// A required top-level string argument — the same one-line check
/// `tools.rs`'s own `required_str` makes, duplicated rather than imported:
/// `plan_tools.rs` already keeps its own copy (`str_arg`) rather than sharing
/// across modules, and this file follows that precedent.
fn required_str(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ToolError::Execution(format!("missing required argument `{key}`")))
}

/// Production's entry point: the real HTTP client and authenticator wired
/// in, matching [`plan_tools::call_plan_tool_live`]'s shape.
pub(crate) fn call_live(
    args: &serde_json::Value,
    session: &mut Option<Session>,
) -> Result<serde_json::Value, ToolError> {
    call(
        args,
        session,
        &mut |path, body, cookie, csrf| http::post_json(path, body, Some(cookie), Some(csrf)),
        &mut auth::authenticate,
    )
}

/// The injectable form: build the named operation, run it through the same
/// two locks a `plan_*` call would, fetch the server's `Plan` for it, and
/// explain that plan. Generic over the POST/auth closures so this is
/// unit-testable without a server — the same shape [`plan_tools::call_plan_tool`]
/// already uses.
pub(crate) fn call(
    args: &serde_json::Value,
    session: &mut Option<Session>,
    post: &mut PostFn<'_>,
    authenticate: &mut dyn FnMut() -> Result<Session, String>,
) -> Result<serde_json::Value, ToolError> {
    let plan_tool = required_str(args, "plan_tool")?;
    let tool_args = args
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let op = match plan_tools::operation_for(&plan_tool, &tool_args) {
        Some(Ok(op)) => op,
        Some(Err(e)) => return Err(e),
        None => {
            return Err(ToolError::Execution(format!(
                "`{plan_tool}` is not a plan_* tool get_lesson can build a lesson for — pass \
                 the exact name you would give a plan_* tool call (e.g. \"plan_merge_branch\")"
            )))
        }
    };
    // The second lock: refuse here, before any request exists, if `op` is
    // one `exposure_of` classifies `Excluded` — see this module's doc and
    // `plan_tools::check_exposure`'s own.
    plan_tools::check_exposure(&plan_tool, &op)?;

    let plan = plan_tools::build_plan_typed(op, session, post, authenticate)?;
    let lesson = lesson_for(plan);
    serde_json::to_value(&lesson)
        .map_err(|e| ToolError::Execution(format!("could not encode the lesson: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use git_vista_protocol::{
        BranchName, GenerationToken, GitOperation, OperationHash, RefName, RefState, RemoteName,
        RepositoryToken, UnixSeconds, WorktreeToken,
    };

    fn session() -> Session {
        Session {
            cookie: "gv_session=live".to_string(),
            csrf: "csrf".to_string(),
        }
    }

    fn ok(body: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    fn oid(byte: char) -> String {
        byte.to_string().repeat(40)
    }

    /// Post a fixed `Plan` and return whatever `get_lesson` answers for the
    /// given `plan_tool`/`arguments`, capturing exactly one request the way
    /// `plan_tools::tests::every_plan_tool_posts_only_to_api_plan` does.
    fn call_against(
        plan_tool: &str,
        arguments: serde_json::Value,
        server_plan: &Plan,
    ) -> serde_json::Value {
        let body = serde_json::to_vec(server_plan).unwrap();
        let mut captured: Vec<(String, Vec<u8>)> = Vec::new();
        let mut sess = Some(session());
        let args = serde_json::json!({ "plan_tool": plan_tool, "arguments": arguments });
        let result = call(
            &args,
            &mut sess,
            &mut |path, req_body, _cookie, _csrf| {
                captured.push((path.to_string(), req_body.to_vec()));
                Ok(ok(&body))
            },
            &mut || panic!("re-authenticated with a live session already present"),
        )
        .unwrap_or_else(|e| panic!("get_lesson({plan_tool}) failed: {e:?}"));
        assert_eq!(
            captured.len(),
            1,
            "get_lesson must send exactly one request"
        );
        assert_eq!(
            captured[0].0, "/api/plan",
            "get_lesson contacted the wrong endpoint"
        );
        let sent: GitOperation = serde_json::from_slice(&captured[0].1)
            .expect("get_lesson's request body was not a GitOperation");
        assert_eq!(
            &sent, &server_plan.operation,
            "get_lesson built a different operation than the one it asked the server about"
        );
        result
    }

    // -----------------------------------------------------------------
    // Representative plans, drawn verbatim from
    // git-vista-protocol/tests/fixtures/plan_v1.json (the golden corpus
    // `plan_golden.rs` and `explain_parity.rs` already anchor on) — real
    // shapes the server actually emits, not invented ones. Field values
    // (oids, names) are the fixture's own placeholders.
    // -----------------------------------------------------------------

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
            preconditions: vec![],
            expected_ref_changes: vec![],
            advisories: vec![],
            recovery,
        }
    }

    /// `create_branch`: `RefAbsent` precondition, one ref change, `Reversible`
    /// risk, `DeleteCreatedBranch` recovery — golden corpus's own shape.
    fn create_branch_plan() -> Plan {
        let mut p = base_plan(
            GitOperation::CreateBranch {
                name: BranchName::new("feature/idea").unwrap(),
                at: CommitOidHelper::of('1'),
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
            after: RefState::At(CommitOidHelper::of('1')),
        }];
        p
    }

    /// `push_branch`, WITH advisories added (the golden corpus's own
    /// push_branch carries none — same reason `explain_parity.rs`'s
    /// `plan_with_advisories` exists) — exercises `Advisory`, `Remote` risk,
    /// and `NetworkNeed::Remote`. The advisories are content nothing about
    /// `plan_tool`/`arguments` alone implies, which is exactly the point:
    /// this plan can only be explained correctly if the lesson comes from
    /// THIS `Plan`, not from a value reconstructed locally from the request.
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
            before: RefState::At(CommitOidHelper::of('4')),
            after: RefState::At(CommitOidHelper::of('2')),
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

    /// `force_delete_branch`: `Destructive` risk, `RecreateBranch` recovery.
    fn force_delete_branch_plan() -> Plan {
        let mut p = base_plan(
            GitOperation::ForceDeleteBranch {
                branch: BranchName::new("feature/abandoned").unwrap(),
            },
            RiskLevel::Destructive,
            RecoveryStrategy::RecreateBranch {
                name: BranchName::new("feature/abandoned").unwrap(),
                at: CommitOidHelper::of('6'),
            },
        );
        p.preconditions = vec![Precondition::RefAt {
            ref_name: RefName::new("refs/heads/feature/abandoned").unwrap(),
            oid: CommitOidHelper::of('6'),
        }];
        p.expected_ref_changes = vec![RefChange {
            ref_name: RefName::new("refs/heads/feature/abandoned").unwrap(),
            before: RefState::At(CommitOidHelper::of('6')),
            after: RefState::Absent,
        }];
        p
    }

    /// `fetch_remote`: `Safe` risk, `NotNeeded` recovery, `NetworkNeed::Remote`
    /// with `WorktreeEffect::Untouched`/`IndexEffect::Untouched` — the case
    /// that proves risk and reach are independent axes.
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

    /// `merge_branch`: `WorktreeEffect::MayConflict`, `IndexEffect::Rebuilt`,
    /// two preconditions, a `Computed` ref-change, `ResetRef` recovery.
    fn merge_branch_plan() -> Plan {
        let mut p = base_plan(
            GitOperation::MergeBranch {
                branch: BranchName::new("feature/idea").unwrap(),
            },
            RiskLevel::Reversible,
            RecoveryStrategy::ResetRef {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                to: CommitOidHelper::of('2'),
            },
        );
        p.preconditions = vec![
            Precondition::BranchCheckedOut {
                branch: BranchName::new("main").unwrap(),
            },
            Precondition::RefAt {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                oid: CommitOidHelper::of('2'),
            },
        ];
        p.expected_ref_changes = vec![RefChange {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            before: RefState::At(CommitOidHelper::of('2')),
            after: RefState::Computed,
        }];
        p
    }

    /// `stage_all`: `IndexEffect::EntriesStaged`, everything else empty.
    fn stage_all_plan() -> Plan {
        base_plan(
            GitOperation::StageAll,
            RiskLevel::Safe,
            RecoveryStrategy::NotNeeded,
        )
    }

    /// `unstage_all`: `IndexEffect::EntriesUnstaged`.
    fn unstage_all_plan() -> Plan {
        base_plan(
            GitOperation::UnstageAll,
            RiskLevel::Safe,
            RecoveryStrategy::NotNeeded,
        )
    }

    /// A tiny local shim so the plans above can spell a commit oid the same
    /// way the golden fixture does (a single repeated hex digit) without a
    /// `CommitOid::new(...).unwrap()` at every call site.
    struct CommitOidHelper;
    impl CommitOidHelper {
        fn of(byte: char) -> git_vista_protocol::CommitOid {
            git_vista_protocol::CommitOid::new(oid(byte)).unwrap()
        }
    }

    fn representative_plans() -> Vec<(&'static str, Plan)> {
        vec![
            ("plan_create_branch", create_branch_plan()),
            ("plan_push_branch", push_branch_plan_with_advisories()),
            ("plan_force_delete_branch", force_delete_branch_plan()),
            ("plan_fetch_remote", fetch_remote_plan()),
            ("plan_merge_branch", merge_branch_plan()),
            ("plan_stage_all", stage_all_plan()),
            ("plan_unstage_all", unstage_all_plan()),
        ]
    }

    /// The arguments each representative plan's own tool actually needs —
    /// enough for `operation_for` to build the SAME operation the mocked
    /// server response carries, so [`call_against`]'s own cross-check (sent
    /// operation == server plan's operation) is exercised honestly rather
    /// than vacuously.
    fn arguments_for(tool: &str) -> serde_json::Value {
        match tool {
            "plan_create_branch" => {
                serde_json::json!({ "name": "feature/idea", "at": oid('1') })
            }
            "plan_push_branch" => serde_json::json!({
                "branch": "main", "remote": "origin",
                "set_upstream": false, "force": { "mode": "none" }
            }),
            "plan_force_delete_branch" => serde_json::json!({ "branch": "feature/abandoned" }),
            "plan_fetch_remote" => serde_json::json!({ "remote": "origin" }),
            "plan_merge_branch" => serde_json::json!({ "branch": "feature/idea" }),
            "plan_stage_all" | "plan_unstage_all" => serde_json::json!({}),
            other => panic!("no argument fixture for {other}"),
        }
    }

    // -----------------------------------------------------------------
    // Fidelity — the plan-anchored half, both directions, at the JSON
    // wire boundary. Independent of `explain` (already proven correct by
    // `explain_parity.rs`) and independent of this file's own `From`
    // impls: every assertion compares the lesson's serialized JSON
    // straight against `plan`'s own fields, the way `explain_parity.rs`
    // compares against `plan` rather than re-deriving the expectation
    // from the function under test.
    // -----------------------------------------------------------------

    fn section<'a>(lesson: &'a serde_json::Value, topic: &str) -> &'a Vec<serde_json::Value> {
        lesson["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["topic"] == topic)
            .unwrap_or_else(|| panic!("no {topic} section in {lesson}"))["facts"]
            .as_array()
            .unwrap()
    }

    fn facts_of_kind<'a>(facts: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
        facts
            .iter()
            .filter(|f| f.get(kind).is_some())
            .map(|f| &f[kind])
            .collect()
    }

    #[test]
    fn no_lesson_fact_lacks_a_plan_field_and_no_plan_field_lacks_a_lesson_fact() {
        for (tool, plan) in representative_plans() {
            let lesson = call_against(tool, arguments_for(tool), &plan);
            let must_be_true_first = section(&lesson, "must_be_true_first");
            let what_moves = section(&lesson, "what_moves");
            let how_to_undo = section(&lesson, "how_to_undo");
            let worth_knowing = section(&lesson, "worth_knowing");

            let plan_json = serde_json::to_value(&plan).unwrap();

            // Preconditions: both directions.
            let lesson_preconditions = facts_of_kind(must_be_true_first, "precondition");
            let plan_preconditions = plan_json["preconditions"].as_array().unwrap();
            assert_eq!(
                lesson_preconditions.len(),
                plan_preconditions.len(),
                "{tool}: precondition count disagrees with the plan"
            );
            for p in plan_preconditions {
                assert!(
                    lesson_preconditions.contains(&p),
                    "{tool}: lesson omits plan precondition {p}"
                );
            }

            // Ref changes: both directions.
            let lesson_ref_moves = facts_of_kind(what_moves, "ref_moves");
            let plan_ref_changes = plan_json["expected_ref_changes"].as_array().unwrap();
            assert_eq!(
                lesson_ref_moves.len(),
                plan_ref_changes.len(),
                "{tool}: ref-change count disagrees with the plan"
            );
            for r in plan_ref_changes {
                assert!(
                    lesson_ref_moves.contains(&r),
                    "{tool}: lesson omits plan ref change {r}"
                );
            }

            // Advisories: both directions.
            let lesson_advisories = facts_of_kind(worth_knowing, "advisory");
            let plan_advisories = plan_json["advisories"].as_array().unwrap();
            assert_eq!(
                lesson_advisories.len(),
                plan_advisories.len(),
                "{tool}: advisory count disagrees with the plan"
            );
            for a in plan_advisories {
                assert!(
                    lesson_advisories.contains(&a),
                    "{tool}: lesson omits plan advisory {a}"
                );
            }

            // Recovery: exactly one fact, exactly the plan's own value.
            let lesson_recovery = facts_of_kind(how_to_undo, "recovery");
            assert_eq!(
                lesson_recovery.len(),
                1,
                "{tool}: recovery must appear exactly once"
            );
            assert_eq!(
                lesson_recovery[0], &plan_json["recovery"],
                "{tool}: lesson's recovery disagrees with the plan's"
            );

            // Risk: exactly one fact, exactly the plan's own value, leading
            // the section.
            let lesson_risk = facts_of_kind(worth_knowing, "risk");
            assert_eq!(
                lesson_risk.len(),
                1,
                "{tool}: risk must appear exactly once"
            );
            assert_eq!(
                lesson_risk[0], &plan_json["risk"],
                "{tool}: lesson's risk disagrees with the plan's"
            );
            assert!(
                worth_knowing[0].get("risk").is_some(),
                "{tool}: worth_knowing must lead with risk"
            );

            // The plan itself travels unchanged.
            assert_eq!(
                lesson["plan"], plan_json,
                "{tool}: the embedded plan was altered"
            );
        }
    }

    #[test]
    fn the_plan_anchored_fidelity_check_is_not_vacuous() {
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

    // -----------------------------------------------------------------
    // Derived facts (worktree/index/remote) — no plan field to trace to,
    // so anchored on a hand-written table instead, same remedy
    // `explain_parity.rs`'s own module doc explains: computing the
    // expected value by calling the accessor would assert f(x) == f(x).
    // -----------------------------------------------------------------

    const DERIVED: &[(&str, &str, &str, &str)] = &[
        ("plan_create_branch", "untouched", "untouched", "local"),
        ("plan_push_branch", "untouched", "untouched", "remote"),
        (
            "plan_force_delete_branch",
            "untouched",
            "untouched",
            "local",
        ),
        ("plan_fetch_remote", "untouched", "untouched", "remote"),
        ("plan_merge_branch", "may_conflict", "rebuilt", "local"),
        ("plan_stage_all", "untouched", "entries_staged", "local"),
        ("plan_unstage_all", "untouched", "entries_unstaged", "local"),
    ];

    #[test]
    fn derived_facts_match_the_independent_table() {
        for (tool, plan) in representative_plans() {
            let (_, want_worktree, want_index, want_remote) = DERIVED
                .iter()
                .find(|(t, ..)| *t == tool)
                .unwrap_or_else(|| panic!("no DERIVED row for {tool}"));
            let lesson = call_against(tool, arguments_for(tool), &plan);
            let index_and_worktree = section(&lesson, "index_and_worktree");
            let remote = section(&lesson, "remote");

            assert_eq!(
                facts_of_kind(index_and_worktree, "worktree"),
                vec![&serde_json::json!(*want_worktree)],
                "{tool}: worktree effect"
            );
            assert_eq!(
                facts_of_kind(index_and_worktree, "index"),
                vec![&serde_json::json!(*want_index)],
                "{tool}: index effect"
            );
            assert_eq!(
                facts_of_kind(remote, "remote"),
                vec![&serde_json::json!(*want_remote)],
                "{tool}: network need"
            );
        }
    }

    #[test]
    fn the_derived_table_exercises_every_variant_it_claims_to() {
        // Anti-vacuity: a table of seven identical rows would agree with a
        // wire-mapping stubbed the same way. Every distinct label used above
        // must appear at least once — this also means a future edit that
        // drops the last row exercising a label fails loudly here.
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

    // -----------------------------------------------------------------
    // Shape and dispatch
    // -----------------------------------------------------------------

    #[test]
    fn every_lesson_has_all_six_topics_in_order() {
        for (tool, plan) in representative_plans() {
            let lesson = call_against(tool, arguments_for(tool), &plan);
            let topics: Vec<&str> = lesson["sections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["topic"].as_str().unwrap())
                .collect();
            assert_eq!(
                topics,
                [
                    "must_be_true_first",
                    "what_moves",
                    "index_and_worktree",
                    "remote",
                    "how_to_undo",
                    "worth_knowing",
                ],
                "{tool}: lesson section order/shape changed"
            );
        }
    }

    #[test]
    fn get_lesson_sends_exactly_one_request_to_api_plan() {
        // Restated as its own test (beyond call_against's inline assertion)
        // because this is #450's own read-only acceptance criterion, not
        // an implementation detail of the test helper.
        let plan = stage_all_plan();
        let _ = call_against("plan_stage_all", serde_json::json!({}), &plan);
    }

    #[test]
    fn a_missing_plan_tool_argument_is_refused_without_authenticating() {
        let mut sess = None;
        let err = call(
            &serde_json::json!({}),
            &mut sess,
            &mut |_, _, _, _| panic!("must not reach the network"),
            &mut || panic!("must not authenticate"),
        )
        .unwrap_err();
        match err {
            ToolError::Execution(msg) => assert!(msg.contains("plan_tool"), "{msg}"),
            other => panic!("expected Execution, got {other:?}"),
        }
        assert!(sess.is_none());
    }

    #[test]
    fn an_unknown_plan_tool_name_is_refused_without_authenticating() {
        let mut sess = None;
        let err = call(
            &serde_json::json!({ "plan_tool": "plan_does_not_exist", "arguments": {} }),
            &mut sess,
            &mut |_, _, _, _| panic!("must not reach the network"),
            &mut || panic!("must not authenticate"),
        )
        .unwrap_err();
        match err {
            ToolError::Execution(msg) => assert!(msg.contains("plan_does_not_exist"), "{msg}"),
            other => panic!("expected Execution, got {other:?}"),
        }
        assert!(sess.is_none());
    }

    /// The excluded operations by name — `resolve_conflict` (#84),
    /// `reset_test_repo` (#153) and the stash three (#77) all have no
    /// `operation_for` dispatch arm at all, so `get_lesson` refuses them the
    /// same way it refuses any unrecognised name: there is no second door.
    #[test]
    fn excluded_operations_are_refused_by_name_the_same_way_an_unknown_tool_is() {
        for name in [
            "plan_resolve_conflict",
            "plan_resolve_conflict_content",
            "plan_reset_test_repo",
            "plan_push_stash",
            "plan_apply_stash",
            "plan_branch_from_stash",
            "plan_drop_stash",
        ] {
            let mut sess = None;
            let err = call(
                &serde_json::json!({ "plan_tool": name, "arguments": {} }),
                &mut sess,
                &mut |_, _, _, _| panic!("{name} must not reach the network"),
                &mut || panic!("{name} must not authenticate"),
            )
            .unwrap_err();
            match err {
                ToolError::Execution(msg) => assert!(msg.contains(name), "{name}: {msg}"),
                other => panic!("{name}: expected Execution, got {other:?}"),
            }
            assert!(
                sess.is_none(),
                "{name} authenticated for a call refused locally"
            );
        }
    }

    #[test]
    fn missing_arguments_defaults_to_an_empty_object() {
        // plan_stage_all takes no arguments; omitting the field entirely
        // must behave like passing `{}`, not like an error.
        let plan = stage_all_plan();
        let body = serde_json::to_vec(&plan).unwrap();
        let mut sess = Some(session());
        let result = call(
            &serde_json::json!({ "plan_tool": "plan_stage_all" }),
            &mut sess,
            &mut |_, _, _, _| Ok(ok(&body)),
            &mut || panic!("must not re-authenticate"),
        )
        .unwrap();
        assert_eq!(result["plan"]["operation"]["op"], "stage_all");
    }

    #[test]
    fn the_tool_catalog_entry_is_closed_and_required_on_plan_tool_only() {
        let catalog = lesson_tool_catalog();
        let tool = &catalog[0];
        assert_eq!(tool["name"], "get_lesson");
        assert_eq!(
            tool["inputSchema"]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            tool["inputSchema"]["required"],
            serde_json::json!(["plan_tool"])
        );
    }

    // -----------------------------------------------------------------
    // Composability with #448's fixture catalogue: a lesson must generate
    // from a `git-vista-fixtures` repository as readily as from a real one.
    //
    // `git-vista-mcp` structurally cannot reach `git-vista-server`'s planner
    // (see `tests/no_write_dependency.rs`), and the server binds only to a
    // compile-time-fixed loopback port with no override, so this crate's own
    // tests cannot stand up a private server instance the way a unit test
    // normally would — the crate's existing `live_handshake.rs` tests hit
    // exactly this wall and are `#[ignore]`d, manual-run-only, for the same
    // reason. This test does not pretend otherwise: it does not drive a live
    // server. What it proves instead is that `get_lesson`'s own mapping is
    // exercised against genuinely-read state from a real repository that
    // real `git` built — the ref-absent precondition, the HEAD oid, the
    // "does this branch already exist" check are all read off the actual
    // fixture on disk, not invented — so the mocked `/api/plan` response fed
    // to `call` is grounded in a real broken-repo-catalogue fixture the same
    // way it would be if the real planner had built it.
    // -----------------------------------------------------------------

    /// Run `git` inside `repo` and return trimmed stdout, panicking with
    /// stderr on failure — a small local helper rather than a dependency on
    /// `git-vista-fixtures`'s own (private) `git` module.
    fn run_git(repo: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("could not spawn git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_lesson_generates_from_a_git_vista_fixtures_repo_as_readily_as_a_real_one() {
        // `seeded()` is git-vista-fixtures' baseline shape: `git init -b
        // main`, one file, one commit — built by real git, not a hand-typed
        // JSON literal.
        let (_dir, repo) = git_vista_fixtures::seeded();

        let head = run_git(&repo, &["rev-parse", "HEAD"]);
        let head_oid = git_vista_protocol::CommitOid::new(head.clone()).unwrap();
        let new_branch = "feature/from-fixture";

        // The precondition a real plan_create_branch build would attach
        // against THIS repository — verified true of it by asking git
        // directly, not assumed.
        let already_exists = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{new_branch}"),
            ])
            .status()
            .unwrap()
            .success();
        assert!(
            !already_exists,
            "the fixture already has refs/heads/{new_branch} — precondition would not hold"
        );

        let mut plan = base_plan(
            GitOperation::CreateBranch {
                name: BranchName::new(new_branch).unwrap(),
                at: head_oid.clone(),
            },
            RiskLevel::Reversible,
            RecoveryStrategy::DeleteCreatedBranch {
                name: BranchName::new(new_branch).unwrap(),
            },
        );
        plan.preconditions = vec![Precondition::RefAbsent {
            ref_name: RefName::new(format!("refs/heads/{new_branch}")).unwrap(),
        }];
        plan.expected_ref_changes = vec![RefChange {
            ref_name: RefName::new(format!("refs/heads/{new_branch}")).unwrap(),
            before: RefState::Absent,
            after: RefState::At(head_oid),
        }];

        let lesson = call_against(
            "plan_create_branch",
            serde_json::json!({ "name": new_branch, "at": head }),
            &plan,
        );

        // The lesson's ref-change fact names the fixture's REAL HEAD oid,
        // read off the real repository above — not a placeholder.
        let what_moves = section(&lesson, "what_moves");
        let ref_moves = facts_of_kind(what_moves, "ref_moves");
        assert_eq!(ref_moves.len(), 1, "expected exactly one ref-change fact");
        assert_eq!(
            ref_moves[0]["after"]["value"].as_str().unwrap(),
            head,
            "the lesson's ref-change does not name the fixture's real HEAD oid"
        );

        // And the full plan-anchored fidelity check applies here exactly as
        // it does for the hand-built representative plans above.
        let plan_json = serde_json::to_value(&plan).unwrap();
        assert_eq!(lesson["plan"], plan_json);
        let must_be_true_first = section(&lesson, "must_be_true_first");
        assert_eq!(
            facts_of_kind(must_be_true_first, "precondition"),
            vec![&plan_json["preconditions"][0]],
        );
    }
}
