//! Tripwires over the real files (M1.12, #65). Test-only.
//!
//! Each test here is an invariant about bytes that ship: `styles.css`, the markup in
//! `app/mod.rs`, the geometry literals in `render/`. They are ratchets — none of them
//! asserts that the app *is* accessible, because nothing on this machine can observe
//! that. What they assert is that the decisions already made stay made, and that the
//! next person to add a hover affordance, an interactive control, or an animation is
//! stopped and made to decide rather than quietly shipping a mouse-only path.
//!
//! Note on subject matter: these watch *production* files, not test files. A tripwire
//! aimed at test-only code can never expire (a lesson the sandbox work paid for), so
//! every `include_str!` below points at something the browser actually loads or the wasm
//! build actually compiles.

use std::collections::BTreeSet;

use super::core::{
    node_hit_extent_px, TapTarget, TargetVerdict, GRAPH_REGION_LABEL, MIN_TAP_TARGET_PX,
    NODE_HIT_PADDING,
};
use super::stylesheet::{length_px, parse, rules_for_selector, Rule};

const STYLES: &str = include_str!("../../../styles.css");
const APP_MOD: &str = include_str!("../../app/mod.rs");
const RENDER_NODES: &str = include_str!("../../render/nodes.rs");
const RENDER_STUBS: &str = include_str!("../../render/stubs.rs");
const MENU: &str = include_str!("../../menu.rs");
/// The commit modal (M2.19c, #224 widened it to three modes). Inline-styled end
/// to end, so the stylesheet census below cannot see a single one of its
/// controls — this file's own bytes are the only place its tap targets can be
/// checked.
const COMMIT_MODAL: &str = include_str!("../../dialogs/commit.rs");
/// The dialogs feature's signal holder — `#[cfg(target_arch = "wasm32")]`, so
/// `cargo test --workspace` never compiles a line of it either. It owns the
/// amend state that #225's ceremony rests on.
const DIALOG_SIGNALS: &str = include_str!("../dialogs/signals.rs");
/// The operations status strip (M1.11, #64; the Cancel button and its live
/// region are #232, M2.20f). `#[cfg(target_arch = "wasm32")]` upstream
/// (`features/operations/mod.rs`), so `cargo test --workspace` never
/// compiles a line of it either — same reason `DIALOG_SIGNALS` above is
/// read as source text rather than exercised directly.
const OPERATIONS_VIEW: &str = include_str!("../operations/view.rs");

fn stylesheet() -> Vec<Rule> {
    parse(STYLES)
}

// ── Anti-vacuity ────────────────────────────────────────────────────────────────
//
// Every tripwire below is a statement of the form "for all X in the stylesheet, …".
// Such a statement is trivially true of an empty set, so if the reader ever silently
// stopped finding rules — a syntax it cannot handle, a moved file — the whole section
// would go green while checking nothing. These two tests are the floor under that.

#[test]
fn the_stylesheet_is_actually_being_read() {
    let rules = stylesheet();
    assert!(
        rules.len() > 80,
        "only {} rules parsed out of styles.css — the reader has lost the file or \
         choked on syntax it does not handle, and every other tripwire in this module \
         is now vacuous",
        rules.len()
    );
    assert!(
        rules_for_selector(&rules, ".refresh")
            .iter()
            .any(|r| r.value_of("cursor").is_some()),
        "the `.refresh` rule (a known, long-standing rule) was not found with its \
         declarations — the reader is misparsing"
    );
}

// ── Keyboard parity with hover ──────────────────────────────────────────────────

/// Selector plus the properties its rule declares, for every selector containing
/// `:hover`.
fn hover_rules(rules: &[Rule]) -> Vec<(Vec<String>, String, Vec<String>)> {
    let mut out = Vec::new();
    for rule in rules {
        for selector in &rule.selectors {
            if selector.contains(":hover") {
                out.push((
                    rule.at_context.clone(),
                    selector.clone(),
                    rule.properties().iter().map(|p| p.to_string()).collect(),
                ));
            }
        }
    }
    out
}

/// The invariant: hover is a mouse-only signal, so anything it changes visually must be
/// changed by keyboard focus too.
///
/// Stated at the level of the *selector*, not the class, so it survives the awkward
/// cases exactly: `button.detail-file:hover .detail-file-path` colours a descendant, and
/// its twin has to colour the same descendant, which a class-level rule would have
/// missed. The twin is required to declare at least the same property names — the same
/// *values* are not required, because a focus style legitimately differs (a disabled
/// control's hover twin is inert either way).
#[test]
fn every_hover_rule_has_a_focus_visible_twin_declaring_the_same_properties() {
    let rules = stylesheet();
    let hovers = hover_rules(&rules);

    assert!(
        hovers.len() >= 20,
        "expected at least the 20 hover selectors this stylesheet had when the \
         invariant was written, found {} — if hover rules were legitimately deleted, \
         lower this floor deliberately; do not let it drift",
        hovers.len()
    );

    for (at_context, selector, properties) in &hovers {
        let twin = selector.replace(":hover", ":focus-visible");
        let twin_properties: BTreeSet<String> = rules
            .iter()
            .filter(|r| &r.at_context == at_context && r.selectors.contains(&twin))
            .flat_map(|r| r.properties().into_iter().map(|p| p.to_string()))
            .collect();

        assert!(
            !twin_properties.is_empty(),
            "`{selector}` styles hover but nothing styles `{twin}` — that state change \
             is invisible to a keyboard or Switch Control user (#65)"
        );
        for property in properties {
            assert!(
                twin_properties.contains(property),
                "`{selector}` changes `{property}` on hover but `{twin}` does not — \
                 keyboard focus must reproduce every visual change hover makes (#65)"
            );
        }
    }
}

/// A universal `:focus-visible` ring exists, and nothing anywhere switches the outline
/// off.
///
/// The second half is the one that matters in practice: `outline: none` is the single
/// most common way a stylesheet destroys keyboard accessibility, it is usually added to
/// silence a ring someone found ugly, and it would leave every twin above still passing.
#[test]
fn a_focus_ring_exists_and_nothing_removes_it() {
    let rules = stylesheet();

    let base = rules_for_selector(&rules, ":focus-visible");
    assert!(
        !base.is_empty(),
        "no universal `:focus-visible` rule in styles.css — focusable elements with no \
         hover styling of their own (inputs, the commit-message textarea) would be left \
         with whatever the browser defaults to"
    );
    let outline = base
        .iter()
        .filter_map(|r| r.value_of("outline"))
        .next_back()
        .expect("the `:focus-visible` rule declares no `outline`");
    assert!(
        outline.value != "none" && outline.value != "0",
        "the universal `:focus-visible` rule sets `outline: {}`, which is no ring at all",
        outline.value
    );

    for rule in &rules {
        for declaration in &rule.declarations {
            let kills_outline = (declaration.property == "outline"
                && (declaration.value == "none" || declaration.value == "0"))
                || (declaration.property == "outline-width" && declaration.value == "0")
                || (declaration.property == "outline-style" && declaration.value == "none");
            assert!(
                !kills_outline,
                "`{}` sets `{}: {}` — that removes the keyboard focus ring (#65). If a \
                 control genuinely needs a different focus treatment, give it one \
                 instead of removing this one.",
                rule.selectors.join(", "),
                declaration.property,
                declaration.value
            );
        }
    }
}

// ── Tap targets ─────────────────────────────────────────────────────────────────

/// Whether the stylesheet *guarantees* an extent of at least [`MIN_TAP_TARGET_PX`] on an
/// axis, from the rules whose selector is exactly `selector`.
///
/// "Guarantee" is a deliberately strict word here. Only an absolute length on a sizing
/// property counts. Padding plus a font size does not: the rendered height also depends
/// on the font's line box, which is a property of the font file and the platform, and a
/// number that depends on the platform is not a guarantee. `width: 100%` does not count
/// either — it inherits its answer from an ancestor this function cannot see.
///
/// The consequence is that this function under-reports: a control could be 44 px on a
/// real iPad and still be recorded as not guaranteed. That is the correct direction to
/// be wrong in for a claim nobody here can check with their eyes.
fn guarantees_min_extent(rules: &[Rule], selector: &str, properties: &[&str]) -> bool {
    rules_for_selector(rules, selector)
        .iter()
        .flat_map(|r| properties.iter().filter_map(|p| r.value_of(p)))
        .filter_map(|d| length_px(&d.value))
        .any(|px| px >= MIN_TAP_TARGET_PX)
}

/// Every selector in `styles.css` that declares `cursor: pointer`, i.e. everything the
/// stylesheet itself calls interactive.
fn interactive_selectors(rules: &[Rule]) -> BTreeSet<String> {
    rules
        .iter()
        .filter(|r| {
            r.value_of("cursor")
                .is_some_and(|d| d.value.eq_ignore_ascii_case("pointer"))
        })
        .flat_map(|r| r.selectors.iter().cloned())
        .collect()
}

