//! The preview core's suite (M10.08 A6, #594).
//!
//! Included from [`super`] with `#[path]`, so it is a child of
//! `features::preview::core` and can see everything private there. Host tests:
//! this core is framework-free by design, so none of it needs a browser.
//!
//! # What is proven here
//!
//! * Each of the four [`PreviewOutcome`] arms produces its **own**
//!   [`PreviewView`] arm — the honesty the engine established in #576, carried
//!   to the last surface before a person sees it.
//! * A commit that is both added and a ref target keeps **both** facts.
//! * A lane shift preserves its direction — `from` and `to` are both `usize`,
//!   so a transposition compiles and round-trips silently.
//! * The summary says "nothing would change" out loud rather than rendering an
//!   absence, and never reports "nothing" for a change it cannot name.

use super::*;

use git_vista_core::model::Oid;

fn oid(d: char) -> Oid {
    Oid((0..40).map(|_| d).collect())
}

fn empty_half() -> Half {
    PreviewGraph {
        rows: Vec::new(),
        edges: Vec::new(),
        stubs: Vec::new(),
        lane_count: 0,
    }
}

fn graph_of(changes: Vec<PreviewChange>) -> PreviewResponse {
    PreviewOutcome::Graph {
        before: empty_half(),
        after: empty_half(),
        changes,
    }
}

/// **The load-bearing test.** Four outcome arms, four distinct view arms.
///
/// The one failure this exists to prevent is a future arm — or a refactor —
/// quietly landing a `Conflict` in the same bucket as an `Unavailable`. Those
/// mean opposite things: one is git having run and answered "no", the other is
/// git never having produced an answer at all. A user who cannot tell them
/// apart has lost exactly what #576 was built to give them.
///
/// # Two mutations
///
/// 1. **Removes the distinction** — map `Conflict` to
///    `PreviewView::Unavailable { .. }`. The `Conflict` assertion goes red.
/// 2. **Weakens it** — map `Unsupported` to `Unavailable` too. A different
///    assertion goes red, on a different arm, so the two failures are
///    distinguishable in the output rather than looking like one break.
#[test]
fn every_outcome_arm_gets_its_own_view() {
    assert!(
        matches!(view_of(graph_of(Vec::new())), PreviewView::Picture(_)),
        "a Graph outcome must produce a picture"
    );
    assert!(
        matches!(
            view_of(PreviewOutcome::Conflict {
                paths: vec!["readme.md".into()]
            }),
            PreviewView::Conflict { .. }
        ),
        "a Conflict is a live established fact and must stay its own arm — \
         never folded into Unavailable, which means the opposite"
    );
    assert!(
        matches!(
            view_of(PreviewOutcome::Unsupported {
                operation: "Rebase".into()
            }),
            PreviewView::Unsupported { .. }
        ),
        "Unsupported is a permanent fact about the operation, not a failure here"
    );
    assert!(
        matches!(
            view_of(PreviewOutcome::Unavailable {
                reason: PreviewUnavailable::RepositoryReadOnly
            }),
            PreviewView::Unavailable { .. }
        ),
        "Unavailable keeps its own arm so its named reason survives to the user"
    );
}

