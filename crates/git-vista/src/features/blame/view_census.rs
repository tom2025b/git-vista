//! A source census over `view.rs` (M5.33, #86) — the blame panel's wasm-only
//! DOM wiring, which `cargo test` cannot execute at all.
//!
//! ADR 0115's rule is that decision logic must not drift into wasm-only
//! modules where no test runner can reach it. `view.rs` is written to hold
//! *no* decisions — every one is delegated to `core.rs`
//! ([`BlameSelection`](super::core::BlameSelection),
//! [`path_state_message`](super::core::path_state_message)) or to house
//! machinery already host-tested elsewhere (`offer_for`, `roving_row_key`,
//! `drag_range`). That claim is exactly the kind that rots silently: someone
//! inlines "just this one" comparison and nothing goes red, because nothing
//! natively compiled ever reads the file.
//!
//! So this reads its bytes and pins the claim. What each assertion below
//! catches is a real regression, named in its own message — not a
//! non-vacuous-looking wrapper around "the file is not empty".
//!
//! **What this cannot prove**, stated plainly rather than left implied: that
//! the browser actually moves focus onto the row it is told to, that the 44px
//! targets are reachable with a real finger, or that the drag gesture feels
//! like anything. Only `ci/browser/tests/blame-touch.spec.mjs` can speak to
//! those, and it does — this census is the half that runs on every
//! `cargo test`, not a substitute for the half that needs a browser.

const VIEW_SRC: &str = include_str!("view.rs");

/// The two-targets rule `features::diff::selection`'s module doc argues for
/// staging, applied here: a tap must never mean both "open this commit" and
/// "select this line for a comparison". Collapsing them into one element is
/// the single most likely "simplification" a later reader would make.
#[test]
fn the_select_target_and_the_row_body_are_two_separate_elements() {
    assert!(
        VIEW_SRC.contains("class=\"blame-select\""),
        "the range-select tap target is gone — selection has probably been \
         folded onto the row body, which makes one tap mean two things (#65, \
         and see `features::diff::selection`'s module doc)"
    );
    assert!(
        VIEW_SRC.contains("class=\"blame-row\""),
        "the row body is gone — the roving keyboard/tap stop went with it"
    );
    // The select target must be its own <button>, not a span the row's own
    // click handler happens to sit under: a native button is what gives
    // VoiceOver an activatable control with a spoken pressed state.
    let select_pos = VIEW_SRC
        .find("class=\"blame-select\"")
        .expect("checked above");
    let button_before = VIEW_SRC[..select_pos].rfind("<button");
    let span_before = VIEW_SRC[..select_pos].rfind("<span");
    assert!(
        button_before > span_before,
        "`.blame-select` is no longer inside a native <button> — a span with a \
         click handler is not an activatable control for VoiceOver or Switch \
         Control"
    );
}

/// Whether the selection is *spoken*, not only painted. Without
/// `aria-pressed` a screen-reader user can move through every row and never
/// learn which ones are selected — the failure mode "touch selection is
/// accessible" (#86's criterion) is specifically about.
#[test]
fn the_select_target_speaks_its_state_and_its_purpose() {
    assert!(
        VIEW_SRC.contains("aria-pressed"),
        "`.blame-select` no longer carries aria-pressed — selection state is \
         painted but not spoken"
    );
    assert!(
        VIEW_SRC.contains("aria-label=format!(\"Select line"),
        "the select target's aria-label is gone or reworded — every row's \
         control would speak the same anonymous name"
    );
    assert!(
        VIEW_SRC.contains("aria-label=label"),
        "the row body's aria-label is gone — the row would speak only its \
         visible text fragments, in whatever order the DOM happens to hold them"
    );
}

