//! The label layer's link, glyph and colour rules — and the source census
//! binding the two wasm-only files that draw them to the functions above.
//!
//! #653, following the shape #650 and #652 set. `render/labels.rs` and
//! `print.rs` are both `#[cfg(target_arch = "wasm32")]`, so `cargo test
//! --workspace` compiles neither. Everything they decided was therefore
//! decided somewhere no runner could reach — including, literally, the three
//! `#[cfg(test)]` assertions that used to sit at the bottom of `print.rs` and
//! reported nothing because the module around them compiled to nothing (ADR
//! 0115). Those three are the first three tests below, moved verbatim in
//! meaning to a place that runs them.

use super::*;

use crate::icons::icon_set;

const COMMIT_ID: &str = "0123456789abcdef0123456789abcdef01234567";
const BASE: &str = "https://github.com/owner/repo";

// ---- commit_link (the three tests print.rs never ran) -----------------------

#[test]
fn pushed_commits_link_to_their_commit_page() {
    assert_eq!(
        commit_link(Some(BASE), true, COMMIT_ID),
        RefLink::To(format!("{BASE}/commit/{COMMIT_ID}"))
    );
}

#[test]
fn unpushed_commits_are_dimmed_not_linked() {
    assert_eq!(
        commit_link(Some(BASE), false, COMMIT_ID),
        RefLink::Unpushed,
        "a commit that is not on the remote has no GitHub page — linking it \
         would 404, and rendering it as ordinary would hide that it is local"
    );
}

#[test]
fn no_github_remote_links_nothing_and_dims_nothing() {
    assert_eq!(
        commit_link(None, true, COMMIT_ID),
        RefLink::NoRemote,
        "a repository with no GitHub base has nothing to link to, and an \
         unlinked label there is not a deficiency to mark"
    );
}

// ---- the three states are mutually exclusive -------------------------------

#[test]
fn clickable_and_unpushed_are_never_both_true_and_no_remote_is_neither() {
    let linked = commit_link(Some(BASE), true, COMMIT_ID);
    let unpushed = commit_link(Some(BASE), false, COMMIT_ID);
    let none = commit_link(None, false, COMMIT_ID);

    assert!(linked.clickable() && !linked.unpushed());
    assert!(!unpushed.clickable() && unpushed.unpushed());
    assert!(
        !none.clickable() && !none.unpushed(),
        "`unpushed` styling in a repo with no GitHub remote would dim every \
         label in the graph — this is the case the old call sites derived by \
         hand as `repo_url.is_some() && url.is_none()`, three separate times"
    );
    assert_eq!(
        linked.into_url(),
        Some(format!("{BASE}/commit/{COMMIT_ID}"))
    );
    assert_eq!(unpushed.into_url(), None);
    assert_eq!(none.into_url(), None);
}

// ---- ref_badge_link, per kind ----------------------------------------------

#[test]
fn head_and_tag_badges_link_the_commit_they_sit_on() {
    for kind in [RefKind::Head, RefKind::Tag] {
        assert_eq!(
            ref_badge_link(&kind, "v1.0", Some(BASE), true, COMMIT_ID, false),
            RefLink::To(format!("{BASE}/commit/{COMMIT_ID}")),
            "{kind:?} links the commit, not a page of its own — a tag's own \
             page cannot be verified offline"
        );
        assert_eq!(
            ref_badge_link(&kind, "v1.0", Some(BASE), false, COMMIT_ID, false),
            RefLink::Unpushed,
            "{kind:?} on an unpushed commit has nowhere to go"
        );
    }
}

#[test]
fn a_local_branch_badge_links_only_when_a_remote_branch_of_that_name_exists() {
    assert_eq!(
        ref_badge_link(&RefKind::Branch, "main", Some(BASE), true, COMMIT_ID, true),
        RefLink::To(format!("{BASE}/tree/main"))
    );
    assert_eq!(
        ref_badge_link(&RefKind::Branch, "wip", Some(BASE), true, COMMIT_ID, false),
        RefLink::Unpushed,
        "a local-only branch has no tree page; linking it would 404 even \
         though the commit under it is pushed"
    );
}

