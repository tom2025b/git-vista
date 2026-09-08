//! #711: the context menu's **v2** working-tree read is pinned to the accepted
//! repository frame, the same way #709/#710 pinned its v1 staged-file read.
//!
//! This is the same defect class one endpoint over, with a worse consequence.
//! The v1 read decided whether an "Unstage Changes" item appeared. This one
//! builds the path lists for "Discard Changes…" and "Delete Untracked Files…",
//! so a reply retained across a repository switch would name another
//! repository's files inside a confirmation for this one — and the second of
//! those operations has no undo of any kind.
//!
//! **The server supplements this gate; it does not replace it.** Both
//! destructive POSTs now carry the repository captured with the path reading,
//! and `planner::plan_and_execute_matching` refuses a different current
//! selection with 412. After that identity check, `verify_path_states`
//! re-derives each path's state in the matched repository. That second guard is
//! a conditional path-state recheck, not general stale-reply protection: two
//! sibling worktrees dirty in the same file produce the same answer on path
//! name and state alone. See `features/status/core.rs`'s
//! `a_colliding_path_name_is_indistinguishable_to_a_path_state_recheck`.
//!
//! The menu was not the only reader. The Activity panel had the same v2 read,
//! unscoped in exactly the same way — the issue named only the menu, and the
//! required `repo` argument is what found the second call site: making the
//! unscoped question unrepresentable turned a silent third instance of this
//! defect into a compile error. It is fixed the same way and censused here
//! beside the menu rather than deferred.
//!
//! **#746 closed the question this note used to leave open**, and reversed its
//! answer. The panel's consequence was called milder, on the reading that it
//! renders a file list rather than feeding a destructive confirmation. That is
//! wrong: the same sections are handed to `stash::core::push_preview`, which
//! writes the "this will capture … this will be left behind" copy for a
//! `git stash push` and whose `may_push` gates the offer, and a `Conflicted`
//! row is a `<button>` opening `ViewerDoc::Conflict` — a view that resolves
//! against the repository selected *now*, writing the file. A retained reading
//! would have described another repository's changes inside this one's stash
//! confirmation. The panel is the menu's defect class, at one more hop.
//!
//! The gate itself is no longer only censused here. `features::status::core`'s
//! `panel_worktree_reading` composes the resolution and the section derivation
//! into one host-compiled function the panel calls, so the cards are reachable
//! only through the gate, and its tests move the selection between the read and
//! the render rather than asserting the mapping by calling the function that
//! defines it.
//!
//! **Why this is a source census and not an executed test.** `menu.rs`,
//! `menu/worktree_items.rs`, `activity.rs` and `api/status.rs` are all
//! `#[cfg(target_arch = "wasm32")]` in `main.rs`, and every UI dependency they
//! import lives under Cargo.toml's
//! `[target.'cfg(target_arch = "wasm32")'.dependencies]`. `cargo test` — here
//! and in CI — therefore never compiles a line of any of them, so a decision
//! made inside the menu could not go red on the host no matter how it was
//! written. `api.rs` is gated too, which is why its stale re-export of the
//! renamed function was caught by the first assertion below rather than by the
//! host test run. This file follows the precedent of `menu_status_scope.rs`,
//! `offline_guard_audit` and `reachability_census`.
//!
//! The *decision* is not censused, because it deliberately does not live in
//! any of these files: the menu resolves its reply through `current_reading`,
//! the host-tested function in `features/status/detail/core.rs`, and the paths
//! come from the host-tested selectors in `features/status/core.rs`. Those two
//! are composed and run — over this exact payload type, including the stale
//! epoch, stale repository, loading, failed and absent arms — by
//! `only_a_reply_matching_the_live_frame_can_name_files_to_destroy`. What this
//! file pins is the wiring that neither of them can see: that the menu asks a
//! scoped question, tags the answer with the scope it asked for, and hands the
//! item builder a *resolved* reading rather than a raw one.
//!
//! # What this cannot prove
//!
//! It observes bytes, not behaviour. It cannot prove the resource is ever
//! polled, that the resolved reading reaches the confirmation the user sees,
//! or that the server honours `?repo=` (it does — `worktree_status_v2` takes
//! `Query<RepoQuery>` and resolves it through `resolve_repo`, which is why
//! this fix needed no server change). It would also pass on a `current_reading`
//! whose own body had been inverted; that function's host tests hold that line.

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
const WORKTREE_ITEMS: &str = include_str!("../src/menu/worktree_items.rs");
const ACTIVITY: &str = include_str!("../src/activity.rs");
const API_STATUS: &str = include_str!("../src/api/status.rs");
const API: &str = include_str!("../src/api.rs");

