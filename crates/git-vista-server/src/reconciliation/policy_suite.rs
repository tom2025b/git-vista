//! Host tests for the sweep's decisions (M12.03 #553, M12.04 #554).
//!
//! Every test here runs under a plain `cargo test` on the host — no repository,
//! no tokio, no clock. That is deliberate: the decisions these pin are the ones
//! a mutation proof has to be able to reach, and a decision inside an async
//! driver behind a `tokio::select!` is one no mutation proof can see fail.

use super::*;

fn token(v: &str) -> GenerationToken {
    GenerationToken::new(v).expect("test tokens are non-empty")
}

fn refs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn reading(generation: &str, pairs: &[(&str, &str)], other: &str) -> SweepOutcome {
    SweepOutcome::Read(SweepReading {
        generation: token(generation),
        refs: refs(pairs),
        other: other.to_string(),
    })
}

fn healthy() -> WatcherState {
    WatcherState::Watching {
        installed: 12,
        wanted: 12,
        budget: WatchBudget::Derived {
            watches: 4038,
            from_watches: 516_898,
            from_instances: 128,
        },
    }
}

/// A policy already publishing normally: the watcher has reported, and one
/// reading is on the wire. The state every test that is not about start-up
/// wants to begin from.
fn settled() -> FeedPolicy {
    let mut policy = FeedPolicy::new();
    policy.note_watcher(healthy());
    let first = policy.observe(
        0,
        UnixSeconds(100),
        SweepTrigger::StreamOpen,
        reading("1", &[("refs/heads/main", "aaa")], "clean"),
    );
    assert!(first.is_some(), "the first reading always publishes");
    policy
}

#[test]
fn the_first_reading_publishes_and_cannot_name_what_moved() {
    let mut policy = FeedPolicy::new();
    policy.note_watcher(healthy());
    let snapshot = policy
        .observe(
            0,
            UnixSeconds(1),
            SweepTrigger::StreamOpen,
            reading("7", &[("refs/heads/main", "aaa")], "clean"),
        )
        .expect("a stream's first snapshot is always published");
    assert_eq!(snapshot.generation, Some(token("7")));
    assert_eq!(
        snapshot.changed,
        RefDelta::Unknown,
        "there is no previous reading to difference against, and an empty \
         Named list would claim there is"
    );
}

#[test]
fn an_unchanged_sweep_publishes_nothing() {
    let mut policy = settled();
    let again = policy.observe(
        3_000,
        UnixSeconds(103),
        SweepTrigger::Timer,
        reading("1", &[("refs/heads/main", "aaa")], "clean"),
    );
    assert!(
        again.is_none(),
        "the state is the one every open stream already holds"
    );
}

#[test]
fn a_moved_ref_is_published_and_named() {
    let mut policy = settled();
    let snapshot = policy
        .observe(
            3_000,
            UnixSeconds(103),
            SweepTrigger::Hint,
            reading("2", &[("refs/heads/main", "bbb")], "clean"),
        )
        .expect("a moved generation publishes");
    match snapshot.changed {
        RefDelta::Named { refs, other } => {
            assert_eq!(
                refs.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
                ["refs/heads/main"]
            );
            assert!(!other, "nothing outside the refs moved");
        }
        RefDelta::Unknown => panic!("the previous reading is right there to compare against"),
    }
}

#[test]
fn a_worktree_only_change_names_no_ref_and_says_so() {
    let mut policy = settled();
    let snapshot = policy
        .observe(
            3_000,
            UnixSeconds(103),
            SweepTrigger::Hint,
            reading("2", &[("refs/heads/main", "aaa")], "one file edited"),
        )
        .expect("a moved generation publishes");
    assert_eq!(
        snapshot.changed,
        RefDelta::Named {
            refs: Vec::new(),
            other: true
        },
        "no ref moved, and the flag is what stops a client calling this \
         irrelevant to a commit"
    );
}

#[test]
fn a_deleted_ref_is_named_too() {
    let mut policy = settled();
    let snapshot = policy
        .observe(
            3_000,
            UnixSeconds(103),
            SweepTrigger::Hint,
            reading("2", &[], "clean"),
        )
        .expect("a moved generation publishes");
    match snapshot.changed {
        RefDelta::Named { refs, .. } => assert_eq!(
            refs.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            ["refs/heads/main"],
            "a ref that disappeared moved as surely as one that was retargeted"
        ),
        RefDelta::Unknown => panic!("there is a previous reading"),
    }
}

