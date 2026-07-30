//! Where the inspector sits in each mode, and — when it is a bottom sheet — which
//! detent a gesture or a mode change resolves to (M1.12, #65).
//!
//! # What this module is, and what it deliberately is not
//!
//! `docs/IPAD_DESIGN.md:63` specifies the sheet in one sentence: *"The bottom sheet has
//! detents for summary, half-height, and full-height content."* Everything about that
//! sentence that is **logic** lives here and is proved on the host — which positions
//! exist, which one a released drag lands on, what happens past either end, and which
//! placement each [`ShellMode`] resolves to.
//!
//! Everything about it that is **rendering** is absent, on purpose and by constraint:
//! `styles.css` is not this lane's file, no sheet element is emitted anywhere in the
//! crate today, and nothing in this module is consumed by
//! [`crate::features::shell::signals`] yet. So the honest status is: *the model is
//! settled and tested; the sheet does not exist on screen.* Wiring it is the next
//! slice, and it needs the CSS half to land first.
//!
//! This follows [`super::core::ModeSettler`]'s shape for the same reason it was
//! written: the part of M1.12 that kept shipping unverified was the part braided into
//! a `web_sys` closure. A pure decision type is checkable without a browser, and on
//! this project nobody has a browser.
//!
//! # What survives a mode change
//!
//! #65's criterion is *"Repository, worktree, branch, and dirty state stay visible"*,
//! and `docs/IPAD_DESIGN.md:73` adds *"Preserve selected commit, viewport, and
//! unfinished form when the width changes."* Three separate mechanisms carry that, and
//! only the third is this module's:
//!
//! 1. **Which overlays are up, and their payloads** — survives by construction, not by
//!    any preservation code. [`super::core::Dock`] is a function of
//!    [`super::core::Overlay`] alone; `ShellMode` is not one of its inputs. A mode
//!    change therefore cannot make two overlays collide on a dock, so it cannot evict
//!    one. That is a property of the type signature, which is why there is no test for
//!    it here — a test that resized a window and then asserted the stack was unchanged
//!    would be asserting that unrelated code is unrelated.
//! 2. **The signals themselves** — `Shell` and the mode signal are both created in
//!    `App` above `graph_canvas` (see `app/mod.rs`, where `install_mode_signal` is
//!    called), specifically so an epoch bump's rebuild cannot tear them down. Also not
//!    this module's doing.
//! 3. **The sheet's detent** — [`SheetState`] remembers it, including *across* modes
//!    that have no sheet at all, so a Portrait → Wide → Portrait round trip returns the
//!    sheet to the height the user left it at rather than to the mode default.
//!
//! What is **not** modelled here, and is not preserved by anything in this lane:
//! selected commit, graph viewport, and unfinished form contents (`IPAD_DESIGN.md:73`).
//! Those live in `features/graph`, `camera.rs` and the dialog signals respectively —
//! other lanes' files.
//!
//! # The numbers are numbers, not laws
//!
//! [`SheetGeometry::default`]'s fractions and its flick threshold are tuning values.
//! Two of them are pinned by the spec's own vocabulary (*half*-height → `0.50`,
//! and `full < 1.0` because `IPAD_DESIGN.md:72` requires the compact header carrying
//! branch and dirty state to stay visible *behind* a full-height sheet). The summary
//! fraction and the flick threshold are invented defaults and cannot be validated
//! without a device. They are constructor inputs, not constants, for exactly that
//! reason.

use super::core::ShellMode;

/// One of the three heights the bottom sheet rests at.
///
/// Ordered shortest to tallest; that order is the model's spine — [`Self::taller`],
/// [`Self::shorter`], the nearest-detent snap and the flick rule all read it, and
/// [`SheetGeometry::new`] refuses fractions that contradict it.
///
/// There is **no fourth "dismissed" detent**, and adding one is a design decision this
/// lane did not take. The spec names three positions and requires (`IPAD_DESIGN.md:64`)
/// that the sheet *"must not cover the selected graph item without an obvious way to
/// restore context"* — a requirement about getting the graph back, which
/// [`SheetDetent::Summary`] already satisfies. Whether a downward flick past Summary
/// should also close the sheet outright is undecided; today it clamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SheetDetent {
    /// `IPAD_DESIGN.md:108` — *"Tap a commit to select it and show a summary sheet."*
    /// The resting height: enough for the selected object's identity, with the graph
    /// still the dominant surface.
    Summary,
    /// `IPAD_DESIGN.md:63` — *"half-height"*. The only one of the three whose fraction
    /// the spec fixes by naming it.
    Half,
    /// `IPAD_DESIGN.md:63` — *"full-height content"*, and `:109` — *"Expand the sheet
    /// for metadata, refs, changed files, and operation entry points."* Full-height is
    /// not viewport-height: see [`SheetGeometry::new`].
    Full,
}

impl SheetDetent {
    /// Every detent, shortest first. Iterating this is how the snap and flick rules
    /// stay in step with the enum instead of restating its order.
    pub const ORDERED: [SheetDetent; 3] =
        [SheetDetent::Summary, SheetDetent::Half, SheetDetent::Full];

