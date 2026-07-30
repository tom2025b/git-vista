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
/// selector, as of M1.12.
///
/// The `bool` is "does the stylesheet guarantee 44 px on **both** axes". Every entry is
/// `false`: `styles.css` contains no `min-height`, `min-width`, or absolute `height` /
/// `width` on any interactive control at all, so this is a documented gap, not a passing
/// grade. It is written down rather than left implicit for two reasons — a new
/// interactive control cannot be added without appearing here, and the moment somebody
/// fixes one, this table forces the fix to be recorded instead of being unnoticed.
///
/// Sizes are not fixable from this lane: `styles.css` is shared with concurrent work and
/// only the focus / reduced-motion rules are in scope here, and several of these
/// controls live in modules another lane owns.
const INTERACTIVE_CENSUS: &[(&str, bool)] = &[
    // Topbar buttons: padding .4rem/.9rem on a .85rem font, no minimum.
    (".refresh", false),
    // Commit-dot and stub hit circles. Sized in SVG user units by `render/`, not by
    // CSS at all — see `commit_dot_hit_target_is_thirty_pixels_at_default_zoom`.
    (".node-hit", false),
    // Context-menu rows: 8px/10px padding, `width: 100%` (an ancestor's answer).
    (".ctx-item", false),
    (".reset-view", false),
    // A bare utility class applied to graph text; carries no box of its own.
    (".clickable", false),
    (".detail-close", false),
    (".detail-parent", false),
    (".act-refresh", false),
    (".act-row", false),
    (".act-undo", false),
    ("button.detail-file", false),
    (".detail-expand", false),
    (".viewer-btn", false),
    (".scale-btn", false),
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
/// stale claim. Today every one is `false`, and this test says so out loud: **no
/// interactive control in this app is guaranteed by CSS to meet the 44x44 criterion.**
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
    for (selector, expected) in [(".big", true), (".rem", true), (".pct", false), (".small", false)]
    {
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
/// 2 * (NODE_RADIUS + 8) = 30 CSS pixels at the default zoom: 14 px short on both axes,
/// and only compliant from about 1.47x zoom upward. Recorded here, next to the census,
/// because "the graph's primary tap target is undersized at default zoom" is the single
/// most consequential #65 fact in this crate and it belongs in a test, not a comment.
#[test]
fn commit_dot_hit_target_is_thirty_pixels_at_default_zoom() {
    let radius = f64::from(crate::geometry::NODE_RADIUS);
    let side = node_hit_extent_px(radius, NODE_HIT_PADDING, 1.0);
    assert_eq!(side, 30.0);
    assert_eq!(
        TapTarget::square(side).verdict(),
        TargetVerdict::Undersized {
            short_by_x_px: 14.0,
            short_by_y_px: 14.0,
        }
    );
}

/// `core::NODE_HIT_PADDING` mirrors a literal in two wasm-only modules that a host test
/// cannot link. This is what keeps the mirror from rotting: change `+ 8` in either
/// render module and the arithmetic above becomes a lie, and this fails.
#[test]
fn node_hit_padding_still_matches_the_render_code() {
    let expected = format!("r=NODE_RADIUS + {}", NODE_HIT_PADDING as i32);
    for (name, src) in [("render/nodes.rs", RENDER_NODES), ("render/stubs.rs", RENDER_STUBS)] {
        assert!(
            src.contains(&expected),
            "{name} no longer draws its hit circle as `{expected}` — \
             `core::NODE_HIT_PADDING` and every tap-target number derived from it are \
             now wrong (#65)"
        );
    }
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
        .expect("the reduced-motion block does not apply to `*` — a per-selector opt-in \
                 is exactly the list someone forgets to extend");

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
        opening_tag("x <section class=\"graph\" aria-label=X> inner </section>", "<section"),
        Some("<section class=\"graph\" aria-label=X>")
    );
    assert_eq!(opening_tag("nothing here", "<section"), None);
    assert_eq!(opening_tag("<section unterminated", "<section"), None);
}