// --- #554: the self-write invariant ----------------------------------------

#[test]
fn the_state_an_app_write_published_is_not_published_again_by_the_next_sweep() {
    // #554 acceptance 2, counted rather than inspected: two publishes for two
    // distinct states, and the sweep that follows the write adds none.
    let mut policy = settled();
    let mut published = 0;

    let after_write = policy.observe(
        1_000,
        UnixSeconds(101),
        SweepTrigger::AppWrite,
        reading("2", &[("refs/heads/main", "bbb")], "clean"),
    );
    published += usize::from(after_write.is_some());

    for tick in 1..=5 {
        let sweep = policy.observe(
            1_000 + tick * 2_000,
            UnixSeconds(101 + tick as i64),
            SweepTrigger::Timer,
            reading("2", &[("refs/heads/main", "bbb")], "clean"),
        );
        published += usize::from(sweep.is_some());
    }

    assert_eq!(
        published, 1,
        "the app's own write published once; five sweeps over the same state \
         published nothing"
    );
}

#[test]
fn a_change_that_lands_between_a_write_and_its_publish_is_announced_not_swallowed() {
    // The direction that loses data (#554 acceptance 3). The app writes, and an
    // external change lands before the post-write reading — so that reading
    // observes the COMBINED state. Publishing it is what makes the window stop
    // mattering: whatever it saw has now been told to every open stream.
    let mut policy = settled();
    let combined = policy
        .observe(
            1_000,
            UnixSeconds(101),
            SweepTrigger::AppWrite,
            reading(
                "3",
                &[("refs/heads/main", "bbb"), ("refs/heads/theirs", "ccc")],
                "clean",
            ),
        )
        .expect("the combined state differs from what was last published");
    match combined.changed {
        RefDelta::Named { refs, .. } => {
            let named: Vec<_> = refs.iter().map(|r| r.as_str()).collect();
            assert!(
                named.contains(&"refs/heads/theirs"),
                "the external ref must be in the announcement: {named:?}"
            );
        }
        RefDelta::Unknown => panic!("there is a previous reading"),
    }
}

#[test]
fn a_write_that_never_published_leaves_the_next_sweep_free_to_publish() {
    // #554 acceptance 4 at the level of the decision: `published` is written
    // only by publishing, so a write that died before publishing recorded
    // nothing, and the next ordinary sweep sees a difference. The failure mode
    // of this mechanism is one extra read — never a suppressed change.
    let mut policy = settled();
    // ... the write happened in the repository; the publish never ran ...
    let sweep = policy
        .observe(
            3_000,
            UnixSeconds(103),
            SweepTrigger::Timer,
            reading("2", &[("refs/heads/main", "bbb")], "clean"),
        )
        .expect("the repository moved and nothing had recorded that it had");
    assert_eq!(sweep.generation, Some(token("2")));
}

// --- #553: the watcher's own health, on evidence ---------------------------

#[test]
fn a_hint_inside_the_grace_window_credits_the_watcher() {
    let mut policy = settled();
    policy.observe(
        3_000,
        UnixSeconds(103),
        SweepTrigger::Timer,
        reading("2", &[("refs/heads/main", "bbb")], "clean"),
    );
    // The hint was a few milliseconds behind the sweep that beat it.
    policy.note_hint(3_100);
    policy.settle_due(10_000);
    assert_eq!(policy.misses().hinted, 1);
    assert_eq!(policy.misses().missed, 0);
}

#[test]
fn a_change_no_hint_ever_followed_is_counted_as_a_miss() {
    let mut policy = settled();
    policy.observe(
        3_000,
        UnixSeconds(103),
        SweepTrigger::Timer,
        reading("2", &[("refs/heads/main", "bbb")], "clean"),
    );
    policy.settle_due(3_000 + MISS_GRACE.as_millis() as u64 + 1);
    assert_eq!(policy.misses().missed, 1, "no hint arrived in the window");
    assert_eq!(policy.misses().last_missed_at_ms, Some(3_000));
}

