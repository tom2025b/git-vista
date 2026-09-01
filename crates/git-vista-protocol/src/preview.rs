//! The graph-preview wire envelope (M10.08, #576).
//!
//! [`PreviewOutcome`] is the whole answer to "what would the graph look like if
//! I ran this?", and it is generic over the row/edge/stub/change types for
//! exactly the reason [`crate::history::HistoryPage`] is: this crate declares
//! the transport *shape* and never the domain types that fill it in. That is
//! not style — it is the one-way dependency this crate's own module doc pins
//! ("`git-vista-core` does *not* depend on it, keeping the domain model free of
//! transport concerns"), and `git-vista-core` is neither pure-of-transport nor
//! a dependency this crate may take outside `[dev-dependencies]`. The server
//! and the frontend each declare their own concrete alias:
//!
//! ```ignore
//! pub type PreviewResponse = PreviewOutcome<GraphRow, Edge, BranchStub, PreviewChange>;
//! ```
//!
//! `PreviewChange` deliberately is **not** here: it names commit ids and lane
//! numbers, which is the repository domain, so it lives in
//! `git_vista_core::preview` beside the function that derives the lane shifts.
//! Everything that IS transport — the four-arm answer, the unavailability
//! vocabulary, the graph envelope — is here.
//!
//! # Four arms, because there are four different things to say
//!
//! `Graph` is a picture. `Conflict` is a live, established negative — git ran
//! and said the merge does not apply. `Unsupported` is a permanent fact about
//! the *operation*: the plumbing cannot express it, and no amount of fixing the
//! repository will change that. [`PreviewUnavailable`] is the fourth, and it is
//! the one this vocabulary would be dishonest without: the operation is fine
//! and the answer still could not be computed *here*.
//!
//! Folding that fourth case into `Unsupported { operation }` would make one
//! variant mean two things — the exact shape `plan.rs` refuses when it explains
//! why `RevertMerge` is a variant and not an `Option<u8>` field. It would also
//! lie to the reader: a UI rendering "git-vista cannot preview a revert" for a
//! repository that is merely open read-only sends someone to the wrong place.
//! The split mirrors `recovery_center::RecoveryClass`, where `Expired { …
//! WouldConflict }` ("A live check ran and returned a definite negative. A fact,
//! not a guess") and `CheckFailed` ("The live check itself could not run. 'No
//! fact', never 'no'.") are kept apart deliberately.
//!
//! # No `#[serde(default)]` on anything here
//!
//! House rule, stated on [`crate::plan::Plan`]: a payload from an older build
//! must fail loudly at the version gate rather than decode as an empty answer
//! that reads as "checked, nothing to report". A `Vec<Change>` that defaulted
//! to empty would say "nothing changed" about an operation that changes
//! everything.
//!
//! (`GraphRow::on_remote` does carry `#[serde(default)]`. That is pre-existing
//! **core** history from M1.10/#63, not an added field on a protocol type, and
//! it is not a breach of the rule above.)

use serde::{Deserialize, Serialize};

/// One laid-out graph — the `before` or the `after` half of a preview.
///
/// Generic over the row type `R`, edge type `E` and stub type `S`, aliased by
/// the server and the frontend to `git_vista_core::{GraphRow, Edge,
/// BranchStub}`.
///
/// `lane_count` is `Graph::lane_count` verbatim — the gutter width, stub
/// columns included — **not** the commit-lane high-water
/// [`crate::history::HistoryPage`] carries. The two differ, and a renderer that
/// assumes the paged meaning here draws stubs off the edge of the gutter.
///
/// `stubs` is carried rather than dropped for a rendering reason worth stating:
/// a local branch that owns no commits of its own is drawn as its own short
/// line. An `after` graph with no stubs, beside a `before` graph with them,
/// reads as "this operation deleted my branches".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewGraph<R, E, S> {
    pub rows: Vec<R>,
    pub edges: Vec<E>,
    pub stubs: Vec<S>,
    pub lane_count: usize,
}

