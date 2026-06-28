//! Tauri desktop shell for git-vista.
//!
//! Wires the Leptos frontend (served by Trunk / embedded from `dist/`) to the
//! native window and registers the IPC commands the UI calls into.

mod commands;

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![commands::list_commits])
        .run(tauri::generate_context!())
        .expect("error while running git-vista");
}
