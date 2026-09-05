//! Whether the plan on screen still describes the repository (M12.05, #555).
//!
//! # The decision this file exists to protect
//!
//! > **A plan that quietly re-derives itself is a plan the user did not
//! > approve.**
//!
//! So nothing here rebuilds anything. It answers one question — *is what you
//! are looking at still true?* — and the four answers it can give are the four
//! this milestone's spec settled on. Rebuilding is an action the **user** takes
//! afterwards, and the plan it produces must be approved again like any other.
//!
//! # Why execute is offered in exactly one of the four
//!
//! `enforce_fresh` compares the **whole** generation digest for equality, so a
//! plan whose generation moved for *any* reason is refused at execution. Leaving
//! the button enabled in the reassuring case would be offering a button whose
//! purpose is to fail — the pattern `m3.23-worktrees.md` spent a section
//! correcting. The panel may never be more optimistic than the gate.
//!
//! # `MovedElsewhere` has to be earned
//!
//! It is the only reassuring answer here ("the repository moved, but not in a
//! way this operation depends on"), so it is reachable only from evidence: a
//! feed that could name every ref that moved since the plan was built, none of
//! them among the ones the plan names, and nothing outside the refs having
//! moved either. Anything less — a gap in what the client saw, a working-tree
//! change, a ref the server could not name — falls to [`PlanFreshness::Moved`],
//! which says less and claims nothing.

use std::collections::VecDeque;

use git_vista_protocol::change_feed::{ChangeFeedHealth, ChangeFeedSnapshot, RefDelta};

use crate::features::operations::kind::OperationKind;

/// How many snapshots the client keeps so it can difference a plan's generation
/// against the present.
///
/// A plan is approved and executed in seconds; a log this deep covers a stream
/// that published on every sweep for two minutes. Past that the honest answer
/// is "I cannot name what moved", which [`PlanFreshness::Moved`] already says.
pub const LOG_DEPTH: usize = 64;

/// What the plan on screen is, as this module needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOnScreen {
    /// The generation the plan was built against — `Plan::generation`.
    pub generation: String,
    /// The refs the plan expects to move — `Plan::expected_ref_changes`, by
    /// full ref name.
    pub expects: Vec<String>,
}

impl PlanOnScreen {
    /// Everything a freshness check needs, taken off the plan the user is being
    /// shown.
    ///
    /// One constructor, so every surface that displays a plan takes the same
    /// two things off the same object. The alternative — each caller reaching
    /// into `Plan` itself — is how the force-with-lease confirmation came to
    /// display a server-built plan and check the freshness of nothing.
    pub fn of(plan: &git_vista_protocol::Plan) -> Self {
        Self {
            generation: plan.generation.as_str().to_string(),
            expects: plan
                .expected_ref_changes
                .iter()
                .map(|change| change.ref_name.as_str().to_string())
                .collect(),
        }
    }
}

/// What the confirmation on screen currently has to approve.
///
/// # Why "no plan" is two different states
///
/// The first slice of #555 asked one question — *is there a plan?* — and a
/// `None` answered it for two situations that could not be more different:
///
/// - **a confirmation that never had a plan.** Most of this modal's arms are
///   built from their arguments and have never seen one, and a previewable
///   arm has not received one yet. #594 decided deliberately that a preview
///   informs and never gates, so these stay offerable.
/// - **a plan that was on screen, found stale, and thrown away to make room
///   for its replacement.** Here we *know* the repository moved.
///
/// Collapsing the second into the first is a defect the review of #664 found
/// and measured: clicking Rebuild cleared the plan, `confirm_enabled` saw
/// `None`, and the execute control went **live** — the stale notice gone, no
/// replacement to review, and the modal's own dispatch path sending a
/// branch-only request the execution generation gate never sees. The button
/// became live precisely because the user acted on being told it was stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanSlot {
    /// No plan, and none has been thrown away to make room for one. The
    /// confirmation's own verdict stands, untouched by this feature.
    Absent,
    /// A replacement was asked for and has not arrived. There is nothing to
    /// approve, and there is a known-stale plan behind the request.
    Rebuilding,
    /// A replacement was asked for and did not arrive. Still nothing to
    /// approve — and unlike `Absent`, silence here is not the absence of a
    /// claim, it is a failed attempt to replace a claim we know was stale.
    RebuildFailed,
    /// The plan on screen.
    Ready(PlanOnScreen),
}