/// A conflict carries the paths through untouched. The server guarantees this
/// vec is never empty in this arm; the panel must not lose it, because the
/// filename is the entire actionable content of the answer.
#[test]
fn a_conflict_keeps_every_path_it_was_given() {
    let view = view_of(PreviewOutcome::Conflict {
        paths: vec!["readme.md".into(), "src/lib.rs".into()],
    });
    match view {
        PreviewView::Conflict { paths } => {
            assert_eq!(
                paths,
                vec!["readme.md".to_string(), "src/lib.rs".to_string()]
            )
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

/// The hypothetical commit is both **added** and the place the branch lands.
/// Both facts must survive; an enum would have forced one to win.
#[test]
fn a_commit_that_is_added_and_a_ref_target_keeps_both_facts() {
    let view = view_of(graph_of(vec![
        PreviewChange::Added { commit: oid('9') },
        PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid('3'),
            to: oid('9'),
        },
        PreviewChange::RefMoved {
            ref_name: "HEAD".into(),
            from: oid('3'),
            to: oid('9'),
        },
    ]));
    let PreviewView::Picture(p) = view else {
        panic!("expected a picture")
    };
    let mark = p.marks.get(&oid('9').0).expect("the new commit is marked");
    assert!(mark.added, "it is a commit that does not exist yet");
    assert_eq!(
        mark.refs_landed,
        vec!["main".to_string(), "HEAD".to_string()],
        "and both refs land on it, in the order the server listed them"
    );
    assert!(mark.is_marked());
}

/// A lane shift keeps its direction.
///
/// `from_lane` and `to_lane` are both `usize`, so transposing them compiles,
/// round-trips, and renders — it is only wrong. The same hazard the core's own
/// `LaneShift` conversion is pinned against, one layer up.
#[test]
fn a_lane_shift_reaches_the_mark_without_transposing() {
    let view = view_of(graph_of(vec![PreviewChange::LaneShifted {
        commit: oid('2'),
        from_lane: 0,
        to_lane: 3,
    }]));
    let PreviewView::Picture(p) = view else {
        panic!("expected a picture")
    };
    assert_eq!(
        p.marks.get(&oid('2').0).and_then(|m| m.lane_shift),
        Some((0, 3)),
        "0 -> 3, not 3 -> 0"
    );
}

/// An empty change list is a **claim** — this operation changes nothing — and
/// the panel says so in words rather than showing an unexplained blank.
#[test]
fn no_changes_is_stated_not_left_blank() {
    let PreviewView::Picture(p) = view_of(graph_of(Vec::new())) else {
        panic!("expected a picture")
    };
    assert_eq!(p.summary, "Nothing would change.");
    assert!(p.marks.is_empty());
}

/// The ordinary case reads as a sentence a person can act on.
#[test]
fn the_summary_reads_as_plain_english() {
    let PreviewView::Picture(p) = view_of(graph_of(vec![
        PreviewChange::Added { commit: oid('9') },
        PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid('3'),
            to: oid('9'),
        },
    ])) else {
        panic!("expected a picture")
    };
    assert_eq!(p.summary, "one new commit and main moves.");
}

/// Three parts join with commas and a final "and" — checked because the
/// list-joining is hand-rolled and an off-by-one there reads as a typo to
/// everyone who sees it.
#[test]
fn three_parts_join_as_a_list() {
    let PreviewView::Picture(p) = view_of(graph_of(vec![
        PreviewChange::Added { commit: oid('9') },
        PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid('3'),
            to: oid('9'),
        },
        PreviewChange::LaneShifted {
            commit: oid('2'),
            from_lane: 1,
            to_lane: 0,
        },
    ])) else {
        panic!("expected a picture")
    };
    assert_eq!(
        p.summary,
        "one new commit, main moves and one commit changes lane."
    );
}

/// Every `Unavailable` reason arrives with a named headline, and the two the
/// user can act on arrive with a remedy. `CheckFailed` deliberately has none:
/// its meaning is "a git step ran and did not answer", and inventing advice for
/// the one arm defined by not knowing would be a guess dressed as help.
#[test]
fn each_unavailable_reason_is_named_and_only_the_actionable_ones_advise() {
    let cases = [
        (PreviewUnavailable::RepositoryReadOnly, true),
        (
            PreviewUnavailable::GitTooOld {
                found: "2.37.1".into(),
                minimum: "2.38".into(),
            },
            true,
        ),
        (
            PreviewUnavailable::ScratchStore {
                detail: "could not create the store".into(),
            },
            false,
        ),
        (
            PreviewUnavailable::CheckFailed {
                detail: "git commit-tree printed no commit oid".into(),
            },
            false,
        ),
    ];
    for (reason, expect_remedy) in cases {
        let named = format!("{reason:?}");
        let view = view_of(PreviewOutcome::Unavailable { reason });
        let PreviewView::Unavailable {
            headline, remedy, ..
        } = view
        else {
            panic!("{named} must stay in the Unavailable arm")
        };
        assert!(
            !headline.is_empty(),
            "{named} needs a headline a person reads"
        );
        assert_eq!(
            remedy.is_some(),
            expect_remedy,
            "{named}: a remedy must appear exactly when there is something to do"
        );
    }
}

