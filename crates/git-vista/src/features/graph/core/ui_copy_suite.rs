//! Pure UI copy-composition tests: the Print Graph button, disabled
//! context-menu items, the mode-picker button label, the Pull item's label,
//! the Create-tag item's label/annotation/sign-choice trio. Extracted
//! verbatim from the back half of `core.rs`'s inline `mod tests` (a
//! `#[cfg(test)]` child module) so the parent file can be read as production
//! code. These share no fixtures with the `LoadedHistory`/prefetch tests
//! that made up the front half of that same block — see `history_suite.rs`
//! for those. `use super::*;` is added here (it was declared once for the
//! whole original `mod tests` and is now needed on each half separately);
//! nothing else changed.

use super::*;

// #217: the disabled Print Graph button's reason must be visible in the
// label itself, not only the `title` attribute — native tooltips don't
// surface on tap. Reverting `print_button_copy` to always return the plain
// "Print Graph" label (the pre-fix behaviour) fails the first assertion
// here, since the two labels would no longer differ.
#[test]
fn print_button_copy_surfaces_a_visible_reason_when_disabled() {
    let (disabled_label, disabled_title) = print_button_copy(false);
    let (ready_label, _) = print_button_copy(true);
    assert_ne!(
        disabled_label, ready_label,
        "the disabled reason must show up in the label text — a title-only \
         change never surfaces on a touch device"
    );
    assert_eq!(disabled_title, "Load all history before printing.");
}

#[test]
fn print_button_copy_is_plain_when_history_is_complete() {
    let (label, title) = print_button_copy(true);
    assert_eq!(label, "Print Graph");
    assert!(!title.is_empty());
}

/// #65: the four disabled context-menu items in `menu.rs` used to convey
/// their reason ONLY via `title=reason` — invisible on tap, unannounced by
/// VoiceOver. This pins the fix's composition: the reason text must appear
/// in BOTH strings this function hands back, not only the one that maps to
/// `title`.
#[test]
fn disabled_menu_item_copy_puts_the_reason_in_both_strings() {
    let (aria_label, visible_line) = disabled_menu_item_copy("Stage Changes", "Nothing to stage");
    assert!(
        aria_label.contains("Nothing to stage"),
        "the aria-label must contain the reason, or VoiceOver announces \
         nothing beyond the bare item name"
    );
    assert!(
        aria_label.contains("Stage Changes"),
        "the aria-label must still name the item, not just the reason"
    );
    assert_eq!(
        visible_line, "Nothing to stage",
        "the visible second line is the reason verbatim — this is what a \
         finger sees without needing hover"
    );
}

// #244 follow-up: both picker buttons used to share one `busy` flag, so
// clicking either went inert with no visible change for up to two minutes
// on a slow retry — indistinguishable from a broken app. The clicked
// button must show distinct "opening…" wording; the other button, if
// reverted to always return the plain label regardless of `opening`,
// fails the first assertion here.
#[test]
fn mode_button_label_shows_opening_only_for_the_clicked_button() {
    let clicked = mode_button_label(RepoMode::Visualize, Some(RepoMode::Visualize));
    let idle = mode_button_label(RepoMode::Visualize, None);
    assert_ne!(
        clicked, idle,
        "the clicked button's label must change while its request is in \
         flight — a silently-disabled button reads as a broken app"
    );
    assert!(
        clicked.contains("opening"),
        "the in-flight label should say what's happening, not just differ"
    );
}

#[test]
fn mode_button_label_leaves_the_other_button_alone() {
    // Visualize was clicked (opening = Some(Visualize)); Active's label
    // must stay its normal wording, not also claim to be opening.
    let other = mode_button_label(RepoMode::Active, Some(RepoMode::Visualize));
    let idle = mode_button_label(RepoMode::Active, None);
    assert_eq!(
        other, idle,
        "only the button the user actually clicked should announce \
         itself as opening — the other one is just disabled"
    );
    assert!(!other.contains("opening"));
}

#[test]
fn mode_button_label_covers_the_active_button_too() {
    let clicked = mode_button_label(RepoMode::Active, Some(RepoMode::Active));
    assert!(clicked.contains("opening"));
    assert!(clicked.contains("Active"));
}

