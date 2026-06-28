//! IPC commands callable from the Leptos frontend via Tauri's `invoke`.

use git_vista_core::layout;
use git_vista_core::model::Graph;

/// Open the repository at `path`, walk its history, and return the laid-out
/// vertical graph. Stub: the `repo` walker isn't implemented yet, so this
/// returns an empty graph for now.
#[tauri::command]
pub fn list_commits(path: String) -> Graph {
    let _ = path; // TODO: git_vista_core::repo::walk_history(Path::new(&path), 5_000)
    layout::layout(Vec::new())
}
