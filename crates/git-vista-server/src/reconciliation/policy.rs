//! The reconciliation sweep's decisions, with no IO in them (M12.03, #553).
//!
//! Everything in this file is a pure function of values the driver hands it:
//! what one sweep read, what the watcher last said, and what time it is in
//! milliseconds. Nothing here opens a repository, spawns a task, or sleeps —
//! which is what lets `cargo test` run all of it on the host, and what lets a
//! mutation proof reach it (ADR 0115: a proof cannot see code the test run
//! never compiled).
//!
//! # The two decisions that matter, stated before the code makes them
//!
//! **1. Publishing is the only thing that records.** [`FeedPolicy::observe`] is
//! the sole writer of `published`, and it writes it in the same expression that
//! returns the snapshot to be sent. There is no "remember what I wrote" path,
//! because that is the path that swallows an external change which landed
//! inside somebody else's write window (spec D3, #554).
//!
//! **2. A sweep the watcher never hinted at is evidence about the watcher.**
//! Counted, never discarded, and the counts are what moves the feed to
//! `SweepOnly` — on evidence rather than on a guess (spec D1, #553).

use std::collections::BTreeMap;
use std::time::Duration;

use git_vista_protocol::change_feed::{
    ChangeFeedHealth, ChangeFeedSnapshot, RefDelta, WatchBudget, WatcherLoss,
};
use git_vista_protocol::{GenerationToken, RefName, UnixSeconds};

/// The sweep interval while a client stream is open, before any backoff.
///
/// Spec D5(b), and Tom's open question 2 — the number that decides whether
/// "promptly" is met. It is a judgement about working rhythm, not a
/// measurement, and it is the ceiling on how long an external change can sit
/// unnoticed while the watcher is healthy (in practice a hint arrives in
/// milliseconds and this only binds in `SweepOnly`).
pub(crate) const SWEEP_BASE: Duration = Duration::from_secs(2);

/// The ceiling of the unchanged-sweep backoff.
pub(crate) const SWEEP_MAX: Duration = Duration::from_secs(60);

/// A hint arriving this soon after a sweep still counts as "the watcher saw
/// it" — a hint can legitimately land a few milliseconds *after* the timer
/// sweep that beat it, and counting that as a miss would slander a healthy
/// watcher into `SweepOnly`.
///
/// `2 ×` the watcher's own `DEBOUNCE`, which is where the number comes from.
pub(crate) const MISS_GRACE: Duration = Duration::from_millis(200);

/// The sweep may occupy at most one part in this many of wall-clock time.
///
/// This is the whole of spec D5(d): the worktree tier never runs sooner than
/// `DUTY_FACTOR ×` its own last measured duration, so the app's cost is capped
/// at 10 % of one core *by construction* rather than by a guessed interval that
/// is wrong on a tiny repository and wrong again on a huge one.
pub(crate) const DUTY_FACTOR: u32 = 10;

/// How many observed changes must accumulate before the miss counts are
/// allowed to condemn the watcher.
///
/// Without a floor, one missed change against zero hinted ones — entirely
/// ordinary in the first seconds of a feed's life — would read as `missed >
/// hinted` and move a healthy watcher to `SweepOnly`.
pub(crate) const MIN_CHANGES_BEFORE_VERDICT: u32 = 10;

/// What the driver managed to learn from one sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SweepOutcome {
    /// The repository was read. Every field came from the same reading.
    Read(SweepReading),
    /// The repository could not be read. **Not** an empty reading: a sweep that
    /// could not look must never publish as a sweep that looked and found
    /// nothing.
    Blind { reason: String },
}

/// One reading, as the policy consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SweepReading {
    pub(crate) generation: GenerationToken,
    /// Full ref name → target oid.
    pub(crate) refs: BTreeMap<String, String>,
    /// Everything else the generation folds, as one comparable value.
    pub(crate) other: String,
}

/// Why a sweep ran. Only [`SweepTrigger::Timer`] produces evidence about the
/// watcher: a sweep the watcher asked for obviously did not miss anything, and
/// a sweep a client's arrival asked for says nothing either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepTrigger {
    Timer,
    Hint,
    StreamOpen,
    /// This process finished a write of its own and is publishing what it left
    /// behind (spec D3, #554).
    AppWrite,
}

/// What the watcher last said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatcherState {
    /// It has not reported yet. Deliberately not a health value: "I have not
    /// heard from the watcher" is not "the watcher is fine", and the driver
    /// turns a start-up window that expires into a named `Lost` rather than
    /// letting this arm reach the wire.
    Starting,
    /// Watching `installed` directories out of `wanted`. Equal means the whole
    /// set is covered; fewer means the budget bound it (spec D5(e)).
    Watching {
        installed: usize,
        wanted: usize,
        budget: WatchBudget,
    },
    Lost {
        reason: WatcherLoss,
        budget: WatchBudget,
    },
}

/// The evidence that the watcher is not seeing what the sweep sees (spec D1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WatcherMisses {
    /// Timer sweeps that found a change with no hint inside the grace window.
    pub(crate) missed: u32,
    /// Timer sweeps that found a change the watcher had already hinted at.
    pub(crate) hinted: u32,
    pub(crate) last_missed_at_ms: Option<u64>,
}

