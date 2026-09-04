//! What the graph panel is doing, and the three rules that move it.
//!
//! Framework-free on purpose: nothing here touches Leptos or `crate::api`, so
//! `cargo test -p git-vista --bins` compiles and runs all of it. The shell in
//! [`crate::app`] is wasm-only and drives these rules rather than restating
//! them; a source census pins it to their answers.

/// What the graph panel is doing, independent of what the seed resource
/// happens to be holding. Each variant carries the reload epoch it belongs to,
/// so a reply for an earlier epoch can never advance the phase.
///
/// The phase exists because a Leptos resource keeps serving its previous value
/// while the next one loads. Rendering off the resource alone would let the
/// *old* history stay mounted across a reload — and, worse, mask the drift
/// notice after a `409`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryPhase {
    SeedLoading { epoch: u64 },
    Ready { epoch: u64 },
    DriftReloading { epoch: u64 },
    SeedError { epoch: u64 },
}

/// What the phase becomes when the graph epoch bumps — `None` to leave it
/// exactly as it is.
///
/// Every epoch (Refresh, a post-operation reload, a drift reload) retires the
/// mounted history, and the default answer is [`HistoryPhase::SeedLoading`]
/// for the new epoch. The one exception is the 409 path: a drift reload has
/// **already** announced itself with the epoch it is reloading *into*, and
/// overwriting that with `SeedLoading` would drop the "History moved" copy
/// that is the only thing explaining why the graph vanished.
///
/// The epoch comparison is the whole rule. A `DriftReloading` left over from
/// an *earlier* epoch is not an announcement about this one, so it is
/// replaced like anything else — otherwise a stale drift notice would sit
/// over a reload it has nothing to do with.
pub fn phase_for_epoch_bump(current: HistoryPhase, epoch: u64) -> Option<HistoryPhase> {
    if current == (HistoryPhase::DriftReloading { epoch }) {
        None
    } else {
        Some(HistoryPhase::SeedLoading { epoch })
    }
}

/// Everything the graph panel does when a page fetch comes back `409` — the
/// server telling this canvas the history moved underneath it.
///
/// Three settings, and they travel together because they answer one question:
/// the drift announcement, print's fate, and the completeness flag. Returned
/// as a value rather than applied, so the wasm-only canvas is left with
/// nothing to decide — only signals to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftReload {
    /// The announcement: [`HistoryPhase::DriftReloading`] carrying the epoch
    /// being reloaded *into*, never the one being left behind. See
    /// [`drift_reload`] for why that distinction is the whole mechanism.
    pub phase: HistoryPhase,
    /// Print closes. A print sheet is rendered from one generation of history
    /// and cannot span two, and this reply says the generation is already
    /// gone — so there is no honest way to keep it open across the reload.
    pub print_open: bool,
    /// The new epoch has not been seeded yet, let alone paged to the end, so
    /// the full-history latch drops. Leaving it set would tell the UI the
    /// reload it is about to start has already finished.
    pub complete: bool,
}

/// Bump the graph epoch and describe the drift announcement that goes with it.
///
/// # Why this takes the bump instead of the epoch
///
/// The announcement has to carry the epoch the bump *produced*. The App's
/// reload effect asks [`phase_for_epoch_bump`], which preserves a
/// `DriftReloading` only when its epoch equals the epoch that just bumped —
/// so an announcement built from the epoch the canvas was standing on *before*
/// the drift fails that comparison and is replaced with a plain
/// [`HistoryPhase::SeedLoading`]. The user-visible cost of getting it backwards
/// is precise: the "History moved" copy, which is the only thing on screen
/// explaining why the graph they were reading vanished, is overwritten by a
/// bare "Loading…".
///
/// That ordering used to live as a comment over four statements in
/// `app/canvas.rs`, where it was a rule a reader had to honour. Taking the
/// bump as a closure makes it a rule the type system honours instead: there is
/// no way to call this function and get back a `DriftReloading` carrying a
/// pre-bump epoch, because the only epoch in scope here is the one `bump`
/// returned. The wrong order is not merely discouraged, it is unrepresentable.
///
/// `bump` is [`FnOnce`] on purpose — an epoch bump is not idempotent, and one
/// 409 must advance the epoch exactly once.
pub fn drift_reload(bump: impl FnOnce() -> u64) -> DriftReload {
    let epoch = bump();
    DriftReload {
        phase: HistoryPhase::DriftReloading { epoch },
        print_open: false,
        complete: false,
    }
}

