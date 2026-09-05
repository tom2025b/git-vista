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

    /// Record one published snapshot.
    pub fn record(&mut self, snapshot: ChangeFeedSnapshot) {
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
pub fn confirm_enabled(prompt_enabled: bool, plan: Option<&PlanFreshness>) -> bool {
    prompt_enabled && plan.is_none_or(|freshness| freshness.execute_offered())
}

/// Why the confirm control is inert, when it is staleness that withdrew it.
///
/// `None` when the plan is current or there is no plan — the dialog's own
/// reasons are unchanged and keep their own words.
pub fn blocked_by_staleness(plan: Option<&PlanFreshness>) -> Option<&'static str> {
    match plan {
        None | Some(PlanFreshness::Current) => None,
        Some(PlanFreshness::Unknown { .. }) => {
            Some("This can't run while it isn't known whether the picture above is current.")
        }
        Some(_) => Some("This can't run: the repository moved after this picture was drawn."),
    }
}

/// What the panel says.
///
/// Every string this feature shows is minted here, so `cargo test` reads the
/// words a browser would.
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
