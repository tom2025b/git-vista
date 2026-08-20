//! The menu items that render regardless of `read_only` or online status:
//! "Open on GitHub", "View details", "Show diff". `menu.rs`'s final
//! assembly renders these ahead of the write-action gate — see that
//! assembly for why.

use leptos::*;

use crate::features::graph::core::disabled_menu_item_copy;
use crate::features::shell::signals::Shell;
use crate::icons::GitIcons;
use crate::state::MenuData;

/// Builds `(open_github, details_item, diff_item)`, in the order
/// `menu_view`'s final template renders them.
pub(super) fn build_view_items(
    shell: Shell,
    ic: &'static GitIcons,
    m: &MenuData,
) -> (View, View, View) {
    let label = m.github_label;
    let open_github = match m.github_url.clone() {
        // Live link: a real anchor, opening GitHub in a new tab. Tapping it
        // also closes the menu.
        Some(url) => view! {
            <a
                class="ctx-item"
                href=url
                target="_blank"
                rel="noopener"
                on:click=move |_| shell.close_menu()
            >
                // The GitHub mark flags the one item that leaves the app.
                <span class="nf ctx-icon">{ic.github}</span>
                {label}
            </a>
        }
        .into_view(),
        // No GitHub page for this target (no github remote, or unpushed):
        // show the option but disabled, with a reason on hover.
        None => {
            const REASON: &str =
                "No GitHub page (no github.com remote, or it isn't pushed)";
            let (aria_label, visible_reason) = disabled_menu_item_copy(label, REASON);
            view! {
                <button
                    class="ctx-item disabled"
                    title=REASON
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{ic.github}</span>
                    {label}
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view()
        }
    };
    // "View details" (Phase 10): open the side panel for this commit. A
    // read, so it's shown for read-only clones too. Set `detail_id` before
    // closing the menu — `shell.close_menu()` disposes this handler's reactive
    // owner, after which a signal write is unreliable (same caveat as below).
    let detail_commit = m.commit.clone();
    let on_details = move |_| {
        // `false`: no "scroll to the Changes section" wish on a plain details
        // open. It is an argument rather than a separate poke precisely so this
        // path cannot forget to clear one left by an earlier "Show diff".
        //
        // Nothing here closes the Activity panel. The two share the right edge
        // and the overlay stack evicts whichever is already docked there — the
        // rule lives in one function now instead of at every opener (M1.11, #64,
        // Task 8).
        shell.open_detail(detail_commit.clone(), false);
        shell.close_menu();
    };
    // "View details" opens a commit's detail panel — the commit glyph.
    let details_item = view! {
        <button class="ctx-item" on:click=on_details>
            <span class="nf ctx-icon">{ic.commit}</span>
            "View details"
        </button>
    };
    // "Show diff": the same detail panel, but with the Changes section
    // scrolled into view once the diff lands — so the tap answers
    // "what did this commit change?" directly. The scroll wish rides
    // as an argument to `open_detail`, raised before the menu closes
    // (the reactive-owner ordering rule).
    let diff_commit = m.commit.clone();
    let on_diff = move |_| {
        shell.open_detail(diff_commit.clone(), true);
        shell.close_menu();
    };
    let diff_item = view! {
        <button class="ctx-item" on:click=on_diff>
            // The diff-modified glyph — this item is about changed files.
            <span class="nf ctx-icon">{ic.modified}</span>
            "Show diff"
        </button>
    };
    (open_github, details_item.into_view(), diff_item.into_view())
}
