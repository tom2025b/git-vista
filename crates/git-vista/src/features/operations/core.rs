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
    fn intent_sequences_are_monotone_and_start_above_zero() {
        // Zero is reserved for "no intent has ever been raised", so the first mint must not
        // collide with the initial value of the counter a caller stores.
        let mut seq = IntentSeq::default();
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
        assert_eq!(seq.next(), 3);
    }
}
