//! The repository change feed (M12, #553–#556) — what `GET
//! /api/repository/events` publishes, and the vocabulary it says it in.
//!
//! # One sentence of design, because the shapes here only make sense with it
//!
//! The server watches the repository for *hints* and sweeps it for *facts*.
//! The watcher never says what changed; every claim on this feed was produced
//! by a sweep that actually read the repository (spec `m3.26-external-changes`
//! D1, ADR 0094). So a [`ChangeFeedSnapshot`] is always a reading, never an
//! inference from an event.
//!
//! # The rule every type here exists to hold
//!
//! > **"I could not tell" must never render as "nothing changed."**
//!
//! That is why [`ChangeFeedHealth`] has a [`Blind`](ChangeFeedHealth::Blind)
//! arm rather than the feed simply going quiet, why
//! [`RefDelta::Unknown`] is a variant rather than an empty list, and why
//! [`WatchBudget::Undetermined`] is a variant rather than the number 64. In
//! each pair the second shape is the one a reader would mistake for good news.
//!
//! # No path ever crosses this boundary
//!
//! [`WatcherLoss::WatchLost`] names the directory it lost as a **git-dir
//! relative label** (`refs/heads/team`), never a filesystem path. Transport
//! never learns paths — the same posture
//! [`RepositoryToken`](crate::RepositoryToken) takes — and a watcher's
//! diagnostic is not a reason to make an exception.

use serde::{Deserialize, Serialize};

use crate::plan::{GenerationToken, RefName, UnixSeconds};

/// The SSE event name every [`ChangeFeedSnapshot`] is published under.
///
/// One name, not two: a health transition and a generation change are the same
/// event — a fresh reading of the whole feed state — and a client that had to
/// merge two event streams could hold a generation from one and a health from
/// the other, which is a state neither sweep ever observed.
pub const SNAPSHOT_EVENT: &str = "snapshot";

/// One reading of the repository, published to every open stream.
///
/// The first event on a newly opened stream is the *current* snapshot rather
/// than the next change, so a client that connects late gets an immediate
/// answer instead of waiting for a transition that already happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeFeedSnapshot {
    /// The live repository generation, in **the planner's recipe** — the same
    /// token `Plan::generation` carries and the execution gate compares
    /// against, so a panel's verdict can never be more optimistic than the
    /// refusal it predicts.
    ///
    /// `None` exactly when `health` is [`ChangeFeedHealth::Blind`]: the sweep
    /// could not read the repository, so there is no reading to publish. It is
    /// an `Option` and not a stale last-known value for the same reason
    /// `Blind` exists at all — a value that outlives the read that produced it
    /// is indistinguishable from a fresh one.
    pub generation: Option<GenerationToken>,
    /// What the feed itself is currently able to do. Never inferable from
    /// silence, and never absent.
    pub health: ChangeFeedHealth,
    /// What moved between the previously published snapshot and this one.
    pub changed: RefDelta,
    /// When this reading was taken.
    pub at: UnixSeconds,
}

/// What moved since the previous published snapshot.
///
/// This is a **difference between two readings**, not a translation of watcher
/// events: the sweep holds the refs it last published and compares them with
/// the refs it just read. A watcher event carries a path and a bitmask, which
/// is not evidence about a value, and nothing here is derived from one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefDelta {
    /// There is no previous reading to difference against — the first snapshot
    /// after a stream opens, after a process restart, or after the feed was
    /// [`Blind`](ChangeFeedHealth::Blind).
    ///
    /// **Not an empty [`Named`](RefDelta::Named).** An empty list is the claim
    /// "no ref moved"; this is the absence of a claim, and a client must not
    /// reassure anybody on the strength of it.
    Unknown,
    /// The difference, named. `refs` holds every ref whose target changed,
    /// appeared or disappeared, as a full ref name (`refs/heads/main`).
    Named {
        refs: Vec<RefName>,
        /// Whether anything the generation digests *other than* a named ref
        /// moved: HEAD's symbolic target, the worktree/index status, the stash,
        /// or `merge.ff`.
        ///
        /// It exists so the reassuring reading has to be earned. A client may
        /// only say "the repository moved, but not in a way this operation
        /// depends on" when the refs it names are untouched **and** this is
        /// `false`; a working-tree change with no ref movement is still
        /// material to a commit, and saying otherwise would be optimistic
        /// about the one direction that costs something.
        other: bool,
    },
}