/// What the slot becomes when a request for a plan is **issued**.
///
/// # Why this is a function and not two lines in the reactive wrapper
///
/// It was two lines in the wrapper, and a mutation collapsing them reported
/// `survived`: `features/preview/signals.rs` is `#[cfg(target_arch =
/// "wasm32")]`, so `cargo test` never compiles it and no host test could fail
/// on the defect. That is ADR 0115's rule — a mutation proof cannot see what it
/// does not run — and the answer it prescribes is to move the decision, not to
/// reach for the code. The browser leg still proves the wiring; this makes the
/// *decision* provable too.
///
/// The distinction it encodes is the one defect 1 of #664's review turned on:
/// a rebuild has a known-stale plan behind it, and until the replacement
/// arrives there is nothing to approve. A first fetch has no such history.
pub fn slot_when_requested(rebuilding: bool) -> PlanSlot {
    if rebuilding {
        PlanSlot::Rebuilding
    } else {
        PlanSlot::Absent
    }
}

/// What the slot becomes when that request **fails**.
///
/// The same distinction, at the other end, and it is the half the review said
/// "persists": a first fetch that fails leaves a confirmation that never had a
/// plan, which #594 leaves offerable. A *rebuild* that fails leaves a
/// confirmation whose plan we know was stale and whose replacement never came.
pub fn slot_when_request_failed(rebuilding: bool) -> PlanSlot {
    if rebuilding {
        PlanSlot::RebuildFailed
    } else {
        PlanSlot::Absent
    }
}

/// Why the feed cannot currently say whether the plan is current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedUnavailable {
    /// No stream, or none that has published yet.
    NotConnected,
    /// The feed is running and cannot read the repository.
    Blind { reason: String },
}

/// The four answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFreshness {
    /// The live generation still equals the plan's. Execute is offered.
    Current,
    /// The generation moved. `refs` names the refs *this plan depends on* that
    /// moved, and is empty when something moved that could not be named — which
    /// is still this arm, because it is the arm that claims least.
    Moved { refs: Vec<String> },
    /// The generation moved, every ref that moved could be named, and none of
    /// them was one this plan depends on — nor did anything outside the refs
    /// move. Still stale, still refused at execution; the difference is what is
    /// *said*, never what is offered.
    MovedElsewhere,
    /// The feed cannot say. **Not** "current": ADR 0055's rule — an undated
    /// reading gets no benefit of the doubt.
    Unknown { reason: FeedUnavailable },
}

impl PlanFreshness {
    /// Whether the confirm/execute control may be offered.
    ///
    /// One arm, and it is a function rather than a comment so a test can hold
    /// the rule: the panel may never be more optimistic than `enforce_fresh`,
    /// which refuses on any digest movement at all.
    pub fn execute_offered(&self) -> bool {
        matches!(self, PlanFreshness::Current)
    }
}

/// The snapshots this client has seen, newest last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedLog {
    entries: VecDeque<ChangeFeedSnapshot>,
}