#[test]
fn a_quiet_repository_still_settles_a_pending_verdict() {
    // The verdict must not wait for the next change: a dead watcher's
    // repository looks quiet, which is exactly when the evidence is needed.
    let mut policy = settled();
    policy.observe(
        3_000,
        UnixSeconds(103),
        SweepTrigger::Timer,
        reading("2", &[("refs/heads/main", "bbb")], "clean"),
    );
    assert_eq!(policy.misses().missed, 0, "not settled yet");
    policy.settle_due(60_000);
    assert_eq!(policy.misses().missed, 1);
}

#[test]
fn one_miss_does_not_condemn_a_watcher_but_a_run_of_them_does() {
    let mut policy = settled();
    let mut generation = 1u32;
    for round in 0..MIN_CHANGES_BEFORE_VERDICT {
        generation += 1;
        let now = 3_000 + u64::from(round) * 3_000;
        let snapshot = policy.observe(
            now,
            UnixSeconds(200 + i64::from(round)),
            SweepTrigger::Timer,
            reading(
                &generation.to_string(),
                &[("refs/heads/main", &format!("oid{generation}"))],
                "clean",
            ),
        );
        assert!(snapshot.is_some(), "each round moved the generation");
        policy.settle_due(now + MISS_GRACE.as_millis() as u64 + 1);
        if round + 1 < MIN_CHANGES_BEFORE_VERDICT {
            assert!(
                matches!(
                    policy.health(&SweepOutcome::Blind {
                        reason: String::new()
                    }),
                    ChangeFeedHealth::Blind { .. }
                ),
                "sanity: blind wins over everything"
            );
        }
    }
    generation += 1;
    let verdict = policy
        .observe(
            90_000,
            UnixSeconds(300),
            SweepTrigger::Timer,
            reading(
                &generation.to_string(),
                &[("refs/heads/main", "final")],
                "clean",
            ),
        )
        .expect("the generation moved again");
    assert_eq!(
        verdict.health,
        ChangeFeedHealth::SweepOnly {
            reason: WatcherLoss::Unreliable {
                missed: MIN_CHANGES_BEFORE_VERDICT,
                hinted: 0
            }
        },
        "ten misses against no hints is evidence, and it is published"
    );
}

// --- health is never inferable from silence --------------------------------

#[test]
fn a_bounded_watch_set_is_a_different_health_from_a_complete_one() {
    let mut policy = settled();
    policy.note_watcher(WatcherState::Watching {
        installed: 64,
        wanted: 9_200,
        budget: WatchBudget::Undetermined { watches: 64 },
    });
    let snapshot = policy
        .observe(
            3_000,
            UnixSeconds(103),
            SweepTrigger::Timer,
            reading("1", &[("refs/heads/main", "aaa")], "clean"),
        )
        .expect("the health moved even though the generation did not");
    assert_eq!(
        snapshot.health,
        ChangeFeedHealth::Bounded {
            watched: 64,
            wanted: 9_200,
            budget: WatchBudget::Undetermined { watches: 64 }
        }
    );
    assert_eq!(
        snapshot.generation,
        Some(token("1")),
        "the reading is unchanged; it is the coverage that moved"
    );
}

#[test]
fn a_sweep_that_could_not_read_publishes_blind_and_carries_no_generation() {
    let mut policy = settled();
    let snapshot = policy
        .observe(
            3_000,
            UnixSeconds(103),
            SweepTrigger::Timer,
            SweepOutcome::Blind {
                reason: "git could not be run".to_string(),
            },
        )
        .expect("becoming unable to look is a change worth publishing");
    assert_eq!(
        snapshot.generation, None,
        "a stale last-known value is indistinguishable from a fresh one"
    );
    assert_eq!(
        snapshot.health,
        ChangeFeedHealth::Blind {
            reason: "git could not be run".to_string(),
            since: UnixSeconds(103)
        }
    );
}

#[test]
fn a_blind_feed_does_not_republish_the_same_blindness_every_sweep() {
    let mut policy = settled();
    let blind = || SweepOutcome::Blind {
        reason: "git could not be run".to_string(),
    };
    assert!(policy
        .observe(3_000, UnixSeconds(103), SweepTrigger::Timer, blind())
        .is_some());
    assert!(
        policy
            .observe(6_000, UnixSeconds(106), SweepTrigger::Timer, blind())
            .is_none(),
        "the client already knows; repeating it every two seconds is noise"
    );
}

