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
        .is_none_or(|v| v != "text")
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
        .is_none_or(|v| v != "off")
}

/// Persist the per-node icons preference. Best-effort, like the icon style.
pub fn store_node_icons_pref(on: bool) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(NODE_ICONS_KEY, if on { "on" } else { "off" });
    }
}

/// localStorage key for the one Fetch/Pull the client is currently
/// tracking across reloads (#232, M2.20f) — see
/// `features::operations::core::InFlightRemoteOp`. At most one entry: the
/// menu (`menu.rs`'s `remote_op_running` gate on `fetch_item`/`pull_item`)
/// renders Fetch and Pull as disabled, with a reason, whenever either is
/// already in flight, so there is never a second one to admit and
/// overwrite this slot with.
const INFLIGHT_REMOTE_OP_KEY: &str = "git-vista.inflight-remote-op";

/// Load the persisted in-flight Fetch/Pull, if any and if it still
/// parses. Malformed or foreign JSON (a private-browsing quirk, a shape
/// written by a future or older client version) is treated as "nothing to
/// resume", never a panic — the same best-effort posture as every other
/// read here.
pub fn load_inflight_remote_op() -> Option<crate::features::operations::core::InFlightRemoteOp> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(INFLIGHT_REMOTE_OP_KEY).ok().flatten())
        .and_then(|v| serde_json::from_str(&v).ok())
}

/// Persist the just-bound Fetch/Pull's identity, right after `bind_id`
/// succeeds (`features::operations::signals::persist_if_remote_op`).
/// Best-effort, like every other write here: private browsing may refuse
/// it, in which case a reload during that operation simply cannot resume —
/// no worse than before this feature existed.
pub fn store_inflight_remote_op(op: &crate::features::operations::core::InFlightRemoteOp) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        if let Ok(json) = serde_json::to_string(op) {
            let _ = s.set_item(INFLIGHT_REMOTE_OP_KEY, &json);
        }
    }
}

/// Clear the persisted entry, but only if it still names `id` — a
/// defensive check against a future multi-entry version of this feature
/// ever clearing the wrong record's storage. Today there is only ever one
/// entry, so this is equivalent to an unconditional clear in practice.
pub fn clear_inflight_remote_op_if_matches(id: &str) {
    let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else {
        return;
    };
    let matches = s
        .get_item(INFLIGHT_REMOTE_OP_KEY)
        .ok()
        .flatten()
        .and_then(|v| {
            serde_json::from_str::<crate::features::operations::core::InFlightRemoteOp>(&v).ok()
        })
        .is_some_and(|entry| entry.id == id);
    if matches {
        let _ = s.remove_item(INFLIGHT_REMOTE_OP_KEY);
    }
}