/// What a preview of one [`Plan`](crate::plan::Plan) has to say.
///
/// Internally tagged on `"outcome"`, matching [`crate::plan::Advisory`] and
/// [`crate::plan::Precondition`].
///
/// Both halves are returned, never `after` alone. Two reasons, and the second
/// is the binding one. A before/after canvas needs both. And
/// `PreviewChange::LaneShifted` is *defined* by comparing lane numbers across
/// the two layouts — a caller handed only `after` cannot check a single one of
/// those numbers against anything, because it cannot reproduce this `before`:
/// paged history is a windowed, cursor-based walk and the preview's is a
/// single capped `walk_history`, so their lane assignments need not agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum PreviewOutcome<R, E, S, C> {
    /// The operation applies cleanly, and this is the graph it would produce.
    Graph {
        before: PreviewGraph<R, E, S>,
        after: PreviewGraph<R, E, S>,
        /// What moved, added or shifted between the two halves. Never
        /// defaulted: an empty vec means "nothing changed", which for these
        /// operations is a claim, not an absence.
        changes: Vec<C>,
    },
    /// Real git performed the real three-way merge and it does not apply.
    /// A live established fact, not an error and not a guessed graph.
    ///
    /// `paths` is repo-relative and lossily decoded — `merge-tree -z`'s
    /// conflicted-file records are bytes, and a path that is not UTF-8 is
    /// still a path the user needs named. It is never empty in this arm:
    /// a conflict git reported with no parseable path is
    /// `Unavailable { CheckFailed }`, because `Conflict { paths: [] }` reads
    /// as "conflicted, nothing conflicted".
    Conflict { paths: Vec<String> },
    /// The plumbing cannot express this operation, so there is no picture.
    /// The default arm: a new `GitOperation` variant is invisible here
    /// rather than wrong. `operation` is the variant's own name, for a human.
    Unsupported { operation: String },
    /// The operation is previewable; this host or this repository could not
    /// compute it. See [`PreviewUnavailable`].
    Unavailable { reason: PreviewUnavailable },
}