impl FeedLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one published snapshot, keeping only deltas this client can
    /// actually read.
    ///
    /// # A delta that followed a publication we never received is not a delta
    ///
    /// The feed's transport keeps only the latest value, so a slow reader can
    /// skip publications **without disconnecting**. Each `changed` is a
    /// difference from the previous *server* publication, so a chain read
    /// across a gap is not a chain — and the verdict it produces is not merely
    /// vaguer, it is wrong in the reassuring direction. Measured: `main` moves,
    /// then an unrelated tag moves before this client polls, and a plan
    /// expecting `main` is told the repository moved "but not in a way this
    /// operation depends on."
    ///
    /// So a snapshot whose `seq` does not immediately follow the last one this
    /// client holds is recorded with its delta **replaced by
    /// [`RefDelta::Unknown`]**. The reading itself is still perfectly good; it
    /// is only the claim about what moved that this client is not entitled to.
    /// The first snapshot on a stream is the same case: it may carry a delta
    /// against a publication made before this client existed.
    pub fn record(&mut self, snapshot: ChangeFeedSnapshot) {
        let continuous = self
            .entries
            .back()
            .is_some_and(|last| snapshot.seq == last.seq.wrapping_add(1));
        let snapshot = if continuous {
            snapshot
        } else {
            ChangeFeedSnapshot {
                changed: RefDelta::Unknown,
                ..snapshot
            }
        };
        if self.entries.len() == LOG_DEPTH {
            self.entries.pop_front();
        }
        self.entries.push_back(snapshot);
    }

    /// Forget everything. Called when the stream drops: what a client saw
    /// before an outage cannot be differenced against what it sees after one,
    /// because the snapshots in between were never delivered.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn latest(&self) -> Option<&ChangeFeedSnapshot> {
        self.entries.back()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Everything that moved since the snapshot whose generation equals
    /// `generation`, as `(named refs, something else moved)`.
    ///
    /// `None` when the client cannot account for the whole span: it never saw a
    /// snapshot at that generation, or one of the snapshots in between could
    /// name nothing (the first after a reconnect, or one taken while the feed
    /// was blind). A gap is `None` and never an empty list — an empty list is
    /// the claim "nothing moved", which is exactly the claim a gap cannot
    /// support.
    pub fn moved_since(&self, generation: &str) -> Option<(Vec<String>, bool)> {
        let start = self.entries.iter().rposition(|entry| {
            entry
                .generation
                .as_ref()
                .is_some_and(|g| g.as_str() == generation)
        })?;
        let mut refs: Vec<String> = Vec::new();
        let mut other = false;
        for entry in self.entries.iter().skip(start + 1) {
            match &entry.changed {
                RefDelta::Unknown => return None,
                RefDelta::Named {
                    refs: moved,
                    other: elsewhere,
                } => {
                    refs.extend(moved.iter().map(|r| r.as_str().to_string()));
                    other |= *elsewhere;
                }
            }
        }
        refs.sort();
        refs.dedup();
        Some((refs, other))
    }
}

/// Is the plan on screen still current?
pub fn freshness(plan: &PlanOnScreen, log: &FeedLog) -> PlanFreshness {
    let Some(latest) = log.latest() else {
        return PlanFreshness::Unknown {
            reason: FeedUnavailable::NotConnected,
        };
    };
    if let ChangeFeedHealth::Blind { reason, .. } = &latest.health {
        return PlanFreshness::Unknown {
            reason: FeedUnavailable::Blind {
                reason: reason.clone(),
            },
        };
    }
    let Some(live) = latest.generation.as_ref() else {
        // A snapshot with no generation and no `Blind` health is a shape the
        // server does not produce. Treating it as "current" would be the one
        // unsafe reading available, so it is not the one taken.
        return PlanFreshness::Unknown {
            reason: FeedUnavailable::NotConnected,
        };
    };
    if live.as_str() == plan.generation {
        return PlanFreshness::Current;
    }
    let Some((moved, other)) = log.moved_since(&plan.generation) else {
        return PlanFreshness::Moved { refs: Vec::new() };
    };
    let depended_on: Vec<String> = moved
        .into_iter()
        .filter(|name| plan.expects.iter().any(|expected| expected == name))
        .collect();
    if !depended_on.is_empty() {
        return PlanFreshness::Moved { refs: depended_on };
    }
    if other {
        // Nothing this plan *names* moved, but the working tree, the index, the
        // stash or `merge.ff` did — any of which can change what a commit
        // writes. The reassuring sentence is not available here.
        return PlanFreshness::Moved { refs: Vec::new() };
    }
    PlanFreshness::MovedElsewhere
}

