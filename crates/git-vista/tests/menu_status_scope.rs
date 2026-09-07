//! #709: the context menu's staged-file read is pinned to the accepted
//! repository frame, the same way #707 pinned the topbar chip's.
//!
//! **Why this is a source census and not an executed test.** `menu.rs` and
//! `api/status.rs` are both `#[cfg(target_arch = "wasm32")]` in `main.rs`, and
//! every UI dependency they import lives under Cargo.toml's
//! `[target.'cfg(target_arch = "wasm32")'.dependencies]`. `cargo test` — here
//! and in CI — therefore never compiles a line of either. A count computed
//! inside the menu could not go red on the host no matter how it was written,
//! which is the same blind spot `offline_guard_audit` and `reachability_census`
//! exist for, and this file follows their precedent.
//!
//! The *decision* is not censused, because it does not live here: the menu
//! resolves its reply through `reading_is_current`, the host-tested predicate
//! in `features/status/detail/core.rs` that the chip path already uses
//! (`retained_readings_must_match_epoch_and_repository` runs it, including the
//! stale-epoch and stale-repository arms). What this file pins is the wiring
//! that predicate cannot see: that the menu asks a scoped question, tags the
//! answer with the scope it asked for, and refuses an answer tagged for some
//! other frame.
//!
//! # What this cannot prove
//!
//! It observes bytes, not behaviour. It cannot prove the resolved count
//! reaches the unstage items, that the resource is ever polled, or that the
//! server honours `?repo=`. It would also pass on a `reading_is_current` whose
//! own body had been inverted — that function's own host tests are what hold
//! that line.

/// Whole-line `//` comments dropped, so a census asserting the *absence* of a
/// call cannot be defeated — or tripped — by prose naming the call it forbids.
/// A trailing comment on a line of code is left alone: that line still ships
/// code.
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

const MENU: &str = include_str!("../src/menu.rs");
const API_STATUS: &str = include_str!("../src/api/status.rs");
const API: &str = include_str!("../src/api.rs");

/// The regression #709 names, in the bytes that ship: no unscoped v1 status
/// read is reachable from the menu, or from anywhere else.
///
/// `fetch_status()` is deliberately checked with its empty argument list: the
/// scoped `fetch_status_for(id)` the menu should call must not trip it, and
/// neither should the v2 per-path read the discard/delete confirmations need
/// (`fetch_worktree_status_for`, scoped by #711 and censused separately in
/// `menu_worktree_scope.rs`).
#[test]
fn no_unscoped_v1_status_read_survives_anywhere() {
    let menu = code_only(MENU);
    assert!(
        !menu.contains("fetch_status()"),
        "menu.rs calls the unscoped v1 status read — the #709 regression"
    );

    let api = code_only(API_STATUS);
    // Required, not `Option<&str>`: there is no argument meaning "unscoped",
    // so this cannot regress by omission.
    assert!(
        api.contains("pub async fn fetch_status_for(repo: &str)"),
        "fetch_status_for must require a repository, not accept an optional one"
    );
    assert!(
        api.contains("\"/api/status?t={}&repo={}\""),
        "the v1 URL must always carry ?repo="
    );
    assert!(
        !api.contains("fn fetch_status()"),
        "the unscoped entry point is back"
    );
    assert!(
        !code_only(API).contains("fetch_status,"),
        "api.rs still re-exports the deleted unscoped entry point"
    );
}

/// The menu asks a *scoped* question: `repo` is part of the resource key, so a
/// repository switch refetches instead of retaining the previous repo's answer.
#[test]
fn the_menu_keys_its_staged_read_on_the_accepted_repository() {
    let menu = code_only(MENU);
    assert!(
        menu.contains("status_state::repo(status)"),
        "the menu must read the accepted frame's repository id"
    );
    assert!(
        menu.contains("(true, Some(id)) => fetch_status_for(id)"),
        "the menu must fetch only once a repository is accepted, and scope the fetch to it"
    );
    // The reply carries the scope it was requested for. Without this the
    // resolution below could only compare the live frame against itself.
    assert!(
        menu.contains("(epoch, repo, count)"),
        "the reply must be tagged with the epoch and repository it was requested for"
    );
}

/// ...and refuses an answer tagged for another frame, through the same
/// host-tested predicate the chip path uses.
///
/// The pair of assertions is the point. Asserting only that
/// `reading_is_current` is *called* would stay green if its verdict were
/// discarded (`let _ = reading_is_current(...);` compiles and gates nothing) —
/// the inert-assertion shape this repo has shipped before. So the call and the
/// consumption of its verdict are both pinned.
#[test]
fn a_reply_tagged_for_another_frame_cannot_become_a_menu_count() {
    let menu = code_only(MENU);
    assert!(
        menu.contains("reading_is_current("),
        "the menu must resolve its reply against the live frame"
    );
    assert!(
        menu.contains(".then_some(count)"),
        "reading_is_current's verdict must gate the count, not merely be evaluated"
    );
    // A refused reading is zero, so the unstage items stay absent — the same
    // thing a failed fetch already meant at this call site.
    assert!(
        menu.contains(".unwrap_or(0)"),
        "a refused or missing reading must resolve to zero staged files"
    );
    // The live side of the comparison is the frame, read at resolution time —
    // never the requested scope compared against itself.
    assert!(
        menu.contains("graph.get().epoch(),")
            && menu.contains("status_state::repo(status).as_deref()"),
        "the live epoch and repository must be read from the frame at resolution time"
    );
}