/// #325 follow-up: pins Pull's label to actually name the branch once
/// one is known, the same shape
/// `print_button_copy_surfaces_a_visible_reason_when_disabled` pins for
/// Print Graph above — the two labels must differ, or the branch never
/// reached the string.
#[test]
fn pull_label_names_the_branch_when_known() {
    let with_branch = pull_label(Some("feature/x"), "origin");
    let without_branch = pull_label(None, "origin");
    assert_ne!(
        with_branch, without_branch,
        "the branch must show up in the label — otherwise every Pull \
         item reads identically regardless of what's checked out"
    );
    assert!(with_branch.contains("feature/x"));
    assert!(with_branch.contains("origin"));
}

#[test]
fn pull_label_falls_back_to_the_remote_when_branch_is_unknown() {
    let label = pull_label(None, "origin");
    assert_eq!(
        label, "Pull from ‘origin’",
        "while `rebase_status` is still loading (or HEAD is detached) \
         the label degrades to naming just the remote, never a blank or \
         placeholder subject"
    );
}

/// Paired negative: proves the property above is not vacuous by showing
/// what a `title`-only fix (the pre-#65 shape) would have looked like —
/// the reason present in neither returned string, because there was
/// nothing here to call. Standing in for the removed code, not exercising
/// production code, the same way `the_previous_padding_would_have_been_undersized`
/// documents `a11y`'s old constant.
#[test]
fn a_title_only_reason_would_not_have_reached_either_string() {
    let title_only_label: &str = "Stage Changes"; // the old span's visible text
    assert!(
        !title_only_label.contains("Nothing to stage"),
        "this is what the bug looked like: the label carries no reason at all"
    );
}

/// #238: the label names the branch as the tag's subject on a stub's
/// own menu, mirroring `create_label`'s "from this branch" wording.
#[test]
fn create_tag_item_label_names_the_branch_on_a_stub() {
    assert_eq!(create_tag_item_label(true), "Create tag from this branch");
}

/// …and the commit on a commit dot's menu.
#[test]
fn create_tag_item_label_names_the_commit_on_a_dot() {
    assert_eq!(create_tag_item_label(false), "Create tag from this commit");
}

/// #238: a typed message becomes an annotated tag's text, trimmed of
/// surrounding whitespace the prompt UI doesn't strip on its own.
#[test]
fn tag_annotation_from_prompt_keeps_trimmed_text() {
    assert_eq!(
        tag_annotation_from_prompt(Some("  first stable release  ".to_string())),
        Some("first stable release".to_string())
    );
}

/// Cancelling the second prompt (`None`) must read as "no annotation" —
/// a lightweight tag — the same as every other case below.
#[test]
fn tag_annotation_from_prompt_none_on_cancel() {
    assert_eq!(tag_annotation_from_prompt(None), None);
}

/// The bug this function exists to prevent: dismissing the prompt with
/// nothing typed (`Some("")`) must NOT silently produce an annotated tag
/// with an empty message — it has to collapse to the same lightweight
/// outcome as an outright cancel.
#[test]
fn tag_annotation_from_prompt_empty_string_is_lightweight_not_annotated() {
    assert_eq!(tag_annotation_from_prompt(Some(String::new())), None);
    assert_eq!(
        tag_annotation_from_prompt(Some("   ".to_string())),
        None,
        "whitespace-only input is the same case, typed with a stray space"
    );
}

/// Cancel and empty-string-typed must be indistinguishable at this
/// function's output — that's the whole point of collapsing them —
/// stated as an explicit equality so a future change can't quietly
/// reintroduce a difference.
#[test]
fn tag_annotation_from_prompt_cancel_and_empty_agree() {
    assert_eq!(
        tag_annotation_from_prompt(None),
        tag_annotation_from_prompt(Some(String::new()))
    );
}

/// M2.21e (#239): signing requires both an annotation and the user's
/// yes — either alone is not enough.
#[test]
fn tag_sign_choice_requires_both_a_message_and_confirmation() {
    assert!(tag_sign_choice(true, true));
    assert!(
        !tag_sign_choice(false, true),
        "a lightweight tag has no object to carry a signature, however the \
         sign prompt was answered"
    );
    assert!(
        !tag_sign_choice(true, false),
        "declining the sign offer must produce an unsigned annotated tag"
    );
    assert!(!tag_sign_choice(false, false));
}