#[test]
fn recovering_from_blind_publishes_and_cannot_name_what_moved_while_it_was_blind() {
    let mut policy = settled();
    policy.observe(
        3_000,
        UnixSeconds(103),
        SweepTrigger::Timer,
        SweepOutcome::Blind {
            reason: "git could not be run".to_string(),
        },
    );
    let back = policy
        .observe(
            6_000,
            UnixSeconds(106),
            SweepTrigger::Timer,
            reading("1", &[("refs/heads/main", "aaa")], "clean"),
        )
        .expect("the health moved back, which every open stream must be told");
    assert_eq!(
        back.changed,
        RefDelta::Unknown,
        "nothing was read while blind, so there is no difference to state"
    );
    assert!(matches!(back.health, ChangeFeedHealth::Watching { .. }));
}

#[test]
fn blindness_outranks_every_other_health_reading() {
    let mut policy = settled();
    policy.note_watcher(WatcherState::Lost {
        reason: WatcherLoss::LimitReached { at: 64 },
        budget: WatchBudget::Undetermined { watches: 64 },
    });
    let snapshot = policy
        .observe(
            3_000,
            UnixSeconds(103),
            SweepTrigger::Timer,
            SweepOutcome::Blind {
                reason: "git could not be run".to_string(),
            },
        )
        .expect("both the watcher and the sweep changed state");
    assert!(
        matches!(snapshot.health, ChangeFeedHealth::Blind { .. }),
        "a feed that cannot read must not describe its watcher instead"
    );
}

// --- the cadence bound -----------------------------------------------------

#[test]
fn the_interval_is_never_shorter_than_ten_times_the_last_sweep() {
    let policy = settled();
    assert_eq!(
        policy.next_sweep_delay(Duration::from_millis(3)),
        SWEEP_BASE,
        "a 3 ms sweep is bounded by the base interval, not by its own cost"
    );
    assert_eq!(
        policy.next_sweep_delay(Duration::from_secs(2)),
        Duration::from_secs(20),
        "a two-second sweep pushes its own interval to twenty, with no \
         configuration and no size heuristic"
    );
}

#[test]
fn quiet_sweeps_back_off_and_a_change_resets_the_backoff() {
    let mut policy = settled();
    for round in 0..6 {
        policy.observe(
            3_000 + round * 1_000,
            UnixSeconds(200 + round as i64),
            SweepTrigger::Timer,
            reading("1", &[("refs/heads/main", "aaa")], "clean"),
        );
    }
    assert_eq!(
        policy.next_sweep_delay(Duration::from_millis(3)),
        SWEEP_MAX,
        "nothing has moved for six sweeps"
    );
    policy
        .observe(
            20_000,
            UnixSeconds(300),
            SweepTrigger::Timer,
            reading("2", &[("refs/heads/main", "bbb")], "clean"),
        )
        .expect("the generation moved");
    assert_eq!(
        policy.next_sweep_delay(Duration::from_millis(3)),
        SWEEP_BASE,
        "a change puts the feed back on its prompt cadence"
    );
}

#[test]
fn a_hint_resets_the_backoff_even_before_the_sweep_it_asks_for() {
    let mut policy = settled();
    for round in 0..6 {
        policy.observe(
            3_000 + round * 1_000,
            UnixSeconds(200 + round as i64),
            SweepTrigger::Timer,
            reading("1", &[("refs/heads/main", "aaa")], "clean"),
        );
    }
    assert_eq!(policy.next_sweep_delay(Duration::from_millis(3)), SWEEP_MAX);
    policy.note_hint(20_000);
    assert_eq!(
        policy.next_sweep_delay(Duration::from_millis(3)),
        SWEEP_BASE,
        "the watcher says it is worth looking; a 60-second backoff would \
         make the hint pointless"
    );
}

