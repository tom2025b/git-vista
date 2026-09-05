//! Host tests for the plan-freshness decision (#555).

use super::*;

use git_vista_protocol::change_feed::{ChangeFeedHealth, WatchBudget};
use git_vista_protocol::{GenerationToken, RefName, UnixSeconds};

fn watching() -> ChangeFeedHealth {
    ChangeFeedHealth::Watching {
        watches: 12,
        budget: WatchBudget::Derived {
            watches: 4038,
            from_watches: 516_898,
            from_instances: 128,
        },
    }
}

/// Snapshots in these tests are numbered by the order `log_of` records them,
/// so the ordinary case is a continuous chain; the gap tests below stamp their
/// own sequence deliberately.
fn named(generation: &str, refs: &[&str], other: bool) -> ChangeFeedSnapshot {
    ChangeFeedSnapshot {
        seq: 0,
        generation: Some(GenerationToken::new(generation).unwrap()),
        health: watching(),
        changed: RefDelta::Named {
            refs: refs.iter().map(|r| RefName::new(*r).unwrap()).collect(),
            other,
        },
        at: UnixSeconds(1),
    }
}

fn unknown_delta(generation: &str) -> ChangeFeedSnapshot {
    ChangeFeedSnapshot {
        seq: 0,
        generation: Some(GenerationToken::new(generation).unwrap()),
        health: watching(),
        changed: RefDelta::Unknown,
        at: UnixSeconds(1),
    }
}

fn blind() -> ChangeFeedSnapshot {
    ChangeFeedSnapshot {
        seq: 0,
        generation: None,
        health: ChangeFeedHealth::Blind {
            reason: "git status could not be run".to_string(),
            since: UnixSeconds(5),
        },
        changed: RefDelta::Unknown,
        at: UnixSeconds(5),
    }
}

fn plan(generation: &str, expects: &[&str]) -> PlanOnScreen {
    PlanOnScreen {
        generation: generation.to_string(),
        expects: expects.iter().map(|e| (*e).to_string()).collect(),
    }
}

/// Record a run of snapshots as a **continuous** feed — seq 1, 2, 3 … — which
/// is what a client that received every publication holds.
fn log_of(snapshots: Vec<ChangeFeedSnapshot>) -> FeedLog {
    let mut log = FeedLog::new();
    for (n, snapshot) in snapshots.into_iter().enumerate() {
        log.record(ChangeFeedSnapshot {
            seq: n as u64 + 1,
            ..snapshot
        });
    }
    log
}

#[test]
fn a_plan_whose_generation_is_still_live_is_current_and_is_the_only_arm_that_is() {
    let log = log_of(vec![unknown_delta("100")]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Current);
    assert!(verdict.execute_offered());
    assert_eq!(freshness_headline(&verdict), None, "nothing to say");
}

#[test]
fn every_stale_arm_withdraws_the_execute_control() {
    // The panel may never be more optimistic than `enforce_fresh`, which
    // compares the whole digest and refuses on any movement.
    for verdict in [
        PlanFreshness::Moved {
            refs: vec!["refs/heads/main".to_string()],
        },
        PlanFreshness::Moved { refs: Vec::new() },
        PlanFreshness::MovedElsewhere,
        PlanFreshness::Unknown {
            reason: FeedUnavailable::NotConnected,
        },
        PlanFreshness::Unknown {
            reason: FeedUnavailable::Blind {
                reason: "unreadable".to_string(),
            },
        },
    ] {
        assert!(
            !verdict.execute_offered(),
            "{verdict:?} must not offer a button whose purpose is to fail"
        );
        assert!(
            freshness_headline(&verdict).is_some(),
            "{verdict:?} must say something"
        );
        assert!(rebuild_framing(&verdict).is_some());
    }
}

#[test]
fn a_ref_the_plan_names_moving_is_named_back_to_the_user() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/heads/main"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Moved {
            refs: vec!["refs/heads/main".to_string()]
        }
    );
    assert_eq!(
        freshness_headline(&verdict).unwrap(),
        "refs/heads/main moved while this was on screen."
    );
}

