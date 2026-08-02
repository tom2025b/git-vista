//! Repository-session state: who we are, how we are connected, and which mode is live.
//!
//! Framework-free (M1.11 D1). Before this module these three facts lived in three separate
//! `thread_local!`s inside `api.rs` — `CSRF_TOKEN`, `UI_MODE`, `VIA_LAN` — each with its own
//! setter and no invariant tying them together, so nothing could state (let alone test) a
//! rule like "a LAN view session may not select Active mode" (design spec D6).
//!
//! `ui_mode` is deliberately `Option<RepoMode>`: `None` means "not known yet", which is a
//! real state (before the first graph loads) and is *not* the same as Visualize. Collapsing
//! it to a default would silently start refusing writes that the old code allowed.

use git_vista_protocol::{HookPolicy, RepoMode};

use crate::features::core_traits::{Applied, FeatureCore};

/// `#[derive(Default)]` gives `hook_policy: HookPolicy::default()`, which is
/// `Blocked` (`git-vista-protocol`'s own fail-closed choice) — the right
/// answer before the first `Established` event: err conservative rather
/// than assume permissive.
///
/// It said `Restricted` until #208; that name was deleted in #202 and the
/// default was never it. `Blocked` is what the test below
/// (`a_fresh_session_has_no_token_is_not_lan_and_has_no_known_mode`) has
/// actually asserted all along.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionCore {
    csrf: Option<String>,
    via_lan: bool,
    ui_mode: Option<RepoMode>,
    hook_policy: HookPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// The session was established or re-checked (`POST`/`GET /api/session`).
    Established {
        csrf: Option<String>,
        via_lan: bool,
        hook_policy: HookPolicy,
    },
    /// The server told us what mode it believes is live (mirrors a loaded Frame).
    UiModeObserved(Option<RepoMode>),
    /// The user picked a mode on the picker's mode screen.
    UiModeSelected(RepoMode),
    SignedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRejection {
    /// ADR 0005: a LAN-view session is structurally read-only. The LAN listener simply has
    /// no `select` route, so a mode the user "picked" there could never have taken effect.
    UiModeChangeWhileLan,
}

impl SessionCore {
    pub fn csrf_token(&self) -> Option<&str> {
        self.csrf.as_deref()
    }

    pub fn is_lan(&self) -> bool {
        self.via_lan
    }

    /// The current hook policy (M1.13a, #66, ADR 0025) — **disclosed, not yet
    /// enforced**; see `git_vista_protocol::HookPolicy`'s own doc comment.
    pub fn hook_policy(&self) -> HookPolicy {
        self.hook_policy
    }

    /// Whether the persistent hook-policy banner
    /// (`crate::hook_policy_banner`) should show for this session's current
    /// policy. Pure, so it's tested here on the host rather than only
    /// visually — `hook_policy_banner.rs` is wasm32-gated (it imports
    /// Leptos) and carries no test of its own, matching this crate's
    /// existing view-file convention.
    pub fn hook_policy_banner_visible(&self) -> bool {
        self.hook_policy.requires_banner()
    }

    pub fn ui_mode(&self) -> Option<RepoMode> {
        self.ui_mode
    }

    /// Whether repository writes are refused up front (ADR 0007's client-side chokepoint).
    /// An unknown mode does **not** refuse — the server's 403 is the real boundary, and
    /// refusing before the first graph load would break writes that work today.
    pub fn refuses_writes(&self) -> bool {
        self.ui_mode == Some(RepoMode::Visualize)
    }
}

impl FeatureCore for SessionCore {
    type Event = SessionEvent;
    type Rejection = SessionRejection;

    fn apply(&mut self, ev: SessionEvent) -> Result<Applied, SessionRejection> {
        match ev {
            SessionEvent::Established {
                csrf,
                via_lan,
                hook_policy,
            } => {
                if self.csrf == csrf && self.via_lan == via_lan && self.hook_policy == hook_policy {
                    return Ok(Applied::NoChange);
                }
                self.csrf = csrf;
                self.via_lan = via_lan;
                self.hook_policy = hook_policy;
                Ok(Applied::Committed)
            }
            SessionEvent::UiModeObserved(m) => {
                if self.ui_mode == m {
                    return Ok(Applied::NoChange);
                }
                self.ui_mode = m;
                Ok(Applied::Committed)
            }
            SessionEvent::UiModeSelected(m) => {
                // Validate BEFORE mutating (global constraint 4).
                if self.via_lan {
                    return Err(SessionRejection::UiModeChangeWhileLan);
                }
                if self.ui_mode == Some(m) {
                    return Ok(Applied::NoChange);
                }
                self.ui_mode = Some(m);
                Ok(Applied::Committed)
            }
            SessionEvent::SignedOut => {
                if self.csrf.is_none() {
                    return Ok(Applied::NoChange);
                }
                self.csrf = None;
                Ok(Applied::Committed)
            }
        }
    }
}