/// The version numbers reach the text. A headline that said "your git is too
/// old" without saying which version, or what it needs, would be true and
/// useless.
#[test]
fn the_too_old_reason_names_both_versions() {
    let PreviewView::Unavailable {
        headline, remedy, ..
    } = view_of(PreviewOutcome::Unavailable {
        reason: PreviewUnavailable::GitTooOld {
            found: "2.37.1".into(),
            minimum: "2.38".into(),
        },
    })
    else {
        panic!("expected Unavailable")
    };
    assert!(headline.contains("2.37.1"), "says what this host has");
    assert!(
        remedy.unwrap_or_default().contains("2.38"),
        "and what it needs"
    );
}

/// The preview never gates the operation, in any arm. Stated as a test so the
/// rule has something holding it down: a future reader who gates the confirm
/// button on a picture existing breaks this, which is the point.
#[test]
fn no_outcome_ever_blocks_the_operation() {
    let views = [
        view_of(graph_of(Vec::new())),
        view_of(PreviewOutcome::Conflict {
            paths: vec!["a".into()],
        }),
        view_of(PreviewOutcome::Unsupported {
            operation: "Rebase".into(),
        }),
        view_of(PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::RepositoryReadOnly,
        }),
    ];
    for v in &views {
        assert!(
            v.advisory_only(),
            "a preview informs; it has never decided whether an operation may run"
        );
    }
    assert!(views[0].has_picture(), "only the Graph arm has a picture");
    assert!(
        !views[1].has_picture(),
        "a Conflict is a successful preview with no picture — not a failed one"
    );
}

/// Which dialogs get a preview, and which get none.
///
/// The mapping matters twice over: asking for a preview the engine cannot give
/// spends two round trips to be told `Unsupported`, and *not* asking where it
/// can is exactly the omission that created #594 in the first place.
///
/// # Two mutations
///
/// 1. **REMOVES the mapping** — return `None` from the `Merge` arm. The merge
///    assertion goes red; the revert one still passes, so the arm that broke
///    is named rather than the whole function.
/// 2. **WEAKENS the mapping** — have `NotPreviewable` fall through to
///    `Merge { branch: "HEAD" }`. Both positive assertions still pass and the
///    negative one goes red — a dialog with nothing to preview would otherwise
///    silently ask the server about a branch nobody named.
///
/// # Why the cherry-pick assertion names the variant, not just the id
///
/// `GitOperation::CherryPick` and `GitOperation::RevertCommit` are the same
/// shape — one `CommitOid` each — so mapping cherry-pick to `RevertCommit`
/// compiles, round-trips, and previews the exact inverse of what the dialog
/// is about to do. A test that only checked the commit id would pass through
/// that. This one matches on the variant, which is the part that can be wrong.
///
/// Proved: returning `None` from the `CherryPick` arm, and mapping it to
/// `RevertCommit`, were both run through `failure-atlas`'s `mutation_check`
/// and both came back `caught`.
#[test]
fn only_the_three_wired_dialogs_ask_for_a_preview() {
    assert!(
        matches!(
            previewable(DialogSubject::Merge { branch: "feature" }),
            Some(GitOperation::MergeBranch { ref branch }) if branch.as_str() == "feature"
        ),
        "a merge confirmation must ask for a preview"
    );
    let commit = "a".repeat(40);
    assert!(
        matches!(
            previewable(DialogSubject::Revert { commit: &commit }),
            Some(GitOperation::RevertCommit { ref commit }) if commit.as_str() == "a".repeat(40)
        ),
        "a revert confirmation must ask for a preview"
    );
    let picked = "b".repeat(40);
    assert!(
        matches!(
            previewable(DialogSubject::CherryPick { commit: &picked }),
            Some(GitOperation::CherryPick { ref commit }) if commit.as_str() == "b".repeat(40)
        ),
        "a cherry-pick confirmation must ask for a preview, and must ask for a \
         CherryPick — RevertCommit is the same shape and the exact inverse"
    );
    assert_eq!(
        previewable(DialogSubject::NotPreviewable),
        None,
        "a dialog the engine cannot preview must not spend two round trips \
         being told so"
    );
}