#[test]
fn a_ref_the_plan_does_not_name_moving_is_said_differently_and_still_refused() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::MovedElsewhere);
    assert!(!verdict.execute_offered());
    assert!(freshness_headline(&verdict)
        .unwrap()
        .contains("not in a way this operation depends on"));
}

#[test]
fn a_working_tree_change_is_never_called_irrelevant() {
    // The reassuring sentence has to be earned. A worktree/index/stash move
    // names no ref and can still change what a commit writes, so it takes the
    // arm that claims least.
    let log = log_of(vec![unknown_delta("100"), named("101", &[], true)]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Moved { refs: Vec::new() });
    assert_eq!(
        freshness_headline(&verdict).unwrap(),
        "The repository changed while this was on screen."
    );
}

#[test]
fn a_gap_in_what_the_client_saw_cannot_produce_the_reassuring_answer() {
    // The client reconnected: the first snapshot after the gap can name
    // nothing. Anything that happened during the outage is unaccounted for, so
    // "not in a way this operation depends on" is not available.
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
        unknown_delta("102"),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Moved { refs: Vec::new() });
}

#[test]
fn a_plan_generation_the_client_never_saw_cannot_be_differenced() {
    let log = log_of(vec![named("101", &["refs/tags/v9"], false)]);
    let verdict = freshness(&plan("nobody-saw-this", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Moved { refs: Vec::new() });
}

#[test]
fn a_blind_feed_says_it_could_not_tell_rather_than_that_nothing_changed() {
    let log = log_of(vec![unknown_delta("100"), blind()]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Unknown {
            reason: FeedUnavailable::Blind {
                reason: "git status could not be run".to_string()
            }
        },
        "the plan's own generation is still the live one as far as anybody \
         knows — and that is exactly the reasoning ADR 0055 forbids"
    );
}

#[test]
fn no_feed_at_all_is_unknown_and_not_current() {
    let verdict = freshness(&plan("100", &[]), &FeedLog::new());
    assert_eq!(
        verdict,
        PlanFreshness::Unknown {
            reason: FeedUnavailable::NotConnected
        }
    );
}

#[test]
fn a_dropped_stream_forgets_what_it_saw_rather_than_differencing_across_the_gap() {
    let mut log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
    ]);
    log.clear();
    assert!(log.is_empty());
    log.record(named("102", &["refs/tags/v9"], false));
    // 100 is no longer in the log, so nothing can be named across the outage.
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::Moved { refs: Vec::new() }
    );
}

#[test]
fn several_moves_accumulate_and_the_ref_the_plan_names_is_still_found() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
        named("102", &["refs/remotes/origin/main"], false),
        named("103", &["refs/heads/main"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Moved {
            refs: vec!["refs/heads/main".to_string()]
        },
        "the plan was built three snapshots ago; the whole span is differenced"
    );
}

#[test]
fn a_plan_that_returns_to_its_own_generation_is_current_again() {
    // A ref moved and moved back. `enforce_fresh` would admit this plan — the
    // digest is equal — so the panel must not be more pessimistic than the gate
    // either.
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/heads/main"], false),
        named("100", &["refs/heads/main"], false),
    ]);
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::Current
    );
}

#[test]
fn the_log_is_bounded_and_drops_the_oldest_first() {
    let mut log = FeedLog::new();
    for n in 0..(LOG_DEPTH + 5) {
        log.record(ChangeFeedSnapshot {
            seq: n as u64 + 1,
            ..named(&n.to_string(), &[], false)
        });
    }
    assert_eq!(
        log.latest().unwrap().generation.as_ref().unwrap().as_str(),
        (LOG_DEPTH + 4).to_string()
    );
    assert!(
        log.moved_since("0").is_none(),
        "a generation older than the log cannot be differenced against"
    );
    assert!(log.moved_since(&(LOG_DEPTH + 3).to_string()).is_some());
}

#[test]
fn two_named_refs_read_as_a_sentence() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/heads/main", "refs/heads/side"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main", "refs/heads/side"]), &log);
    assert_eq!(
        freshness_headline(&verdict).unwrap(),
        "refs/heads/main and refs/heads/side moved while this was on screen."
    );
}