#[test]
fn a_local_branch_badge_ignores_whether_its_commit_is_pushed() {
    // The branch rule asks about the *branch*, not the commit: a branch that
    // exists on the remote has a tree page regardless of whether this
    // particular tip commit has been pushed to it yet.
    assert_eq!(
        ref_badge_link(&RefKind::Branch, "main", Some(BASE), false, COMMIT_ID, true),
        RefLink::To(format!("{BASE}/tree/main"))
    );
}

#[test]
fn a_remote_branch_badge_strips_its_remote_prefix() {
    assert_eq!(
        ref_badge_link(
            &RefKind::RemoteBranch,
            "origin/feature/x",
            Some(BASE),
            false,
            COMMIT_ID,
            false,
        ),
        RefLink::To(format!("{BASE}/tree/feature/x")),
        "GitHub's tree URLs name the branch, not the remote — and only the \
         FIRST segment is the remote, so a slashed branch name keeps its rest"
    );
}

#[test]
fn a_remote_branch_badge_always_links_it_is_on_the_remote_by_definition() {
    assert_eq!(
        ref_badge_link(
            &RefKind::RemoteBranch,
            "origin/main",
            Some(BASE),
            false,
            COMMIT_ID,
            false,
        ),
        RefLink::To(format!("{BASE}/tree/main"))
    );
}

#[test]
fn no_github_base_means_no_badge_links_at_all() {
    for kind in [
        RefKind::Head,
        RefKind::Tag,
        RefKind::Branch,
        RefKind::RemoteBranch,
    ] {
        assert_eq!(
            ref_badge_link(&kind, "origin/main", None, true, COMMIT_ID, true),
            RefLink::NoRemote,
            "{kind:?} must not be dimmed as `unpushed` in a repo that has no \
             GitHub remote to be pushed to"
        );
    }
}

// ---- glyphs and colours ----------------------------------------------------

#[test]
fn every_ref_kind_gets_a_distinct_badge_glyph() {
    let ic = icon_set(true);
    let glyphs: Vec<&str> = [
        RefKind::Head,
        RefKind::Tag,
        RefKind::Branch,
        RefKind::RemoteBranch,
    ]
    .iter()
    .map(|k| ref_glyph(ic, k))
    .collect();
    let mut sorted = glyphs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        glyphs.len(),
        "two ref kinds share a glyph: {glyphs:?}. Local and remote branch \
         pills in particular are meant to differ at a glance before the name \
         is read"
    );
    assert_eq!(ref_glyph(ic, &RefKind::Branch), ic.branch);
    assert_eq!(ref_glyph(ic, &RefKind::RemoteBranch), ic.branch_alt);
}

#[test]
fn head_is_the_only_badge_whose_colours_differ_between_screen_and_paper() {
    let branch = git_vista_core::color::branch_color(2);
    for kind in [RefKind::Tag, RefKind::Branch, RefKind::RemoteBranch] {
        assert_eq!(
            badge_colors(&kind, branch, BadgeSurface::Screen),
            badge_colors(&kind, branch, BadgeSurface::Paper),
            "{kind:?} must look the same on both surfaces — the print sheet's \
             one intended divergence is HEAD's outline, and anything else \
             diverging is the two copies drifting apart again"
        );
    }
    let screen = badge_colors(&RefKind::Head, branch, BadgeSurface::Screen);
    let paper = badge_colors(&RefKind::Head, branch, BadgeSurface::Paper);
    assert_eq!(screen.fill, paper.fill);
    assert_eq!(screen.text, paper.text);
    assert_ne!(
        screen.stroke, paper.stroke,
        "HEAD's fill is near-white; on a white sheet a same-colour stroke \
         leaves the pill with no edge at all"
    );
    assert_eq!(screen.stroke, screen.fill);
}