/// The recorded state of issue #65's 44x44 criterion, one entry per interactive
/// selector.
///
/// The `bool` is "does the stylesheet guarantee 44 px on **both** axes". The
/// CSS-sized controls are all `true` as of the #65 tap-target rule at the end of
/// `styles.css` (`min-height`/`min-width: 44px`, unconditional — see that rule's
/// comment for why it is not scoped to `pointer: coarse`). The table is written
/// down rather than left implicit for two reasons — a new interactive control
/// cannot be added without appearing here, and a fix or a regression must be
/// recorded in the same change that moves the CSS instead of passing unnoticed.
///
/// The two `false` entries are SVG-sized and stay `false` honestly: CSS cannot
/// guarantee the rendered extent of a shape whose size is user units scaled by
/// the camera. Their coverage story lives elsewhere —
/// `commit_dot_hit_target_is_thirty_pixels_at_default_zoom` pins the hit circle's
/// geometry, and zooming out shrinks every target below any fixed threshold no
/// matter what number is written here.
const INTERACTIVE_CENSUS: &[(&str, bool)] = &[
    (".refresh", true),
    // Commit-dot and stub hit circles. Sized in SVG user units by `render/`, not by
    // CSS at all — see `commit_dot_hit_target_is_thirty_pixels_at_default_zoom`.
    (".node-hit", false),
    (".ctx-item", true),
    (".reset-view", true),
    // GitHub-linked ref badges and commit messages (`render/labels.rs`). Applied to
    // SVG `<rect>` / `<text>`, so like `.node-hit` the size is user units set in
    // `render/`, scaled by the camera — the badge rect is `geometry::BADGE_HEIGHT`
    // tall, and the `<text>` has only its glyph box.
    (".clickable", false),
    (".detail-close", true),
    (".detail-parent", true),
    (".act-refresh", true),
    (".act-row", true),
    (".act-undo", true),
    ("button.detail-file", true),
    (".detail-expand", true),
    (".viewer-btn", true),
    (".scale-btn", true),
    // Hunk headers in the flat diff rendering (M2.16e, #210): roving
    // keyboard/tap stops, sized by their own declaration rather than the
    // shared #65 rule because they live in the diff colour block.
    (".diff-hunk", true),
    // The staging selection view's hunk row (M2.17d, #215): the header
    // text (roving keyboard/tap stop, mirrors `.diff-hunk`) and its own
    // adjacent selection checkbox — two separate 44px targets, see
    // `features::diff::selection`'s module doc for why they're split.
    (".stage-hunk-text", true),
    (".stage-hunk-check", true),
    // The Fetch/Pull cancel button in the operations status strip (#232):
    // used to be inline-styled like its neighbouring Dismiss button,
    // which made it invisible to this census (`interactive_selectors` can
    // only see CSS selectors) — given its own class specifically to close
    // that gap, and picked up the shared #65 44x44 rule at the same time.
    (".op-cancel-btn", true),
];

#[test]
fn the_interactive_control_census_matches_the_stylesheet() {
    let rules = stylesheet();
    let found = interactive_selectors(&rules);
    let recorded: BTreeSet<String> = INTERACTIVE_CENSUS
        .iter()
        .map(|(s, _)| (*s).to_string())
        .collect();

    let missing: Vec<_> = found.difference(&recorded).collect();
    let stale: Vec<_> = recorded.difference(&found).collect();
    assert!(
        missing.is_empty(),
        "interactive selectors in styles.css with no entry in INTERACTIVE_CENSUS: {missing:?} \
         — a new tappable control needs a 44x44 decision recorded, not skipped (#65)"
    );
    assert!(
        stale.is_empty(),
        "INTERACTIVE_CENSUS names selectors that no longer declare `cursor: pointer`: \
         {stale:?} — the census has rotted, delete them"
    );
}

/// The census's verdicts are recomputed from the stylesheet, so an entry cannot become a
/// stale claim: if the #65 tap-target rule is deleted or weakened, the `true` entries go
/// red here, and if someone sizes the SVG targets from CSS, the `false` entries do.
#[test]
fn recorded_tap_target_verdicts_still_match_the_stylesheet() {
    let rules = stylesheet();
    for (selector, expected_meets) in INTERACTIVE_CENSUS {
        let wide = guarantees_min_extent(&rules, selector, &["min-width", "width"]);
        let tall = guarantees_min_extent(&rules, selector, &["min-height", "height"]);
        let meets = wide && tall;
        assert_eq!(
            meets, *expected_meets,
            "`{selector}`: stylesheet guarantee is now {meets} but INTERACTIVE_CENSUS \
             records {expected_meets} (width guaranteed: {wide}, height guaranteed: \
             {tall}) — update the census in the same change that moved the CSS"
        );
    }
    assert!(
        !INTERACTIVE_CENSUS.is_empty(),
        "an empty census would make the loop above vacuous"
    );
}

/// The guarantee predicate is exercised on both answers here, against fixture CSS rather
/// than against `styles.css` — otherwise a predicate hard-wired to return `false` would
/// satisfy the census test above perfectly.
#[test]
fn the_guarantee_predicate_can_say_yes() {
    let rules = parse(
        ".big { min-height: 44px; min-width: 44px; } \
         .rem { min-height: 2.75rem; min-width: 3rem; } \
         .pct { min-height: 100%; min-width: 100%; } \
         .small { min-height: 30px; min-width: 30px; }",
    );
    for (selector, expected) in [
        (".big", true),
        (".rem", true),
        (".pct", false),
        (".small", false),
    ] {
        assert_eq!(
            guarantees_min_extent(&rules, selector, &["min-width", "width"]),
            expected,
            "width guarantee for `{selector}`"
        );
        assert_eq!(
            guarantees_min_extent(&rules, selector, &["min-height", "height"]),
            expected,
            "height guarantee for `{selector}`"
        );
    }
}

/// The commit dot's hit circle is the one interactive target whose size is decidable
/// exactly, because it is arithmetic rather than a rendered font box — and it is the
/// target the whole app is built around tapping.
///
/// 2 * (NODE_RADIUS + 15) = 44 CSS pixels at the default zoom — meets the 44x44
/// guidance exactly, at 1.0x, with no zoom required. This test used to pin a 14px
/// shortfall (padding was 8, giving 30px); #65's audit flagged that as the single
/// most consequential undersized target in the app, so the padding moved rather
/// than the test being left to document a known gap. Recorded here, next to the
/// census, because whether the app's primary tap target actually meets guidance
/// belongs in a test, not a comment.
///
/// The larger circle does not collide with a neighbour: same-lane rows are
/// `ROW_HEIGHT` (56px) apart and the nearest cross-lane neighbour is
/// `sqrt(ROW_HEIGHT^2 + LANE_WIDTH^2)` (~65.5px) apart — both comfortably clear
/// of two 44px circles needing 44px of separation.
#[test]
fn commit_dot_hit_target_meets_guidance_at_default_zoom() {
    let radius = f64::from(crate::geometry::NODE_RADIUS);
    let side = node_hit_extent_px(radius, NODE_HIT_PADDING, 1.0);
    assert_eq!(side, 44.0);
    assert_eq!(TapTarget::square(side).verdict(), TargetVerdict::Meets);
}

/// Paired negative for the test above: this is what the OLD constant would still
/// report, so a reader can see the shortfall this fix closed without having to
/// find the removed commit. Not exercising production code — a fixed literal,
/// standing in for the value `NODE_HIT_PADDING` used to be.
#[test]
fn the_previous_padding_would_have_been_undersized() {
    let radius = f64::from(crate::geometry::NODE_RADIUS);
    let old_padding = 8.0;
    let side = node_hit_extent_px(radius, old_padding, 1.0);
    assert_eq!(side, 30.0);
    assert_eq!(
        TapTarget::square(side).verdict(),
        TargetVerdict::Undersized {
            short_by_x_px: 14.0,
            short_by_y_px: 14.0,
        }
    );
}

// ── Keyboard reachability of the commit rows (M1.13) ──────────────────────────
//
// `core`/`focus`'s tests prove the *state machine* is correct in isolation. This
// tripwire proves the render code actually wires that state machine to the DOM
// element it is supposed to govern — the failure mode a pure-state-machine test
// suite cannot see on its own: `GraphFocus` could be built, tested, imported, and
// then never actually attached to `.node-hit`, and every test above would still
// be green while the graph stayed exactly as pointer-only as it started.

/// The commit-row hit circle carries what a roving-tabindex control needs: a
/// reactive `tabindex`, `role="button"`, an accessible name, and the keydown
/// handling that drives `GraphFocus`. Checked as source text — the only thing
/// available without a browser — so it is honest about what it proves: that the
/// wiring is present in the file the wasm build actually compiles, not that a
/// screen reader announces it or that `.focus()` succeeds on a real device.
#[test]
fn the_commit_row_hit_circle_is_keyboard_reachable() {
    assert!(
        RENDER_NODES.contains("role=\"button\""),
        "render/nodes.rs's `.node-hit` circle has no `role=\"button\"` — without it \
         a screen reader has no reason to treat the commit dot as interactive at all \
         (#65)"
    );
    assert!(
        RENDER_NODES.contains("tabindex=tabindex"),
        "render/nodes.rs's `.node-hit` circle is not wired to a reactive `tabindex` — \
         the roving-tabindex pattern needs exactly one row's hit circle in the tab \
         order at a time, and a static/absent tabindex can't do that (#65)"
    );
    assert!(
        RENDER_NODES.contains("aria-label=title"),
        "render/nodes.rs's `.node-hit` circle carries no accessible name — a \
         keyboard/Switch Control user landing on it would hear nothing to say which \
         commit it is (#65)"
    );
    assert!(
        RENDER_NODES.contains("on:keydown=on_node_keydown"),
        "render/nodes.rs's `.node-hit` circle no longer wires `gestures::on_node_keydown` \
         — without it, focusing the circle does nothing: arrow keys, Home/End, and \
         Enter/Space all fall through to the browser default (#65)"
    );
}