    /// The next detent up, or `None` at [`SheetDetent::Full`].
    pub fn taller(self) -> Option<Self> {
        match self {
            Self::Summary => Some(Self::Half),
            Self::Half => Some(Self::Full),
            Self::Full => None,
        }
    }

    /// The next detent down, or `None` at [`SheetDetent::Summary`].
    pub fn shorter(self) -> Option<Self> {
        match self {
            Self::Full => Some(Self::Half),
            Self::Half => Some(Self::Summary),
            Self::Summary => None,
        }
    }
}

/// How tall each detent actually is, as a fraction of viewport height, plus the speed
/// at which a release counts as a flick rather than a slow drag.
///
/// Constructed rather than `const` because only two of these four numbers are decided
/// by anything other than taste — see the module doc.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SheetGeometry {
    summary: f64,
    half: f64,
    full: f64,
    flick_threshold: f64,
}

impl SheetGeometry {
    /// Build a geometry, or `None` if it could not describe a usable sheet.
    ///
    /// Rejected, and why each rule is a rule rather than a clamp:
    ///
    /// - Any non-finite input. A NaN fraction would make every distance comparison in
    ///   [`Self::resolve_release`] false and silently pin the sheet to whichever detent
    ///   the loop happened to see first.
    /// - `summary <= 0.0`. A sheet with no height is not a summary detent, it is a
    ///   dismissed sheet — and dismissal is deliberately not modelled (see
    ///   [`SheetDetent`]).
    /// - `summary >= half` or `half >= full`. The enum's order *is* the height order;
    ///   a geometry that inverts it would make [`SheetDetent::taller`] return something
    ///   shorter, and every test below would still pass while the sheet ran backwards.
    /// - `full >= 1.0`. `IPAD_DESIGN.md:72` requires the compact header — the one
    ///   carrying branch and dirty state, i.e. exactly the state #65 says must *"stay
    ///   visible"* — to survive the narrow layout. A sheet occupying the whole viewport
    ///   covers it. "Full-height" is full-height *of the content area*, not of the
    ///   window, and this bound is where that distinction is enforced rather than
    ///   remembered.
    /// - `flick_threshold <= 0.0`. At zero every release is a flick, including a
    ///   perfectly still finger, so the nearest-detent rule would become unreachable.
    pub fn new(summary: f64, half: f64, full: f64, flick_threshold: f64) -> Option<Self> {
        let all = [summary, half, full, flick_threshold];
        if all.iter().any(|v| !v.is_finite()) {
            return None;
        }
        if summary <= 0.0 || summary >= half || half >= full || full >= 1.0 {
            return None;
        }
        if flick_threshold <= 0.0 {
            return None;
        }
        Some(Self {
            summary,
            half,
            full,
            flick_threshold,
        })
    }

    /// The fraction of viewport height the sheet occupies at `detent`.
    pub fn fraction(&self, detent: SheetDetent) -> f64 {
        match detent {
            SheetDetent::Summary => self.summary,
            SheetDetent::Half => self.half,
            SheetDetent::Full => self.full,
        }
    }

    /// The sheet's height in CSS pixels at `detent`, for a viewport `viewport_height`
    /// tall. The only place this model touches pixels; everything else is fractions so
    /// that a Stage Manager resize changes one input and nothing else.
    pub fn height_px(&self, detent: SheetDetent, viewport_height: f64) -> f64 {
        self.fraction(detent) * viewport_height
    }

    /// The speed, in fractions of viewport height per second, at or above which a
    /// release is treated as a flick.
    pub fn flick_threshold(&self) -> f64 {
        self.flick_threshold
    }