#[test]
fn a_remote_branch_pill_is_outlined_and_a_local_one_filled() {
    let branch = git_vista_core::color::branch_color(3);
    let local = badge_colors(&RefKind::Branch, branch, BadgeSurface::Screen);
    let remote = badge_colors(&RefKind::RemoteBranch, branch, BadgeSurface::Screen);
    assert_eq!((local.fill, local.stroke), (branch, branch));
    assert_eq!(
        (remote.fill, remote.stroke, remote.text),
        ("none", branch, branch),
        "the remote pill is an outline in the branch colour — that hollow \
         look is half of how local and remote badges are told apart"
    );
}

// ---- the seam --------------------------------------------------------------
//
// Everything above proves the rules. These prove the two wasm-only files still
// ask them. Both are `#[cfg(target_arch = "wasm32")]`, so a change that
// re-derives any of these rules inline would leave every test above green
// while the pixels on screen stopped using the answer — and, before #653,
// that is exactly the state both files were in.

/// The interactive label layer, read as text.
const LABELS: &str = include_str!("../../../render/labels.rs");

/// The print sheet, read as text, for the same reason and by the same means.
const PRINT: &str = include_str!("../../../print.rs");

#[test]
fn the_interactive_badge_arm_asks_core_for_its_link_glyph_and_colours() {
    assert!(
        LABELS.contains("ref_badge_link("),
        "render/labels.rs no longer calls `ref_badge_link`. Which page a badge \
         points at is a decision, and this file is wasm-only — a copy \
         re-derived here is unreachable from every test above"
    );
    assert!(
        LABELS.contains("ref_glyph(") && LABELS.contains("badge_colors("),
        "render/labels.rs no longer asks core for its glyph and pill colours; \
         `print.rs` draws the same badges from the same two functions, and a \
         second copy here is how the two surfaces drift apart"
    );
    assert!(
        !LABELS.contains("/tree/"),
        "render/labels.rs builds a GitHub tree URL again instead of asking \
         `ref_badge_link`. That is the local-branch 404 rule — a tree page \
         only exists when a remote branch of that name does — and it is \
         invisible to every test above when it lives here"
    );
    assert!(
        !LABELS.contains("repo_url.is_some()"),
        "render/labels.rs derives the `unpushed` styling from `repo_url` by \
         hand again. `RefLink` exists so that flag travels with the URL rule \
         it belongs to; derived separately, the two drift and a label is \
         dimmed while it links, or links while dimmed"
    );
}

#[test]
fn the_interactive_message_label_asks_core_where_a_commit_links() {
    assert!(
        LABELS.contains("commit_link("),
        "render/labels.rs no longer calls `commit_link` for the message tier"
    );
    assert!(
        !LABELS.contains("/commit/"),
        "render/labels.rs builds a GitHub commit URL again rather than asking \
         core — the print sheet builds the same one, and they are one rule"
    );
}

#[test]
fn the_print_sheet_asks_core_for_the_same_link_glyph_and_colours() {
    assert!(
        PRINT.contains("commit_link("),
        "print.rs no longer calls `commit_link`. Its own `#[cfg(test)]` tests \
         for this rule never ran — the module is wasm-only — so a copy here \
         is unwatched by construction"
    );
    assert!(
        PRINT.contains("ref_glyph(") && PRINT.contains("badge_colors("),
        "print.rs no longer asks core for its glyph and pill colours"
    );
    assert!(
        !PRINT.contains("/commit/"),
        "print.rs builds a GitHub commit URL again instead of asking core"
    );
    assert!(
        PRINT.contains("BadgeSurface::Paper"),
        "print.rs no longer names the paper surface. HEAD's grey outline is \
         the one deliberate difference between the two surfaces; if this file \
         stops asking for `Paper`, HEAD prints as a white pill on white paper \
         with no edge"
    );
}

#[test]
fn the_print_sheet_does_not_keep_a_test_module_that_can_never_run() {
    assert!(
        !PRINT.contains("mod tests"),
        "print.rs has a `mod tests` again. The whole file is \
         `#[cfg(target_arch = \"wasm32\")]`, so `cargo test --workspace` \
         compiles nothing in it and such a module reports no result at all — \
         not a failure, not a pass, silence (ADR 0115). Tests for anything \
         this file decides belong in a module the host build compiles, like \
         this one"
    );
}