/// A name the newtypes refuse yields no preview, never a panic.
///
/// A panic inside a Leptos view is a blank app, not a diagnostic — and the
/// operation itself is still confirmable, because refusing a malformed name is
/// the server's job, not a preview panel's.
///
/// # Two mutations
///
/// 1. **REMOVES the guard** — `BranchName::new(branch).unwrap()`. The empty
///    branch name panics and the test is red on the panic itself.
/// 2. **WEAKENS the guard** — fall back to a placeholder
///    (`BranchName::new("HEAD").unwrap()`) when validation fails. Nothing
///    panics, so a panic-only check would pass; the `None` assertions go red,
///    which is the failure that matters — a preview of an operation the user
///    never asked for.
#[test]
fn a_name_the_newtypes_refuse_yields_no_preview_rather_than_a_panic() {
    assert_eq!(previewable(DialogSubject::Merge { branch: "" }), None);
    assert_eq!(previewable(DialogSubject::Revert { commit: "nope" }), None);
    assert_eq!(
        previewable(DialogSubject::CherryPick { commit: "nope" }),
        None
    );
}

/// Every arm without a picture says the operation is still available; the one
/// with a picture says nothing.
///
/// This is the sentence that stops a conflict list or a "git is too old" note
/// from reading as a refusal of the operation. It is wired to
/// [`PreviewView::advisory_only`] deliberately: a future change that let a
/// preview gate an operation would stop the promise being printed rather than
/// leave it printed under a dead button.
///
/// # Two mutations
///
/// 1. **REMOVES the wiring** — delete the `advisory_only` guard. Every
///    assertion still passes, which is why the *second* mutation is the
///    load-bearing one; run it with `advisory_only` returning `false` and the
///    three no-picture assertions go red exactly as designed.
/// 2. **WEAKENS the coverage** — return `None` for `Unavailable` too. The
///    `Conflict` and `Unsupported` assertions pass and the `Unavailable` one
///    is red — the arm where the sentence matters most, because it is the one
///    that names the host rather than the request.
#[test]
fn every_pictureless_arm_promises_the_operation_is_still_available() {
    let conflict = PreviewView::Conflict {
        paths: vec!["a.rs".into()],
    };
    let unsupported = PreviewView::Unsupported {
        operation: "Rebase".into(),
    };
    let unavailable = view_of(PreviewOutcome::Unavailable {
        reason: PreviewUnavailable::GitTooOld {
            found: "2.34.1".into(),
            minimum: "2.38".into(),
        },
    });

    for (view, name) in [
        (&conflict, "Conflict"),
        (&unsupported, "Unsupported"),
        (&unavailable, "Unavailable"),
    ] {
        let note = reassurance(view)
            .unwrap_or_else(|| panic!("{name} left the user with no picture and no reassurance"));
        assert!(
            note.contains("still available"),
            "{name}'s note does not say the operation is still available: {note}"
        );
    }

    assert_eq!(
        reassurance(&view_of(graph_of(Vec::new()))),
        None,
        "a picture needs no reassurance — the graph is the answer"
    );
    // The rule the note depends on, stated where a reader of this test finds it.
    assert!(conflict.advisory_only());
    assert!(!conflict.has_picture());
    assert!(view_of(graph_of(Vec::new())).has_picture());
}