    /// Where the sheet lands when the finger lifts.
    ///
    /// `released_fraction` is how much of the viewport the sheet covers at the instant
    /// of release; `velocity` is in fractions of viewport height per second, **positive
    /// meaning growing** (the user dragged the sheet up). Two rules, in this order:
    ///
    /// 1. **Flick** — `|velocity| >= flick_threshold`. Land on the first detent
    ///    *strictly past* the release position in the direction of travel; if there is
    ///    none, land on the extreme detent in that direction. Keyed off the release
    ///    position rather than off the detent the drag started from, so that a long
    ///    drag that ends in a flick is not dragged back to where it began: released
    ///    just under Full and flicked up lands on Full, not on the one detent above
    ///    Summary. Because the comparison is *strict*, a flick from a standing start —
    ///    released exactly on a detent — still moves, which is the whole point of a
    ///    flick.
    /// 2. **Slow release** — snap to the nearest detent by absolute distance. An exact
    ///    tie resolves to the **shorter** detent: `IPAD_DESIGN.md:64` says the sheet
    ///    *"must not cover the selected graph item"*, so when the gesture is genuinely
    ///    ambiguous the model uncovers the graph rather than covering it.
    ///
    /// Boundaries clamp. A release past `full` resolves to [`SheetDetent::Full`] and a
    /// release below `summary` — or at a negative fraction, which an over-drag can
    /// legitimately produce — resolves to [`SheetDetent::Summary`]; neither is an
    /// error and neither closes the sheet. A non-finite `released_fraction` also
    /// resolves to [`SheetDetent::Summary`] (never a crash, and the safe direction is
    /// the one that reveals the graph), and a non-finite `velocity` is treated as
    /// stationary.
    pub fn resolve_release(&self, released_fraction: f64, velocity: f64) -> SheetDetent {
        if !released_fraction.is_finite() {
            return SheetDetent::Summary;
        }
        let at = released_fraction.clamp(self.summary, self.full);
        let velocity = if velocity.is_finite() { velocity } else { 0.0 };

        if velocity >= self.flick_threshold {
            return SheetDetent::ORDERED
                .into_iter()
                .find(|&d| self.fraction(d) > at)
                .unwrap_or(SheetDetent::Full);
        }
        if velocity <= -self.flick_threshold {
            return SheetDetent::ORDERED
                .into_iter()
                .rev()
                .find(|&d| self.fraction(d) < at)
                .unwrap_or(SheetDetent::Summary);
        }

        let mut best = SheetDetent::Summary;
        let mut best_distance = (self.summary - at).abs();
        // Shortest-first, and strictly-closer wins, so an exact tie keeps the shorter
        // detent already held in `best`.
        for d in SheetDetent::ORDERED.into_iter().skip(1) {
            let distance = (self.fraction(d) - at).abs();
            if distance < best_distance {
                best = d;
                best_distance = distance;
            }
        }
        best
    }
}

impl Default for SheetGeometry {
    /// The starting numbers. `half` is `0.50` because the spec calls that detent
    /// "half-height"; `full` is `0.92` rather than `1.00` because the compact header
    /// has to survive (`IPAD_DESIGN.md:72`) — the exact 8% is a guess. `summary` at
    /// `0.25` and a flick threshold of `0.6` viewport-heights per second are invented
    /// and unvalidated: nobody on this project can put a finger on a screen.
    fn default() -> Self {
        Self {
            summary: 0.25,
            half: 0.50,
            full: 0.92,
            flick_threshold: 0.6,
        }
    }
}

/// Where the inspector — the selected object's detail surface — is presented.
///
/// This is the mode-dependent half of what `super::core::Dock::RightEdge` means. The
/// dock itself stays mode-independent (see the module doc's point 1); only its
/// rendering moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorPlacement {
    /// A persistent third column. `IPAD_DESIGN.md:33` — the wide layout's
    /// `inspector / plan` column — and `:43`: *"The inspector preserves selected-object
    /// context while the graph remains usable."*
    RightColumn,
    /// A bottom sheet resting at the given detent. `IPAD_DESIGN.md:59` — *"inspector or
    /// operation plan as bottom sheet"*.
    BottomSheet(SheetDetent),
}

impl InspectorPlacement {
    /// The detent, when the inspector is a sheet at all.
    pub fn detent(self) -> Option<SheetDetent> {
        match self {
            Self::BottomSheet(d) => Some(d),
            Self::RightColumn => None,
        }
    }

    /// Whether the inspector is currently a sheet.
    pub fn is_sheet(self) -> bool {
        matches!(self, Self::BottomSheet(_))
    }
}

/// The placement a mode uses when the inspector is first presented in it.
///
/// One `match`, four arms, each traceable to a line of the design doc:
///
/// - [`ShellMode::Compact`] → a sheet at [`SheetDetent::Full`]. `IPAD_DESIGN.md:70` —
///   *"Replace persistent inspector with a full-height sheet"* — reinforced by `:71`,
///   *"Present one primary task at a time"*.
/// - [`ShellMode::Portrait`] → a sheet at [`SheetDetent::Summary`]. `IPAD_DESIGN.md:59`
///   puts the inspector in a sheet here, and `:108` says selecting a commit *"show[s] a
///   summary sheet"* — so Portrait opens at the smallest detent and the user expands.
/// - [`ShellMode::Wide`] and [`ShellMode::UltraWide`] → [`InspectorPlacement::RightColumn`].
///   The wide skeleton at `IPAD_DESIGN.md:33` has a dedicated inspector column, and `:78`
///   keeps it at ultra-wide: *"Permit graph plus side-by-side diff plus inspector when
///   width allows."* UltraWide differs from Wide in density, not skeleton — which is why
///   they share an arm rather than having two identical ones.
///
/// This is the *default* placement, not the current one: once the user has moved the
/// sheet, [`SheetState`] answers instead, and its answer wins.
pub fn default_placement_for(mode: ShellMode) -> InspectorPlacement {
    match mode {
        ShellMode::Compact => InspectorPlacement::BottomSheet(SheetDetent::Full),
        ShellMode::Portrait => InspectorPlacement::BottomSheet(SheetDetent::Summary),
        ShellMode::Wide | ShellMode::UltraWide => InspectorPlacement::RightColumn,
    }
}

