//! DOM adapter for the host-tested storage policy; exercised by Playwright.
use wasm_bindgen::JsCast;

use super::storage::{export_preferences, owns_storage_key, EXPORT_KEYS};

const UNAVAILABLE: &str = "Browser storage is unavailable. No completion can be confirmed.";

pub fn export_saved_preferences() -> Result<(), String> {
    let window = web_sys::window().ok_or(UNAVAILABLE)?;
    let storage = window
        .local_storage()
        .map_err(|_| UNAVAILABLE)?
        .ok_or(UNAVAILABLE)?;
    let mut entries = Vec::new();
    for key in EXPORT_KEYS {
        if let Some(value) = storage.get_item(key).map_err(|_| UNAVAILABLE)? {
            entries.push((key.to_string(), value));
        }
    }
    let document = window.document().ok_or(UNAVAILABLE)?;
    let anchor = document
        .create_element("a")
        .map_err(|_| UNAVAILABLE)?
        .dyn_into::<web_sys::HtmlAnchorElement>()
        .map_err(|_| UNAVAILABLE)?;
    let encoded = js_sys::encode_uri_component(&export_preferences(&entries));
    anchor.set_href(&format!("data:application/json;charset=utf-8,{encoded}"));
    anchor.set_download("git-vista-preferences.json");
    let body = document.body().ok_or(UNAVAILABLE)?;
    body.append_child(&anchor).map_err(|_| UNAVAILABLE)?;
    anchor.click();
    anchor.remove();
    Ok(())
}

/// Snapshot keys before removal, since Storage indices shift on every delete.
/// A failure is surfaced even if earlier keys were successfully removed.
pub fn clear_saved_data() -> Result<(), String> {
    let window = web_sys::window().ok_or(UNAVAILABLE)?;
    for storage in [window.local_storage(), window.session_storage()] {
        let storage = storage.map_err(|_| UNAVAILABLE)?.ok_or(UNAVAILABLE)?;
        let mut keys = Vec::new();
        for index in 0..storage.length().map_err(|_| UNAVAILABLE)? {
            if let Some(key) = storage.key(index).map_err(|_| UNAVAILABLE)? {
                if owns_storage_key(&key) {
                    keys.push(key);
                }
            }
        }
        for key in keys {
            storage.remove_item(&key).map_err(|_| UNAVAILABLE)?;
        }
    }
    window
        .location()
        .reload()
        .map_err(|_| "Saved data cleared, but reload failed. Reload this tab manually.".to_string())
}