/// The plan this confirmation is showing, whichever way it got there.
///
/// A dialog can hold a plan two ways and only one of them was being checked
/// (#664 review, finding 7). The graph preview fetches one and keeps it — but
/// `preview_subject(Push)` is `NotPreviewable`, so a **force-with-lease**
/// confirmation has no preview at all while displaying a server-built plan's
/// explanation, its risk and the oid it will overwrite. Freshness taken only
/// from the preview therefore saw `None` on the single most destructive
/// confirmation in the app, and left its button enabled.
///
/// So the question is asked once, here, of the operation itself: *is there a
/// plan on this screen?*
pub fn plan_on_screen(op: &OperationKind, previewed: PlanSlot) -> PlanSlot {
    match previewed {
        // A rebuild in flight, or one that failed, is about the plan this
        // confirmation was showing — whichever way that plan arrived. It
        // outranks the carried plan below, which is the one being replaced.
        slot @ (PlanSlot::Ready(_) | PlanSlot::Rebuilding | PlanSlot::RebuildFailed) => slot,
        PlanSlot::Absent => match op {
            OperationKind::Push {
                force: Some(force), ..
            } => PlanSlot::Ready(force.plan.clone()),
            _ => PlanSlot::Absent,
        },
    }
}

/// Whether a confirmation may run, given the dialog's own verdict and the
/// freshness of the plan on screen.
///
/// One function, called from the wasm-only dialog, so the composition itself is
/// host-tested rather than only its two halves — #612's own origin was a
/// composition living where no test runner could reach it.
///
/// A dialog with **no plan on screen** is unaffected: this feature makes no
/// claim about an operation nobody built a plan for, and inventing one would
/// disable half the app's confirmations on a feed that had not connected yet.
pub fn confirm_enabled(prompt_enabled: bool, plan: &PlanVerdict) -> bool {
    prompt_enabled
        && match plan {
            PlanVerdict::NoPlan => true,
            PlanVerdict::Fresh(freshness) => freshness.execute_offered(),
            // Nothing to approve. Both of these follow a plan we know was
            // stale, so re-enabling here would offer the operation on the
            // strength of having discarded the evidence against it.
            PlanVerdict::Rebuilding | PlanVerdict::RebuildFailed => false,
        }
}

/// What this feature has to say about the confirmation on screen.
///
/// One value, folded once from the slot and the feed, so the button, the
/// notice and the Rebuild offer cannot disagree with each other — they are
/// three readings of this, not three computations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanVerdict {
    /// This confirmation is not plan-backed, or its plan has not arrived and
    /// none was discarded to make room for it. No claim is made.
    NoPlan,
    /// There is a plan, and this is how fresh it is.
    Fresh(PlanFreshness),
    /// A replacement is on its way.
    Rebuilding,
    /// A replacement was asked for and did not arrive.
    RebuildFailed,
}

/// Fold the plan on screen and the feed into the one verdict everything reads.
pub fn verdict(slot: &PlanSlot, log: &FeedLog) -> PlanVerdict {
    match slot {
        PlanSlot::Absent => PlanVerdict::NoPlan,
        PlanSlot::Rebuilding => PlanVerdict::Rebuilding,
        PlanSlot::RebuildFailed => PlanVerdict::RebuildFailed,
        PlanSlot::Ready(plan) => PlanVerdict::Fresh(freshness(plan, log)),
    }
}

/// Whether to offer a Rebuild control.
///
/// Spec D4 requires Rebuild and Discard on a stale plan, and the first slice
/// shipped the *sentence* telling the user to rebuild with no way to do it —
/// the browser tests asserted the wording and the disabled state, so they
/// passed straight over the missing action (#664 review, finding 6).
///
/// Offered on every stale arm and on none of the current ones: there is nothing
/// to rebuild about a plan that still describes the repository, and `Unknown`
/// is included deliberately — "couldn't tell" is exactly when a user most wants
/// a fresh answer.
pub fn rebuild_is_offered(plan: &PlanVerdict) -> bool {
    match plan {
        PlanVerdict::Fresh(freshness) => !freshness.execute_offered(),
        // Already rebuilding: offering it again would fire a second request
        // for the same replacement.
        PlanVerdict::Rebuilding => false,
        // The attempt failed, so offering it again is the only useful thing
        // left on the dialog.
        PlanVerdict::RebuildFailed => true,
        PlanVerdict::NoPlan => false,
    }
}

