//! The reactive wrapper over the operations core — wasm only (M1.11, #64).
//!
//! Everything decidable is decided in [`super::core`], on the host, under test. This file
//! holds only what genuinely needs Leptos: reading the live epoch out of a signal, and
//! keeping the pending intent in a `StoredValue` that survives the closures that write it.

use leptos::{RwSignal, SignalGetUntracked, StoredValue};

use crate::features::core_traits::{RequestKey, RequestTarget};
use crate::features::operations::core::{latest_wins, IntentSeq, PendingIntent};

/// Mint the next click-order sequence.
///
/// Call this **synchronously inside the event handler**, before any `await`. That is the
/// whole point: the sequence must record when the user acted, and a value taken after the
/// pre-check resolves would record when the network answered instead.
pub fn next_seq(intent_seq: StoredValue<IntentSeq>) -> u64 {
    // `try_update_value` returns `None` only when the owning scope is already disposed, in
    // which case the continuation cannot write anything either. Sequence 0 is the reserved
    // "no intent" value, so falling back to it makes such an intent lose every comparison
    // rather than spuriously win one.
    intent_seq.try_update_value(|s| s.next()).unwrap_or(0)
}

/// Stamp a request with the repository state it was raised against.
///
/// `generation` is `None` here: the branch-operation endpoints predate M1.10 and do not
/// report one, so these intents are fenced by epoch alone — which is exactly the case
/// [`RequestKey::is_current`] documents as correct for pre-generation endpoints.
pub fn request_key(reload: RwSignal<u32>, target: RequestTarget) -> RequestKey {
    RequestKey {
        epoch: u64::from(reload.get_untracked()),
        generation: None,
        target,
    }
}

/// Whether a resolved pre-check may still open its dialog; records it if so.
///
/// Two independent reasons to drop a continuation, and both matter:
///
/// * the repository moved while the pre-check was in flight (a Refresh, a repo switch, a
///   drift reload), so the answer describes a repository the user is no longer looking at;
/// * a later tap already owns the dialog, so committing now would replace what the user is
///   looking at with something they asked for *earlier*.
pub fn admit_intent(
    pending_intent: StoredValue<Option<PendingIntent>>,
    reload: RwSignal<u32>,
    intent: &PendingIntent,
) -> bool {
    if !intent
        .key
        .is_current(u64::from(reload.get_untracked()), None)
    {
        return false;
    }
    let wins = pending_intent
        .try_with_value(|current| latest_wins(current.as_ref(), intent))
        .unwrap_or(false);
    if !wins {
        return false;
    }
    pending_intent.set_value(Some(intent.clone()));
    true
}
