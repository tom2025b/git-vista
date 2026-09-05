//! The canvas keyboard map and the roving-row key map, plus the source
//! censuses binding `gestures.rs` (and, for the roving map, the staging view)
//! to them.
//!
//! #653. `gestures.rs` is `#[cfg(target_arch = "wasm32")]`: before this,
//! every shortcut the app has — and every one of the three ordering rules
//! that make them correct — was decided in a module `cargo test --workspace`
//! compiles to nothing (ADR 0115).

use super::*;

use crate::features::a11y::focus::FocusMove;

/// No modifier held — the ordinary press.
const PLAIN: KeyMods = KeyMods {
    shift: false,
    ctrl: false,
    meta: false,
    alt: false,
};

const SHIFT: KeyMods = KeyMods {
    shift: true,
    ctrl: false,
    meta: false,
    alt: false,
};

/// The action for a plain press with nothing else going on.
fn plain(key: &str) -> Option<CanvasKey> {
    canvas_key_action(key, PLAIN, false, false).map(|a| a.action)
}

// ---- the map itself --------------------------------------------------------

#[test]
fn the_canvas_shortcuts_map_the_way_the_help_copy_says() {
    assert_eq!(plain("+"), Some(CanvasKey::ZoomIn));
    assert_eq!(plain("="), Some(CanvasKey::ZoomIn));
    assert_eq!(plain("-"), Some(CanvasKey::ZoomOut));
    assert_eq!(plain("_"), Some(CanvasKey::ZoomOut));
    assert_eq!(plain("0"), Some(CanvasKey::ResetView));
    assert_eq!(plain("Home"), Some(CanvasKey::ResetView));
    assert_eq!(plain("PageDown"), Some(CanvasKey::PageDown));
    assert_eq!(plain(" "), Some(CanvasKey::PageDown));
    assert_eq!(plain("PageUp"), Some(CanvasKey::PageUp));
    assert_eq!(plain("r"), Some(CanvasKey::Reload));
    assert_eq!(plain("R"), Some(CanvasKey::Reload));
    assert_eq!(plain("Escape"), Some(CanvasKey::DismissTopOverlay));
}

#[test]
fn an_unbound_key_does_nothing() {
    for key in ["a", "F5", "ArrowDown", "Tab", "", "1"] {
        assert_eq!(plain(key), None, "{key:?} is not a canvas shortcut");
    }
}

// ---- rule 3: Shift selects, it does not bail out ---------------------------

#[test]
fn shift_space_pages_back_and_space_alone_pages_forward() {
    assert_eq!(
        canvas_key_action(" ", SHIFT, false, false).map(|a| a.action),
        Some(CanvasKey::PageUp),
        "Shift-Space pages back, matching every browser and reader — Space \
         alone is \"more\", Shift-Space is \"back\""
    );
    assert_eq!(plain(" "), Some(CanvasKey::PageDown));
}

#[test]
fn shift_is_not_part_of_the_modifier_bail_out() {
    // If Shift were folded in with Ctrl/Cmd/Alt, Shift-Space would be
    // swallowed and nothing about the remaining Space binding would look
    // wrong — the deletion would be silent.
    assert!(
        !SHIFT.bails_out(),
        "Shift is a selector for these bindings, not a reason to ignore the \
         press"
    );
    assert!(canvas_key_action(" ", SHIFT, false, false).is_some());
}

#[test]
fn ctrl_cmd_and_alt_leave_the_key_to_the_browser() {
    for mods in [
        KeyMods {
            ctrl: true,
            ..PLAIN
        },
        KeyMods {
            meta: true,
            ..PLAIN
        },
        KeyMods { alt: true, ..PLAIN },
    ] {
        assert_eq!(
            canvas_key_action("r", mods, false, false),
            None,
            "Cmd/Ctrl-R is the browser's reload, not this app's refresh: {mods:?}"
        );
    }
}

// ---- rule 1: Escape is decided above the guard -----------------------------