/// The inspector's placement right now, carried across mode changes.
///
/// The one piece of #65's *"stay visible"* criterion that needs actual preservation
/// code rather than falling out of a type signature. It holds two things: the mode, and
/// the detent the sheet was last at **while it was a sheet** — the second surviving
/// stretches of Wide/UltraWide where there is no sheet to be at a detent.
///
/// The rule, stated once: **a mode change never resets the user's detent.** The mode
/// default from [`default_placement_for`] applies only the first time the inspector
/// becomes a sheet at all. So Portrait-at-Half narrowing to Compact stays at Half; it
/// is *not* forced up to Compact's full-height default.
///
/// **That is a design decision, and the other answer is defensible.** Compact's spec
/// line (`IPAD_DESIGN.md:70`, *"Replace persistent inspector with a full-height
/// sheet"*) can be read as a hard requirement of the mode rather than as its opening
/// position, in which case entering Compact should force [`SheetDetent::Full`]. This
/// lane chose preservation because `IPAD_DESIGN.md:73` — *"Preserve selected commit,
/// viewport, and unfinished form when the width changes"* — is the sharper instruction
/// and #65's own acceptance criterion is about state surviving a resize. Reversing it
/// is a one-arm change in [`Self::on_mode_change`], and the tests below pin the current
/// answer with literals so the reversal cannot happen silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SheetState {
    mode: ShellMode,
    /// `None` until the inspector has been a sheet at least once — i.e. a session that
    /// opened on an external monitor has no remembered height to restore yet.
    remembered: Option<SheetDetent>,
}

impl SheetState {
    /// Start in `mode`, with the detent that mode opens at (and nothing remembered if
    /// that mode has no sheet).
    pub fn new(mode: ShellMode) -> Self {
        Self {
            mode,
            remembered: default_placement_for(mode).detent(),
        }
    }

    /// The mode this state is currently in.
    pub fn mode(&self) -> ShellMode {
        self.mode
    }

    /// Where the inspector is right now.
    pub fn placement(&self) -> InspectorPlacement {
        match default_placement_for(self.mode) {
            InspectorPlacement::RightColumn => InspectorPlacement::RightColumn,
            InspectorPlacement::BottomSheet(mode_default) => {
                InspectorPlacement::BottomSheet(self.remembered.unwrap_or(mode_default))
            }
        }
    }

    /// The mode changed. Returns the placement that results.
    ///
    /// Entering a sheet mode records the resulting detent, so that a later stretch in a
    /// column mode does not erase it; entering a column mode records nothing and erases
    /// nothing, which is exactly what makes the Portrait → Wide → Portrait round trip
    /// come back to where the user left it.
    pub fn on_mode_change(&mut self, to: ShellMode) -> InspectorPlacement {
        self.mode = to;
        let placement = self.placement();
        if let Some(d) = placement.detent() {
            self.remembered = Some(d);
        }
        placement
    }

    /// A drag finished. Resolves through [`SheetGeometry::resolve_release`] and records
    /// the result.
    ///
    /// Returns `None` — and changes nothing — when the inspector is not a sheet right
    /// now. A drag cannot happen against a column, and if one arrives anyway (a
    /// pointer sequence that began before a resize landed, which is precisely the
    /// Stage Manager case), it must not quietly rewrite the height the user will see
    /// when they return to a sheet mode.
    pub fn drag_released(
        &mut self,
        geometry: &SheetGeometry,
        released_fraction: f64,
        velocity: f64,
    ) -> Option<SheetDetent> {
        if !self.placement().is_sheet() {
            return None;
        }
        let landed = geometry.resolve_release(released_fraction, velocity);
        self.remembered = Some(landed);
        Some(landed)
    }

    /// `IPAD_DESIGN.md:109` — *"Expand the sheet for metadata, refs, changed files, and
    /// operation entry points."* One detent taller, saturating at
    /// [`SheetDetent::Full`].
    ///
    /// Returns the resulting detent, or `None` when the inspector is not a sheet. At
    /// the top the result equals the previous detent rather than being `None`: "already
    /// full" and "there is no sheet" are different states and a caller that announces
    /// them must be able to tell them apart.
    pub fn expand(&mut self) -> Option<SheetDetent> {
        self.step(SheetDetent::taller)
    }

    /// One detent shorter, saturating at [`SheetDetent::Summary`] — the counterpart to
    /// [`Self::expand`], and the *"obvious way to restore context"* that
    /// `IPAD_DESIGN.md:65` requires. Returns `None` only when there is no sheet.
    pub fn collapse(&mut self) -> Option<SheetDetent> {
        self.step(SheetDetent::shorter)
    }