/// The regression #711 names, in the bytes that ship: no unscoped v2 status
/// read is reachable from anywhere.
///
/// The requirement is on the *signature*, not on the call sites. A scoped call
/// site can be written unscoped again by a later edit; a required argument
/// cannot be omitted at all. `Option<&str>` would still express "no
/// repository" — `&str` cannot, so the unscoped question is unrepresentable
/// rather than merely unasked.
#[test]
fn no_unscoped_v2_worktree_read_survives_anywhere() {
    let api = code_only(API_STATUS);
    assert!(
        api.contains("pub async fn fetch_worktree_status_for(repo: &str)"),
        "fetch_worktree_status_for must require a repository, not accept an optional one"
    );
    assert!(
        api.contains("\"/api/status/v2?t={}&repo={}\""),
        "the v2 URL must always carry ?repo="
    );
    assert!(
        !api.contains("fn fetch_worktree_status()"),
        "the unscoped entry point is back"
    );
    assert!(
        !code_only(API).contains("fetch_worktree_status,"),
        "api.rs still re-exports the unscoped entry point"
    );
    assert!(
        !code_only(MENU).contains("fetch_worktree_status()"),
        "menu.rs calls the unscoped v2 status read — the #711 regression"
    );
    assert!(
        !code_only(ACTIVITY).contains("fetch_worktree_status()"),
        "activity.rs calls the unscoped v2 status read — the #711 regression"
    );
}

/// The Activity panel's copy of the same read, pinned the same way.
///
/// #746 asked whether a stale reading here was the milder, read-only case this
/// file once called it. It is not: these sections write the stash push preview
/// and decide whether that push is offered, and a conflicted card is a button
/// into a view that resolves against the repository selected now. The panel is
/// the menu's defect class, and is gated identically.
///
/// What this census pins is only the wiring. The *decision* is
/// `features::status::core::panel_worktree_reading`, whose host tests move the
/// selection between the read and the render — the sequence no census over
/// bytes can execute.
#[test]
fn the_activity_panel_keys_and_resolves_its_worktree_read_the_same_way() {
    let activity = code_only(ACTIVITY);
    assert!(
        activity.contains("status_state::repo(status)"),
        "the panel must read the accepted frame's repository id"
    );
    assert!(
        activity.contains("(true, Some(id)) => fetch_worktree_status_for(id)"),
        "the panel must fetch only once a repository is accepted, and scope the fetch to it"
    );
    assert!(
        activity.contains("(epoch, repo, reading)"),
        "the reply must be tagged with the epoch and repository it was requested for"
    );
    assert!(
        activity.contains("panel_worktree_reading("),
        "the panel must resolve its reply against the live frame"
    );
    // The sections must be reachable only *through* that resolution. Deriving
    // them here again from a `WorktreeStatus` would let a later edit build
    // cards straight from the raw reply — the #746 defect rewritten by hand,
    // in a file `cargo test` never compiles.
    assert!(
        !activity.contains("StatusSections::from_worktree_status("),
        "the panel derives its sections outside the frame gate"
    );
    // Both consumers must go through the resolved reading. A single remaining
    // `.get().flatten()` would put an unresolved reply back on screen.
    assert!(
        !activity.contains("worktree_status.get().flatten()"),
        "a consumer still reads the panel's reply without resolving it"
    );
    assert!(
        activity.matches("worktree_now()").count() >= 2,
        "both the push-preview signal and the rendered section must read the resolved value"
    );
}

/// The menu asks a *scoped* question: `repo` is part of the resource key, so a
/// repository switch refetches instead of retaining the previous repo's answer,
/// and the reply records the parsed request-key repository beside the status it
/// returned.
#[test]
fn the_menu_keys_its_worktree_read_on_the_accepted_repository() {
    let menu = code_only(MENU);
    assert!(
        menu.contains("(true, Some(id)) => match id.parse()")
            && menu.contains("fetch_worktree_status_for(id).await.ok().map(|status|"),
        "the menu must fetch only once a repository is accepted, and scope the fetch to it"
    );
    // The reply carries the parsed request-key id beside the status and the
    // outer frame scope. Without both, resolution could compare the live frame
    // against itself or dispatch a selector unrelated to the path reading.
    assert!(
        menu.contains("ScopedWorktreeStatus { repo, status }")
            && menu.contains("(epoch, repo, reading)"),
        "the v2 reply must carry its parsed repository id and outer frame scope"
    );
}