#[test]
fn a_confirmation_with_no_plan_on_screen_is_left_exactly_as_it_was() {
    // Most confirmations in this app have no preview and therefore no plan.
    // This feature must make no claim about them: a feed that has not connected
    // yet would otherwise disable half the dialogs in the app.
    assert!(confirm_enabled(true, None));
    assert!(!confirm_enabled(false, None));
    assert_eq!(blocked_by_staleness(None), None);
}

#[test]
fn a_stale_plan_withdraws_the_confirmation_and_says_which_kind_of_stale() {
    let moved = PlanFreshness::Moved {
        refs: vec!["refs/heads/main".to_string()],
    };
    assert!(!confirm_enabled(true, Some(&moved)));
    assert!(blocked_by_staleness(Some(&moved))
        .unwrap()
        .contains("moved after this picture was drawn"));

    let unknown = PlanFreshness::Unknown {
        reason: FeedUnavailable::NotConnected,
    };
    assert!(!confirm_enabled(true, Some(&unknown)));
    assert!(blocked_by_staleness(Some(&unknown))
        .unwrap()
        .contains("isn't known"));

    // Reassuring, and still refused — `enforce_fresh` compares the whole
    // digest, so this plan would 409.
    assert!(!confirm_enabled(true, Some(&PlanFreshness::MovedElsewhere)));
    assert!(blocked_by_staleness(Some(&PlanFreshness::MovedElsewhere)).is_some());
}

#[test]
fn a_current_plan_leaves_the_dialogs_own_verdict_alone() {
    assert!(confirm_enabled(true, Some(&PlanFreshness::Current)));
    assert!(
        !confirm_enabled(false, Some(&PlanFreshness::Current)),
        "freshness may withdraw a confirmation, never grant one"
    );
    assert_eq!(blocked_by_staleness(Some(&PlanFreshness::Current)), None);
}

// --- the seam census -------------------------------------------------------
//
// `core.rs` being pure and host-tested does not prove the wasm-only code asks
// it the questions it answers. `cargo test` compiles none of the three files
// below, so what binds them is a source-level census — the habit
// `features/mod.rs` describes, applied at the moment a decision moved out of a
// view file. It is deliberately about *specific callers asking specific
// functions*, not about purity in general.

/// The wasm-only reactive wrapper: one `EventSource`, and the log the decision
/// reads.
const FEED_SIGNALS: &str = include_str!("signals.rs");

/// The wasm-only preview wrapper: where the plan on screen is remembered.
const PREVIEW_SIGNALS: &str = include_str!("../preview/signals.rs");

/// The wasm-only confirm dialog: where the verdict withdraws a button.
const CONFIRM_DIALOG: &str = include_str!("../../dialogs/confirm.rs");

#[test]
fn the_feed_subscription_asks_core_for_the_verdict_and_forgets_across_a_gap() {
    assert!(
        FEED_SIGNALS.contains("freshness(plan, log)"),
        "the wasm wrapper must ask `core::freshness`, never re-derive the \
         verdict where no host test compiles it"
    );
    assert!(
        FEED_SIGNALS.contains("log.clear()"),
        "a dropped stream must forget what it saw — differencing across a gap \
         is how the reassuring sentence gets printed over changes nobody saw"
    );
    assert!(
        FEED_SIGNALS.contains("PROTOCOL_QUERY"),
        "EventSource cannot set headers, so the version rides in the query \
         string — the server allows it for this path alone"
    );
}

#[test]
fn the_preview_remembers_the_plan_it_drew_and_drops_it_with_the_picture() {
    assert!(
        PREVIEW_SIGNALS.contains("PlanOnScreen {"),
        "the plan's generation and expected refs are what make a freshness \
         question answerable at all"
    );
    assert!(
        PREVIEW_SIGNALS.contains("expected_ref_changes"),
        "the refs the plan names are what separate `Moved` from \
         `MovedElsewhere`"
    );
    let clear = PREVIEW_SIGNALS
        .split("pub fn clear(&self)")
        .nth(1)
        .expect("Preview::clear exists");
    assert!(
        clear.contains("self.plan.set(None)"),
        "clearing the panel must clear the plan: a generation left behind \
         answers the next dialog's freshness question with the last one's plan"
    );
}