#[test]
fn escape_still_dismisses_while_a_text_field_has_focus() {
    assert_eq!(
        canvas_key_action("Escape", PLAIN, true, false).map(|a| a.action),
        Some(CanvasKey::DismissTopOverlay),
        "backing out of an overlay has to work while the cursor is in that \
         overlay's own commit or URL box — a typing guard placed above this \
         makes the dialog un-escapable"
    );
    assert_eq!(
        plain("r"),
        Some(CanvasKey::Reload),
        "sanity: the same key with typing=false is bound"
    );
    assert_eq!(
        canvas_key_action("r", PLAIN, true, false),
        None,
        "typing an \"r\" into a commit message must not reload the repository"
    );
}

#[test]
fn escape_still_dismisses_with_a_modifier_held() {
    assert_eq!(
        canvas_key_action("Escape", SHIFT, false, false).map(|a| a.action),
        Some(CanvasKey::DismissTopOverlay)
    );
    assert_eq!(
        canvas_key_action(
            "Escape",
            KeyMods {
                ctrl: true,
                ..PLAIN
            },
            false,
            false,
        )
        .map(|a| a.action),
        Some(CanvasKey::DismissTopOverlay)
    );
}

// ---- rule 2: a consumed Escape is not dismissed twice ----------------------

#[test]
fn an_escape_a_closer_handler_already_consumed_is_left_alone() {
    assert_eq!(
        canvas_key_action("Escape", PLAIN, false, true),
        None,
        "the diff's hunk navigation calls prevent_default when Escape \
         disengages it; that press must not ALSO dismiss the overlay the \
         reader is still inside"
    );
}

#[test]
fn default_prevented_only_gates_escape() {
    // The flag is Escape's alone. Applying it to every key would make the
    // canvas go deaf after any unrelated handler suppressed a default.
    assert_eq!(
        canvas_key_action("r", PLAIN, false, true).map(|a| a.action),
        Some(CanvasKey::Reload)
    );
    assert_eq!(
        canvas_key_action(" ", PLAIN, false, true).map(|a| a.action),
        Some(CanvasKey::PageDown)
    );
}

// ---- prevent_default does not line up with the action ----------------------

#[test]
fn the_scrolling_keys_suppress_their_default_and_the_others_do_not() {
    let prevents = |key: &str, mods: KeyMods| {
        canvas_key_action(key, mods, false, false)
            .expect("bound key")
            .prevent_default
    };
    for key in [" ", "PageDown", "PageUp", "Home"] {
        assert!(
            prevents(key, PLAIN),
            "{key:?} scrolls the document by default and must be suppressed"
        );
    }
    assert!(prevents(" ", SHIFT));
    for key in ["+", "=", "-", "_", "0", "r", "R", "Escape"] {
        assert!(
            !prevents(key, PLAIN),
            "{key:?} has no browser default worth taking"
        );
    }
}

#[test]
fn home_and_zero_reset_the_same_view_but_only_home_stops_the_browser() {
    let home = canvas_key_action("Home", PLAIN, false, false).expect("bound");
    let zero = canvas_key_action("0", PLAIN, false, false).expect("bound");
    assert_eq!(home.action, zero.action);
    assert_ne!(
        home.prevent_default, zero.prevent_default,
        "the two agree on the action and disagree on the suppression — which \
         is exactly why `CanvasKeyAction` carries both rather than letting the \
         caller infer one from the other"
    );
    assert!(home.prevent_default);
}

// ---- the roving-row map ----------------------------------------------------

#[test]
fn the_roving_row_keys_map_to_the_focus_moves_they_are_named_for() {
    assert_eq!(
        roving_row_key("ArrowDown"),
        Some(RowKey::Move(FocusMove::Next))
    );
    assert_eq!(
        roving_row_key("ArrowUp"),
        Some(RowKey::Move(FocusMove::Prev))
    );
    assert_eq!(roving_row_key("Home"), Some(RowKey::Move(FocusMove::First)));
    assert_eq!(roving_row_key("End"), Some(RowKey::Move(FocusMove::Last)));
    assert_eq!(roving_row_key("Enter"), Some(RowKey::Activate));
    assert_eq!(roving_row_key(" "), Some(RowKey::Activate));
    assert_eq!(roving_row_key("Escape"), Some(RowKey::Dismiss));
    assert_eq!(roving_row_key("PageDown"), None);
    assert_eq!(roving_row_key("r"), None);
}

