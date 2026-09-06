//! A source census over the three `force_bump` call sites that follow a
//! repository/worktree selection (#676, issue quoting fable's design report
//! `~/projects/git-vista-reports/fable-2026-09-05-0845.md` §2 item C):
//! `picker.rs`'s mode choice, `dialogs/confirm.rs`'s "open that worktree
//! instead" path, and `dialogs/open_url.rs`'s clone-and-view flow.
//!
//! All three files are `#[cfg(target_arch = "wasm32")]`-gated (`main.rs`), so
//! `cargo test` never compiles a line of them (ADR 0115) — this census reads
//! their bytes instead and pins the one claim that matters: **every
//! `force_bump()` that follows a selection also tears down the confirm
//! dialog**, because a confirmation is about an operation in the repository
//! the user just left.
//!
//! # What this proves, and what it does not
//!
//! Proves the *shape* is present in the right place: `shell.close_confirm()`
//! (or the pre-existing `shell.close_confirm()` at `confirm.rs`'s own earlier
//! line, for that one site) appears in the source text between the site's
//! selection-outcome anchor and its `force_bump()` call.
//!
//! Does NOT prove the call actually runs, that it runs before the epoch bump
//! at the DOM/timing level, or that a confirm dialog visibly disappears in a
//! real browser — a mutation that moves the exact same text into a branch
//! that never executes (e.g. `if false { shell.close_confirm(); }`) would
//! still read as present here. `ci/browser/tests/repo-selection-teardown.spec.mjs`
//! is the other half: it drives the real DOM and is the one thing that can
//! catch that shape of defect.

const PICKER_SRC: &str = include_str!("picker.rs");
const CONFIRM_SRC: &str = include_str!("dialogs/confirm.rs");
const OPEN_URL_SRC: &str = include_str!("dialogs/open_url.rs");

/// Asserts `close_needle` occurs in `src` after `anchor_needle` and before
/// the next `"g.force_bump();"` following it — i.e. beside the same
/// selection outcome, not merely present somewhere else in the file.
fn close_confirm_precedes_force_bump(src: &str, site: &str, anchor_needle: &str) {
    let anchor_pos = src
        .find(anchor_needle)
        .unwrap_or_else(|| panic!("{site}: anchor text moved or was rewritten — re-pin this census against the new shape: {anchor_needle:?}"));
    let after_anchor = &src[anchor_pos..];
    let bump_pos = after_anchor.find("g.force_bump();").unwrap_or_else(|| {
        panic!(
            "{site}: no `g.force_bump();` found after the selection outcome — re-pin this census"
        )
    });
    let window = &after_anchor[..bump_pos];
    assert!(
        window.contains("shell.close_confirm();"),
        "{site}: `shell.close_confirm()` does not appear between the selection outcome and \
         `force_bump()` — a confirmation left open here would show a plan against the \
         repository the user just left (#676)"
    );
}

#[test]
fn picker_mode_choice_closes_confirm_before_bumping() {
    close_confirm_precedes_force_bump(PICKER_SRC, "picker.rs mode_view", "picker_open.set(false);");
}

#[test]
fn confirm_open_that_worktree_instead_closes_confirm_before_bumping() {
    close_confirm_precedes_force_bump(
        CONFIRM_SRC,
        "dialogs/confirm.rs open-that-worktree-instead",
        "select_worktree_request(&id, mode).await",
    );
}

#[test]
fn open_url_clone_closes_confirm_before_bumping() {
    close_confirm_precedes_force_bump(
        OPEN_URL_SRC,
        "dialogs/open_url.rs clone flow",
        "if bump_epoch {",
    );
}
