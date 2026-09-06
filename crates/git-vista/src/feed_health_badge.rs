//! The change-feed-health affordance (#663; ADR 0094 §7) — wasm only.
//!
//! Mounted by `App` unconditionally, in the topbar beside the working-tree
//! status chip: **permanent**, present in every state including the quiet
//! resting one. Every decision — the label, the announcement sentence,
//! whether to look degraded — is `features::freshness::core::feed_health_display`,
//! host-tested; this file only reads the live signal, the live clock, and
//! draws what that function returns.
//!
//! Same permanently-mounted `role="status"` shape `offline_banner` uses, and
//! for the identical reason: iOS VoiceOver often stays silent for a
//! `role="status"` element inserted into the DOM already populated, so the
//! wrapper exists from mount and only its *content* changes.

use leptos::*;

use crate::features::freshness::core::feed_health_display;
use crate::features::freshness::signals::Freshness;

fn now_secs() -> i64 {
    (js_sys::Date::now() / 1000.0) as i64
}

/// The topbar's change-feed-health affordance. Not interactive — a passive
/// disclosure, like the working-tree status chip beside it — so it carries
/// no touch-target floor.
pub fn feed_health_badge_view(freshness: Freshness) -> impl IntoView {
    view! {
        <span role="status" class="feed-health">
            {move || {
                let display = feed_health_display(
                    freshness.health().as_ref(),
                    git_vista_protocol::UnixSeconds(now_secs()),
                );
                let dot_class = if display.degraded {
                    "feed-health-dot degraded"
                } else {
                    "feed-health-dot"
                };
                view! {
                    <span title=display.announcement.clone()>
                        <span class=dot_class aria-hidden="true"></span>
                        {display.degraded.then(|| view! {
                            <span class="feed-health-label">{display.label}</span>
                        })}
                    </span>
                    <span class="sr-only">{display.announcement}</span>
                }
            }}
        </span>
    }
}
