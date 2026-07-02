//! Persisted UI preferences, stored in the browser's `localStorage`.
//!
//! Two boolean toggles live here — the icon style (Nerd Font glyphs vs the
//! plain-text fallback) and whether the per-node icons show. Both persist so a
//! device without a Nerd Font (an iPad, where only system fonts exist and a PUA
//! glyph renders as tofu) keeps its choice across reloads. Every write is
//! best-effort: private browsing can refuse `localStorage`, in which case the
//! toggle still works for the current session, it just won't be remembered.

/// localStorage key for the icon-style preference: "nerd" (glyphs) or "text".
const ICON_PREF_KEY: &str = "git-vista.icons";

/// Load the persisted icon preference. Defaults to Nerd Font glyphs; "text"
/// selects the plain-text fallback (crate::icons::TEXT_ICONS) for devices with
/// no Nerd Font installed — e.g. an iPad, where only system fonts exist and a
/// PUA glyph renders as tofu.
pub fn load_icon_pref() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(ICON_PREF_KEY).ok().flatten())
        .map_or(true, |v| v != "text")
}

/// Persist the icon preference. Best-effort: private browsing may refuse the
/// write, in which case the toggle still works for this session.
pub fn store_icon_pref(nerd: bool) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(ICON_PREF_KEY, if nerd { "nerd" } else { "text" });
    }
}

/// localStorage key for the per-node icons preference: "on" (default) or "off".
const NODE_ICONS_KEY: &str = "git-vista.node-icons";

/// Load the persisted "icons beside the commit dots" preference (default on).
pub fn load_node_icons_pref() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(NODE_ICONS_KEY).ok().flatten())
        .map_or(true, |v| v != "off")
}

/// Persist the per-node icons preference. Best-effort, like the icon style.
pub fn store_node_icons_pref(on: bool) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(NODE_ICONS_KEY, if on { "on" } else { "off" });
    }
}