/// `core::NODE_HIT_PADDING` mirrors a literal in two wasm-only modules that a host test
/// cannot link. This is what keeps the mirror from rotting: change `+ 8` in either
/// render module and the arithmetic above becomes a lie, and this fails.
#[test]
fn node_hit_padding_still_matches_the_render_code() {
    let expected = format!("r=NODE_RADIUS + {}", NODE_HIT_PADDING as i32);
    for (name, src) in [
        ("render/nodes.rs", RENDER_NODES),
        ("render/stubs.rs", RENDER_STUBS),
    ] {
        assert!(
            src.contains(&expected),
            "{name} no longer draws its hit circle as `{expected}` — \
             `core::NODE_HIT_PADDING` and every tap-target number derived from it are \
             now wrong (#65)"
        );
    }
}

// ── Operations status strip (#232, M2.20f) ──────────────────────────────────────

/// The Cancel button is a real interactive control with a per-row
/// accessible name, not a decoration: `aria-label` names the operation it
/// targets, so a screen reader hears "Cancel: Fetching 'origin'" rather
/// than the bare word "Cancel" repeated once per in-flight row when more
/// than one write is running at once.
#[test]
fn the_operations_cancel_button_carries_an_accessible_label() {
    assert!(
        OPERATIONS_VIEW.contains("aria-label=format!(\"Cancel: {what}\")"),
        "features/operations/view.rs's Cancel button lost its per-row \
         accessible name — without it every Cancel button in a multi-\
         operation strip reads identically to a screen reader (#232)"
    );
}

/// The in-flight row is a live region: a Cancel tap's "cancelling…" text,
/// and every stage transition before it, must reach a screen reader
/// without the user re-focusing the strip — the a11y face of #232's own
/// acceptance criterion that a cancel produces a *visible* state change,
/// not a silent one.
#[test]
fn the_inflight_operation_row_is_an_announced_live_region() {
    assert!(
        OPERATIONS_VIEW.contains("role=\"status\""),
        "features/operations/view.rs's in-flight row has no `role=\"status\"` \
         — a screen reader has no reason to announce its stage or progress \
         text changing (#232)"
    );
    assert!(
        OPERATIONS_VIEW.contains("aria-live=\"polite\""),
        "features/operations/view.rs's in-flight row is missing \
         `aria-live=\"polite\"` — without it a stage change or a cancel \
         request updates the DOM silently (#232)"
    );
}

// ── Reduced motion ──────────────────────────────────────────────────────────────

fn reduced_motion_rules(rules: &[Rule]) -> Vec<&Rule> {
    rules
        .iter()
        .filter(|r| {
            r.at_context.iter().any(|c| {
                let c = c.to_ascii_lowercase();
                c.contains("prefers-reduced-motion") && c.contains("reduce")
            })
        })
        .collect()
}

#[test]
fn a_reduced_motion_block_neutralises_animation_and_transition() {
    let rules = stylesheet();
    let reduced = reduced_motion_rules(&rules);
    assert!(
        !reduced.is_empty(),
        "styles.css has no `@media (prefers-reduced-motion: reduce)` block"
    );

    let universal = reduced
        .iter()
        .find(|r| r.selectors.iter().any(|s| s == "*"))
        .expect(
            "the reduced-motion block does not apply to `*` — a per-selector opt-in \
                 is exactly the list someone forgets to extend",
        );

    for property in ["animation-duration", "transition-duration"] {
        let declaration = universal
            .value_of(property)
            .unwrap_or_else(|| panic!("the reduced-motion `*` rule does not set {property}"));
        assert!(
            declaration.important,
            "`{property}` in the reduced-motion block is not `!important`, so any \
             ordinary rule outranks it"
        );
    }
}

/// The reduced-motion block wins by `!important`, which an `!important` motion
/// declaration elsewhere would beat. There are none today; this stops the first one from
/// arriving unnoticed.
#[test]
fn no_important_motion_declaration_escapes_the_reduced_motion_block() {
    let rules = stylesheet();
    for rule in &rules {
        let in_reduced_block = rule.at_context.iter().any(|c| {
            let c = c.to_ascii_lowercase();
            c.contains("prefers-reduced-motion")
        });
        if in_reduced_block {
            continue;
        }
        for declaration in &rule.declarations {
            let motion = declaration.property.starts_with("animation")
                || declaration.property.starts_with("transition")
                || declaration.property == "scroll-behavior";
            assert!(
                !(motion && declaration.important),
                "`{}` declares `{}: {} !important` outside the reduced-motion block, \
                 which outranks the reduced-motion override (#65)",
                rule.selectors.join(", "),
                declaration.property,
                declaration.value
            );
        }
    }
}

// ── Text zoom (Dynamic Type parity) ──────────────────────────────────────────────
//
// iOS/iPadOS Safari's "Larger Text" accessibility setting (Dynamic Type) scales any
// text sized in a unit relative to the root font size (`rem`, `em`, `%`); text set in
// absolute `px` never moves no matter how far the user turns the system setting up.
// The design doc's "Dynamic Type-like... without clipping" validation row therefore
// depends on every piece of HTML chrome using a relative unit.
//
// The one deliberate exception is the handful of `<text>` labels drawn *inside* the
// SVG graph canvas (commit labels, badge text, the per-node glyph): those are scaled
// by the graph's own pinch-zoom camera (`geometry.rs` / `render/`), not by the page's
// root font size, so `rem` would be the wrong unit for them. They get their own
// accessible-zoom path — pinch — and are recorded here by name rather than silently
// exempted, the same way `INTERACTIVE_CENSUS` records the SVG-sized tap targets above.

/// Selectors that legitimately size text in `px` because the text they style is drawn
/// inside the SVG graph canvas and scaled by the camera, not by the page's root font
/// size. Anything *outside* this list that declares `font-size` in `px` is HTML chrome
/// that Dynamic Type cannot reach.
const SVG_TEXT_PX_FONT_SIZE_CENSUS: &[&str] = &[
    ".node-icon",
    ".label-msg",
    ".label-meta",
    ".stub-label",
    ".badge-text",
];

/// One `(selector, font-size value)` pair per selector in `styles.css` that declares a
/// font size — every selector in the file, not a hand-picked subset, so a new rule added
/// anywhere is caught rather than silently missed by a fixed list of places to look.
///
/// **The `font:` shorthand counts.** `font: 14px sans-serif` sets a font size just as
/// surely as `font-size: 14px` does, and a scan that reads only the longhand goes green
/// on it — verified by measurement, not assumed: appending one shorthand rule left this
/// tripwire passing before this function handled it. The shorthand's size is the first
/// length-looking token; `font: inherit` (every current use in this file) declares no
/// size of its own and is skipped.
fn font_size_declarations(rules: &[Rule]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for rule in rules {
        let size = rule
            .value_of("font-size")
            .map(|d| d.value.clone())
            .or_else(|| rule.value_of("font").and_then(|d| shorthand_size(&d.value)));
        if let Some(size) = size {
            for selector in &rule.selectors {
                out.push((selector.clone(), size.clone()));
            }
        }
    }
    out
}

/// The size component of a `font:` shorthand, if it declares one.
///
/// The shorthand's grammar puts the size after the optional style/variant/weight/stretch
/// keywords and before an optional `/line-height`, so the first token that starts with a
/// digit or `.` is the size. Keyword-only forms (`inherit`, `initial`, the system fonts
/// like `menu`) declare no length and yield `None` — they inherit a size rather than
/// pinning one, which is exactly what this module wants.
fn shorthand_size(value: &str) -> Option<String> {
    value
        .split([' ', '\t'])
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit() || c == '.'))
        .map(|t| t.split('/').next().unwrap_or(t).to_string())
}

fn is_absolute_px(value: &str) -> bool {
    value.trim().to_ascii_lowercase().ends_with("px")
}

/// Every `font-size` declaration in the stylesheet is either a relative unit (so
/// Dynamic Type can scale it) or a recorded SVG exception. Checked over *all* rules the
/// parser finds, so a new fixed-px label anywhere in the file — not just in the places
/// this test's author thought to look — trips it.
#[test]
fn every_font_size_declaration_is_relative_or_a_recorded_svg_exception() {
    let rules = stylesheet();
    let declared = font_size_declarations(&rules);
    assert!(
        declared.len() >= 25,
        "found only {} font-size declarations in styles.css — below the floor this \
         tripwire was written against, so the reader has likely stopped finding them \
         and this check would pass over nothing",
        declared.len()
    );

    let census: BTreeSet<&str> = SVG_TEXT_PX_FONT_SIZE_CENSUS.iter().copied().collect();
    let mut seen_px: BTreeSet<&str> = BTreeSet::new();

    for (selector, value) in &declared {
        if is_absolute_px(value) {
            assert!(
                census.contains(selector.as_str()),
                "`{selector}` sets `font-size: {value}` — an absolute px size that iOS \
                 Dynamic Type / Safari's text-size setting cannot scale, so a user who \
                 turns up their system text size sees no change here. Use `rem`/`em`/`%` \
                 instead, or if this selector styles SVG graph text scaled by the pinch \
                 camera rather than the page, add it to SVG_TEXT_PX_FONT_SIZE_CENSUS \
                 with that reasoning recorded (a `font:` shorthand carrying a px size \
                 reads the same way here as the longhand — use a relative unit in it)"
            );
            seen_px.insert(selector.as_str());
        }
    }

    let stale: Vec<_> = census.difference(&seen_px).collect();
    assert!(
        stale.is_empty(),
        "SVG_TEXT_PX_FONT_SIZE_CENSUS names selector(s) that no longer declare an \
         absolute-px font-size: {stale:?} — the census has rotted, delete them (if they \
         now use rem/em/%, nothing else needs to change; Dynamic Type already reaches \
         them)"
    );
}