impl WatcherMisses {
    fn observed(&self) -> u32 {
        self.missed.saturating_add(self.hinted)
    }

    /// Whether the counts are enough to stop believing the watcher.
    ///
    /// Both halves are required. `missed > hinted` alone condemns a watcher on
    /// the first change it happens to lose; the floor is what makes this
    /// evidence rather than an accident.
    fn condemns_the_watcher(&self) -> bool {
        self.observed() >= MIN_CHANGES_BEFORE_VERDICT && self.missed > self.hinted
    }
}

/// The last snapshot this feed *published*, and the reading it was published
/// from.
///
/// Named for what it is. Anything that assigns this without also sending the
/// snapshot reintroduces the defect the design exists to remove — see the
/// module header, decision 1.
#[derive(Debug, Clone)]
struct Published {
    generation: Option<GenerationToken>,
    health: ChangeFeedHealth,
    /// The refs behind `generation`, or `None` when the published snapshot was
    /// `Blind` and therefore has no reading behind it to difference against.
    reading: Option<(BTreeMap<String, String>, String)>,
}

/// One repository's change-feed decisions.
pub(crate) struct FeedPolicy {
    published: Option<Published>,
    watcher: WatcherState,
    misses: WatcherMisses,
    /// A timer sweep that found a change and has not yet been told whether a
    /// hint was coming. Holds the millisecond it completed.
    pending_miss: Option<u64>,
    last_hint_at_ms: Option<u64>,
    /// Consecutive sweeps that published nothing — the backoff exponent.
    unchanged: u32,
    /// Set once, on evidence, and never cleared: a watcher that has been caught
    /// missing changes is not re-trusted by a quiet minute. It is cleared only
    /// by a restart, which is honest — nothing observed says it recovered.
    untrusted: bool,
    blind_since: Option<UnixSeconds>,
}

impl FeedPolicy {
    pub(crate) fn new() -> Self {
        Self {
            published: None,
            watcher: WatcherState::Starting,
            misses: WatcherMisses::default(),
            pending_miss: None,
            last_hint_at_ms: None,
            unchanged: 0,
            untrusted: false,
            blind_since: None,
        }
    }

    pub(crate) fn misses(&self) -> WatcherMisses {
        self.misses
    }

    /// Record what the watcher said about itself. Never publishes on its own —
    /// the next sweep does, because a health value published beside a
    /// generation nobody just read is a snapshot no reading ever produced.
    pub(crate) fn note_watcher(&mut self, state: WatcherState) {
        self.watcher = state;
    }

    /// Record a hint. Resolves a pending miss verdict in the watcher's favour
    /// when it lands inside the grace window.
    pub(crate) fn note_hint(&mut self, now_ms: u64) {
        self.last_hint_at_ms = Some(now_ms);
        if let Some(swept_at) = self.pending_miss {
            if now_ms.saturating_sub(swept_at) <= MISS_GRACE.as_millis() as u64 {
                self.misses.hinted = self.misses.hinted.saturating_add(1);
                self.pending_miss = None;
            }
        }
        self.unchanged = 0;
    }

    /// Settle any miss verdict whose grace window has expired.
    ///
    /// Called on every driver tick, not only after a sweep: a verdict that only
    /// settles when the *next* change arrives is a verdict a quiet repository
    /// never reaches, which is exactly the repository a dead watcher produces.
    pub(crate) fn settle_due(&mut self, now_ms: u64) {
        let Some(swept_at) = self.pending_miss else {
            return;
        };
        if now_ms.saturating_sub(swept_at) > MISS_GRACE.as_millis() as u64 {
            self.misses.missed = self.misses.missed.saturating_add(1);
            self.misses.last_missed_at_ms = Some(swept_at);
            self.pending_miss = None;
            if self.misses.condemns_the_watcher() {
                self.untrusted = true;
            }
        }
    }

    /// Fold one sweep's outcome in, and say what to publish.
    ///
    /// `Some(snapshot)` means *send this, to every open stream*. Returning it
    /// and recording it are the same statement here, and that is the invariant
    /// #554 rests on: there is no way to reach the assignment below without
    /// also handing the caller the snapshot to send.
    pub(crate) fn observe(
        &mut self,
        now_ms: u64,
        at: UnixSeconds,
        trigger: SweepTrigger,
        outcome: SweepOutcome,
    ) -> Option<ChangeFeedSnapshot> {
        self.settle_due(now_ms);

        let (generation, reading) = match &outcome {
            SweepOutcome::Read(read) => (
                Some(read.generation.clone()),
                Some((read.refs.clone(), read.other.clone())),
            ),
            SweepOutcome::Blind { .. } => (None, None),
        };

        // `blind_since` dates the *condition*, not this reading of it, so a
        // client can see how long the feed has been unable to look.
        match &outcome {
            SweepOutcome::Blind { .. } => {
                self.blind_since.get_or_insert(at);
            }
            SweepOutcome::Read(_) => self.blind_since = None,
        }

        let health = self.health(&outcome);
        let generation_moved = self
            .published
            .as_ref()
            .is_none_or(|last| last.generation != generation);
        let health_moved = self
            .published
            .as_ref()
            .is_none_or(|last| last.health != health);

        if generation_moved && matches!(trigger, SweepTrigger::Timer) && self.published.is_some() {
            self.note_timer_found_a_change(now_ms);
        }

        if !generation_moved && !health_moved {
            self.unchanged = self.unchanged.saturating_add(1);
            return None;
        }

        let changed = self.delta(reading.as_ref());
        let snapshot = ChangeFeedSnapshot {
            generation: generation.clone(),
            health: health.clone(),
            changed,
            at,
        };
        self.unchanged = 0;
        // The one assignment, in the one place that also returns the snapshot.
        self.published = Some(Published {
            generation,
            health,
            reading,
        });
        Some(snapshot)
    }

