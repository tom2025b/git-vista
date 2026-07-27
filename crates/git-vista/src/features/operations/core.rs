//! The in-flight operations registry — framework-free (M1.11 D1).

use crate::features::core_traits::RequestKey;
use crate::features::operations::kind::OperationKind;

/// Mints the monotone sequence that orders user intents by *click* time.
///
/// The graph epoch cannot do this job. Two menu taps land in the same epoch — nothing
/// invalidates the graph between them — so ordering by epoch would leave every tie to the
/// incoming write, which is precisely the network-order bug being fixed. A counter that
/// advances once per user action is the only thing that records what the user did last.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IntentSeq(u64);

impl IntentSeq {
    /// Mint the next sequence. Called synchronously at click time, before any `await`.
    pub fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Whether a result stamped `seq` may overwrite the one currently shown, stamped
/// `shown_seq`.
///
/// The same ordering rule as [`latest_wins`], for the places that display a *result* rather
/// than raise an operation — the repo picker's one status line, written by both the
/// delete-clone handler and the Rescan button (`picker.rs`). Without a stamp the line shows
/// whichever request answered last, which after a quick Delete-then-Rescan is the wrong one.
pub fn result_is_newest(shown_seq: u64, seq: u64) -> bool {
    seq >= shown_seq
}

/// A user intent that has been raised but whose pre-check has not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingIntent {
    /// Click order, from [`IntentSeq::next`]. Minted before the `spawn_local`, so it
    /// records when the user *acted*, not when the network *answered*.
    pub seq: u64,
    /// Which repository state the intent was raised against, so a repository switch or a
    /// generation bump can strand it even when it is the newest intent.
    pub key: RequestKey,
    pub kind: OperationKind,
}

/// Whether `incoming` may replace `current`.
///
/// Fixes the `menu.rs` race (design spec §3): every branch item does a live
/// `fetch_head_branch()` pre-check and today writes `confirm_op` unconditionally in its
/// continuation (`menu.rs:352-363,378-389,422-433,540-548`), so dialogs open in *network*
/// order rather than *click* order — tap Checkout then Merge, and a slow Checkout pre-check
/// reopens the Checkout dialog over the Merge one the user is looking at.
///
/// This is only half the gate. A caller must also check
/// [`RequestKey::is_current`](crate::features::core_traits::RequestKey::is_current), which
/// strands an intent whose repository moved underneath it. Sequence answers "did the user
/// ask for something newer?"; the key answers "is what they asked for still meaningful?".
///
/// Ties go to `incoming`: sequences are unique in practice, and admitting the later of two
/// equal values keeps the function total without a special case.
pub fn latest_wins(current: Option<&PendingIntent>, incoming: &PendingIntent) -> bool {
    match current {
        None => true,
        Some(cur) => incoming.seq >= cur.seq,
    }
}

#[cfg(test)]
mod core_tests {
    use super::*;
    use git_vista_protocol::operation::OperationStage;

    fn key(s: &str) -> IdempotencyKey {
        IdempotencyKey::new(s).expect("valid idempotency key")
    }

    fn id(s: &str) -> OperationId {
        OperationId::new(s).expect("valid operation id")
    }

    fn merge() -> OperationKind {
        OperationKind::Merge {
            branch: "feature".into(),
            into: Some("main".into()),
        }
    }

    fn succeeded(generation: &str) -> Settlement {
        Settlement {
            state: OperationState::Succeeded,
            message: None,
            generation: Some(GenerationToken::new(generation).expect("valid generation")),
        }
    }