// ── Landmarks ───────────────────────────────────────────────────────────────────

/// The opening tag of the first element in `src` matching `needle`, i.e. `needle` up to
/// the first `>`.
fn opening_tag<'a>(src: &'a str, needle: &str) -> Option<&'a str> {
    let start = src.find(needle)?;
    let rest = &src[start..];
    let end = rest.find('>')?;
    Some(&rest[..=end])
}

/// A `<section>` is only exposed as a `region` landmark when it has an accessible name.
/// Without one, VoiceOver's rotor has no entry for the graph and there is nothing to
/// jump to — the criterion "VoiceOver and keyboard paths exist" fails at the first step.
///
/// This checks the markup source rather than a rendered DOM, which is all that is
/// available here. It proves the attribute is written and bound to
/// `core::GRAPH_REGION_LABEL`; it does not prove any screen reader announces it.
#[test]
fn the_graph_section_carries_an_accessible_name() {
    let tag = opening_tag(APP_MOD, "<section class=\"graph\"")
        .expect("app/mod.rs no longer contains `<section class=\"graph\"`");
    assert!(
        tag.contains("aria-label=GRAPH_REGION_LABEL"),
        "the graph <section> has no `aria-label=GRAPH_REGION_LABEL`, so it is an \
         anonymous container rather than a named landmark. Found: {tag}"
    );
    assert!(
        APP_MOD.contains("GRAPH_REGION_LABEL"),
        "app/mod.rs does not import the label constant"
    );
    // And the constant is the words a user would hear.
    assert_eq!(GRAPH_REGION_LABEL, "Commit history graph");
}

#[test]
fn opening_tag_stops_at_the_first_angle_bracket() {
    assert_eq!(
        opening_tag(
            "x <section class=\"graph\" aria-label=X> inner </section>",
            "<section"
        ),
        Some("<section class=\"graph\" aria-label=X>")
    );
    assert_eq!(opening_tag("nothing here", "<section"), None);
    assert_eq!(opening_tag("<section unterminated", "<section"), None);
}

// ── Disabled controls that still have to explain themselves ─────────────────────
//
// #65's finding was that a disabled item's reason lived only in `title`, which a
// finger never surfaces. The fix put the reason on screen *and* into the item's
// accessible name (`graph::core::disabled_menu_item_copy`). Both halves of that fix
// are load-bearing, and both are quietly undone by rendering the item as a `<span>`:
// a `<span>` has no role that supports `aria-label`/`aria-disabled`, so those two
// attributes are discarded, and it is not focusable, so `Tab` walks past the one
// item in the menu that most needs explaining. `menu.rs`'s module docs write the
// full reasoning; this is what holds it.

const DISABLED_CTX_ITEM: &str = "class=\"ctx-item disabled\"";

/// The opening tag (element name plus attributes, without the angle brackets) of
/// every element in `src` carrying `class="ctx-item disabled"`.
///
/// These `view!` blocks put the class on its own line, so the element name is found
/// by walking back to the nearest `<`.
fn disabled_ctx_item_tags(src: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = src[from..].find(DISABLED_CTX_ITEM) {
        let at = from + rel;
        from = at + DISABLED_CTX_ITEM.len();
        let Some(open) = src[..at].rfind('<') else {
            continue;
        };
        let Some(close_rel) = src[open..].find('>') else {
            continue;
        };
        out.push(&src[open + 1..open + close_rel]);
    }
    out
}

/// The element name a tag from [`disabled_ctx_item_tags`] declares.
fn element_of(tag: &str) -> &str {
    tag.split_whitespace().next().unwrap_or("")
}

/// Every greyed-out context-menu item is a focusable `<button>` carrying
/// `aria-disabled`, never a `<span>` and never a natively-`disabled` button.
///
/// The two ways to fail this are opposites of each other and both silently drop the
/// reason: a `<span>` is skipped by `Tab` and ignores the ARIA attributes outright,
/// and a `<button prop:disabled=true>` is removed from the tab order by the browser.
/// The shipped shape is the third one — a `<button>` with `aria-disabled="true"`, no
/// `prop:disabled`, and no `on:click`, so it is inert by construction while staying
/// reachable. `dialogs/confirm.rs` argues the same case for its confirm button.
#[test]
fn every_disabled_context_menu_item_is_focusable() {
    let tags = disabled_ctx_item_tags(MENU);

    // Anti-vacuity floor. "Every tag is a button" is trivially true of an empty
    // list, so if this census ever stopped finding the items — file moved, markup
    // reshaped, class renamed — the assertions below would go green while checking
    // nothing. Eight is what menu.rs has today (GitHub link, two commit items,
    // "Amend last commit" since M2.19c/#224, Stage Changes, Discard Changes,
    // Delete Untracked Files, the offline notice).
    assert!(
        tags.len() >= 8,
        "only {} `.ctx-item.disabled` item(s) found in menu.rs — this census has \
         lost its subject and every assertion below it is now vacuous. If items \
         were genuinely removed, lower this floor deliberately rather than \
         letting the tripwire expire on its own.",
        tags.len()
    );

    for tag in &tags {
        assert_eq!(
            element_of(tag),
            "button",
            "a disabled context-menu item is rendered as `<{}>`. Only a `<button>` \
             is focusable and only a `<button>` honours the `aria-label` / \
             `aria-disabled` that `disabled_menu_item_copy` builds — on a `<span>` \
             the reason reaches nobody who cannot see it. Tag: {tag}",
            element_of(tag)
        );
        assert!(
            !tag.contains("prop:disabled"),
            "a disabled context-menu item sets `prop:disabled`, which takes it out \
             of the tab order and makes its own reason unreachable by the user it \
             was written for. `aria-disabled` plus no `on:click` is the shape that \
             stays reachable. Tag: {tag}"
        );
        assert!(
            tag.contains("aria-disabled=\"true\""),
            "a disabled context-menu item does not announce itself as unavailable \
             — it reads as an ordinary, actionable button. Tag: {tag}"
        );
    }
}

/// The census helper is proved on both shapes it has to tell apart, so the tripwire
/// above is known capable of failing rather than assumed to be.
#[test]
fn the_disabled_item_census_can_tell_a_span_from_a_button() {
    // The shape menu.rs shipped before this tripwire existed — this is the paired
    // negative, and it is what proves the assertion is not decorative.
    let old = "view! {\n    <span\n        class=\"ctx-item disabled\"\n        \
               aria-disabled=\"true\"\n    >\n";
    let old_tags = disabled_ctx_item_tags(old);
    assert_eq!(old_tags.len(), 1);
    assert_eq!(element_of(old_tags[0]), "span");

    let new = "view! {\n    <button\n        class=\"ctx-item disabled\"\n        \
               aria-disabled=\"true\"\n    >\n";
    let new_tags = disabled_ctx_item_tags(new);
    assert_eq!(new_tags.len(), 1);
    assert_eq!(element_of(new_tags[0]), "button");
    assert!(new_tags[0].contains("aria-disabled=\"true\""));

    // The other refusable shape, and the empty case.
    let native = "<button class=\"ctx-item disabled\" prop:disabled=true>";
    assert!(disabled_ctx_item_tags(native)[0].contains("prop:disabled"));
    assert!(disabled_ctx_item_tags("no menu items here").is_empty());
}

// ── The commit modal's tap targets (M2.19c, #224) ───────────────────────────────
//
// This modal is inline-styled (see `dialogs/mod.rs` for the iPad-proven recipe it
// follows), which puts every one of its controls outside the stylesheet census
// above: `INTERACTIVE_CENSUS` reads CSS selectors, and there are none to read.
// `dialogs/confirm.rs` hit the same gap in M2.18b and answered it by naming the
// 44x44 floor once, as `TOUCH_TARGET_STYLE`, and pairing it with the button base
// style at every site. #224 rewrote this modal — three modes, a scope review, a
// re-check banner — and brought its buttons onto the same pairing; before that
// they were `padding:6px 14px` on a 13px font, roughly 30px tall, under the floor
// the rest of the app was raised to in #65.
//
// The tripwire is over that pairing rather than over rendered geometry, because
// the pairing is the part that exists in this repository. What it cannot prove is
// that the button is 44px on a real device — no test here can.

/// Every place the modal's button base style is used, with what follows it.
///
/// Returns one entry per `{BUTTON_BASE}` occurrence, so a floor over the count is
/// what stops the assertions going vacuous if the constant is renamed away.
fn button_base_uses(src: &str) -> Vec<&str> {
    const NEEDLE: &str = "{BUTTON_BASE}";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = src[from..].find(NEEDLE) {
        let at = from + rel + NEEDLE.len();
        from = at;
        out.push(&src[at..src.len().min(at + NEEDLE.len() + 24)]);
    }
    out
}