    /// How long to wait before the next timer sweep, given how long the last
    /// one took.
    ///
    /// Two rules, and the *larger* wins: back off while nothing is changing, and
    /// never run sooner than [`DUTY_FACTOR`] times the last sweep's own cost.
    /// The second is what makes the bound derived rather than picked — on a
    /// repository where `git status` takes two seconds it pushes the interval
    /// to twenty on its own, with no size heuristic and no configuration.
    pub(crate) fn next_sweep_delay(&self, last: Duration) -> Duration {
        let backoff = SWEEP_BASE
            .saturating_mul(1u32 << self.unchanged.min(5))
            .min(SWEEP_MAX);
        backoff.max(last.saturating_mul(DUTY_FACTOR))
    }

    /// The health this feed would publish right now.
    fn health(&self, outcome: &SweepOutcome) -> ChangeFeedHealth {
        if let SweepOutcome::Blind { reason } = outcome {
            return ChangeFeedHealth::Blind {
                reason: reason.clone(),
                since: self.blind_since.unwrap_or(UnixSeconds(0)),
            };
        }
        if self.untrusted {
            return ChangeFeedHealth::SweepOnly {
                reason: WatcherLoss::Unreliable {
                    missed: self.misses.missed,
                    hinted: self.misses.hinted,
                },
            };
        }
        match &self.watcher {
            WatcherState::Starting => ChangeFeedHealth::SweepOnly {
                reason: WatcherLoss::Backend {
                    detail: "the watcher has not reported yet".to_string(),
                },
            },
            WatcherState::Lost { reason, .. } => ChangeFeedHealth::SweepOnly {
                reason: reason.clone(),
            },
            WatcherState::Watching {
                installed,
                wanted,
                budget,
            } if installed < wanted => ChangeFeedHealth::Bounded {
                watched: *installed,
                wanted: *wanted,
                budget: budget.clone(),
            },
            WatcherState::Watching {
                installed, budget, ..
            } => ChangeFeedHealth::Watching {
                watches: *installed,
                budget: budget.clone(),
            },
        }
    }

    fn note_timer_found_a_change(&mut self, now_ms: u64) {
        let hinted_recently = self.last_hint_at_ms.is_some_and(|hint| {
            now_ms.saturating_sub(hint) <= MISS_GRACE.as_millis() as u64 || hint >= now_ms
        });
        if hinted_recently {
            self.misses.hinted = self.misses.hinted.saturating_add(1);
        } else {
            // Not a miss yet: a hint may still be a few milliseconds behind.
            self.pending_miss = Some(now_ms);
        }
    }

    /// What moved between the last published reading and this one.
    fn delta(&self, reading: Option<&(BTreeMap<String, String>, String)>) -> RefDelta {
        let (Some((refs, other)), Some(previous)) = (reading, self.published.as_ref()) else {
            return RefDelta::Unknown;
        };
        let Some((was_refs, was_other)) = previous.reading.as_ref() else {
            return RefDelta::Unknown;
        };
        let mut moved: Vec<RefName> = Vec::new();
        let mut unnameable = false;
        for (name, target) in refs {
            if was_refs.get(name) != Some(target) {
                unnameable |= !push_ref(&mut moved, name);
            }
        }
        for name in was_refs.keys() {
            if !refs.contains_key(name) {
                unnameable |= !push_ref(&mut moved, name);
            }
        }
        moved.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        moved.dedup();
        RefDelta::Named {
            refs: moved,
            // A ref that moved but could not be named is folded into `other`
            // rather than dropped: the reassuring reading ("nothing this
            // operation depends on moved") must never be reachable through a
            // name this server could not put on the wire.
            other: other != was_other || unnameable,
        }
    }
}

/// Add one ref name to the delta. `false` when the name will not pass the wire
/// validator and was therefore not added.
///
/// A name `RefName` refuses is a name no client can act on, and inventing a
/// substitute would put a ref on the wire that the repository does not have.
/// The caller folds a `false` into the delta's `other` flag, so an unnameable
/// ref still reaches the client as "something else moved" — never as silence.
fn push_ref(into: &mut Vec<RefName>, name: &str) -> bool {
    match RefName::new(name) {
        Ok(name) => {
            into.push(name);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
#[path = "policy_suite.rs"]
mod suite;