    /// An admitted operation whose server id is already bound — the state every test
    /// that cares about progress or settlement starts from.
    fn running() -> OperationsCore {
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).expect("first admit accepted");
        c.bind_id(&key("k1"), id("op-1")).expect("bind accepted");
        c
    }

    #[test]
    fn an_admitted_operation_is_in_flight() {
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).expect("first admit accepted");
        assert_eq!(c.in_flight().count(), 1);
    }

    #[test]
    fn readmitting_the_same_key_is_a_noop_not_a_second_operation() {
        // ADR 0020: a key is minted per USER ACTION and reused across network retries. A
        // retry must never become a second operation.
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).unwrap();
        let applied = c.admit(key("k1"), merge()).expect("retry accepted");
        assert_eq!(applied, Applied::NoChange);
        assert_eq!(c.in_flight().count(), 1, "a retry is the same operation");
    }

    #[test]
    fn reusing_a_key_with_a_different_operation_is_refused() {
        // Mirrors the server's own 409 (ADR 0020): a key alone is a footgun.
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).unwrap();
        let err = c
            .admit(
                key("k1"),
                OperationKind::Delete {
                    branch: "other".into(),
                    current: None,
                },
            )
            .unwrap_err();
        assert_eq!(err, OperationsRejection::KeyBoundToDifferentOperation);
        assert_eq!(c.in_flight().count(), 1, "the refused admit changed nothing");
    }

    #[test]
    fn settling_yields_the_post_execution_generation_as_an_invalidation() {
        // The criterion-4 datum: reconcile against the generation the server observed
        // AFTER execution, instead of blindly re-reading everything.
        let mut c = running();
        let inv = c.settle(&id("op-1"), succeeded("77")).expect("settle accepted");
        assert_eq!(inv.generation.as_ref().map(|g| g.as_str()), Some("77"));
        assert_eq!(inv.scope, InvalidateScope::Everything);
        assert_eq!(
            c.in_flight().count(),
            0,
            "a settled operation is no longer in flight"
        );
    }

    #[test]
    fn settling_an_unknown_id_is_refused_and_changes_nothing() {
        let mut c = running();
        let err = c.settle(&id("nope"), succeeded("77")).unwrap_err();
        assert_eq!(err, OperationsRejection::UnknownOperation);
        assert_eq!(c.in_flight().count(), 1);
    }

    #[test]
    fn settling_twice_is_refused_so_a_reconnected_stream_cannot_double_apply() {
        // A resumed SSE stream replays the terminal event. Applying it twice must not
        // publish a second invalidation — which would bump the graph epoch again and
        // re-read the whole repository for nothing.
        let mut c = running();
        c.settle(&id("op-1"), succeeded("77")).unwrap();
        let err = c.settle(&id("op-1"), succeeded("77")).unwrap_err();
        assert_eq!(err, OperationsRejection::AlreadySettled);
    }

    #[test]
    fn an_operation_survives_observation_of_every_stage() {
        // Criterion 2 in core form: nothing about a panel appears here, so nothing a panel
        // does can drop this state.
        let mut c = running();
        for stage in [
            OperationStage::Queued,
            OperationStage::Planning,
            OperationStage::Waiting,
            OperationStage::Checking,
            OperationStage::Executing,
        ] {
            c.observe(&id("op-1"), OperationState::Running, stage)
                .expect("stage accepted");
        }
        assert_eq!(c.in_flight().count(), 1);
        let live = c.in_flight().next().unwrap();
        assert_eq!(live.stage, OperationStage::Executing);
        assert_eq!(live.state, OperationState::Running);
    }

    #[test]
    fn observing_the_same_stage_twice_reports_no_change() {
        // The stream heartbeats and can repeat; a repeat is not a transition.
        let mut c = running();
        c.observe(&id("op-1"), OperationState::Running, OperationStage::Planning)
            .unwrap();
        let applied = c
            .observe(&id("op-1"), OperationState::Running, OperationStage::Planning)
            .unwrap();
        assert_eq!(applied, Applied::NoChange);
    }

    #[test]
    fn a_failed_operation_stays_visible_after_it_settles() {
        // Task 5 replaces the native `window.alert()` failure path with reactive state.
        // That only works if a failure survives settlement instead of vanishing.
        let mut c = running();
        c.settle(
            &id("op-1"),
            Settlement {
                state: OperationState::Failed,
                message: Some("not fully merged".into()),
                generation: None,
            },
        )
        .expect("a failure is an outcome, so it settles");
        assert_eq!(c.in_flight().count(), 0);
        let settled: Vec<_> = c.recent().collect();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].outcome.state, OperationState::Failed);
        assert_eq!(settled[0].outcome.message.as_deref(), Some("not fully merged"));
    }

    #[test]
    fn a_settled_entry_can_be_dismissed() {
        let mut c = running();
        c.settle(&id("op-1"), succeeded("77")).unwrap();
        assert_eq!(c.dismiss(&id("op-1")), Applied::Committed);
        assert_eq!(c.recent().count(), 0);
        assert_eq!(
            c.dismiss(&id("op-1")),
            Applied::NoChange,
            "dismissing twice is harmless"
        );
    }

    #[test]
    fn the_settled_list_is_bounded_so_a_long_session_cannot_grow_without_limit() {
        let mut c = OperationsCore::default();
        for n in 0..(MAX_RECENT + 3) {
            let k = key(&format!("k{n}"));
            let i = id(&format!("op-{n}"));
            c.admit(k.clone(), merge()).unwrap();
            c.bind_id(&k, i.clone()).unwrap();
            c.settle(&i, succeeded("77")).unwrap();
        }
        assert_eq!(c.recent().count(), MAX_RECENT);
        assert_eq!(
            c.recent().next().unwrap().id.as_str(),
            format!("op-{}", MAX_RECENT + 2),
            "the newest settlement is first; the oldest were dropped"
        );
    }

    #[test]
    fn binding_an_id_to_an_unknown_key_is_refused() {
        let mut c = OperationsCore::default();
        let err = c.bind_id(&key("k1"), id("op-1")).unwrap_err();
        assert_eq!(err, OperationsRejection::UnknownOperation);
    }

    #[test]
    fn progress_for_an_operation_whose_id_is_not_yet_bound_is_refused() {
        // The dispatch writes, the response binds the id, and only then can the stream
        // say anything. An event arriving before the bind names an operation this client
        // cannot yet match, and must not invent one.
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).unwrap();
        let err = c
            .observe(&id("op-1"), OperationState::Running, OperationStage::Planning)
            .unwrap_err();
        assert_eq!(err, OperationsRejection::UnknownOperation);
    }

    #[test]
    fn a_settlement_is_built_only_from_a_terminal_record() {
        // `GET /api/operations/{id}` answers with a full record whether or not it has
        // finished. Reconciling from a non-terminal one would record an outcome that has
        // not happened.
        assert!(Settlement::from_terminal(OperationState::Running, None, None).is_none());
        let s = Settlement::from_terminal(
            OperationState::Succeeded,
            Some("Fast-forward".into()),
            GenerationToken::new("9").ok(),
        )
        .expect("a terminal record settles");
        assert_eq!(s.state, OperationState::Succeeded);
        assert_eq!(s.generation.as_ref().map(|g| g.as_str()), Some("9"));
    }
}