/// Drag-select needs all three pointer phases. Losing `pointerenter`
/// specifically is the quiet one: tapping still selects one row, so the
/// feature looks alive while "drag across several lines" — the whole touch
/// gesture — is gone.
#[test]
fn drag_select_is_wired_on_all_three_pointer_phases() {
    for handler in ["on:pointerdown", "on:pointerenter", "on:pointerup"] {
        assert!(
            VIEW_SRC.contains(handler),
            "{handler} is not wired on the blame select target — without all \
             three phases the drag gesture degrades to one-row taps, silently"
        );
    }
    // The pressed-button guard: `pointerenter` fires on plain hover too, so
    // without it a mouse sweeping across the panel would select rows nobody
    // asked for.
    assert!(
        VIEW_SRC.contains("ev.buttons() != 1"),
        "the pointerenter handler no longer checks that the primary pointer is \
         down — hovering would extend the selection"
    );
}

/// The keyboard half of the same gesture. `roving_row_key` is the shared
/// key→intent map (#653 pulled it out of two wasm-only copies for exactly
/// this reason); a hand-rolled `match ev.key()` here would be a third copy
/// no host test reads.
#[test]
fn keyboard_navigation_uses_the_shared_key_map_not_a_local_one() {
    assert!(
        VIEW_SRC.contains("roving_row_key(&ev.key())"),
        "the blame rows no longer route keys through the shared \
         `roving_row_key` — a local key match is a third copy of a map #653 \
         deliberately unified"
    );
    assert!(
        VIEW_SRC.contains("ev.shift_key()"),
        "shift-extend is gone — keyboard users can move between rows but can \
         no longer select a *range*, which a pointer drag still can (the \
         equivalence #210 set and #215 extended to selection)"
    );
}

/// Every decision stays in `core.rs` or in already-host-tested machinery.
/// These are the specific shapes a drifted decision would take here.
#[test]
fn no_decision_logic_has_drifted_into_the_wasm_only_view() {
    assert!(
        VIEW_SRC.contains("offer_for(anchor.as_deref(), &this)"),
        "the comparison offer is no longer computed by `offer_for` — a local \
         anchor comparison here is the exact defect \
         `compare_offer_suite::the_anchor_is_the_base_and_the_menus_own_commit_is_the_target` \
         exists to catch, moved somewhere that suite cannot see"
    );
    assert!(
        VIEW_SRC.contains("path_state_message(&page.path_state)"),
        "the binary/absent banner text is no longer built by \
         `core::path_state_message` — the three distinct absent states would \
         be worded somewhere no host test reads them"
    );
    assert!(
        VIEW_SRC.contains("rename_limit_banner(&page.rename_limit_hits)"),
        "the rename-limit banner is no longer built by \
         `core::rename_limit_banner` — the one place that says a history may \
         be missing a rename"
    );
    // The range arithmetic itself: `BlameSelection` delegates to
    // `drag_range`, which is host-tested for order-independence (a drag that
    // runs *upward* is the case that breaks under a naive `start..=current`).
    // A second implementation here would be untested by construction.
    //
    // Checked as three specific shapes rather than a blanket ban on `.min(`
    // — this file legitimately calls `len().min(7)` to shorten an object id,
    // and a census that fires on that teaches its reader to stop believing
    // it. (Caught by this very test on its first run, against itself.)
    assert!(
        !VIEW_SRC.contains("drag_range("),
        "the view calls `drag_range` directly — the extend gesture must go \
         through `BlameSelection::extend_to` so the anchor state has exactly \
         one owner"
    );
    assert!(
        VIEW_SRC.contains("s.extend_to(idx)") || VIEW_SRC.contains("s.extend_to(next)"),
        "the drag/shift gestures no longer delegate extension to \
         `BlameSelection::extend_to`"
    );
    for swap in [
        "idx.min(",
        "idx.max(",
        "if start <= current",
        "if anchor <= idx",
    ] {
        assert!(
            !VIEW_SRC.contains(swap),
            "a hand-rolled range ordering (`{swap}`) has appeared in the view — \
             `drag_range` already does this and is the only copy with a test \
             for the upward-drag case"
        );
    }
}

