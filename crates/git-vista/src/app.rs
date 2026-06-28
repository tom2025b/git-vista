//! Leptos components for git-vista.
//!
//! Scaffold only: a shell with a top bar and a placeholder where the vertical
//! graph will render. Later this will `invoke` the Tauri `list_commits` command,
//! receive a `git_vista_core::model::Graph`, and draw it as SVG.

use leptos::*;

#[component]
pub fn App() -> impl IntoView {
    view! {
        <main class="app">
            <header class="topbar">
                <h1>"git-vista"</h1>
                <span class="subtitle">"vertical git history"</span>
            </header>
            <section class="graph">
                <p class="placeholder">
                    "The zoomable vertical git graph will render here."
                </p>
            </section>
        </main>
    }
}