#[cfg(test)]
mod intent_tests {
    use super::*;
    use crate::features::core_traits::{RequestKey, RequestTarget};

    /// An intent raised by the `seq`-th click of the session, against graph epoch 1.
    fn intent(seq: u64, branch: &str) -> PendingIntent {
        PendingIntent {
            seq,
            key: RequestKey {
                epoch: 1,
                generation: None,
                target: RequestTarget::Branch(branch.to_string()),
            },
            kind: OperationKind::Delete {
                branch: branch.to_string(),
                current: None,
            },
        }
    }

    #[test]
    fn a_slower_earlier_response_cannot_replace_a_newer_pending_intent() {
        // The menu.rs race (design spec §3): the user taps Merge, then Delete. Delete's
        // pre-check resolves first and opens the dialog. Merge's pre-check then resolves
        // and must NOT overwrite the dialog the user is looking at.
        let delete = intent(5, "other-branch");
        let stale_merge = intent(4, "feature");
        assert!(
            !latest_wins(Some(&delete), &stale_merge),
            "an intent from an earlier click must be dropped, not committed"
        );
    }

    #[test]
    fn a_newer_intent_replaces_an_older_pending_one() {
        let old = intent(4, "feature");
        let new = intent(5, "other-branch");
        assert!(latest_wins(Some(&old), &new));
    }

    #[test]
    fn the_first_intent_always_wins_when_nothing_is_pending() {
        assert!(latest_wins(None, &intent(1, "main")));
    }

    #[test]
    fn two_intents_with_the_same_sequence_resolve_to_the_incoming_one() {
        // Sequences are unique in practice; admitting the later of two equal values keeps
        // the function total rather than leaving an unreachable special case.
        let a = intent(5, "a");
        let b = intent(5, "b");
        assert!(latest_wins(Some(&a), &b));
    }

    #[test]
    fn intents_racing_within_one_epoch_are_still_ordered() {
        // The defect this whole task exists to fix. Both taps happen against the SAME graph
        // epoch — nothing invalidated the graph between them — so epoch comparison alone
        // would call them equal and let whichever response arrived last win. Only the click
        // sequence records what the user actually asked for most recently.
        let mut seq = IntentSeq::default();
        let first = PendingIntent {
            seq: seq.next(),
            ..intent(0, "checkout-target")
        };
        let second = PendingIntent {
            seq: seq.next(),
            ..intent(0, "merge-source")
        };
        assert_eq!(
            first.key.epoch, second.key.epoch,
            "the premise: one epoch spans both clicks"
        );
        assert!(latest_wins(Some(&first), &second), "the later tap commits");
        assert!(
            !latest_wins(Some(&second), &first),
            "and the earlier tap's straggling response is dropped"
        );
    }

    #[test]
    fn a_stale_result_cannot_overwrite_the_message_a_newer_action_already_showed() {
        // The picker bug: tap Delete, then Rescan. Rescan answers first and writes its
        // line; Delete's slower reply then replaces it, so the user reads the outcome of
        // the action they did NOT do most recently.
        let mut seq = IntentSeq::default();
        let delete = seq.next();
        let rescan = seq.next();
        assert!(
            result_is_newest(0, rescan),
            "nothing shown yet, so it shows"
        );
        assert!(
            !result_is_newest(rescan, delete),
            "the earlier action's reply must not overwrite the later action's line"
        );
    }

    #[test]
    fn intent_sequences_are_monotone_and_start_above_zero() {
        // Zero is reserved for "no intent has ever been raised", so the first mint must not
        // collide with the initial value of the counter a caller stores.
        let mut seq = IntentSeq::default();
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
        assert_eq!(seq.next(), 3);
    }
}