#[test]
fn the_confirm_dialog_composes_the_two_halves_rather_than_reimplementing_them() {
    assert!(
        CONFIRM_DIALOG.contains("confirm_enabled(enabled, plan_freshness.as_ref())"),
        "the composition itself must be the host-tested function — an `&&` \
         written in this file is exactly #612's origin"
    );
    assert!(
        CONFIRM_DIALOG.contains("blocked_by_staleness(plan_freshness.as_ref())"),
        "a withdrawn button must carry its reason; #65's finding is that an \
         unspoken one is unreachable by the user it was written for"
    );
    assert!(
        CONFIRM_DIALOG.contains("freshness_notice_view(preview, freshness)"),
        "and the notice must actually be rendered, above the picture it \
         invalidates"
    );
    let notice = CONFIRM_DIALOG
        .find("freshness_notice_view(preview, freshness)")
        .expect("the notice is rendered");
    let panel = CONFIRM_DIALOG
        .find("preview_panel_view(preview)")
        .expect("the picture is rendered");
    assert!(
        notice < panel,
        "the staleness warning goes ABOVE the picture: printed under it, a \
         reader meets it after they have already believed the picture"
    );
}

// --- #664 review, finding 3: a delta read across a gap is not a delta ------

/// Record snapshots at exactly the sequence numbers given — a client that
/// missed the ones in between.
fn log_at(numbered: Vec<(u64, ChangeFeedSnapshot)>) -> FeedLog {
    let mut log = FeedLog::new();
    for (seq, snapshot) in numbered {
        log.record(ChangeFeedSnapshot { seq, ..snapshot });
    }
    log
}

#[test]
fn a_publication_this_client_never_received_cannot_produce_the_reassuring_answer() {
    // codex's reproduction, as a test. The feed's transport keeps only the
    // latest value, so a slow reader skips publications WITHOUT disconnecting:
    // `refs/heads/main` moves in publication 2, an unrelated tag moves in
    // publication 3, and this client polls once and sees only 3.
    //
    // Read as a chain, that says "only a tag moved" — and a plan expecting
    // `main` is told the repository moved "but not in a way this operation
    // depends on". The button is still withdrawn, so nothing unsafe happens;
    // the EXPLANATION is false, which is this milestone's own failure shape
    // pointed at itself.
    let log = log_at(vec![
        (1, unknown_delta("100")),
        // publication 2 — naming refs/heads/main — never arrived
        (3, named("102", &["refs/tags/v9"], false)),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Moved { refs: Vec::new() },
        "a gap in the sequence means this client cannot name what moved"
    );
    assert_ne!(
        verdict,
        PlanFreshness::MovedElsewhere,
        "and it certainly cannot say the change was irrelevant"
    );
}

#[test]
fn a_continuous_run_is_still_read_as_a_chain() {
    // The other half: the fix must not make every delta unusable. A client that
    // received every publication reads them exactly as before.
    let log = log_at(vec![
        (1, unknown_delta("100")),
        (2, named("101", &["refs/tags/v9"], false)),
        (3, named("102", &["refs/tags/v10"], false)),
    ]);
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::MovedElsewhere,
        "two tags moved, this plan names neither, and nothing was missed"
    );
}

#[test]
fn the_first_snapshot_on_a_stream_is_never_read_as_a_delta() {
    // A client that connects to a feed already running receives the current
    // snapshot, whose `changed` is a difference against a publication made
    // before this client existed.
    let log = log_at(vec![(9, named("102", &["refs/tags/v9"], false))]);
    assert!(
        log.moved_since("102").is_some(),
        "the reading itself is perfectly good"
    );
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::Moved { refs: Vec::new() },
        "but its delta describes a span this client did not watch"
    );
}