#[test]
fn every_commit_modal_button_carries_the_44px_floor() {
    let uses = button_base_uses(COMMIT_MODAL);
    // Anti-vacuity floor. Four is what the modal has today: Cancel, the confirm
    // button's two style branches, and the re-check banner's action. "Every use
    // is paired" is trivially true of no uses at all, so a rename that made this
    // census find nothing would otherwise pass silently.
    assert!(
        uses.len() >= 4,
        "only {} `{{BUTTON_BASE}}` use(s) found in dialogs/commit.rs — this census \
         has lost its subject and the assertion below it is now vacuous. If buttons \
         were genuinely removed, lower this floor deliberately.",
        uses.len()
    );
    for tail in &uses {
        assert!(
            tail.starts_with("{TOUCH_TARGET_STYLE}"),
            "a commit-modal button styles itself from BUTTON_BASE without the 44x44 \
             floor beside it, so it inherits only the padding — the exact shape #65 \
             found undersized. Follows BUTTON_BASE here: {tail:?}"
        );
    }

    // The pre-#224 shape, named so a revert is caught rather than merely becoming
    // un-asserted: a `style="padding:…"` literal is a control styled without going
    // through `BUTTON_BASE` at all, which is how the old `padding:6px 14px`
    // buttons (~30px tall on this font) escaped every check there was. Matched on
    // the attribute rather than on the declaration alone, so the prose above and
    // in `dialogs/commit.rs` may keep quoting the old value.
    assert!(
        !COMMIT_MODAL.contains("style=\"padding:"),
        "a commit-modal control declares its own padding instead of building on \
         BUTTON_BASE + TOUCH_TARGET_STYLE, so nothing holds it to the 44px floor"
    );

    // And the floor itself is still a floor. `TOUCH_TARGET_STYLE`'s own value is
    // asserted where it is defined; this is the half that would notice the
    // constant being pointed somewhere else.
    assert!(
        crate::features::dialogs::core::TOUCH_TARGET_STYLE.contains("min-height:44px"),
        "TOUCH_TARGET_STYLE no longer declares the 44px floor the pairing above \
         relies on"
    );
}

/// The census helper is proved on both answers, against fixture source — otherwise
/// a helper that returned an empty list would satisfy the loop above perfectly.
#[test]
fn the_button_base_census_can_spot_an_unfloored_button() {
    let floored = r#"style=format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:#fff;")"#;
    let uses = button_base_uses(floored);
    assert_eq!(uses.len(), 1);
    assert!(uses[0].starts_with("{TOUCH_TARGET_STYLE}"));

    // The shape this modal actually shipped before #224 — the paired negative.
    let unfloored = r#"style=format!("{BUTTON_BASE}color:#fff;")"#;
    let uses = button_base_uses(unfloored);
    assert_eq!(uses.len(), 1);
    assert!(!uses[0].starts_with("{TOUCH_TARGET_STYLE}"));

    assert!(button_base_uses("no buttons here").is_empty());
}

// ── The amend seams (M2.19c, #224) ──────────────────────────────────────────────
//
// `dialogs/commit.rs` and `menu.rs` are wasm-only: `cargo test --workspace` never
// compiles them, and this crate has no wasm-bindgen-test harness, so nothing
// *executes* a line of either. Two decisions used to live in them anyway — which
// endpoint the confirm button reaches, and whether "Amend last commit" is offered
// — and both were reachable-only-by-hand. They now come from host-tested
// functions in `features::dialogs::commit`. These tripwires are the half that
// notices if a later edit stops asking and starts deciding again; the tests over
// the functions themselves are in that module.
//
// What they cannot prove: that the returned answer is rendered correctly. Only
// that the answer is the one being consulted.

/// Whether the guided re-check fills the message box before it announces what
/// the box holds.
///
/// Fail-closed on a missing subject: if either landmark is gone this returns
/// false rather than quietly passing, because a census with nothing to census
/// is the vacuous-green shape this file exists to avoid.
fn seeds_before_it_announces(src: &str) -> bool {
    match (src.find("seed_amend_msg("), src.find("Recheck::Retargeted")) {
        (Some(seed), Some(announce)) => seed < announce,
        _ => false,
    }
}

#[test]
fn the_amend_seams_are_decided_in_the_host_tested_core() {
    assert!(
        COMMIT_MODAL.contains("submit_path("),
        "the commit modal no longer asks `submit_path` which endpoint a press \
         reaches, so the plain-commit / amend dispatch is being decided in a file \
         no test compiles. An amend routed to the plain-commit closure writes a \
         second commit instead of rewriting the tip."
    );
    assert!(
        MENU.contains("amend_offer("),
        "menu.rs no longer asks `amend_offer` whether to offer \"Amend last \
         commit\", so the gate is a condition in a file no test compiles"
    );
    // The hand-rolled form, named so a revert is caught rather than merely
    // becoming un-asserted.
    assert!(
        !MENU.contains("is_head && !is_stub"),
        "the amend gate has been inlined back into menu.rs; inverting or dropping \
         it there would offer an amend on every stub with nothing going red"
    );
}

/// The ordering bug the M2.19c review found: the retarget banner claimed "your
/// message below is unchanged" and the next statement re-seeded the box from the
/// new tip, replacing exactly the text it had just vouched for.
#[test]
fn the_recheck_seeds_the_box_before_it_announces_what_the_box_holds() {
    assert!(
        seeds_before_it_announces(COMMIT_MODAL),
        "dialogs/commit.rs sets Recheck::Retargeted before it calls \
         seed_amend_msg. The banner speaks about the message box; seeding can \
         replace the message box. Announcing first is a claim the next line can \
         contradict, and the user's next act is to press Amend."
    );
}

/// Both answers, against fixture source — a predicate that only ever returned
/// true would satisfy the tripwire above perfectly.
#[test]
fn the_seed_ordering_census_can_spot_the_shape_that_shipped() {
    let fixed = "let seeded = dialogs.seed_amend_msg(&d.message); \
                 set(Recheck::Retargeted { new_tip, summary, message: seeded })";
    assert!(seeds_before_it_announces(fixed));

    // The paired negative: the pre-fix order.
    let broken = "set(Recheck::Retargeted { new_tip, summary }); \
                  dialogs.seed_amend_msg(&d.message);";
    assert!(!seeds_before_it_announces(broken));

    // And a source with no subject at all must not read as a pass.
    assert!(!seeds_before_it_announces("nothing to see"));
    assert!(!seeds_before_it_announces(
        "dialogs.seed_amend_msg(&d.message);"
    ));
}

// ── The published-history ceremony's seams (M2.19d, #225) ───────────────────────
//
// Same shape as the #224 tripwires above and the same limitation: these prove
// which function is consulted and in what order, never that its answer is drawn
// correctly. Both wasm-only files are the subject.

/// Whether the amend submit path consults the pre-flight gate **before** it
/// reaches the network.
///
/// Fail-closed on a missing subject, like `seeds_before_it_announces`: a source
/// with no gate call and no request call is not a passing source, it is a
/// source that no longer contains the thing being checked.
fn gates_before_it_sends(src: &str) -> bool {
    match (
        src.find("amend_preflight("),
        src.find("amend_commit_request("),
    ) {
        (Some(gate), Some(send)) => gate < send,
        _ => false,
    }
}

/// Whether `Dialogs::reset_amend` clears the pre-flight knowledge along with
/// everything else it clears.
///
/// Scoped to that one function's body — from its `fn` line to the next method —
/// rather than to the whole file, so a mention of the field anywhere else
/// cannot stand in for the reset.
fn reset_amend_clears_preflight(src: &str) -> bool {
    let Some(start) = src.find("fn reset_amend(") else {
        return false;
    };
    let body = &src[start..];
    let end = body[1..]
        .find("\n    pub fn ")
        .map(|i| i + 1)
        .unwrap_or(body.len());
    body[..end].contains("amend_preflight")
}

/// Whether the ceremony's second step records the agreement **before** it
/// re-submits.
///
/// Scoped to the `confirm_published` closure's body — from its `let` line to
/// the next top-level `let` in the same component — so neither the `submit_amend`
/// definition above it nor the `SubmitPath::Amend(target) => submit_amend(target)`
/// call below it can stand in for the re-submit this is ordering against.
///
/// Fail-closed on a missing subject, like its two siblings: a body with no
/// `confirm_amend_target` call, or no closure at all, is not a passing source.
fn records_consent_before_it_resubmits(src: &str) -> bool {
    let Some(start) = src.find("let confirm_published = ") else {
        return false;
    };
    let body = &src[start..];
    let end = body[1..]
        .find("\n    let ")
        .map(|i| i + 1)
        .unwrap_or(body.len());
    let body = &body[..end];
    match (
        body.find("confirm_amend_target("),
        body.find("submit_amend("),
    ) {
        (Some(record), Some(resubmit)) => record < resubmit,
        _ => false,
    }
}