/// ...and refuses an answer tagged for another frame, through the same
/// host-tested function the chip path uses.
///
/// The pair of assertions is the point. Asserting only that `current_reading`
/// is *called* would stay green if its verdict were discarded — the
/// inert-assertion shape this repo has shipped before. So the call and the
/// consumption of its result are both pinned.
#[test]
fn a_worktree_reply_tagged_for_another_frame_cannot_reach_the_menu() {
    let menu = code_only(MENU);
    assert!(
        menu.contains("let worktree_now = move || {") && menu.contains("current_reading("),
        "the menu must resolve its v2 reply against the live frame"
    );
    assert!(
        menu.contains("worktree.loading().get(),") && menu.contains("worktree.get(),"),
        "the resolution must be over this resource's own reply and loading state"
    );
    // The live side of the comparison is the frame, read at resolution time —
    // never the requested scope compared against itself.
    assert!(
        menu.contains("graph.get().epoch(),")
            && menu.contains("status_state::repo(status).as_deref()"),
        "the live epoch and repository must be read from the frame at resolution time"
    );
    assert!(
        menu.contains("worktree_now(),"),
        "the item builder must receive the resolved reading, not the raw resource"
    );
    assert!(
        !menu.contains("staged_now(),\n                worktree,"),
        "the item builder is being handed the raw resource again"
    );
}

/// The item builder cannot skip that resolution, because it never sees a reply
/// to skip it on: it takes the resolved `Option<ScopedWorktreeStatus>`. The
/// wrapper carries both values to the builder, where their association is a
/// convention rather than an invariant enforced by private fields.
///
/// This is the structural half of the fix. Everything above can be re-broken by
/// an edit to `menu.rs`; this makes the unresolved reply unreachable from the
/// file where the destructive path lists are actually built.
#[test]
fn the_item_builder_takes_a_resolved_reading_not_a_resource() {
    let items = code_only(WORKTREE_ITEMS);
    assert!(
        items.contains("live_status: Option<ScopedWorktreeStatus>,"),
        "build_worktree_items must take a resolved reading with its captured repository scope"
    );
    assert!(
        !items.contains("Resource<"),
        "build_worktree_items still accepts a raw resource it could read unresolved"
    );
    assert!(
        !items.contains("worktree.get()"),
        "the item builder is reading the resource directly again"
    );
    // The path lists must still come from the host-tested selectors rather than
    // being rebuilt here, or the classification the server re-derives and the
    // one the user was shown could drift apart again.
    assert!(
        items.contains("discardable_tracked_paths") && items.contains("deletable_untracked_paths"),
        "the destructive path lists must come from the host-tested selectors"
    );
}

/// A refused reading must read as *waiting*, never as *nothing to do* — #711
/// asks for this by name, because the two disabled-item wordings make very
/// different promises.
///
/// "No tracked file has uncommitted changes" is a claim about the repository.
/// Saying it because a reply was refused for belonging to another frame would
/// state, confidently and wrongly, that this working tree is clean — the same
/// class of lie as the item silently vanishing. `current_reading` resolves a
/// refusal to `None`, and `None` is exactly what selects the waiting arm, so
/// this holds by construction; it is pinned because it is a stated criterion
/// and a one-word edit away from being wrong.
#[test]
fn a_refused_reading_reads_as_waiting_not_as_nothing_to_do() {
    let items = code_only(WORKTREE_ITEMS);
    assert_eq!(
        items
            .matches("let reason = if live_status.is_none() {")
            .count(),
        2,
        "both destructive items must branch their copy on the resolved reading"
    );
    assert_eq!(
        items
            .matches("\"Waiting for a working-tree status read\"")
            .count(),
        2,
        "both destructive items must have the waiting wording to fall back to"
    );
}

/// Reproducible entry point for `failure-atlas mutation_check` over the wasm
/// client. The ordinary host suite can census these files but cannot compile
/// them; this ignored test builds the wasm path and runs only #733's browser
/// contract. Failure Atlas holds its own outer lock, so child build commands
/// deliberately use the normal build lock rather than waiting on their parent.
#[test]
#[ignore = "failure-atlas compiles and drives the browser client through this harness"]
fn captured_selector_browser_contract_for_mutation_check() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root.join("target");
    let run = |args: &[&str]| {
        std::process::Command::new("buildlock")
            .args(args)
            .current_dir(&root)
            .env_remove("BUILDLOCK_FILE")
            .env_remove("NO_COLOR")
            .env("CARGO_TARGET_DIR", &target)
            .status()
            .unwrap_or_else(|e| panic!("could not run buildlock {args:?}: {e}"))
    };

    assert!(
        run(&["trunk", "build", "--config", "crates/git-vista/Trunk.toml"]).success(),
        "the mutated wasm client must compile before its browser test runs"
    );
    assert!(
        run(&["./dev", "browser", "destructive-selector.spec.mjs"]).success(),
        "the captured-selector browser contract failed"
    );
}