/// What one seed reply is allowed to do to the phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedPromotion {
    /// The reply belongs to a retired epoch. Change nothing at all — not the
    /// phase, not the completeness flag, not the full-history latch.
    Ignore,
    /// The seed landed for the live epoch. `complete` says whether page 1
    /// already held the whole history.
    Ready { epoch: u64, complete: bool },
    /// The seed did not land for the live epoch.
    Failed { epoch: u64 },
}

/// Promote a seed to its phase — but only the *current* epoch's.
///
/// The resource keeps its previous value while the next load runs, so an
/// out-of-order completion would otherwise mark a live reload `Ready` with
/// retired data. `complete` is `None` when the seed failed, which is why the
/// failure and success arms cannot be told apart by the epoch alone.
///
/// The epoch in the answer is deliberately the **seed's** own, not the live
/// one: they are equal on every path that returns anything but
/// [`SeedPromotion::Ignore`], and taking it from the seed is what makes that
/// equality visible rather than assumed.
pub fn promote_seed(seed_epoch: u64, live_epoch: u64, complete: Option<bool>) -> SeedPromotion {
    if seed_epoch != live_epoch {
        return SeedPromotion::Ignore;
    }
    match complete {
        Some(complete) => SeedPromotion::Ready {
            epoch: seed_epoch,
            complete,
        },
        None => SeedPromotion::Failed { epoch: seed_epoch },
    }
}

