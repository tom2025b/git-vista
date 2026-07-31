//! Accessibility facts that can be *proved on this machine* (M1.12, #65).
//!
//! Issue #65's accessibility criteria — "usable by finger without hover or right-click",
//! "interactive targets meet 44-by-44 CSS pixel guidance", "VoiceOver and keyboard paths
//! exist" — are mostly claims about what a rendered page does on a real device. Nobody
//! working on this repository has a browser, a screen reader, or an iPad in the loop, so
//! any test that *asserts* one of those outcomes would be asserting something it cannot
//! observe. That is the failure mode this milestone has already hit six times: a green
//! test that proves nothing.
//!
//! So this module deliberately proves a smaller thing, exactly:
//!
//! - [`core`] is pure arithmetic and pure constants — the 44 px threshold, the
//!   shortfall of a given target, and the camera scale at which the commit-dot hit
//!   circle first reaches guidance. Inputs in, verdicts out, no DOM.
//! - [`focus`] (M1.13) is the roving-tabindex state machine that closes the gap the
//!   M1.12 lane's report named as the single largest remaining one: the commit graph
//!   was pointer-only. Same shape as `core` — a plain state machine, host-tested, no
//!   DOM — because "which row is focused and what arrow keys do to it" is exactly as
//!   decidable off-device as the tap-target arithmetic is.
//! - [`stylesheet`] is a small, fixture-tested CSS reader. It exists so the audits below
//!   are statements about the *actual* `styles.css` rather than about a paraphrase of it.
//! - [`audit`] is test-only and holds the tripwires: invariants over the real
//!   `styles.css`, the real `app/mod.rs` markup, and the real `render/nodes.rs`
//!   geometry. They are ratchets — they fail when someone adds a hover affordance with
//!   no keyboard twin, adds an interactive control with no tap-target decision, or moves
//!   a number this module's arithmetic depends on.
//!
//! **What is proved here, precisely.** That the stylesheet *declares* a keyboard-focus
//! twin for every hover affordance, and that the numbers quoted in the tap-target census
//! are the numbers the stylesheet and the render code actually contain.
//!
//! **What is not, and cannot be, proved here.** That the focus ring is *visible* against
//! every background it lands on; that VoiceOver announces anything sensible; that a
//! finger can actually hit a 30 px commit dot. Those need a device. The census in
//! [`audit`] is the honest record of the gap, not a claim that the gap is closed.

pub mod core;
pub mod focus;
pub mod stylesheet;

#[cfg(test)]
mod audit;