#[test]
fn space_and_enter_are_equivalent_on_a_roving_row() {
    // Keyboard/VoiceOver equivalence (#215 Task 1, #65): whatever a tap does,
    // Space or Enter on the focused row does too. Binding only one of them
    // leaves VoiceOver users without the other.
    assert_eq!(roving_row_key(" "), roving_row_key("Enter"));
}

#[test]
fn every_arrow_direction_has_a_distinct_move() {
    let moves: Vec<RowKey> = ["ArrowDown", "ArrowUp", "Home", "End"]
        .iter()
        .map(|k| roving_row_key(k).expect("bound"))
        .collect();
    let mut seen = moves.clone();
    seen.dedup();
    assert_eq!(
        seen.len(),
        moves.len(),
        "two navigation keys resolved to the same move: {moves:?} — the \
         omission shape that leaves Home and ArrowUp both meaning Prev"
    );
}

// ---- the seam --------------------------------------------------------------
//
// Everything above proves the maps. These prove the wasm-only handlers still
// ask them; both files are `#[cfg(target_arch = "wasm32")]`, so a change that
// re-derives a map inline leaves every test above green while the keyboard
// stops behaving that way.

const GESTURES: &str = include_str!("../../../gestures.rs");
const STAGING_VIEW: &str = include_str!("../../diff/staging_view.rs");

#[test]
fn the_window_key_listener_asks_core_what_a_key_press_means() {
    assert!(
        GESTURES.contains("canvas_key_action("),
        "gestures.rs's window keydown listener no longer calls \
         `canvas_key_action`. The whole shortcut map would be back inside a \
         wasm-only module, where no test above can execute a line of it"
    );
    assert!(
        !GESTURES.contains("ev.shift_key() {"),
        "gestures.rs branches on Shift inside its own listener again. \
         Shift-Space paging back is the rule most easily deleted by folding \
         Shift into the modifier bail-out, and `canvas_key_action` is where \
         that is pinned"
    );
    assert!(
        !GESTURES.contains("\"PageDown\""),
        "gestures.rs matches key names again rather than acting on a decided \
         `CanvasKey`"
    );
}

#[test]
fn the_window_key_listener_still_yields_a_consumed_escape() {
    assert!(
        GESTURES.contains("ev.default_prevented()"),
        "gestures.rs no longer passes `default_prevented` to the key map. \
         Without it an Escape the diff's hunk navigation already consumed \
         would ALSO dismiss the overlay the reader is inside — and \
         stop_propagation cannot prevent that, since this listener and \
         Leptos's delegated handlers share the window target"
    );
}

#[test]
fn the_node_keydown_handler_asks_core_what_a_row_key_means() {
    assert!(
        GESTURES.contains("roving_row_key("),
        "gestures.rs's per-row keydown handler no longer calls \
         `roving_row_key`"
    );
    assert!(
        !GESTURES.contains("FocusMove::First"),
        "gestures.rs names a `FocusMove` variant again, which means it is \
         re-deriving the arrow/Home/End map instead of asking for it — the \
         staging view drives the same model with the same keys"
    );
}

#[test]
fn the_staging_view_asks_core_what_a_row_key_means() {
    assert!(
        STAGING_VIEW.contains("roving_row_key("),
        "features/diff/staging_view.rs no longer calls `roving_row_key`. It \
         is wasm-only too, so its copy of the map was the second unwatched \
         one — that duplication is the whole reason the map moved to core"
    );
    assert!(
        !STAGING_VIEW.contains("FocusMove::Prev"),
        "features/diff/staging_view.rs re-derives the focus-move map again"
    );
}