    fn step(&mut self, next: fn(SheetDetent) -> Option<SheetDetent>) -> Option<SheetDetent> {
        let current = self.placement().detent()?;
        let landed = next(current).unwrap_or(current);
        self.remembered = Some(landed);
        Some(landed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A geometry with round, well-separated numbers so that every expectation below is
    /// arithmetic a reader can do in their head — deliberately *not*
    /// `SheetGeometry::default()`, whose fractions are tuning values that should be
    /// free to move without rewriting the rules' tests.
    fn geo() -> SheetGeometry {
        SheetGeometry::new(0.2, 0.5, 0.9, 1.0).expect("test geometry is well-formed")
    }

    // -- The detent order ---------------------------------------------------------

    #[test]
    fn detents_step_one_at_a_time_and_stop_at_the_ends() {
        assert_eq!(SheetDetent::Summary.taller(), Some(SheetDetent::Half));
        assert_eq!(SheetDetent::Half.taller(), Some(SheetDetent::Full));
        assert_eq!(SheetDetent::Full.taller(), None);
        assert_eq!(SheetDetent::Full.shorter(), Some(SheetDetent::Half));
        assert_eq!(SheetDetent::Half.shorter(), Some(SheetDetent::Summary));
        assert_eq!(SheetDetent::Summary.shorter(), None);
    }

    #[test]
    fn ordered_is_shortest_first_and_lists_every_detent_once() {
        assert_eq!(
            SheetDetent::ORDERED,
            [SheetDetent::Summary, SheetDetent::Half, SheetDetent::Full]
        );
        let g = geo();
        assert!(g.fraction(SheetDetent::Summary) < g.fraction(SheetDetent::Half));
        assert!(g.fraction(SheetDetent::Half) < g.fraction(SheetDetent::Full));
    }

    // -- Geometry validation ------------------------------------------------------

    #[test]
    fn a_full_detent_that_would_swallow_the_compact_header_is_rejected() {
        // IPAD_DESIGN.md:72 keeps branch and dirty state in the compact header, which
        // is the very state #65 says must stay visible. A sheet at 1.0 covers it.
        assert!(SheetGeometry::new(0.2, 0.5, 1.0, 0.6).is_none());
        assert!(SheetGeometry::new(0.2, 0.5, 1.5, 0.6).is_none());
        assert!(SheetGeometry::new(0.2, 0.5, 0.99, 0.6).is_some());
    }

    #[test]
    fn a_geometry_that_contradicts_the_detent_order_is_rejected() {
        assert!(
            SheetGeometry::new(0.6, 0.5, 0.9, 0.6).is_none(),
            "summary above half"
        );
        assert!(
            SheetGeometry::new(0.2, 0.95, 0.9, 0.6).is_none(),
            "half above full"
        );
        assert!(
            SheetGeometry::new(0.5, 0.5, 0.9, 0.6).is_none(),
            "summary equal to half"
        );
        assert!(
            SheetGeometry::new(0.2, 0.9, 0.9, 0.6).is_none(),
            "half equal to full"
        );
    }

    #[test]
    fn a_zero_height_summary_is_rejected_because_it_would_be_a_dismissal() {
        assert!(SheetGeometry::new(0.0, 0.5, 0.9, 0.6).is_none());
        assert!(SheetGeometry::new(-0.1, 0.5, 0.9, 0.6).is_none());
    }

    #[test]
    fn a_non_positive_flick_threshold_is_rejected() {
        // At zero, every release including a stationary one is a flick and the
        // nearest-detent branch becomes dead code.
        assert!(SheetGeometry::new(0.2, 0.5, 0.9, 0.0).is_none());
        assert!(SheetGeometry::new(0.2, 0.5, 0.9, -1.0).is_none());
    }

    #[test]
    fn non_finite_geometry_inputs_are_rejected() {
        assert!(SheetGeometry::new(f64::NAN, 0.5, 0.9, 0.6).is_none());
        assert!(SheetGeometry::new(0.2, f64::NAN, 0.9, 0.6).is_none());
        assert!(SheetGeometry::new(0.2, 0.5, f64::INFINITY, 0.6).is_none());
        assert!(SheetGeometry::new(0.2, 0.5, 0.9, f64::NAN).is_none());
    }

    #[test]
    fn the_shipped_defaults_satisfy_their_own_rules() {
        let d = SheetGeometry::default();
        assert!(
            SheetGeometry::new(
                d.fraction(SheetDetent::Summary),
                d.fraction(SheetDetent::Half),
                d.fraction(SheetDetent::Full),
                d.flick_threshold(),
            )
            .is_some(),
            "Default is hand-written and bypasses new(); it must still be constructible by it"
        );
        // The one fraction the spec fixes by naming the detent "half-height".
        assert_eq!(d.fraction(SheetDetent::Half), 0.50);
        assert!(
            d.fraction(SheetDetent::Full) < 1.0,
            "full-height is full-height of the content area, not of the window"
        );
    }

    #[test]
    fn height_in_pixels_is_the_fraction_of_the_viewport() {
        let g = geo();
        assert_eq!(g.height_px(SheetDetent::Summary, 1000.0), 200.0);
        assert_eq!(g.height_px(SheetDetent::Half, 1000.0), 500.0);
        assert_eq!(g.height_px(SheetDetent::Full, 1000.0), 900.0);
        // A zero-height viewport is what a backgrounded tab reports; it must not be a
        // special case, just a zero-height sheet.
        assert_eq!(g.height_px(SheetDetent::Full, 0.0), 0.0);
    }

    // -- Slow release: nearest detent ---------------------------------------------

    #[test]
    fn a_slow_release_snaps_to_the_nearest_detent() {
        let g = geo();
        // Detents at 0.2 / 0.5 / 0.9. Each case is written as the literal detent it
        // must land on, not derived from the geometry.
        assert_eq!(g.resolve_release(0.21, 0.0), SheetDetent::Summary);
        assert_eq!(g.resolve_release(0.30, 0.0), SheetDetent::Summary);
        assert_eq!(g.resolve_release(0.40, 0.0), SheetDetent::Half);
        assert_eq!(g.resolve_release(0.55, 0.0), SheetDetent::Half);
        assert_eq!(g.resolve_release(0.69, 0.0), SheetDetent::Half);
        assert_eq!(g.resolve_release(0.75, 0.0), SheetDetent::Full);
        assert_eq!(g.resolve_release(0.89, 0.0), SheetDetent::Full);
    }

    #[test]
    fn releasing_exactly_on_a_detent_stays_there() {
        let g = geo();
        assert_eq!(g.resolve_release(0.2, 0.0), SheetDetent::Summary);
        assert_eq!(g.resolve_release(0.5, 0.0), SheetDetent::Half);
        assert_eq!(g.resolve_release(0.9, 0.0), SheetDetent::Full);
    }

    #[test]
    fn an_exact_tie_resolves_to_the_shorter_detent_and_uncovers_the_graph() {
        // 0.35 is exactly between 0.2 and 0.5; 0.7 exactly between 0.5 and 0.9.
        // IPAD_DESIGN.md:64 — the sheet "must not cover the selected graph item" — so
        // an ambiguous gesture resolves downward.
        let g = geo();
        assert_eq!(g.resolve_release(0.35, 0.0), SheetDetent::Summary);
        assert_eq!(g.resolve_release(0.70, 0.0), SheetDetent::Half);
    }

    #[test]
    fn a_release_past_either_end_clamps_and_never_dismisses() {
        let g = geo();
        assert_eq!(g.resolve_release(2.0, 0.0), SheetDetent::Full);
        assert_eq!(g.resolve_release(1.0, 0.0), SheetDetent::Full);
        assert_eq!(g.resolve_release(0.05, 0.0), SheetDetent::Summary);
        assert_eq!(g.resolve_release(0.0, 0.0), SheetDetent::Summary);
        // An over-drag downward can genuinely produce a negative fraction.
        assert_eq!(g.resolve_release(-3.0, 0.0), SheetDetent::Summary);
        // ...and a hard downward flick past the bottom still only reaches Summary.
        assert_eq!(g.resolve_release(-3.0, -50.0), SheetDetent::Summary);
    }

    // -- Flicks --------------------------------------------------------------------

    #[test]
    fn an_upward_flick_from_a_standing_start_still_moves_one_detent() {
        // Released exactly on Summary with no travel: the comparison is strict, so
        // "first detent above 0.2" is Half rather than Summary itself.
        let g = geo();
        assert_eq!(g.resolve_release(0.2, 1.0), SheetDetent::Half);
        assert_eq!(g.resolve_release(0.5, 1.0), SheetDetent::Full);
    }

    #[test]
    fn a_downward_flick_from_a_standing_start_still_moves_one_detent() {
        let g = geo();
        assert_eq!(g.resolve_release(0.9, -1.0), SheetDetent::Half);
        assert_eq!(g.resolve_release(0.5, -1.0), SheetDetent::Summary);
    }

    #[test]
    fn a_flick_at_the_end_of_a_long_drag_does_not_snap_back_to_where_it_started() {
        // The reason the flick rule keys off the release position, not off the detent
        // the drag began at: a drag from Summary all the way up to just under Full,
        // finished with an upward flick, must land on Full. A "one detent from the
        // starting detent" rule would land on Half — a visible, wrong, halfway stop.
        let g = geo();
        assert_eq!(g.resolve_release(0.85, 1.0), SheetDetent::Full);
        // And the mirror: dragged from Full down to just above Summary, then flicked
        // down.
        assert_eq!(g.resolve_release(0.25, -1.0), SheetDetent::Summary);
    }

    #[test]
    fn a_flick_never_skips_past_an_intervening_detent() {
        // Released just above Summary and flicked hard upward: the next detent up is
        // Half, and speed alone must not carry it to Full.
        let g = geo();
        assert_eq!(g.resolve_release(0.22, 100.0), SheetDetent::Half);
        assert_eq!(g.resolve_release(0.88, -100.0), SheetDetent::Half);
    }

    #[test]
    fn a_flick_at_the_top_stays_at_the_top() {
        let g = geo();
        assert_eq!(g.resolve_release(0.9, 5.0), SheetDetent::Full);
        assert_eq!(g.resolve_release(0.2, -5.0), SheetDetent::Summary);
    }

    #[test]
    fn the_threshold_is_inclusive_and_just_below_it_is_a_slow_release() {
        // The geometry's threshold is 1.0. At exactly 1.0 the flick rule applies; a
        // hair under it, the nearest-detent rule does — and from 0.55 those two rules
        // give different answers, which is what makes this test able to fail.
        let g = geo();
        assert_eq!(g.resolve_release(0.55, 1.0), SheetDetent::Full, "flick");
        assert_eq!(
            g.resolve_release(0.55, 0.999),
            SheetDetent::Half,
            "slow release"
        );
        assert_eq!(g.resolve_release(0.45, -1.0), SheetDetent::Summary, "flick");
        assert_eq!(
            g.resolve_release(0.45, -0.999),
            SheetDetent::Half,
            "slow release"
        );
    }

    #[test]
    fn a_non_finite_gesture_resolves_to_summary_rather_than_panicking() {
        // A pointer sequence interrupted by a suspend can produce a NaN velocity from a
        // zero elapsed time, and NaN makes every comparison false — which would have
        // silently pinned the result to whichever detent the loop saw first.
        let g = geo();
        assert_eq!(g.resolve_release(f64::NAN, 0.0), SheetDetent::Summary);
        assert_eq!(g.resolve_release(f64::INFINITY, 0.0), SheetDetent::Summary);
        // A NaN velocity is stationary, so the position still decides.
        assert_eq!(g.resolve_release(0.85, f64::NAN), SheetDetent::Full);
        assert_eq!(g.resolve_release(0.30, f64::NAN), SheetDetent::Summary);
    }

    // -- Mode -> placement ---------------------------------------------------------

    #[test]
    fn compact_presents_the_inspector_as_a_full_height_sheet() {
        // IPAD_DESIGN.md:70 — "Replace persistent inspector with a full-height sheet."
        assert_eq!(
            default_placement_for(ShellMode::Compact),
            InspectorPlacement::BottomSheet(SheetDetent::Full)
        );
    }

    #[test]
    fn portrait_opens_the_sheet_at_summary() {
        // IPAD_DESIGN.md:59 puts the inspector in a sheet; :108 — "Tap a commit to
        // select it and show a summary sheet."
        assert_eq!(
            default_placement_for(ShellMode::Portrait),
            InspectorPlacement::BottomSheet(SheetDetent::Summary)
        );
    }

    #[test]
    fn wide_and_ultrawide_keep_the_inspector_as_a_column() {
        // IPAD_DESIGN.md:33's skeleton has an "inspector / plan" column; :78 keeps it
        // at ultra-wide.
        assert_eq!(
            default_placement_for(ShellMode::Wide),
            InspectorPlacement::RightColumn
        );
        assert_eq!(
            default_placement_for(ShellMode::UltraWide),
            InspectorPlacement::RightColumn
        );
    }

    #[test]
    fn exactly_the_two_narrow_modes_use_a_sheet() {
        // Written as four literal booleans rather than by asking is_sheet() what it
        // thinks: the point is which modes, not that the accessor agrees with itself.
        assert!(default_placement_for(ShellMode::Compact).is_sheet());
        assert!(default_placement_for(ShellMode::Portrait).is_sheet());
        assert!(!default_placement_for(ShellMode::Wide).is_sheet());
        assert!(!default_placement_for(ShellMode::UltraWide).is_sheet());
    }

    // -- Preservation across a mode change ------------------------------------------

    #[test]
    fn a_fresh_state_opens_at_its_modes_default() {
        assert_eq!(
            SheetState::new(ShellMode::Portrait).placement(),
            InspectorPlacement::BottomSheet(SheetDetent::Summary)
        );
        assert_eq!(
            SheetState::new(ShellMode::Compact).placement(),
            InspectorPlacement::BottomSheet(SheetDetent::Full)
        );
        assert_eq!(
            SheetState::new(ShellMode::Wide).placement(),
            InspectorPlacement::RightColumn
        );
    }

    #[test]
    fn narrowing_from_portrait_to_compact_keeps_the_height_the_user_chose() {
        // The design decision this lane took, pinned with a literal so reversing it is
        // a visible test change: Compact's *default* is Full, but a user sitting at
        // Half does not get shoved there by a Stage Manager drag. See SheetState's doc
        // for the case against.
        let g = geo();
        let mut s = SheetState::new(ShellMode::Portrait);
        assert_eq!(s.drag_released(&g, 0.5, 0.0), Some(SheetDetent::Half));
        assert_eq!(
            s.on_mode_change(ShellMode::Compact),
            InspectorPlacement::BottomSheet(SheetDetent::Half),
            "not Compact's full-height default"
        );
    }

    #[test]
    fn a_portrait_wide_portrait_round_trip_restores_the_height_the_user_left_at() {
        // #65's criterion is that state survives a resize. Wide has no sheet at all, so
        // this is the case where the detent has to be remembered somewhere other than
        // in the sheet itself.
        let g = geo();
        let mut s = SheetState::new(ShellMode::Portrait);
        assert_eq!(s.drag_released(&g, 0.9, 0.0), Some(SheetDetent::Full));

        assert_eq!(
            s.on_mode_change(ShellMode::Wide),
            InspectorPlacement::RightColumn
        );
        assert_eq!(
            s.on_mode_change(ShellMode::Portrait),
            InspectorPlacement::BottomSheet(SheetDetent::Full),
            "back to Full, not reset to Portrait's Summary default"
        );
    }

    #[test]
    fn a_session_that_starts_wide_adopts_the_mode_default_the_first_time_it_narrows() {
        // Nothing to preserve yet: the inspector has never been a sheet, so there is no
        // user-chosen height and the mode default is the right answer.
        let mut s = SheetState::new(ShellMode::UltraWide);
        assert_eq!(s.placement(), InspectorPlacement::RightColumn);
        assert_eq!(
            s.on_mode_change(ShellMode::Portrait),
            InspectorPlacement::BottomSheet(SheetDetent::Summary)
        );
    }

    #[test]
    fn a_stage_manager_drag_through_every_mode_never_resets_the_users_height() {
        // The shape a real drag has: several band crossings in a row, some of them
        // through modes with no sheet at all. The height chosen once at the start must
        // survive all of it.
        let g = geo();
        let mut s = SheetState::new(ShellMode::Portrait);
        assert_eq!(s.drag_released(&g, 0.5, 0.0), Some(SheetDetent::Half));

        let seen: Vec<InspectorPlacement> = [
            ShellMode::Compact,
            ShellMode::Portrait,
            ShellMode::Wide,
            ShellMode::UltraWide,
            ShellMode::Wide,
            ShellMode::Portrait,
        ]
        .into_iter()
        .map(|m| s.on_mode_change(m))
        .collect();

        assert_eq!(
            seen,
            vec![
                InspectorPlacement::BottomSheet(SheetDetent::Half),
                InspectorPlacement::BottomSheet(SheetDetent::Half),
                InspectorPlacement::RightColumn,
                InspectorPlacement::RightColumn,
                InspectorPlacement::RightColumn,
                InspectorPlacement::BottomSheet(SheetDetent::Half),
            ]
        );
        assert_eq!(s.mode(), ShellMode::Portrait);
    }

    #[test]
    fn a_drag_that_lands_while_the_inspector_is_a_column_is_ignored_entirely() {
        // The Stage Manager case: a pointer sequence begun in Portrait can finish after
        // the resize has already published Wide. Applying it would rewrite the height
        // the user sees when they come back to a narrow window.
        let g = geo();
        let mut s = SheetState::new(ShellMode::Portrait);
        assert_eq!(s.drag_released(&g, 0.9, 0.0), Some(SheetDetent::Full));
        s.on_mode_change(ShellMode::Wide);

        assert_eq!(s.drag_released(&g, 0.2, 0.0), None, "no sheet to drag");
        assert_eq!(
            s.on_mode_change(ShellMode::Portrait),
            InspectorPlacement::BottomSheet(SheetDetent::Full),
            "the stray drag must not have overwritten the remembered height"
        );
    }

    // -- Expand / collapse ----------------------------------------------------------

    #[test]
    fn expand_and_collapse_step_one_detent_and_saturate() {
        let mut s = SheetState::new(ShellMode::Portrait);
        assert_eq!(s.placement().detent(), Some(SheetDetent::Summary));
        assert_eq!(s.expand(), Some(SheetDetent::Half));
        assert_eq!(s.expand(), Some(SheetDetent::Full));
        assert_eq!(s.expand(), Some(SheetDetent::Full), "saturates at the top");
        assert_eq!(s.collapse(), Some(SheetDetent::Half));
        assert_eq!(s.collapse(), Some(SheetDetent::Summary));
        assert_eq!(
            s.collapse(),
            Some(SheetDetent::Summary),
            "saturates at the bottom"
        );
    }

    #[test]
    fn already_full_and_no_sheet_at_all_are_distinguishable() {
        // A caller announcing the result must be able to tell "nothing moved because
        // you are already at the top" from "there is no sheet here" — so the top of the
        // range returns Some(Full), not None.
        let mut portrait = SheetState::new(ShellMode::Compact);
        assert_eq!(portrait.expand(), Some(SheetDetent::Full));

        let mut wide = SheetState::new(ShellMode::Wide);
        assert_eq!(wide.expand(), None);
        assert_eq!(wide.collapse(), None);
        assert_eq!(
            wide.placement(),
            InspectorPlacement::RightColumn,
            "a no-op step must not have invented a sheet"
        );
    }

    #[test]
    fn expanding_in_a_column_mode_does_not_disturb_the_remembered_height() {
        let g = geo();
        let mut s = SheetState::new(ShellMode::Portrait);
        assert_eq!(s.drag_released(&g, 0.5, 0.0), Some(SheetDetent::Half));
        s.on_mode_change(ShellMode::UltraWide);
        assert_eq!(s.expand(), None);
        assert_eq!(s.collapse(), None);
        assert_eq!(
            s.on_mode_change(ShellMode::Portrait),
            InspectorPlacement::BottomSheet(SheetDetent::Half)
        );
    }
}