#[test]
fn a_watcher_that_has_not_reported_is_not_reported_as_watching() {
    let mut policy = FeedPolicy::new();
    let snapshot = policy
        .observe(
            0,
            UnixSeconds(1),
            SweepTrigger::StreamOpen,
            reading("1", &[("refs/heads/main", "aaa")], "clean"),
        )
        .expect("the first reading publishes");
    match snapshot.health {
        ChangeFeedHealth::SweepOnly { reason } => assert!(
            matches!(reason, WatcherLoss::Backend { .. }),
            "silence from the watcher is stated, not rendered as health"
        ),
        other => panic!("a watcher that has said nothing cannot be Watching: {other:?}"),
    }
}

// --- #664 review, finding 5: the two counts must be over one population ----

#[test]
fn an_ordinary_successful_hint_is_credited_to_the_watcher() {
    // The counts only mean something if they are drawn from the same
    // population: changes the watcher had an opportunity to announce. Counting
    // only timer sweeps sampled misses and near-races and excluded every
    // ordinary success — so twenty hint-driven changes left `hinted` at zero,
    // and ten later unhinted changes could latch `Unreliable` on a watcher that
    // had been working the whole time.
    let mut policy = settled();
    for round in 1..=20u32 {
        policy.note_hint(u64::from(round) * 1_000);
        let published = policy.observe(
            u64::from(round) * 1_000 + 10,
            UnixSeconds(200 + i64::from(round)),
            SweepTrigger::Hint,
            reading(
                &(round + 1).to_string(),
                &[("refs/heads/main", &format!("oid{round}"))],
                "clean",
            ),
        );
        assert!(published.is_some(), "each round moved the generation");
    }
    assert_eq!(
        policy.misses().hinted,
        20,
        "twenty changes the watcher announced are twenty successes"
    );
    assert_eq!(policy.misses().missed, 0);
}

#[test]
fn a_watcher_with_a_working_record_is_not_condemned_by_a_later_run_of_misses() {
    // The consequence of the fix, stated as the property that matters. Twenty
    // successes then ten misses is a watcher that mostly works; before the fix
    // this latched `Unreliable { missed: 10, hinted: 0 }` — permanently, since
    // `untrusted` is never cleared.
    let mut policy = settled();
    let mut generation = 1u32;
    for round in 1..=20u32 {
        generation += 1;
        policy.note_hint(u64::from(round) * 1_000);
        policy.observe(
            u64::from(round) * 1_000 + 10,
            UnixSeconds(200 + i64::from(round)),
            SweepTrigger::Hint,
            reading(
                &generation.to_string(),
                &[("refs/heads/main", &format!("oid{generation}"))],
                "clean",
            ),
        );
    }
    for round in 1..=10u32 {
        generation += 1;
        let now = 100_000 + u64::from(round) * 3_000;
        policy.observe(
            now,
            UnixSeconds(400 + i64::from(round)),
            SweepTrigger::Timer,
            reading(
                &generation.to_string(),
                &[("refs/heads/main", &format!("oid{generation}"))],
                "clean",
            ),
        );
        policy.settle_due(now + MISS_GRACE.as_millis() as u64 + 1);
    }
    assert_eq!(policy.misses().missed, 10);
    assert_eq!(policy.misses().hinted, 20);

    generation += 1;
    let verdict = policy
        .observe(
            200_000,
            UnixSeconds(500),
            SweepTrigger::Timer,
            reading(
                &generation.to_string(),
                &[("refs/heads/main", "final")],
                "clean",
            ),
        )
        .expect("the generation moved");
    assert!(
        matches!(verdict.health, ChangeFeedHealth::Watching { .. }),
        "ten misses against twenty successes is not evidence of an unreliable \
         watcher: {:?}",
        verdict.health
    );
}

#[test]
fn the_duty_floor_is_ten_times_the_last_read_whoever_asks() {
    // The floor is a separate bound from the backoff, and separate for a
    // reason: the backoff decides how long a quiet feed waits, while this
    // decides how soon ANY read may follow another. A hint that schedules a
    // read for `now` respects the first and bypasses the second.
    let policy = settled();
    assert_eq!(
        policy.duty_floor(Duration::from_millis(44)),
        Duration::from_millis(440)
    );
    assert_eq!(
        policy.duty_floor(Duration::from_secs(2)),
        Duration::from_secs(20)
    );
    assert!(
        policy.duty_floor(Duration::from_millis(3)) < SWEEP_BASE,
        "on a cheap repository the base interval is what binds, and the floor \
         must not inflate it"
    );
}