/// Why the confirm control is inert, when it is staleness that withdrew it.
///
/// `None` when the plan is current or there is no plan — the dialog's own
/// reasons are unchanged and keep their own words.
pub fn blocked_by_staleness(plan: &PlanVerdict) -> Option<&'static str> {
    match plan {
        PlanVerdict::NoPlan | PlanVerdict::Fresh(PlanFreshness::Current) => None,
        PlanVerdict::Rebuilding => {
            Some("This can't run yet: a new plan is being built for you to review.")
        }
        PlanVerdict::RebuildFailed => {
            Some("This can't run: the new plan couldn't be built, so there is nothing to review.")
        }
        PlanVerdict::Fresh(PlanFreshness::Unknown { .. }) => {
            Some("This can't run while it isn't known whether the picture above is current.")
        }
        PlanVerdict::Fresh(_) => {
            Some("This can't run: the repository moved after this picture was drawn.")
        }
    }
}

/// What the panel says.
///
/// Every string this feature shows is minted here, so `cargo test` reads the
/// words a browser would.
pub fn verdict_headline(verdict: &PlanVerdict) -> Option<String> {
    match verdict {
        PlanVerdict::NoPlan => None,
        PlanVerdict::Rebuilding => Some("Building a new plan…".to_string()),
        PlanVerdict::RebuildFailed => {
            Some("Couldn't build a new plan for the repository as it is now.".to_string())
        }
        PlanVerdict::Fresh(freshness) => freshness_headline(freshness),
    }
}

/// How rebuilding is framed, beneath the headline.
pub fn verdict_framing(verdict: &PlanVerdict) -> Option<&'static str> {
    match verdict {
        PlanVerdict::NoPlan => None,
        PlanVerdict::Rebuilding => {
            Some("Nothing can run until it arrives, and you will be asked to approve it.")
        }
        PlanVerdict::RebuildFailed => Some("Try again, or close this and start over."),
        PlanVerdict::Fresh(freshness) => rebuild_framing(freshness),
    }
}

pub fn freshness_headline(freshness: &PlanFreshness) -> Option<String> {
    match freshness {
        PlanFreshness::Current => None,
        PlanFreshness::Moved { refs } if refs.is_empty() => {
            Some("The repository changed while this was on screen.".to_string())
        }
        PlanFreshness::Moved { refs } => Some(format!(
            "{} moved while this was on screen.",
            join_names(refs)
        )),
        PlanFreshness::MovedElsewhere => {
            Some("The repository moved, but not in a way this operation depends on.".to_string())
        }
        PlanFreshness::Unknown { reason } => Some(match reason {
            FeedUnavailable::NotConnected => {
                "Couldn't tell whether this is still current.".to_string()
            }
            FeedUnavailable::Blind { reason } => {
                format!("Couldn't tell whether this is still current — {reason}.")
            }
        }),
    }
}

/// How rebuilding is framed, beneath the headline.
pub fn rebuild_framing(freshness: &PlanFreshness) -> Option<&'static str> {
    match freshness {
        PlanFreshness::Current => None,
        PlanFreshness::Moved { refs } if refs.is_empty() => {
            Some("Rebuild to see what this would do now. You will be asked to approve it again.")
        }
        PlanFreshness::Moved { .. } => {
            Some("What this does will be different. Rebuild it and review it again.")
        }
        PlanFreshness::MovedElsewhere => {
            Some("Rebuilding will produce the same operation against the current state.")
        }
        PlanFreshness::Unknown { .. } => Some("Rebuild to be sure."),
    }
}

fn join_names(refs: &[String]) -> String {
    match refs {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
#[path = "core_suite.rs"]
mod suite;