/// The brace-matched body that follows `marker` — the block from the first
/// `{` after it to its matching `}`, exclusive.
///
/// Every census below that says "on the live path" needs this. A whole-file
/// `src.contains("some_call(")` cannot tell a call the browser reaches from
/// text that merely exists in the file: moving the call into an `if false {}`
/// beneath its old home, or into a helper nothing invokes, leaves the string
/// present and the mechanism dead. Scoping to a body is what makes the
/// difference checkable.
///
/// Fail-closed by construction: a missing marker, no block after it, or an
/// unbalanced one all return `None`, and every caller reads `None` as a
/// failed census rather than as "nothing to check".
///
/// Brace counting, not parsing — it is counting braces in Rust source it is
/// compiled alongside, so an unbalanced brace inside a string literal in the
/// scanned region would confuse it. None of the scanned regions contain one,
/// and if one is ever added the census fails closed (unbalanced ⇒ `None`)
/// rather than passing vacuously.
fn braced_body<'a>(src: &'a str, marker: &str) -> Option<&'a str> {
    let start = src.find(marker)?;
    let rest = &src[start..];
    let open = rest.find('{')?;
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether `needle` appears in `body` at `body`'s **own statement level** —
/// brace depth zero within it, i.e. not tucked inside a nested block.
///
/// The difference between "the string is somewhere in this block" and "this
/// block runs it": `if false { .. }`, `match { .. }`, a closure and a nested
/// `if let` all raise the depth, and a call under any of them is a call the
/// block does not unconditionally make.
///
/// Brace counting, with `braced_body`'s caveat and for its reason — a brace
/// inside a string literal in the scanned region would confuse it, and none of
/// the scanned regions contain one.
fn at_statement_level(body: &str, needle: &str) -> bool {
    let mut depth = 0i32;
    for (i, c) in body.char_indices() {
        if depth == 0 && body[i..].starts_with(needle) {
            return true;
        }
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    false
}

/// Whether the context menu's amend opener applies the tip's detail **from the
/// read itself and unconditionally** — a statement of the `if let Ok(detail)`
/// arm of the `fetch_commit_detail` in its own `on_amend` handler.
///
/// Scoped three ways, and it is worth being exact about what each buys, because
/// this predicate's previous incarnation was reported as pinning more than it
/// did.
///
/// * The **handler** scope rejects a call that lives elsewhere in `menu.rs`.
/// * The **`if let Ok` arm** scope rejects a call that runs without a detail to
///   apply.
/// * The **statement-level** scope rejects a call nested inside anything within
///   that arm — `if false { .. }` chief among them.
///
/// What it still does not prove, and what no source census can: that
/// `on_amend` is itself reachable, or that no `return` precedes the call. The
/// earlier doc here claimed to catch "an unreachable branch *inside*
/// `on_amend`", and the mutation that was cited as proof placed the dead branch
/// *outside* the `Ok` arm. Placed inside it — `if let Ok(detail) = .. { if
/// false { record } }` — the whole suite stayed green. The statement-level
/// scope above is what closes that gap; the reachability of the handler itself
/// remains unpinned, and is what the iPad testbed pass is for.
fn menu_applies_the_detail_from_the_read(src: &str) -> bool {
    let Some(handler) = braced_body(src, "let on_amend = ") else {
        return false;
    };
    let Some(read) = braced_body(handler, "if let Ok(detail) = fetch_commit_detail(") else {
        return false;
    };
    at_statement_level(read, "apply_amend_detail(")
}

/// Whether `menu.rs` reaches the pre-flight's inputs **only** through the
/// guarded chokepoint.
///
/// The point of `Dialogs::apply_amend_detail` is that this callback resumes
/// after an `await` and cannot assume the dialog still points at the tip it was
/// spawned for. A direct `record_amend_detail`/`seed_amend_msg` here is that
/// assumption re-made by hand — and it is the exact shape of the bug the guard
/// was added to close, so a later edit reaching for the raw pair must not pass
/// unnoticed. Whole-file on purpose: there is no legitimate second caller in
/// this file, so any occurrence is the finding.
fn menu_writes_the_detail_only_through_the_guard(src: &str) -> bool {
    !src.contains("record_amend_detail(") && !src.contains("seed_amend_msg(")
}

/// Whether `Dialogs::apply_amend_detail` actually consults the currency check
/// before it writes — and writes only under it.
///
/// Both halves are needed. Consulting `detail_read_use` and then writing
/// regardless is a guard in name only, so the two writes must be *nested*
/// (statement-level would mean unconditional); and nesting them under some
/// other condition while never asking `detail_read_use` is the same bug wearing
/// a different `if`. `signals.rs` is `#[cfg(target_arch = "wasm32")]`, so this
/// census is the only thing that reads it.
fn signals_guard_the_detail_before_applying_it(src: &str) -> bool {
    let Some(body) = braced_body(src, "pub fn apply_amend_detail(") else {
        return false;
    };
    let (Some(guard), Some(record), Some(seed)) = (
        body.find("detail_read_use("),
        body.find("record_amend_detail("),
        body.find("seed_amend_msg("),
    ) else {
        return false;
    };
    guard < record
        && guard < seed
        && !at_statement_level(body, "record_amend_detail(")
        && !at_statement_level(body, "seed_amend_msg(")
}

/// Whether the amend opener holds the confirm button **before** it spawns the
/// read that decides the pre-flight (#225).
///
/// The ordering is the whole fix: `Dialogs::open` leaves the phase `Idle`
/// (confirm enabled) and `amend_preflight` reads an unlanded detail as
/// `Unknown`, which sends. Entering the hold after the spawn — or not at all —
/// re-opens the window in which a press POSTs an amend of published history
/// with no ceremony at all.
fn menu_holds_the_press_until_the_read_answers(src: &str) -> bool {
    let Some(handler) = braced_body(src, "let on_amend = ") else {
        return false;
    };
    match (
        handler.find("begin_publication_read("),
        handler.find("spawn_local("),
    ) {
        (Some(hold), Some(spawn)) => hold < spawn,
        _ => false,
    }
}

/// Whether that hold is released on **both** outcomes of the read.
///
/// The mirror of the census above, and it needs to be here: a hold nothing
/// releases is not a safer version of the bug, it is a different bug — the
/// confirm button inert forever whenever a single `GET /api/commit/{id}`
/// fails, making amend unreachable through the UI. So the release must be in
/// the spawned block and **not** confined to its `Ok` arm.
fn menu_releases_the_press_on_either_outcome(src: &str) -> bool {
    let Some(handler) = braced_body(src, "let on_amend = ") else {
        return false;
    };
    let Some(spawned) = braced_body(handler, "spawn_local(") else {
        return false;
    };
    let Some(read) = braced_body(spawned, "if let Ok(detail) = fetch_commit_detail(") else {
        return false;
    };
    spawned.contains("finish_publication_read(") && !read.contains("finish_publication_read(")
}

/// Whether the guided re-check records the *new* tip's published answer on its
/// own live path: after the detail read that supplies it, and before the phase
/// that re-enables the confirm button.
///
/// Both bounds matter. Recording before the read is recording nothing;
/// recording after `Recheck::Retargeted` is recording it after the button the
/// gate protects has already come back on.
fn recheck_records_detail_before_it_re_enables(src: &str) -> bool {
    let Some(body) = braced_body(src, "let recheck = ") else {
        return false;
    };
    match (
        body.find("let detail = match fetch_commit_detail("),
        body.find("record_amend_detail("),
        body.find("Recheck::Retargeted {"),
    ) {
        (Some(read), Some(record), Some(retargeted)) => read < record && record < retargeted,
        _ => false,
    }
}

/// The gate has to be in front of the POST, not behind it.
///
/// What it is guarding against is not a hypothetical: `submit_amend` is a
/// closure in a file `cargo test --workspace` never compiles, so moving the
/// pre-flight below `amend_commit_request` — or deleting it — would leave every
/// test in the suite green while an amend of pushed history went out with no
/// warning shown at all. ADR 0040 records that the server will not stop it.
#[test]
fn the_published_history_ceremony_runs_before_the_request_does() {
    assert!(
        gates_before_it_sends(COMMIT_MODAL),
        "dialogs/commit.rs no longer consults `amend_preflight` before calling \
         `amend_commit_request`. The server deliberately does not block an amend \
         of pushed history (ADR 0040) — this gate is the only thing that asks."
    );
    assert!(
        COMMIT_MODAL.contains("AwaitingPublishedConfirm"),
        "the ceremony's phase is gone from the modal, so `Preflight::Confirm` has \
         nowhere to land and the warning it decides on is never rendered"
    );
    // Scoped to the live path, not to the file, and to a statement of that
    // path rather than to anything nested inside it. A whole-file `contains`
    // used to pass with the call moved into an unreachable branch under the
    // very same handler; an arm-wide one still passed with that branch moved
    // *inside* the arm.
    assert!(
        menu_applies_the_detail_from_the_read(MENU),
        "menu.rs's `on_amend` handler no longer applies the `CommitDetail` as a \
         statement of the `if let Ok(detail) = fetch_commit_detail(..)` arm, so \
         the pre-flight gate has nothing to read and every amend looks \
         unpublished to it. A call elsewhere in the file, or nested inside \
         anything within that arm, does not count."
    );
    assert!(
        menu_writes_the_detail_only_through_the_guard(MENU),
        "menu.rs writes the pre-flight's input directly again instead of going \
         through `Dialogs::apply_amend_detail`. That callback resumes after an \
         `await`: writing an abandoned tip's answer evicts the answer for the \
         commit the dialog now shows, and the published-history ceremony stops \
         firing for it."
    );
    assert!(
        signals_guard_the_detail_before_applying_it(DIALOG_SIGNALS),
        "`Dialogs::apply_amend_detail` no longer consults `detail_read_use` \
         before recording the flag and seeding the box, so the chokepoint menu.rs \
         routes through has stopped checking anything"
    );
    assert!(
        recheck_records_detail_before_it_re_enables(COMMIT_MODAL),
        "the guided re-check no longer records the retargeted commit's own \
         published answer between reading it and re-enabling the confirm button, \
         so amending after a stale tip is gated on the previous commit's flag or \
         on nothing"
    );
}

/// Both answers for each of the two live-path censuses above — a predicate
/// that always said "yes" would satisfy them perfectly.
#[test]
fn the_live_path_censuses_can_spot_a_call_that_never_runs() {
    let live = "    let on_amend = move |_| {\n        \
                spawn_local(async move {\n            \
                if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                dialogs.apply_amend_detail(&tip, detail.on_remote, &detail.message);\n            \
                }\n        });\n    };\n";
    assert!(menu_applies_the_detail_from_the_read(live));
    assert!(menu_writes_the_detail_only_through_the_guard(live));

    // The shape that ships the bug: present in the handler, unreachable.
    let dead = "    let on_amend = move |_| {\n        \
                spawn_local(async move {\n            \
                if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                log(&detail.message);\n            }\n            \
                if false {\n                \
                dialogs.apply_amend_detail(&tip, true, \"m\");\n            }\n        \
                });\n    };\n";
    assert!(!menu_applies_the_detail_from_the_read(dead));

    // The same mutation the arm-scoped predicate could not see: the dead branch
    // moved *inside* the `Ok` arm, so an arm-wide `contains` finds the string
    // while nothing runs it. This is the case the previous census claimed to
    // catch and did not — the whole suite stayed green against it.
    let dead_inside = "    let on_amend = move |_| {\n        \
                       spawn_local(async move {\n            \
                       if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                       log(&detail.message);\n                \
                       if false {\n                    \
                       dialogs.apply_amend_detail(&tip, true, \"m\");\n                \
                       }\n            }\n        });\n    };\n";
    assert!(
        !menu_applies_the_detail_from_the_read(dead_inside),
        "a call nested inside the arm is not a call the arm makes"
    );

    // Present in the file, but in another handler entirely.
    let elsewhere = "    let on_amend = move |_| {\n        \
                     if let Ok(detail) = fetch_commit_detail(&tip).await {\n            \
                     log(&detail.message);\n        }\n    };\n\n    \
                     let something_else = move || {\n        \
                     dialogs.apply_amend_detail(&tip, true, \"m\");\n    };\n";
    assert!(!menu_applies_the_detail_from_the_read(elsewhere));

    // No subject at all reads as a failure, not a vacuous pass.
    assert!(!menu_applies_the_detail_from_the_read(
        "nothing to see here"
    ));

    // The guard-bypass census, both ways: the raw pair is the finding.
    assert!(!menu_writes_the_detail_only_through_the_guard(
        "dialogs.record_amend_detail(&tip, detail.on_remote);"
    ));
    assert!(!menu_writes_the_detail_only_through_the_guard(
        "dialogs.seed_amend_msg(&detail.message);"
    ));

    let recheck_live = "    let recheck = move |t: String| {\n        \
                        let detail = match fetch_commit_detail(&new_tip).await {\n            \
                        Ok(d) => d,\n            Err(e) => return,\n        };\n        \
                        dialogs.record_amend_detail(&new_tip, detail.on_remote);\n        \
                        set(stale(Recheck::Retargeted { new_tip }));\n    };\n";
    assert!(recheck_records_detail_before_it_re_enables(recheck_live));

    // Recorded after the confirm button is back on — too late to gate it.
    let recheck_late = "    let recheck = move |t: String| {\n        \
                        let detail = match fetch_commit_detail(&new_tip).await {\n            \
                        Ok(d) => d,\n            Err(e) => return,\n        };\n        \
                        set(stale(Recheck::Retargeted { new_tip }));\n        \
                        dialogs.record_amend_detail(&new_tip, detail.on_remote);\n    };\n";
    assert!(!recheck_records_detail_before_it_re_enables(recheck_late));

    // Dropped entirely.
    let recheck_none = "    let recheck = move |t: String| {\n        \
                        let detail = match fetch_commit_detail(&new_tip).await {\n            \
                        Ok(d) => d,\n            Err(e) => return,\n        };\n        \
                        set(stale(Recheck::Retargeted { new_tip }));\n    };\n";
    assert!(!recheck_records_detail_before_it_re_enables(recheck_none));

    assert!(!recheck_records_detail_before_it_re_enables("nothing here"));
}

/// The statement-level scanner the strengthened census rests on, answered both
/// ways.
///
/// It is the whole difference between "the block mentions this call" and "the
/// block makes this call", so a version that ignored depth would quietly return
/// the census to the one that let `if false { .. }` through.
#[test]
fn the_statement_level_scanner_ignores_calls_nested_inside_the_block() {
    let body = "    plain();\n    if false {\n        buried();\n    }\n    match x {\n        \
                _ => armed(),\n    }\n    tail();\n";
    assert!(at_statement_level(body, "plain("));
    assert!(at_statement_level(body, "tail("));
    assert!(
        !at_statement_level(body, "buried("),
        "a call under `if false` is not a statement of this block"
    );
    assert!(!at_statement_level(body, "armed("));
    assert!(!at_statement_level(body, "absent("));

    // A closure body is nested too — the call is deferred, not made.
    assert!(!at_statement_level(
        "    spawn(|| { later(); });\n",
        "later("
    ));
}

/// The chokepoint census, answered both ways.
///
/// `signals.rs` is wasm-only, so nothing but this reads whether the guard it
/// holds is actually in front of the writes it authorises.
#[test]
fn the_chokepoint_census_can_spot_a_guard_that_guards_nothing() {
    let guarded = "    pub fn apply_amend_detail(&self, tip: &str, on_remote: bool, m: &str) {\n\
                   \x20       match detail_read_use(&self.amend_phase.get_untracked(), tip) {\n\
                   \x20           DetailUse::Apply => {\n\
                   \x20               self.record_amend_detail(tip, on_remote);\n\
                   \x20               self.seed_amend_msg(m);\n\
                   \x20           }\n            DetailUse::Discard => {}\n        }\n    }\n";
    assert!(signals_guard_the_detail_before_applying_it(guarded));

    // Consulted, then ignored — the writes run either way.
    let advisory = "    pub fn apply_amend_detail(&self, tip: &str, on_remote: bool, m: &str) {\n\
                    \x20       let _ = detail_read_use(&self.amend_phase.get_untracked(), tip);\n\
                    \x20       self.record_amend_detail(tip, on_remote);\n\
                    \x20       self.seed_amend_msg(m);\n    }\n";
    assert!(!signals_guard_the_detail_before_applying_it(advisory));

    // Guarded, but by something that is not the currency check.
    let wrong_guard = "    pub fn apply_amend_detail(&self, tip: &str, on_remote: bool, m: &str) \
                       {\n        if !m.is_empty() {\n            \
                       self.record_amend_detail(tip, on_remote);\n            \
                       self.seed_amend_msg(m);\n        }\n    }\n";
    assert!(!signals_guard_the_detail_before_applying_it(wrong_guard));

    // Only half of it behind the guard: the seed still escapes.
    let half = "    pub fn apply_amend_detail(&self, tip: &str, on_remote: bool, m: &str) {\n\
                \x20       if let DetailUse::Apply = detail_read_use(&self.phase(), tip) {\n\
                \x20           self.record_amend_detail(tip, on_remote);\n        }\n        \
                self.seed_amend_msg(m);\n    }\n";
    assert!(!signals_guard_the_detail_before_applying_it(half));

    // The method itself gone reads as a failed census, not a vacuous pass.
    assert!(!signals_guard_the_detail_before_applying_it("nothing here"));
}

/// The window this issue's gate is useless in: opening amend mode is
/// synchronous, the read that tells the gate whether the commit is published
/// is not.
///
/// Between the two, `PreflightKnowledge` answers `Unknown` and
/// `amend_preflight` maps `Unknown` to *send*. So for as long as
/// `GET /api/commit/{tip}` takes, a press of the green Amend button would POST
/// a rewrite of published history with no ceremony shown — not the documented
/// failed-read gap, but the ordinary first moments of every amend. The fix is
/// an explicit hold, and both halves of it live in a file `cargo test
/// --workspace` never compiles, so this census is the only thing that checks
/// them.
#[test]
fn the_press_is_held_until_the_publication_answer_lands() {
    assert!(
        menu_holds_the_press_until_the_read_answers(MENU),
        "menu.rs's `on_amend` no longer calls `dialogs.begin_publication_read(..)` \
         before it spawns the detail read. Without that hold the confirm button is \
         live while the pre-flight has nothing to read, and `amend_preflight` sends \
         on an unread detail — an amend of pushed history goes out with no warning \
         at all if the user presses inside that window."
    );
    assert!(
        menu_releases_the_press_on_either_outcome(MENU),
        "menu.rs's spawned read no longer releases the hold outside its `Ok` arm. A \
         hold that only a successful read clears leaves the confirm button inert \
         forever whenever `GET /api/commit/{{id}}` fails, which makes amend \
         unreachable through the UI."
    );
    assert!(
        DIALOG_SIGNALS.contains("fn finish_publication_read("),
        "the release half of the hold is gone from Dialogs, so `menu.rs` is calling \
         something that no longer decides anything"
    );
}

/// Both answers for the hold census, and for its mirror.
#[test]
fn the_hold_census_can_spot_a_hold_that_is_too_late_or_never_lifted() {
    let held = "    let on_amend = move |_| {\n        \
                dialogs.open(Dialog::Commit);\n        \
                dialogs.begin_publication_read(&tip);\n        \
                spawn_local(async move {\n            \
                if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                dialogs.record_amend_detail(&tip, detail.on_remote);\n            }\n            \
                dialogs.finish_publication_read(&tip);\n        });\n    };\n";
    assert!(menu_holds_the_press_until_the_read_answers(held));
    assert!(menu_releases_the_press_on_either_outcome(held));

    // The shape that ships the bug: the hold is entered inside the async
    // block, i.e. after the window it was supposed to close has opened.
    let late = "    let on_amend = move |_| {\n        \
                dialogs.open(Dialog::Commit);\n        \
                spawn_local(async move {\n            \
                dialogs.begin_publication_read(&tip);\n            \
                if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                dialogs.record_amend_detail(&tip, detail.on_remote);\n            }\n            \
                dialogs.finish_publication_read(&tip);\n        });\n    };\n";
    assert!(!menu_holds_the_press_until_the_read_answers(late));

    // No hold at all.
    let unheld = "    let on_amend = move |_| {\n        \
                  dialogs.open(Dialog::Commit);\n        \
                  spawn_local(async move {\n            \
                  if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                  dialogs.record_amend_detail(&tip, detail.on_remote);\n            }\n        \
                  });\n    };\n";
    assert!(!menu_holds_the_press_until_the_read_answers(unheld));

    // Released only when the read succeeds — the inert-forever failure.
    let ok_only = "    let on_amend = move |_| {\n        \
                   dialogs.begin_publication_read(&tip);\n        \
                   spawn_local(async move {\n            \
                   if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                   dialogs.record_amend_detail(&tip, detail.on_remote);\n                \
                   dialogs.finish_publication_read(&tip);\n            }\n        \
                   });\n    };\n";
    assert!(menu_holds_the_press_until_the_read_answers(ok_only));
    assert!(!menu_releases_the_press_on_either_outcome(ok_only));

    // Never released at all.
    let never = "    let on_amend = move |_| {\n        \
                 dialogs.begin_publication_read(&tip);\n        \
                 spawn_local(async move {\n            \
                 if let Ok(detail) = fetch_commit_detail(&tip).await {\n                \
                 dialogs.record_amend_detail(&tip, detail.on_remote);\n            }\n        \
                 });\n    };\n";
    assert!(!menu_releases_the_press_on_either_outcome(never));

    assert!(!menu_holds_the_press_until_the_read_answers("nothing here"));
    assert!(!menu_releases_the_press_on_either_outcome("nothing here"));
}

/// The scanner every live-path census above is built on, answered both ways.
///
/// Worth its own test because a `braced_body` that returned the rest of the
/// file on a nested block would silently turn all four of them back into the
/// whole-file checks they replaced.
#[test]
fn the_body_scanner_stops_at_the_matching_brace() {
    let src = "let a = || {\n    inner { nested }\n    keep;\n};\nlet b = || {\n    other;\n};\n";
    let a = braced_body(src, "let a = ").expect("a has a body");
    assert!(a.contains("keep;"), "{a:?}");
    assert!(a.contains("nested"), "nesting must not end the body early");
    assert!(
        !a.contains("other;"),
        "the body ran past its own closing brace into the next item: {a:?}"
    );

    let b = braced_body(src, "let b = ").expect("b has a body");
    assert!(b.contains("other;"));
    assert!(!b.contains("keep;"));

    // Fail-closed on every degenerate input.
    assert!(braced_body(src, "let missing = ").is_none());
    assert!(braced_body("let a = no_block_here;", "let a = ").is_none());
    assert!(braced_body("let a = || { unterminated", "let a = ").is_none());
}

/// Both answers, against fixture source — a predicate that always returned true
/// would satisfy the tripwire above perfectly.
#[test]
fn the_gate_ordering_census_can_spot_a_gate_behind_the_send() {
    let gated = "match amend_preflight(target, &k) { … }; amend_commit_request(&m, &t).await";
    assert!(gates_before_it_sends(gated));

    // The shape that would ship the bug: the request goes out and the gate is
    // consulted afterwards (or in a later, unrelated branch).
    let ungated = "amend_commit_request(&m, &t).await; amend_preflight(target, &k)";
    assert!(!gates_before_it_sends(ungated));

    // No gate at all, and no subject at all, both read as failures.
    assert!(!gates_before_it_sends("amend_commit_request(&m, &t).await"));
    assert!(!gates_before_it_sends("nothing to see here"));
}

/// The way *past* the banner has to record the agreement before it re-submits.
///
/// The gate tripwire above only proves the banner gets raised; this one proves
/// the user can get through it. `submit_amend` re-reads `dialogs.amend_knowledge()`
/// synchronously to decide the pre-flight, so a `confirm_published` that
/// re-submitted first and recorded second would answer `Preflight::Confirm`
/// again and re-enter `AwaitingPublishedConfirm` — the banner's own button
/// permanently inert, and no amend of published history reachable through the
/// UI at all. The closure lives in a file `cargo test --workspace` never
/// compiles, so both the swap and the deletion are green-suite failures.
#[test]
fn the_way_past_the_banner_records_the_agreement_before_it_resubmits() {
    assert!(
        records_consent_before_it_resubmits(COMMIT_MODAL),
        "`confirm_published` in dialogs/commit.rs no longer calls \
         `dialogs.confirm_amend_target(..)` before `submit_amend(..)`. \
         `submit_amend` re-reads the knowledge to decide the pre-flight, so this \
         order is the only thing that lets a confirmed amend of published history \
         ever be sent — reversed or dropped, the confirm button loops on its own \
         banner forever."
    );
    assert!(
        COMMIT_MODAL.contains("on:click=move |_| confirm_published(target.clone())"),
        "the ceremony's confirm button is no longer wired to `confirm_published`, \
         so the ordering above guards a closure nothing presses"
    );
}

/// Both answers, against fixture source — and the scoping, which is the whole
/// reason this reads a closure body rather than the file.
#[test]
fn the_consent_ordering_census_can_spot_a_record_behind_the_resubmit() {
    let fixed = "    let confirm_published = move |target: AmendTarget| {\n        \
                 dialogs.confirm_amend_target(target.expected_tip());\n        \
                 submit_amend(target);\n    };\n";
    assert!(records_consent_before_it_resubmits(fixed));

    // The shape that would ship the bug: re-submit first, record afterwards.
    let swapped = "    let confirm_published = move |target: AmendTarget| {\n        \
                   submit_amend(target.clone());\n        \
                   dialogs.confirm_amend_target(target.expected_tip());\n    };\n";
    assert!(!records_consent_before_it_resubmits(swapped));

    // Dropping the record entirely is a failure, not a vacuous pass.
    let unrecorded = "    let confirm_published = move |target: AmendTarget| {\n        \
                      submit_amend(target);\n    };\n";
    assert!(!records_consent_before_it_resubmits(unrecorded));

    // The paired negative that justifies the scoping: the record is present in
    // the file, but in a *different* closure. A whole-file `find` would compare
    // that call against the first `submit_amend(` and pass.
    let elsewhere = "    let confirm_published = move |target: AmendTarget| {\n        \
                     submit_amend(target);\n    };\n\n    \
                     let something_else = move || {\n        \
                     dialogs.confirm_amend_target(tip);\n    };\n";
    assert!(!records_consent_before_it_resubmits(elsewhere));

    // …and the mirror of it: an earlier closure that records must not be
    // credited to `confirm_published`.
    let recorded_earlier = "    let something_else = move || {\n        \
                            dialogs.confirm_amend_target(tip);\n    };\n\n    \
                            let confirm_published = move |target: AmendTarget| {\n        \
                            submit_amend(target);\n    };\n";
    assert!(!records_consent_before_it_resubmits(recorded_earlier));

    // No subject at all is not a pass.
    assert!(!records_consent_before_it_resubmits("nothing to see here"));
}

/// A fresh dialog must not inherit the previous amend's consent.
///
/// `PreflightKnowledge` is tip-scoped, so a leak needs *two* mistakes — a
/// forgotten reset and a second amend of the same commit id — but the second is
/// ordinary (open the amend dialog, cancel, open it again on the same tip), and
/// the reset lives in a wasm-only file no test executes.
#[test]
fn opening_a_dialog_forgets_what_the_last_amend_agreed_to() {
    assert!(
        reset_amend_clears_preflight(DIALOG_SIGNALS),
        "Dialogs::reset_amend no longer clears the pre-flight knowledge. It runs \
         on every `Dialogs::open`, which is what stops one amend's agreement to \
         rewrite published history being spent on the next one."
    );
}

#[test]
fn the_reset_census_reads_only_the_function_it_names() {
    let clears = "fn reset_amend(&self) {\n        self.amend_msg.set(String::new());\n        \
                  self.amend_preflight.set_value(PreflightKnowledge::default());\n    }\n";
    assert!(reset_amend_clears_preflight(clears));

    // The paired negative that matters: the field is mentioned in the file, but
    // in a *different* method, so a whole-file `contains` would pass here.
    let elsewhere = "fn reset_amend(&self) {\n        self.amend_msg.set(String::new());\n    }\n\
                     \n    pub fn amend_knowledge(&self) -> PreflightKnowledge {\n        \
                     self.amend_preflight.with_value(|k| k.clone())\n    }\n";
    assert!(!reset_amend_clears_preflight(elsewhere));

    // And a source with no such function is not a pass.
    assert!(!reset_amend_clears_preflight("fn something_else() {}"));
}

/// The stylesheet already dresses the focused-but-disabled item, so making these
/// focusable needs no CSS change — but that only holds while the rule is there.
#[test]
fn a_focused_disabled_context_menu_item_has_a_declared_appearance() {
    let rules = stylesheet();
    assert!(
        !rules_for_selector(&rules, ".ctx-item.disabled:focus-visible").is_empty(),
        "`.ctx-item.disabled:focus-visible` has gone from styles.css. These items \
         are deliberately focusable (see menu.rs's module docs), so a keyboard user \
         will land on one and the stylesheet has to say what that looks like."
    );
}