/// What the change feed can currently do, and — when that is less than
/// everything — why.
///
/// Every arm below `Watching` is a *stated* condition. The one shape this
/// vocabulary refuses to have is a feed that quietly reduces its coverage and
/// keeps reporting `Watching`, which is the failure the whole milestone exists
/// to prevent, aimed at itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChangeFeedHealth {
    /// Hints and sweeps both running; the whole watch set is installed.
    Watching {
        watches: usize,
        budget: WatchBudget,
    },
    /// Part of the ref tree is not watched because the budget bound it. The
    /// sweep still covers those refs, so nothing that was true stops being
    /// true — it arrives at the sweep cadence rather than promptly.
    Bounded {
        watched: usize,
        wanted: usize,
        budget: WatchBudget,
    },
    /// No watcher at all. Correctness is unchanged (the sweep was always the
    /// only thing making a claim); promptness falls to the sweep cadence.
    SweepOnly { reason: WatcherLoss },
    /// The **sweep** could not read the repository. Nothing on this feed is
    /// evidence about the repository while this holds, and a client renders it
    /// as "couldn't tell" rather than as "nothing changed".
    Blind { reason: String, since: UnixSeconds },
}

impl ChangeFeedHealth {
    /// Whether a reading published under this health may be treated as a fact
    /// about the repository.
    ///
    /// `Bounded` and `SweepOnly` are both `true`: they cost latency, not
    /// truth. Only `Blind` is `false`, and it is the arm that carries no
    /// generation at all.
    pub fn is_a_reading(&self) -> bool {
        !matches!(self, ChangeFeedHealth::Blind { .. })
    }
}

/// How many inotify watches this process allows itself, and where that number
/// came from.
///
/// The provenance is a variant rather than a flag because the two cases can
/// produce the **same number**: a kernel reporting `max_user_watches = 8192`
/// and `max_user_instances = 128` divides to exactly the floor a machine that
/// could not be read falls back to. A reader that only saw `64` could not tell
/// a computed budget from a defaulted one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provenance", rename_all = "snake_case")]
pub enum WatchBudget {
    /// Computed from the kernel's own two numbers. Both raw values ride along
    /// so the whole derivation — dividend, divisor, result — is readable, and
    /// so a clamped budget can be told from an unclamped one.
    Derived {
        watches: usize,
        from_watches: usize,
        from_instances: usize,
    },
    /// One or both kernel limits could not be read (or the divisor was zero).
    /// The chosen floor is in force and says so. Never rendered as a computed
    /// budget.
    Undetermined { watches: usize },
}

impl WatchBudget {
    /// The number of watches in force, whichever way it was arrived at.
    pub fn watches(&self) -> usize {
        match self {
            WatchBudget::Derived { watches, .. } | WatchBudget::Undetermined { watches } => {
                *watches
            }
        }
    }
}

