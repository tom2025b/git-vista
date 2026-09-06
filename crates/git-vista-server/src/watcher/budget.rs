//! How many inotify watches this process allows itself (M12.06, #556).
//!
//! # The point of this module is that the number is not picked
//!
//! #556's first acceptance criterion is that the bound be *derived from
//! something*. An earlier design picked 64, justified against the historical
//! 8 192 inotify default; the operator's box reports **516 898**, so the
//! constant the bound was reasoned against was wrong there by a factor of 63.
//!
//! The correction has to go all the way down or it is the same mistake one
//! level in. So:
//!
//! | number | where it comes from |
//! |---|---|
//! | the dividend | `/proc/sys/fs/inotify/max_user_watches`, **read** |
//! | the divisor | `/proc/sys/fs/inotify/max_user_instances`, **read** |
//! | dividing by it at all | a **chosen policy**, argued below |
//! | the floor, 64 | a **chosen** safety factor |
//! | the ceiling, 4096 | a **chosen** safety factor |
//!
//! > `budget = clamp(max_user_watches / max_user_instances, 64, 4096)`
//!
//! **The division is a policy, not arithmetic the kernel endorses.** Nothing in
//! Linux says a process may hold `watches / instances` watches;
//! `max_user_instances` caps how many inotify *instances* one user may open and
//! `max_user_watches` caps the watches held across all of them. What the
//! division encodes is politeness: *budget as though every instance the user is
//! permitted were one of ours, and none of them may overspend.* Unlike a
//! hardcoded divisor it follows a machine configured differently, which is the
//! whole reason to read it.
//!
//! **Both clamp bounds are chosen, and each is chosen against a measurement.**
//! The floor is 64 against a watch set measured at 7 on a fresh clone, 12 on the
//! operator's live checkout and 14 for a linked worktree — 4.5× the largest of
//! them, so a starved or unreadable box still runs the whole set with room for
//! the `refs/` tree to quadruple. The ceiling is 4096 against the largest watch
//! shape ever measured for this repository (268 directories for a recursive
//! `.git`, a shape the design rejects anyway) — 15× it. Above the ceiling the
//! honest reading is not "this repository needs more watches" but "this
//! repository's ref tree is a pathology", and the bounded state is the right
//! answer rather than a bigger number.
//!
//! # "I could not read the limit" must never render as "the limit is large"
//!
//! Both files are absent on non-Linux and either can be unreadable in a
//! hardened container. That case falls to the floor — the conservative end —
//! and says so through a **distinct variant**, never through the number. The
//! distinction is not decorative: `8192 / 128` is exactly 64, so the number
//! alone cannot tell a computed budget from a defaulted one.

use git_vista_protocol::change_feed::WatchBudget;

/// A chosen safety factor, not a kernel value: 4.5× the largest watch set this
/// design has measured. See the module header.
pub(crate) const CHOSEN_FLOOR: usize = 64;

/// A chosen safety factor, not a kernel value: 15× the largest watch shape ever
/// measured for this repository. See the module header.
pub(crate) const CHOSEN_CEILING: usize = 4096;

const MAX_USER_WATCHES: &str = "/proc/sys/fs/inotify/max_user_watches";
const MAX_USER_INSTANCES: &str = "/proc/sys/fs/inotify/max_user_instances";

/// The two kernel limits the budget is a function of. Both are **per user** and
/// shared with every editor, language server and file manager the operator is
/// running, so this process takes a *share* of them rather than a fixed count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InotifyLimits {
    pub(crate) max_user_watches: Option<usize>,
    pub(crate) max_user_instances: Option<usize>,
}

impl InotifyLimits {
    /// Read both sysctls. Neither existing is the ordinary case off Linux, and
    /// is not an error.
    pub(crate) fn read() -> Self {
        Self {
            max_user_watches: read_count(MAX_USER_WATCHES),
            max_user_instances: read_count(MAX_USER_INSTANCES),
        }
    }
}

fn read_count(path: &str) -> Option<usize> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Derive the budget from whatever the kernel was willing to say.
///
/// Read once at start-up and never re-read: a budget that changes under a
/// running watcher is a budget nothing can reason about.
pub(crate) fn derive(limits: InotifyLimits) -> WatchBudget {
    match (limits.max_user_watches, limits.max_user_instances) {
        (Some(watches), Some(instances)) if instances > 0 => WatchBudget::Derived {
            watches: (watches / instances).clamp(CHOSEN_FLOOR, CHOSEN_CEILING),
            from_watches: watches,
            from_instances: instances,
        },
        // Either file unreadable, or a divisor of zero. Not an optimistic
        // default — a stated condition, at the conservative end.
        _ => WatchBudget::Undetermined {
            watches: CHOSEN_FLOOR,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(watches: Option<usize>, instances: Option<usize>) -> InotifyLimits {
        InotifyLimits {
            max_user_watches: watches,
            max_user_instances: instances,
        }
    }

    #[test]
    fn the_operators_box_divides_to_its_measured_budget() {
        // The two numbers read from this machine on 2026-08-30 (spec E6a).
        assert_eq!(
            derive(limits(Some(516_898), Some(128))),
            WatchBudget::Derived {
                watches: 4038,
                from_watches: 516_898,
                from_instances: 128
            }
        );
    }

    #[test]
    fn a_starved_box_is_held_up_by_the_chosen_floor() {
        // 1024 / 128 = 8, which is below the 12 watches this repository needs
        // today. Without the floor the budget would bind before the watch set
        // was even installed.
        assert_eq!(
            derive(limits(Some(1024), Some(128))),
            WatchBudget::Derived {
                watches: CHOSEN_FLOOR,
                from_watches: 1024,
                from_instances: 128
            }
        );
    }

    #[test]
    fn a_generous_box_is_held_down_by_the_chosen_ceiling() {
        assert_eq!(
            derive(limits(Some(10_000_000), Some(128))),
            WatchBudget::Derived {
                watches: CHOSEN_CEILING,
                from_watches: 10_000_000,
                from_instances: 128
            }
        );
    }

    #[test]
    fn the_divisor_is_read_rather_than_remembered() {
        // The same dividend against a different instance limit must produce a
        // different budget. If it does not, the divisor has been hardcoded
        // again — which is the exact failure this module was rewritten to fix.
        let with_128 = derive(limits(Some(516_898), Some(128))).watches();
        let with_1024 = derive(limits(Some(516_898), Some(1024))).watches();
        assert_ne!(with_128, with_1024);
        assert_eq!(with_1024, 504);
    }

    #[test]
    fn an_unreadable_limit_is_a_named_condition_not_a_number() {
        // 8192 / 128 == 64 == CHOSEN_FLOOR. The number cannot tell these apart,
        // which is why the caller reads the variant and never the number.
        let computed = derive(limits(Some(8192), Some(128)));
        let defaulted = derive(limits(None, Some(128)));
        assert_eq!(computed.watches(), defaulted.watches());
        assert!(matches!(computed, WatchBudget::Derived { .. }));
        assert!(matches!(defaulted, WatchBudget::Undetermined { .. }));
    }

    #[test]
    fn either_file_being_unreadable_is_enough_to_default() {
        assert!(matches!(
            derive(limits(Some(516_898), None)),
            WatchBudget::Undetermined { .. }
        ));
        assert!(matches!(
            derive(limits(None, None)),
            WatchBudget::Undetermined { .. }
        ));
    }

    #[test]
    fn a_zero_divisor_defaults_rather_than_dividing() {
        assert!(matches!(
            derive(limits(Some(516_898), Some(0))),
            WatchBudget::Undetermined { .. }
        ));
    }
}