/// The index-space bug the substring checks could not see (#86 review).
///
/// `BlameSelection` stores ROW INDICES; `BlameRange::start_line`/`end_line`
/// are 1-based SOURCE LINE numbers. The first version of the toolbar searched
/// the ranges for one whose line interval contained the selected row index —
/// a category error that made row 0 offer no comparison at all (no line is
/// numbered 0) and made later rows resolve to whichever earlier range
/// happened to span that small integer. Every substring assertion in this
/// file passed straight over it, which is the honest limit of a source
/// census: it can see that `offer_for` is called, not that it is called with
/// the right commit.
///
/// So this pins the one composition that carries the whole mapping.
#[test]
fn the_toolbar_indexes_the_range_slice_rather_than_searching_line_numbers() {
    assert!(
        VIEW_SRC.contains("ranges_for_toolbar\n            .get(*range.start())")
            || VIEW_SRC.contains("ranges_for_toolbar.get(*range.start())"),
        "the compare toolbar must index the range slice by the selected ROW, not \
         search it by line number — the two are different coordinate spaces and \
         mixing them silently opens a comparison on the wrong commit"
    );
    assert!(
        !VIEW_SRC.contains("r.start_line <= start && start <= r.end_line"),
        "the line-interval search is back — that is the round's index/line \
         category error, which no other assertion in this file can see"
    );
}

/// The tap that undid itself (#86 review). `pointerdown` committed a
/// selection and the click that inevitably follows toggled it straight back
/// off, so a plain tap left the control looking dead. The browser spec could
/// not see it because it dispatched pointer events directly and never a real
/// click.
#[test]
fn a_tap_is_decided_in_one_place_not_two() {
    let down = VIEW_SRC
        .split("let on_select_pointer_down")
        .nth(1)
        .and_then(|s| s.split("};").next())
        .expect("the pointerdown handler exists");
    assert!(
        !down.contains("s.start("),
        "pointerdown must only ANCHOR a possible drag, never commit a selection \
         — committing here and toggling again on the click that follows is what \
         made a tap select and instantly deselect"
    );
    assert!(
        VIEW_SRC.contains("if dragged.get_value()"),
        "the click handler must be able to tell a tap from the click that merely \
         ends a drag, or ending a drag re-decides what the drag selected"
    );
}

/// The two criteria "blame ranges map to commits and comparisons" reduce to
/// on this surface: a row opens the existing commit-detail panel, and the
/// toolbar opens the existing comparison viewer. Both must go through the
/// shell rather than growing a private overlay.
#[test]
fn ranges_reach_commits_and_comparisons_through_the_existing_surfaces() {
    assert!(
        VIEW_SRC.contains("shell.open_detail("),
        "a blame range no longer opens the commit detail panel — the \
         \"ranges map to commits\" half of #86 has no route"
    );
    assert!(
        VIEW_SRC.contains("shell.open_viewer(ViewerDoc::Spec {"),
        "the compare action no longer opens the M4.27 comparison viewer — the \
         \"ranges map to comparisons\" half of #86 has no route"
    );
    assert!(
        VIEW_SRC.contains("CommitOid::new("),
        "commit ids reach `DiffSpec` without going through `CommitOid`'s \
         validation — the same guard `menu::compare_items` applies before \
         opening a comparison"
    );
}

/// A refused path (binary, or any of the three absent states) must not render
/// a row list at all. Rendering an empty list under a banner reads as "this
/// file has no lines", which is a different and wrong statement.
#[test]
fn a_refused_path_returns_before_any_row_list_is_built() {
    let guard = VIEW_SRC
        .find("if !matches!(page.path_state, PathState::Readable)")
        .expect(
            "the non-Readable early return is gone — a binary or absent path \
             would fall through to the row list",
        );
    let rows = VIEW_SRC
        .find("let rows: Vec<View>")
        .expect("the row list is gone entirely");
    assert!(
        guard < rows,
        "the non-Readable guard now runs *after* the row list is built — the \
         early return exists precisely so it does not"
    );
}