/// Why there is no watcher, or no longer a trustworthy one.
///
/// Every arm is a **fact that was observed**, never an inference from silence:
/// inotify refused a watch, a watched directory went away, the sweep caught
/// the watcher missing changes, or the platform has no backend at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum WatcherLoss {
    /// inotify refused a watch — the shared per-user limit is exhausted. `at`
    /// is how many watches were installed when the refusal came.
    LimitReached { at: usize },
    /// A watched directory was lost and could not be re-established.
    ///
    /// `location` is a **git-dir-relative label** (`refs/heads/team`), never a
    /// filesystem path: see this module's header.
    WatchLost { location: String },
    /// The sweep caught the watcher missing changes often enough to stop
    /// believing it. Evidence, with the counts it was drawn from — not a guess.
    Unreliable { missed: u32, hinted: u32 },
    /// This platform has no watcher backend at all.
    Unsupported { detail: String },
    /// The watcher backend failed or its driver died.
    Backend { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(v: &str) -> GenerationToken {
        GenerationToken::new(v).expect("test token is non-empty")
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let snapshot = ChangeFeedSnapshot {
            generation: Some(token("1234")),
            health: ChangeFeedHealth::Watching {
                watches: 12,
                budget: WatchBudget::Derived {
                    watches: 4038,
                    from_watches: 516_898,
                    from_instances: 128,
                },
            },
            changed: RefDelta::Named {
                refs: vec![RefName::new("refs/heads/main").unwrap()],
                other: false,
            },
            at: UnixSeconds(1_700_000_000),
        };
        let json = serde_json::to_string(&snapshot).expect("snapshot serialises");
        let back: ChangeFeedSnapshot = serde_json::from_str(&json).expect("snapshot parses");
        assert_eq!(back, snapshot);
    }

    #[test]
    fn an_unknown_delta_is_not_an_empty_named_one() {
        // The whole point of the variant: these must never collapse into the
        // same wire value, because one is "nothing moved" and the other is
        // "I have nothing to compare against".
        let unknown = serde_json::to_string(&RefDelta::Unknown).unwrap();
        let empty = serde_json::to_string(&RefDelta::Named {
            refs: Vec::new(),
            other: false,
        })
        .unwrap();
        assert_ne!(unknown, empty);
        assert!(unknown.contains("unknown"), "unknown delta tags itself: {unknown}");
    }

    #[test]
    fn a_defaulted_budget_is_distinguishable_from_a_computed_one_of_the_same_size() {
        // 8192 / 128 == 64 == the chosen floor. The number cannot tell these
        // apart; the variant must.
        let derived = WatchBudget::Derived {
            watches: 64,
            from_watches: 8192,
            from_instances: 128,
        };
        let defaulted = WatchBudget::Undetermined { watches: 64 };
        assert_eq!(derived.watches(), defaulted.watches());
        assert_ne!(
            serde_json::to_string(&derived).unwrap(),
            serde_json::to_string(&defaulted).unwrap()
        );
    }

    #[test]
    fn only_blind_is_not_a_reading() {
        let budget = WatchBudget::Undetermined { watches: 64 };
        assert!(ChangeFeedHealth::Watching {
            watches: 7,
            budget: budget.clone()
        }
        .is_a_reading());
        assert!(ChangeFeedHealth::Bounded {
            watched: 4,
            wanted: 9,
            budget
        }
        .is_a_reading());
        assert!(ChangeFeedHealth::SweepOnly {
            reason: WatcherLoss::Unsupported {
                detail: "no backend".into()
            }
        }
        .is_a_reading());
        assert!(!ChangeFeedHealth::Blind {
            reason: "git could not be run".into(),
            since: UnixSeconds(1)
        }
        .is_a_reading());
    }

    #[test]
    fn a_lost_watch_carries_a_label_not_a_path() {
        // The wire field is named `location` and the type is a plain String,
        // so this test cannot enforce the *value*. What it pins is the field
        // name a reviewer greps for, and the shape of the label the server is
        // required to build (see `reconciliation::watch_label`).
        let loss = WatcherLoss::WatchLost {
            location: "refs/heads/team".into(),
        };
        let json = serde_json::to_string(&loss).unwrap();
        assert!(json.contains("\"location\":\"refs/heads/team\""), "{json}");
        let WatcherLoss::WatchLost { location } = &loss else {
            unreachable!("constructed one line above")
        };
        assert!(
            !location.starts_with('/'),
            "a label is relative to the git directory, never rooted: {location}"
        );
    }
}
