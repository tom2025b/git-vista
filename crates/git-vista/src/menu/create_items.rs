//! "Create branch from this commit…" / "Create tag from this commit…" — the
//! two prompt-then-POST creation flows (Issue #33, M2.21d/#238).

use leptos::*;

use crate::api::{create_branch_request, create_tag_request};
use crate::features::dialogs::core::{branch_name_space_fix, Dialog, ErrorNotice};
use crate::features::graph::core::{create_tag_item_label, tag_annotation_from_prompt, tag_sign_choice};
use crate::icons::GitIcons;
use crate::state::{Features, MenuData};

/// Builds the full "Create branch…" and "Create tag…" buttons — icon,
/// label and click handler together, so `menu.rs`'s final template just
/// embeds the result rather than wiring a bare handler onto inline markup.
pub(super) fn build_create_items(features: Features, ic: &'static GitIcons, m: &MenuData) -> (View, View) {
    let Features {
        graph,
        dialogs,
        shell,
        ..
    } = features;
    // "Create branch from this commit": prompt for a name, POST it, then
    // refresh the graph on success or show git's error on failure (B3).
    let commit = m.commit.clone();
    let on_branch = move |_| {
        shell.close_menu();
        let Some(win) = web_sys::window() else { return };
        // A native prompt — simple and works in iPad Safari. Empty / cancel
        // does nothing.
        let name = match win.prompt_with_message("Name for the new branch:") {
            Ok(Some(n)) => n.trim().to_string(),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        // The one pre-flight check worth doing client-side (#316):
        // a space is the common typo, and catching it here means an
        // offer to fix instead of a server round-trip to git's
        // "not a valid branch name". Everything else stays git's
        // call — its stderr now arrives unwrapped via the modal.
        let name = match branch_name_space_fix(&name) {
            Some(fixed) => {
                let accepted = win
                    .confirm_with_message(&format!(
                        "Branch names can't contain spaces.\nUse '{fixed}' instead?"
                    ))
                    .unwrap_or(false);
                if !accepted {
                    return;
                }
                fixed
            }
            None => name,
        };
        let commit = commit.clone();
        spawn_local(async move {
            match create_branch_request(&name, &commit).await {
                // Bump the fetch counter so the new branch appears.
                Ok(()) => graph.update(|g| {
                    g.force_bump();
                }),
                // The failure path finally meets the confirmation
                // path's bar (#316): the app's own modal, showing the
                // envelope's message — never raw JSON in an alert().
                Err(e) => {
                    dialogs.open(Dialog::Error);
                    shell.open_error(ErrorNotice {
                        title: "Couldn't create branch",
                        body: e,
                    });
                }
            }
        });
    };
    let create_label = m.create_label;
    let create_branch_item = view! {
        <button class="ctx-item" on:click=on_branch>
            // Creating a branch — the branch glyph.
            <span class="nf ctx-icon">{ic.branch}</span>
            {create_label}
        </button>
    };
    // "Create tag from this commit": the same prompt-then-POST shape
    // as "Create branch" just above (M2.21d, #238), plus a second
    // prompt for an optional annotation message and, when that
    // produces one, a third confirm asking whether to sign it
    // (M2.21e, #239). Both the "cancel vs empty vs typed text"
    // mapping onto "lightweight vs annotated" and the "message
    // present + confirmed" mapping onto "sign" go through
    // `tag_annotation_from_prompt`/`tag_sign_choice` rather than
    // being read inline here — exactly the sort of decision this
    // wasm-only file cannot itself pin with a test (see those
    // functions' own doc comments).
    let create_tag_label = create_tag_item_label(m.is_branch);
    let tag_commit = m.commit.clone();
    let on_tag = move |_| {
        shell.close_menu();
        let Some(win) = web_sys::window() else { return };
        let name = match win.prompt_with_message("Name for the new tag:") {
            Ok(Some(n)) => n.trim().to_string(),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        // Same space pre-flight as branch names (#316) — the check
        // is generic to any git ref name, not branch-specific.
        let name = match branch_name_space_fix(&name) {
            Some(fixed) => {
                let accepted = win
                    .confirm_with_message(&format!(
                        "Tag names can't contain spaces.\nUse '{fixed}' instead?"
                    ))
                    .unwrap_or(false);
                if !accepted {
                    return;
                }
                fixed
            }
            None => name,
        };
        let raw_message = win
            .prompt_with_message(
                "Optional annotation message (leave blank for a lightweight tag):",
            )
            .ok()
            .flatten();
        let message = tag_annotation_from_prompt(raw_message);
        // Only offer to sign once there is a message to attach one
        // to — a lightweight tag has no tag object to carry it.
        let confirmed_sign = message.is_some()
            && win
                .confirm_with_message(
                    "Sign this tag with GPG?\n\n\
                     OK = signed tag, Cancel = unsigned annotated tag",
                )
                .unwrap_or(false);
        let sign = tag_sign_choice(message.is_some(), confirmed_sign);
        let commit = tag_commit.clone();
        spawn_local(async move {
            match create_tag_request(&name, &commit, message.as_deref(), sign).await {
                // Bump the fetch counter so the new tag's badge appears.
                Ok(()) => graph.update(|g| {
                    g.force_bump();
                }),
                Err(e) => {
                    dialogs.open(Dialog::Error);
                    shell.open_error(ErrorNotice {
                        title: if sign {
                            "Couldn't sign the tag"
                        } else {
                            "Couldn't create tag"
                        },
                        body: e,
                    });
                }
            }
        });
    };
    let create_tag_item = view! {
        <button class="ctx-item" on:click=on_tag>
            // Creating a tag — the tag glyph, shared with tag
            // badges and the Activity panel's tag list.
            <span class="nf ctx-icon">{ic.tag}</span>
            {create_tag_label}
        </button>
    };
    (create_branch_item.into_view(), create_tag_item.into_view())
}