/// Why a previewable operation produced no answer here.
///
/// Every variant is a *named* reason. The one thing this vocabulary exists to
/// prevent is "I could not tell" arriving as a confident statement about
/// something else — which is what `Unsupported { operation }` would have been
/// for a read-only repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "unavailable", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum PreviewUnavailable {
    /// The repository is open read-only (Visualize mode), so the sandbox
    /// policy grants it no read-write tree and the scratch object store has
    /// nowhere to live. Actionable: reopen in Active mode.
    RepositoryReadOnly,
    /// `merge-tree --write-tree` needs git 2.38; this host has an older one.
    /// The product floor is 2.32 and stays 2.32 — this is one feature's floor,
    /// which is why it degrades here instead of refusing at boot.
    GitTooOld {
        /// The host's version, `major.minor.patch`, re-rendered from the
        /// parsed triple — never git's raw line, whose vendor suffixes are
        /// not the caller's business.
        found: String,
        /// The floor, `major.minor`.
        minimum: String,
    },
    /// The scratch object store could not be created, seeded or read.
    /// `detail` is git's own text where there is any.
    ScratchStore { detail: String },
    /// A git step ran and did not produce an answer — the `_ => Err(stderr)`
    /// arm `activity::revert_would_conflict` already has, given a name here.
    /// "No fact", never "no".
    CheckFailed { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_core::model::{BranchStub, CommitSummary, Edge, GitRef, GraphRow, Oid, RefKind};
    use git_vista_core::preview::PreviewChange;
    use serde_json::{json, Value};

    /// The alias the server and the frontend each declare for themselves. Its
    /// presence here is the compile-time half of the generic contract: if the
    /// envelope ever stopped carrying the core domain types losslessly, this
    /// module would not build.
    type Outcome = PreviewOutcome<GraphRow, Edge, BranchStub, PreviewChange>;

    fn oid(digit: char) -> Oid {
        Oid((0..40).map(|_| digit).collect())
    }

    fn graph_half() -> PreviewGraph<GraphRow, Edge, BranchStub> {
        PreviewGraph {
            rows: vec![GraphRow {
                commit: CommitSummary {
                    id: oid('9'),
                    parents: vec![oid('3')],
                    summary: "Revert \"add thing\"".into(),
                    author: "Test".into(),
                    time: 400,
                },
                row: 0,
                lane: 0,
                refs: vec![GitRef {
                    name: "main".into(),
                    kind: RefKind::Branch,
                    target: oid('9'),
                }],
                color: 0,
                on_remote: false,
            }],
            edges: vec![Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 1,
                to_lane: 0,
            }],
            stubs: vec![BranchStub {
                name: "spike".into(),
                anchor_row: 0,
                anchor_lane: 0,
                lane: 2,
                color: 5,
                depth: 0,
            }],
            lane_count: 3,
        }
    }

    fn graph_arm() -> Outcome {
        PreviewOutcome::Graph {
            before: graph_half(),
            after: graph_half(),
            changes: vec![PreviewChange::Added { commit: oid('9') }],
        }
    }

    /// The invariant: the four arms are told apart on the wire by four
    /// *distinct* `outcome` tags. Distinguishing them is the entire reason
    /// there are four rather than three-plus-a-reason-field, so a payload that
    /// could not be discriminated would defeat the design silently.
    ///
    /// Expected tags are literals, one per case, never re-derived from the
    /// value being serialized.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M-P1a — REMOVES the mechanism.** Drop `#[serde(tag = "outcome")]`.
    ///   Serde falls back to external tagging (`{"conflict":{…}}`), so there is
    ///   no `outcome` key at all and every one of the four lookups returns
    ///   `None`. Red four times over.
    /// * **M-P1b — WEAKENS the mechanism.** Keep the tag and add
    ///   `#[serde(rename = "unsupported")]` to the `Unavailable` variant. Three
    ///   of the four literals still pass and the payloads still look
    ///   well-formed; red on the `Unavailable` literal and on the
    ///   all-distinct assertion — which is the failure that matters, because a
    ///   client would then read "cannot ever be previewed" for a repository it
    ///   only needs to reopen.
    #[test]
    fn the_four_arms_serialize_to_four_distinct_outcome_tags() {
        let cases: [(Outcome, &str); 4] = [
            (graph_arm(), "graph"),
            (
                PreviewOutcome::Conflict {
                    paths: vec!["src/main.rs".into()],
                },
                "conflict",
            ),
            (
                PreviewOutcome::Unsupported {
                    operation: "RebaseBranch".into(),
                },
                "unsupported",
            ),
            (
                PreviewOutcome::Unavailable {
                    reason: PreviewUnavailable::RepositoryReadOnly,
                },
                "unavailable",
            ),
        ];

        let mut seen: Vec<String> = Vec::new();
        for (outcome, expected_tag) in cases {
            let json: Value = serde_json::to_value(&outcome).unwrap();
            assert_eq!(
                json.get("outcome").and_then(Value::as_str),
                Some(expected_tag),
                "wrong outcome tag — payload was {json}"
            );
            seen.push(expected_tag.to_string());

            let back: Outcome = serde_json::from_value(json).unwrap();
            assert_eq!(back, outcome, "{expected_tag} did not survive a round trip");
        }

        let mut distinct = seen.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            4,
            "two arms share a tag, so a client cannot tell them apart: {seen:?}"
        );
    }

    /// The invariant: `Unsupported` and `Unavailable { RepositoryReadOnly }`
    /// are not merely differently spelled — neither decodes as the other.
    ///
    /// This is the concrete form of the design decision. `Unsupported` means
    /// "nothing to do, ever"; `Unavailable { RepositoryReadOnly }` means
    /// "reopen in Active mode and it works". A client that confused them would
    /// send someone to the wrong place, and the confusion would look like
    /// correct behaviour on both sides.
    ///
    /// Both payloads are decoded into `Result`s before anything is asserted,
    /// and the `unsupported` one is asserted first. Both choices were forced by
    /// measurement, not taste: with an inline `unwrap` on the read-only payload
    /// the two mutations below aborted the test at the *same* line, and "it
    /// went red" is not the same as "it went red for this reason".
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M-P2a — REMOVES the arm from the decoder.** Add
    ///   `#[serde(skip_deserializing)]` to `Unavailable`. The `unsupported`
    ///   payload is untouched, so the first assertion passes; the read-only
    ///   payload comes back
    ///   `Err("unknown variant `unavailable`, expected one of `graph`,
    ///   `conflict`, `unsupported`")` and the **last** assertion is red.
    /// * **M-P2b — WEAKENS the gap between the arms.** Add
    ///   `#[serde(rename = "unavailable")]` to `Unsupported`, so both arms
    ///   answer to one tag and `Unsupported` — declared first — claims it.
    ///   Now *neither* payload decodes, and the **first** assertion is red:
    ///   the genuinely unsupported operation stopped decoding altogether,
    ///   while the read-only payload dies separately on
    ///   `Err("unknown field `reason`, expected `operation`")`. A different
    ///   line and a different error, which is what tells the two apart.
    #[test]
    fn a_read_only_repository_does_not_decode_as_an_unsupported_operation() {
        let read_only = serde_json::from_value::<Outcome>(json!({
            "outcome": "unavailable",
            "reason": { "unavailable": "repository_read_only" },
        }));
        let unsupported = serde_json::from_value::<Outcome>(json!({
            "outcome": "unsupported",
            "operation": "RebaseBranch",
        }));

        // The `unsupported` half is asserted FIRST, on purpose. See the
        // mutation notes: it is the only ordering under which the two
        // mutations below stop at different lines.
        assert!(
            matches!(unsupported, Ok(PreviewOutcome::Unsupported { ref operation }) if operation == "RebaseBranch"),
            "a genuinely unsupported operation decoded as {unsupported:?}"
        );

        assert!(
            matches!(
                read_only,
                Ok(PreviewOutcome::Unavailable {
                    reason: PreviewUnavailable::RepositoryReadOnly
                })
            ),
            "a read-only repository decoded as {read_only:?}; it must arrive as \
             the arm that means 'reopen in Active mode', never as one that \
             means 'nothing to do, ever'"
        );
    }

    /// The invariant, two halves of one house rule:
    ///
    /// 1. `changes` has **no** `#[serde(default)]`, so a `graph` payload that
    ///    omits it fails to decode rather than arriving as "nothing changed"
    ///    for an operation that changes everything.
    /// 2. `deny_unknown_fields` holds, so a stray key is refused rather than
    ///    quietly dropped.
    ///
    /// The positive case is asserted too — a rule that refused everything would
    /// satisfy both negatives.
    ///
    /// Note on what is deliberately *not* asserted: the error text. Internal
    /// tagging makes serde buffer the payload, so the message is not the plain
    /// "missing field `changes`" a bare struct would give; pinning it would
    /// pin serde's buffering, not this contract.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M-P3a — REMOVES the mechanism.** Add `#[serde(default)]` to
    ///   `changes`. Measured, this does not even build: serde's derive emits a
    ///   `C: Default` bound for a defaulted generic field, so it fails with
    ///   `error[E0277]: the trait bound \`PreviewChange: Default\` is not
    ///   satisfied` — the loudest possible red, and a happy accident of the
    ///   field being generic rather than something this test earned.
    /// * **M-P3b — WEAKENS the mechanism.** Drop `#[serde(deny_unknown_fields)]`
    ///   from `PreviewOutcome`. The stray key is silently ignored. Red on the
    ///   third assertion only, with the first still green — a different
    ///   failure, and the one a "does it decode?" test would never see.
    #[test]
    fn a_graph_payload_needs_its_changes_list_and_refuses_a_stray_key() {
        let half = serde_json::to_value(graph_half()).unwrap();

        let without_changes = json!({
            "outcome": "graph",
            "before": half,
            "after": half,
        });
        assert!(
            serde_json::from_value::<Outcome>(without_changes).is_err(),
            "a graph payload with no `changes` decoded — an absent list would \
             read as 'nothing changed' about an operation that changes everything"
        );

        let complete = json!({
            "outcome": "graph",
            "before": half,
            "after": half,
            "changes": [],
        });
        let decoded: Outcome = serde_json::from_value(complete).unwrap();
        match decoded {
            PreviewOutcome::Graph { changes, .. } => assert_eq!(changes.len(), 0),
            other => panic!("expected Graph, got {other:?}"),
        }

        let stray = json!({
            "outcome": "graph",
            "before": half,
            "after": half,
            "changes": [],
            "summary": "looks harmless",
        });
        assert!(
            serde_json::from_value::<Outcome>(stray).is_err(),
            "an unknown key was accepted; deny_unknown_fields is what stops a \
             newer field from being silently dropped by an older peer"
        );
    }

    /// The invariant: each unavailability reason carries its own `snake_case`
    /// tag, with its fields beside it. Literals, one per case.
    ///
    /// The whole point of this enum is that "could not compute" arrives as a
    /// *named* reason rather than as a confident statement about something
    /// else, and a reason nobody can tell from its neighbour is not named.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M-P4a — REMOVES the mechanism.** Drop
    ///   `#[serde(tag = "unavailable")]`. External tagging serializes the unit
    ///   variant as the bare string `"repository_read_only"` — not an object at
    ///   all — so `json.get("unavailable")` is `None`. Red on every case, and
    ///   red on the *shape* rather than on the spelling.
    /// * **M-P4b — WEAKENS the mechanism.** Drop `rename_all = "snake_case"`.
    ///   Every payload keeps its object shape and its `unavailable` key; only
    ///   the spellings change to `RepositoryReadOnly`, `GitTooOld` and so on.
    ///   Red on the four tag literals, green on the `found`/`minimum` field
    ///   assertions — the near-miss that a shape-only check would wave through.
    #[test]
    fn each_unavailable_reason_carries_its_own_snake_case_tag() {
        let cases = [
            (
                PreviewUnavailable::RepositoryReadOnly,
                "repository_read_only",
            ),
            (
                PreviewUnavailable::GitTooOld {
                    found: "2.34.1".into(),
                    minimum: "2.38".into(),
                },
                "git_too_old",
            ),
            (
                PreviewUnavailable::ScratchStore {
                    detail: "mkdir: permission denied".into(),
                },
                "scratch_store",
            ),
            (
                PreviewUnavailable::CheckFailed {
                    detail: "merge-tree exited with signal 9".into(),
                },
                "check_failed",
            ),
        ];

        for (reason, expected_tag) in cases {
            let json: Value = serde_json::to_value(&reason).unwrap();
            assert_eq!(
                json.get("unavailable").and_then(Value::as_str),
                Some(expected_tag),
                "wrong tag — payload was {json}"
            );
            let back: PreviewUnavailable = serde_json::from_value(json).unwrap();
            assert_eq!(back, reason);
        }

        // The one reason with structure: both fields reach the wire under their
        // own names, so a UI can say "found 2.34.1, needs 2.38" rather than
        // "too old".
        let too_old = serde_json::to_value(PreviewUnavailable::GitTooOld {
            found: "2.34.1".into(),
            minimum: "2.38".into(),
        })
        .unwrap();
        assert_eq!(too_old.get("found").and_then(Value::as_str), Some("2.34.1"));
        assert_eq!(too_old.get("minimum").and_then(Value::as_str), Some("2.38"));
    }

    /// The invariant: a conflicted path survives the wire byte-for-byte,
    /// including the `U+FFFD` a lossy decode leaves behind.
    ///
    /// `merge-tree -z`'s conflicted-file records are bytes; a path that is not
    /// UTF-8 is still a path the user needs named, and dropping it would turn
    /// `Conflict { paths: [ … ] }` into the `Conflict { paths: [] }` this
    /// type's own doc forbids.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M-P5a — REMOVES the field.** Change `Conflict { paths: Vec<String> }`
    ///   to a unit variant `Conflict`. The payload no longer decodes and the
    ///   match arm no longer binds — red at compile time here, which is the
    ///   loudest possible failure.
    /// * **M-P5b — WEAKENS the field.** Make it `#[serde(skip)] paths`. The arm
    ///   still exists and still decodes; `paths` arrives empty. Red on the
    ///   equality assertion at run time — "conflicted, nothing conflicted".
    #[test]
    fn a_conflict_names_its_paths_including_a_lossily_decoded_one() {
        let paths = vec![
            "src/main.rs".to_string(),
            "docs/na\u{fffd}me.md".to_string(),
            "with space/and'quote.txt".to_string(),
        ];
        let outcome: Outcome = PreviewOutcome::Conflict {
            paths: paths.clone(),
        };

        let wire = serde_json::to_string(&outcome).unwrap();
        let back: Outcome = serde_json::from_str(&wire).unwrap();
        match back {
            PreviewOutcome::Conflict { paths: got } => assert_eq!(got, paths),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    /// `PreviewGraph` carries the stub list and the *stub-inclusive*
    /// `lane_count`, and both survive the round trip.
    ///
    /// Stated as its own test because dropping either is invisible in a
    /// single-graph payload and only wrong beside the other half: an `after`
    /// graph with no stubs, beside a `before` graph with them, reads as "this
    /// operation deleted my branches", and a `lane_count` that excluded stub
    /// columns draws them off the edge of the gutter.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M-P6a — REMOVES the field.** Delete `stubs` from `PreviewGraph`.
    ///   Compile-time red here and at every construction site.
    /// * **M-P6b — WEAKENS the field.** Add `#[serde(skip)]` to `stubs`.
    ///   Measured, this is also a build failure — `#[serde(skip)]` needs
    ///   `S: Default` to reconstruct the field — so it reads
    ///   `error[E0277]: the trait bound \`S: Default\` is not satisfied`.
    ///   Red at compile time, one step earlier than M-P6a's missing-field
    ///   error at every construction site.
    #[test]
    fn a_preview_graph_keeps_its_stubs_and_its_stub_inclusive_lane_count() {
        let half = graph_half();
        let wire = serde_json::to_string(&half).unwrap();
        let back: PreviewGraph<GraphRow, Edge, BranchStub> = serde_json::from_str(&wire).unwrap();

        assert_eq!(back.stubs.len(), 1);
        assert_eq!(back.stubs[0].name, "spike");
        assert_eq!(back.stubs[0].lane, 2);
        assert_eq!(
            back.lane_count, 3,
            "lane_count is Graph::lane_count verbatim — wide enough for the \
             stub column at lane 2"
        );
        assert_eq!(back, half);
    }
}