/// Whether an armed seed-retry timer still speaks for the failure it was
/// armed for.
///
/// #218's bounded auto-retry arms a `set_timeout` against one
/// [`HistoryPhase::SeedError`] epoch. By the time it fires the user may have
/// refreshed manually, or a drift reload may have superseded the failure — the
/// panel is no longer showing the epoch this retry was for, and bumping anyway
/// would race a reload already in flight.
///
/// Exact equality, not "is it still a SeedError": a *different* epoch's
/// failure is a different failure chain with its own budget, and letting this
/// timer fire into it would spend an attempt the new chain never counted.
pub fn seed_retry_still_wanted(current: HistoryPhase, armed_for: u64) -> bool {
    current == (HistoryPhase::SeedError { epoch: armed_for })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    /// The source of the wasm-only shell, read as text. Nothing below mounts a
    /// DOM; these read the effects the way `features::a11y::audit` reads the
    /// markup it cannot mount, and for the same reason — `app/mod.rs` is
    /// `#[cfg(target_arch = "wasm32")]`, so this is the only way a host test
    /// can see it at all.
    const APP_MOD: &str = include_str!("../../app/mod.rs");

    /// The canvas, read as text, for the same reason and by the same means.
    /// `app/canvas.rs` is wasm-only too, so its 409 arm is invisible to every
    /// test above; this is how the arm gets pinned to `drift_reload` instead of
    /// being trusted to keep calling it.
    const CANVAS: &str = include_str!("../../app/canvas.rs");

    /// Text between `open` and the first `close` after it.
    fn block_after(haystack: &str, open: &str, close: &str, what: &str) -> String {
        let after = haystack
            .split_once(open)
            .unwrap_or_else(|| panic!("the source no longer contains {what} (anchor: {open:?})"))
            .1;
        let end = after.find(close).unwrap_or_else(|| {
            panic!("{what} is no longer a closed block (looking for {close:?})")
        });
        after[..end].to_string()
    }

    // ---- phase_for_epoch_bump -------------------------------------------

    #[test]
    fn an_epoch_bump_retires_the_mounted_history() {
        assert_eq!(
            phase_for_epoch_bump(HistoryPhase::Ready { epoch: 4 }, 5),
            Some(HistoryPhase::SeedLoading { epoch: 5 }),
            "a bump off a mounted graph must announce the new epoch is loading"
        );
        assert_eq!(
            phase_for_epoch_bump(HistoryPhase::SeedError { epoch: 4 }, 5),
            Some(HistoryPhase::SeedLoading { epoch: 5 }),
            "a manual Retry bumps off a failure and must clear it"
        );
        assert_eq!(
            phase_for_epoch_bump(HistoryPhase::SeedLoading { epoch: 4 }, 5),
            Some(HistoryPhase::SeedLoading { epoch: 5 }),
            "a bump during a load re-announces for the epoch now being loaded"
        );
    }

    #[test]
    fn a_drift_reload_keeps_its_own_announcement() {
        // The 409 path: canvas.rs sets DriftReloading with the epoch it is
        // reloading *into*, then bumps. If this returned Some(SeedLoading) the
        // "History moved — reloading…" copy would be replaced by a bare
        // "Loading history…", and the only explanation the user gets for the
        // graph disappearing would be gone.
        assert_eq!(
            phase_for_epoch_bump(HistoryPhase::DriftReloading { epoch: 5 }, 5),
            None,
            "a drift reload's own announcement must survive the bump it made"
        );
    }

    #[test]
    fn a_stale_drift_announcement_does_not_survive_a_later_bump() {
        // The half of the rule the epoch comparison buys, and the half a
        // "is it DriftReloading?" test would miss entirely: a drift notice
        // left over from epoch 5 says nothing about epoch 6, so it is
        // replaced like any other phase.
        assert_eq!(
            phase_for_epoch_bump(HistoryPhase::DriftReloading { epoch: 5 }, 6),
            Some(HistoryPhase::SeedLoading { epoch: 6 }),
            "a drift notice from an earlier epoch must not sit over a later reload"
        );
        assert_eq!(
            phase_for_epoch_bump(HistoryPhase::DriftReloading { epoch: 7 }, 6),
            Some(HistoryPhase::SeedLoading { epoch: 6 }),
            "and neither must one from a later epoch"
        );
    }

    // ---- drift_reload ---------------------------------------------------

    #[test]
    fn a_drift_reload_announces_the_epoch_the_bump_produced() {
        let epoch = Cell::new(5u64);
        let reload = drift_reload(|| {
            epoch.set(epoch.get() + 1);
            epoch.get()
        });

        assert_eq!(
            reload.phase,
            HistoryPhase::DriftReloading { epoch: 6 },
            "the announcement must carry the post-bump epoch, not the one the \
             canvas was standing on when the 409 arrived"
        );
        assert!(!reload.print_open, "a 409 closes print");
        assert!(
            !reload.complete,
            "the new epoch has not been seeded, so the full-history latch drops"
        );
    }

    #[test]
    fn the_bump_runs_exactly_once() {
        // An epoch bump is not idempotent: calling it twice would advance past
        // the generation the announcement names, and `phase_for_epoch_bump`
        // would then discard that announcement as stale (see the test below).
        let calls = Cell::new(0u32);
        let reload = drift_reload(|| {
            calls.set(calls.get() + 1);
            7
        });
        assert_eq!(calls.get(), 1, "drift_reload must bump the epoch once");
        assert_eq!(reload.phase, HistoryPhase::DriftReloading { epoch: 7 });
    }

    #[test]
    fn the_drift_announcement_survives_the_reload_effect_it_triggers() {
        // The composition, and the whole point of the pair. Everything above
        // proves each half in isolation; #612's origin was a defect that lived
        // in the *line joining* two host-tested halves, in wasm-only code no
        // runner executed. So this asserts the join: the phase `drift_reload`
        // produces is fed to the rule `app/mod.rs`'s reload effect applies when
        // it observes the very epoch that bump created.
        let epoch = Cell::new(5u64);
        let reload = drift_reload(|| {
            epoch.set(epoch.get() + 1);
            epoch.get()
        });

        assert_eq!(
            phase_for_epoch_bump(reload.phase, epoch.get()),
            None,
            "the reload effect must leave the drift announcement alone — \
             overwriting it replaces the only copy explaining why the graph \
             vanished with a bare \"Loading…\""
        );
    }

    #[test]
    fn announcing_before_the_bump_would_lose_the_drift_notice() {
        // The ordering claim, made checkable rather than only documented.
        //
        // `drift_reload` takes the bump as a closure precisely so this shape is
        // unrepresentable through it — there is no pre-bump epoch in scope to
        // announce. This test builds the wrong announcement by hand anyway, to
        // pin down *why* that matters: an announcement made before the bump
        // carries the old epoch, and the reload effect then discards it.
        //
        // If this assertion ever flips to `None`, the epoch comparison in
        // `phase_for_epoch_bump` has stopped discriminating and `drift_reload`'s
        // closure signature is guarding nothing.
        let before_the_bump = HistoryPhase::DriftReloading { epoch: 5 };
        assert_eq!(
            phase_for_epoch_bump(before_the_bump, 6),
            Some(HistoryPhase::SeedLoading { epoch: 6 }),
            "an announcement made before the bump is exactly the regression the \
             closure signature exists to prevent: the reload effect replaces it"
        );
    }

    // ---- promote_seed ---------------------------------------------------

    #[test]
    fn a_retired_epochs_reply_changes_nothing() {
        // The resource keeps serving its previous value while the next load
        // runs. Both outcomes of a stale reply are Ignore — a stale *failure*
        // raising SeedError over a live load is the worse of the two, because
        // it would show an error for a request that is still in flight.
        assert_eq!(
            promote_seed(4, 5, Some(true)),
            SeedPromotion::Ignore,
            "a retired epoch's success must not mount over the live reload"
        );
        assert_eq!(
            promote_seed(4, 5, None),
            SeedPromotion::Ignore,
            "a retired epoch's failure must not raise an error over a live load"
        );
    }

    #[test]
    fn a_reply_from_an_epoch_the_shell_has_not_reached_is_stale_too() {
        // The half a `seed_epoch < live_epoch` guard would silently drop.
        // `force_bump` is not the only writer of the epoch, and a signal read
        // with `get_untracked` inside an effect can lag the resource that was
        // keyed on it — so "ahead" is reachable, and mounting off it would put
        // a graph on screen for an epoch the rest of the shell has not
        // switched to. Equality is the rule; ordering is not.
        assert_eq!(promote_seed(6, 5, Some(false)), SeedPromotion::Ignore);
        assert_eq!(promote_seed(6, 5, None), SeedPromotion::Ignore);
    }

    #[test]
    fn a_live_seed_carries_its_completeness_through_unchanged() {
        assert_eq!(
            promote_seed(5, 5, Some(true)),
            SeedPromotion::Ready {
                epoch: 5,
                complete: true
            },
            "a repository complete on page 1 must say so — Print needs all of it"
        );
        assert_eq!(
            promote_seed(5, 5, Some(false)),
            SeedPromotion::Ready {
                epoch: 5,
                complete: false
            },
            "and a paged one must not claim completeness it does not have"
        );
    }

    #[test]
    fn a_failed_seed_is_attributed_to_the_epoch_it_was_fetched_for() {
        assert_eq!(
            promote_seed(5, 5, None),
            SeedPromotion::Failed { epoch: 5 },
            "the failure must name its own epoch, or the retry chain and the \
             status line disagree about which failure they are describing"
        );
    }

    #[test]
    fn a_failure_never_reports_a_completeness() {
        // The shape of the enum is the guarantee: `Failed` has no `complete`
        // field, so the "history is complete" flag and the full-history latch
        // are unreachable from the failure path by construction. This test
        // exists so that flattening `SeedPromotion` into something with an
        // `Option<bool>` on every arm cannot happen silently.
        for live in 0..3u64 {
            match promote_seed(live, live, None) {
                SeedPromotion::Failed { epoch } => assert_eq!(epoch, live),
                other => panic!("a seed that did not land reported {other:?}"),
            }
        }
    }

    // ---- seed_retry_still_wanted ----------------------------------------

    #[test]
    fn an_armed_retry_fires_into_the_failure_it_was_armed_for() {
        assert!(
            seed_retry_still_wanted(HistoryPhase::SeedError { epoch: 5 }, 5),
            "the failure is still on screen — the retry is exactly what it is for"
        );
    }

    #[test]
    fn an_armed_retry_does_not_fire_once_the_failure_is_off_screen() {
        // The user refreshed manually, or a drift reload superseded the
        // failure. Firing now races a reload already in flight.
        for superseded in [
            HistoryPhase::SeedLoading { epoch: 5 },
            HistoryPhase::Ready { epoch: 5 },
            HistoryPhase::DriftReloading { epoch: 5 },
        ] {
            assert!(
                !seed_retry_still_wanted(superseded, 5),
                "the failure was superseded by {superseded:?} — bumping now races \
                 a reload already in flight"
            );
        }
    }

    #[test]
    fn an_armed_retry_does_not_fire_into_a_different_failure_chain() {
        // Still a SeedError, so a phase-only check would wave this through —
        // and the phase-only check is the plausible simplification, because
        // `HistoryPhase::SeedError` reads like the whole condition. It is not:
        // #218 budgets attempts *per failure chain*, so a timer armed for
        // epoch 5 firing into epoch 6's failure spends an attempt that chain
        // never counted, and the give-up arrives early and unexplained.
        assert!(!seed_retry_still_wanted(
            HistoryPhase::SeedError { epoch: 6 },
            5
        ));
        assert!(!seed_retry_still_wanted(
            HistoryPhase::SeedError { epoch: 4 },
            5
        ));
    }

    // ---- the seam ------------------------------------------------------
    //
    // Everything above proves the rules. These prove the shell still asks them,
    // which is the half that cannot be assumed: `app/mod.rs`'s effects and
    // `app/canvas.rs`'s fetch arms are both wasm-only, so a change that
    // re-derives any of these rules inline would leave every test above passing
    // while the shell stopped using the answer.

    #[test]
    fn the_epoch_reset_effect_asks_core_which_phase_to_set() {
        let body = block_after(
            APP_MOD,
            "let epoch = graph.get().epoch();",
            "    });",
            "the epoch-reset effect",
        );
        assert!(
            body.contains("phase_for_epoch_bump("),
            "the epoch-reset effect no longer calls `phase_for_epoch_bump`. \
             Effect body was:\n{body}"
        );
        assert!(
            !body.contains("HistoryPhase::DriftReloading"),
            "the epoch-reset effect names `DriftReloading` again, which means the \
             409 exception has been re-derived here instead of asked for. That is \
             precisely the shape #612 is about: this file is wasm-only, so the \
             second copy is unreachable from every test above. Effect body was:\n{body}"
        );
    }

    #[test]
    fn the_canvas_409_arm_asks_core_what_a_drift_reload_does() {
        let body = block_after(
            CANVAS,
            "status: HTTP_CONFLICT,",
            "\n                }\n",
            "the canvas's 409 arm",
        );
        assert!(
            body.contains("drift_reload("),
            "the canvas's 409 arm no longer calls `drift_reload`. The drift \
             response is a decision, and `app/canvas.rs` is wasm-only — a copy \
             re-derived here is unreachable from every test above. Arm body \
             was:\n{body}"
        );
        assert!(
            !body.contains("HistoryPhase::DriftReloading"),
            "the canvas's 409 arm names `DriftReloading` again, which means it \
             is constructing the announcement itself instead of asking for it. \
             That is the shape #612 is about, and it is also how the ordering \
             rule gets lost: built here, the announcement can carry the \
             pre-bump epoch, which `phase_for_epoch_bump` then discards. Arm \
             body was:\n{body}"
        );
        assert!(
            !body.contains("print_open.set(false)") && !body.contains("complete.set(false)"),
            "the canvas's 409 arm hard-codes print_open/complete again rather \
             than writing the values `drift_reload` returned. Those two flags \
             are part of the same decision; restating them here lets them drift \
             apart from the phase they travel with. Arm body was:\n{body}"
        );
    }

    #[test]
    fn the_promotion_effect_asks_core_what_a_seed_reply_may_do() {
        let body = block_after(
            APP_MOD,
            "let Some((epoch, complete, worktree))",
            "    });",
            "the seed-promotion effect",
        );
        assert!(
            body.contains("promote_seed("),
            "the seed-promotion effect no longer calls `promote_seed`. Body was:\n{body}"
        );
        assert!(
            !body.contains("!= graph.get_untracked().epoch()"),
            "the promotion effect compares epochs itself again. The stale-reply \
             rule has two homes now, and only one of them is tested. Body was:\n{body}"
        );
    }

    #[test]
    fn the_armed_retry_timer_asks_core_whether_it_is_still_wanted() {
        let body = block_after(
            APP_MOD,
            "let next_attempt = attempts_used + 1;",
            "    });",
            "the seed-retry timer",
        );
        assert!(
            body.contains("seed_retry_still_wanted("),
            "the armed retry timer no longer calls `seed_retry_still_wanted`. \
             Body was:\n{body}"
        );
        assert!(
            !body.contains("HistoryPhase::SeedError"),
            "the armed retry timer rebuilds the SeedError comparison inline. \
             Body was:\n{body}"
        );
    }

    #[test]
    fn every_phase_the_shell_can_set_comes_from_a_decision_this_file_makes() {
        // A completeness census in the shape #531 taught: one flag per variant,
        // ticked by an exhaustive match so a new variant is a *compile* error,
        // then asserted by name so a missing entry is a named red assertion and
        // never a stale count.
        #[derive(Default)]
        struct Census {
            seed_loading: bool,
            ready: bool,
            drift_reloading: bool,
            seed_error: bool,
        }
        let mut census = Census::default();
        for phase in [
            HistoryPhase::SeedLoading { epoch: 1 },
            HistoryPhase::Ready { epoch: 1 },
            HistoryPhase::DriftReloading { epoch: 1 },
            HistoryPhase::SeedError { epoch: 1 },
        ] {
            match phase {
                HistoryPhase::SeedLoading { .. } => census.seed_loading = true,
                HistoryPhase::Ready { .. } => census.ready = true,
                HistoryPhase::DriftReloading { .. } => census.drift_reloading = true,
                HistoryPhase::SeedError { .. } => census.seed_error = true,
            }
        }
        assert!(census.seed_loading, "SeedLoading is not in the list above");
        assert!(census.ready, "Ready is not in the list above");
        assert!(
            census.drift_reloading,
            "DriftReloading is not in the list above"
        );
        assert!(census.seed_error, "SeedError is not in the list above");

        // Which of them this crate decides where a test can watch. `Ready` and
        // `SeedError` come out of `promote_seed`, `SeedLoading` out of
        // `phase_for_epoch_bump`, and `DriftReloading` — the one that used to
        // be the exception, constructed inline in `canvas.rs`'s wasm-only 409
        // handler — now comes out of `drift_reload`. All four are decided here.
        assert_eq!(
            phase_for_epoch_bump(HistoryPhase::Ready { epoch: 0 }, 1),
            Some(HistoryPhase::SeedLoading { epoch: 1 })
        );
        assert_eq!(
            promote_seed(1, 1, Some(false)),
            SeedPromotion::Ready {
                epoch: 1,
                complete: false
            }
        );
        assert_eq!(promote_seed(1, 1, None), SeedPromotion::Failed { epoch: 1 });
        // The 409 path has moved into core, so this no longer guards a
        // decision — `the_canvas_409_arm_asks_core_what_a_drift_reload_does`
        // does that. What is left in `app/mod.rs` is the match arm that *renders*
        // the phase, and that is worth keeping pinned on its own account: with
        // no arm drawing it, `drift_reload` would still set a phase nothing put
        // on screen, and #64's drift notice would be gone with every test in
        // this file still green.
        assert!(
            APP_MOD.contains("HistoryPhase::DriftReloading"),
            "no code in app/mod.rs mentions DriftReloading any more. The 409 \
             path itself lives in `drift_reload` now, so this is about the \
             render arm: nothing draws the drift notice, and #64's \
             \"History moved\" copy went with it"
        );
    }
}