/// How long to wait before re-checking a session bootstrap that failed at the
/// transport level (#218), or `None` once the budget is spent.
///
/// `attempt` is the number of attempts already made: `0` means the initial
/// `establish_session` has just failed and the first retry is being scheduled.
///
/// # Why retrying at all, and why bounded
///
/// The session resource is created with a constant source, so it runs once and
/// never re-runs on its own. Before this, an `Err` — the transport failing
/// during the very first load, exactly what a flaky SSH tunnel to an iPad
/// produces — left the app permanently stuck: every subsequent read 401s with
/// no cookie ever set, nothing in the reactive graph reacts to `Err`, and only
/// a full browser reload recovers. That is consistent with the symptom #218
/// reports (history rendering as a single status line until a manual retry).
/// Bounded because an unbounded retry against a genuinely down server is a
/// request storm, and the codebase already refuses that pattern elsewhere
/// (see the graph's page-fetch loop, which never auto-retries an error).
///
/// # Why the backoff is spaced the way it is
///
/// Deliberately slower than a tight loop: a tunnel that dropped mid-handshake
/// usually needs seconds, not milliseconds, to come back. The total (~12s
/// across three tries) is well inside the window a user would still perceive
/// as "loading" rather than "broken", while leaving the server alone if it is
/// genuinely gone.
pub fn session_retry_delay_ms(attempt: u32) -> Option<u32> {
    const BACKOFF_MS: [u32; 3] = [1_000, 3_000, 8_000];
    BACKOFF_MS.get(attempt as usize).copied()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_session_retry_budget_backs_off_and_then_gives_up() {
        // Spaced, not a tight loop — a dropped tunnel needs seconds.
        assert_eq!(super::session_retry_delay_ms(0), Some(1_000));
        assert_eq!(super::session_retry_delay_ms(1), Some(3_000));
        assert_eq!(super::session_retry_delay_ms(2), Some(8_000));
        // Bounded: an exhausted budget must stop, never wrap or repeat —
        // an unbounded retry against a genuinely down server is a request
        // storm, which this codebase refuses elsewhere too.
        assert_eq!(super::session_retry_delay_ms(3), None);
        assert_eq!(super::session_retry_delay_ms(99), None);
        // Strictly increasing, so a longer outage is not hammered at the
        // same rate as a momentary blip.
        let d: Vec<u32> = (0..3).filter_map(super::session_retry_delay_ms).collect();
        assert!(d.windows(2).all(|w| w[0] < w[1]), "{d:?}");
    }

    use super::*;

    fn established(via_lan: bool) -> SessionCore {
        let mut s = SessionCore::default();
        s.apply(SessionEvent::Established {
            csrf: Some("abc".into()),
            via_lan,
            // Two contrasting policies, one silent and one banner-flying, so
            // the tests below exercise both sides of the banner rule.
            //
            // They are keyed off `via_lan` purely as a convenient switch in
            // this helper — **the server no longer derives hook policy from
            // `via_lan` at all** (#202: it discloses the measured
            // per-repository policy, identically on both listeners). Nothing
            // here should be read as mirroring a server mapping; a session's
            // policy and its `via_lan` flag are independent values that arrive
            // in the same event.
            hook_policy: if via_lan {
                HookPolicy::Strict
            } else {
                HookPolicy::Unsandboxed
            },
        })
        .expect("establish is always accepted");
        s
    }

    #[test]
    fn a_fresh_session_has_no_token_is_not_lan_and_has_no_known_mode() {
        let s = SessionCore::default();
        assert_eq!(s.csrf_token(), None);
        assert!(!s.is_lan());
        assert_eq!(s.ui_mode(), None);
        // Fail-closed default, before any Established event — see this
        // struct's own doc comment. `Blocked`, not `Strict`: an absent field
        // must not become an unearned green light.
        assert_eq!(s.hook_policy(), HookPolicy::Blocked);
    }

    #[test]
    fn establishing_a_session_records_the_hook_policy() {
        assert_eq!(established(false).hook_policy(), HookPolicy::Unsandboxed);
        assert_eq!(established(true).hook_policy(), HookPolicy::Strict);
    }

    /// INV-15's polarity, and the reason this is not the old
    /// `the_banner_shows_only_for_allow`.
    ///
    /// When `HookPolicy` had two variants, `matches!(_, Allow)` and "not
    /// `Strict`" were the same predicate. Widening it to the four tier names
    /// split them apart, and the old expression kept the *narrow* half: it
    /// went silent for `Network` (sandboxed, but hooks reach the network) and
    /// for `Blocked` (hooks silently did not run) — under-warning on exactly
    /// the two values that did not exist when it was written. Enumerated
    /// explicitly here rather than by calling `requires_banner()`, so an
    /// inverted implementation of that method would still fail this.
    #[test]
    fn the_banner_shows_for_everything_except_strict() {
        for policy in [
            HookPolicy::Network,
            HookPolicy::Unsandboxed,
            HookPolicy::Blocked,
        ] {
            let mut s = SessionCore::default();
            s.apply(SessionEvent::Established {
                csrf: Some("abc".into()),
                via_lan: false,
                hook_policy: policy,
            })
            .expect("establish is always accepted");
            assert!(
                s.hook_policy_banner_visible(),
                "{policy:?} is not the fullest isolation, so the user must be told"
            );
        }
        assert!(!established(true).hook_policy_banner_visible());
        assert!(established(false).hook_policy_banner_visible());
        // The fail-closed default (`Blocked`) *does* fly the banner. This
        // reverses the old comment here on purpose: a fresh, not-yet-
        // established session has confirmed no guarantee, and the safe
        // direction for a banner is to over-warn and then go quiet once the
        // server discloses `strict` — not to stay silent on no evidence.
        assert!(SessionCore::default().hook_policy_banner_visible());
    }

    /// A change in `hook_policy` alone (everything else identical) must still
    /// be reported as a real change — this is exactly the kind of field a
    /// three-way `&&` comparison could silently forget to include.
    #[test]
    fn a_hook_policy_change_alone_is_reported_as_a_change() {
        let mut s = established(false);
        let applied = s
            .apply(SessionEvent::Established {
                csrf: Some("abc".into()),
                via_lan: false,
                hook_policy: HookPolicy::Strict,
            })
            .unwrap();
        assert_eq!(applied, Applied::Committed);
        assert_eq!(s.hook_policy(), HookPolicy::Strict);
    }

    #[test]
    fn establishing_a_session_records_the_token_and_the_lan_flag() {
        let s = established(true);
        assert_eq!(s.csrf_token(), Some("abc"));
        assert!(s.is_lan());
    }

    #[test]
    fn a_lan_session_refuses_a_user_initiated_mode_change() {
        // ADR 0005: the LAN listener has no select route, so a mode "picked" there could
        // never take effect. Enforcing it in the core means the rule is tested, not merely
        // rendered.
        let mut s = established(true);
        let before = s.ui_mode();
        let err = s
            .apply(SessionEvent::UiModeSelected(RepoMode::Active))
            .unwrap_err();
        assert_eq!(err, SessionRejection::UiModeChangeWhileLan);
        assert_eq!(
            s.ui_mode(),
            before,
            "a rejected transition must not mutate the core"
        );
    }

    #[test]
    fn a_lan_session_still_accepts_a_mode_the_server_reports() {
        // Observing what the Frame says is not a user action and must not be refused,
        // otherwise the client's view of server truth would drift.
        let mut s = established(true);
        s.apply(SessionEvent::UiModeObserved(Some(RepoMode::Active)))
            .expect("observation accepted");
        assert_eq!(s.ui_mode(), Some(RepoMode::Active));
    }

    #[test]
    fn a_local_session_accepts_a_user_initiated_mode_change() {
        let mut s = established(false);
        s.apply(SessionEvent::UiModeSelected(RepoMode::Active))
            .expect("not a LAN session, so the change is admitted");
        assert_eq!(s.ui_mode(), Some(RepoMode::Active));
    }

    #[test]
    fn signing_out_clears_the_token_but_keeps_the_transport_fact() {
        let mut s = established(true);
        s.apply(SessionEvent::SignedOut).unwrap();
        assert_eq!(s.csrf_token(), None, "the credential is gone");
        assert!(s.is_lan(), "how we are connected did not change");
    }

    #[test]
    fn writes_are_refused_in_visualize_and_only_in_visualize() {
        // The exact truth table of the old `refuse_if_visualize()`, which compared against
        // `Some(RepoMode::Visualize)`. An unknown mode must stay permissive: the frontend
        // issues writes before the first graph lands, and the server's 403 is the boundary.
        let mut s = established(false);
        assert!(!s.refuses_writes(), "unknown mode does not refuse");
        s.apply(SessionEvent::UiModeObserved(Some(RepoMode::Visualize)))
            .unwrap();
        assert!(s.refuses_writes());
        s.apply(SessionEvent::UiModeObserved(Some(RepoMode::Active)))
            .unwrap();
        assert!(!s.refuses_writes());
    }

    #[test]
    fn re_establishing_with_identical_facts_reports_no_change() {
        // `crate::session` establishes on bootstrap and again on the GET fallback; the
        // second call carrying the same facts must not read as a state change.
        let mut s = established(false);
        let applied = s
            .apply(SessionEvent::Established {
                csrf: Some("abc".into()),
                via_lan: false,
                hook_policy: HookPolicy::Unsandboxed,
            })
            .unwrap();
        assert_eq!(applied, Applied::NoChange);
    }
}
