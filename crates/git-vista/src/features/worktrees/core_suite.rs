//! Host tests for the worktree drawer's decisions (M11.03, #548).

use super::*;
use git_vista_protocol::{BranchName, CommitOid};

fn sibling(name: &str, branch: Option<&str>, serviceable: Serviceable) -> WorktreeSibling {
    WorktreeSibling {
        repository: "repo-1".to_string(),
        id: format!("worktree-{name}"),
        name: name.to_string(),
        path: None,
        branch: branch.map(|b| BranchName::new(b).unwrap()),
        head: Some(CommitOid::new("abcdef1234567890abcdef1234567890abcdef12".to_string()).unwrap()),
        is_current: false,
        locked: false,
        prunable: false,
        bare: false,
        serviceable,
    }
}

fn rows(view: DrawerView) -> Vec<WorktreeRow> {
    match view {
        DrawerView::Rows(rows) => rows,
        DrawerView::Unreadable { reason, .. } => {
            panic!("expected rows, got an unreadable census: {reason}")
        }
    }
}

fn observed(siblings: Vec<WorktreeSibling>) -> Result<WorktreeCensus, String> {
    Ok(WorktreeCensus::Observed { siblings })
}

// ---------------------------------------------------------------------------
// Every sibling is listed, including the refused ones
// ---------------------------------------------------------------------------

/// Acceptance 1. Hiding a refused sibling is the option the spec weighs and
/// rejects: it would also make the drawer disagree with M11.02's collision
/// check, which counts every worktree git counts.
#[test]
fn every_sibling_is_listed_including_the_ones_the_app_refuses() {
    let view = drawer_view(observed(vec![
        sibling("here", Some("main"), Serviceable::Yes),
        sibling("desk-two", Some("feature/x"), Serviceable::Yes),
        sibling(
            "outside",
            Some("feature/y"),
            Serviceable::OutsideAllowedRoots,
        ),
        sibling("ghost", Some("feature/z"), Serviceable::Missing),
    ]));
    let listed = rows(view);
    let names: Vec<&str> = listed.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["here", "desk-two", "outside", "ghost"]);
}

// ---------------------------------------------------------------------------
// git's facts and the app's verdict stay two statements
// ---------------------------------------------------------------------------

/// Acceptance 2, the load-bearing half: a locked worktree inside the allowed
/// roots is still openable. Locking is git's business with `worktree remove`;
/// it says nothing about whether this app may serve the directory. Folding the
/// two would make this row unopenable for a reason nobody holds.
#[test]
fn a_locked_worktree_inside_the_roots_is_still_openable() {
    let mut locked = sibling("locked-desk", Some("feature/x"), Serviceable::Yes);
    locked.locked = true;
    let rows = rows(drawer_view(observed(vec![locked])));
    let row = &rows[0];

    assert_eq!(
        row.git_facts,
        vec![RowFact {
            label: "locked",
            source: FactSource::Git
        }]
    );
    assert_eq!(row.app_fact.source, FactSource::App);
    assert_eq!(row.app_fact.label, "can open");
    assert!(
        matches!(row.offer, RowOffer::Open { .. }),
        "git's lock must not become this app's refusal: {:?}",
        row.offer
    );
}

/// The mirror: a refused worktree that git is perfectly happy with. git
/// reports no flags at all, and the app still refuses — so a view rendering
/// only `git_facts` would show a row with nothing wrong with it.
#[test]
fn a_worktree_git_likes_can_still_be_refused_by_the_app() {
    let rows = rows(drawer_view(observed(vec![sibling(
        "outside",
        Some("feature/y"),
        Serviceable::OutsideAllowedRoots,
    )])));
    let row = &rows[0];
    assert!(row.git_facts.is_empty(), "{:?}", row.git_facts);
    assert_eq!(row.app_fact.source, FactSource::App);
    assert!(
        matches!(row.offer, RowOffer::Refused { .. }),
        "{:?}",
        row.offer
    );
}

/// The app's verdict is on **every** row, openable ones included. A badge that
/// appears only on refusal teaches a reader that its absence means nothing was
/// checked.
#[test]
fn the_apps_verdict_is_stated_on_every_row_not_only_the_refused_ones() {
    let view = drawer_view(observed(vec![
        sibling("desk-two", Some("feature/x"), Serviceable::Yes),
        sibling(
            "outside",
            Some("feature/y"),
            Serviceable::OutsideAllowedRoots,
        ),
        sibling("ghost", Some("feature/z"), Serviceable::Missing),
    ]));
    for row in rows(view) {
        assert_eq!(
            row.app_fact.source,
            FactSource::App,
            "{} has no app verdict",
            row.name
        );
        assert!(!row.app_fact.label.is_empty(), "{}", row.name);
    }
}

/// No fact may claim both sources, and the three app verdicts must read
/// differently from each other — one "unusable" badge covering every case is
/// the failure this criterion names.
#[test]
fn the_three_app_verdicts_are_three_different_sentences() {
    let labels: Vec<&str> = [
        Serviceable::Yes,
        Serviceable::OutsideAllowedRoots,
        Serviceable::Missing,
    ]
    .iter()
    .map(|s| app_fact(s).label)
    .collect();
    let unique: std::collections::BTreeSet<&&str> = labels.iter().collect();
    assert_eq!(unique.len(), 3, "verdicts collapsed into: {labels:?}");
}

// ---------------------------------------------------------------------------
// The offer
// ---------------------------------------------------------------------------

/// Acceptance 3: a refused row carries its reason as text the view can render,
/// and that text is the protocol's — the same sentence the server answers
/// `POST /api/select-worktree` with.
#[test]
fn a_refused_row_carries_the_servers_own_reason() {
    for serviceable in [Serviceable::OutsideAllowedRoots, Serviceable::Missing] {
        let rows = rows(drawer_view(observed(vec![sibling(
            "refused",
            Some("feature/x"),
            serviceable.clone(),
        )])));
        match &rows[0].offer {
            RowOffer::Refused { reason } => assert_eq!(
                Some(*reason),
                serviceable.refusal(),
                "the drawer's reason drifted from the server's"
            ),
            other => panic!("{serviceable:?} must be refused, got {other:?}"),
        }
    }
}

/// The worktree you are already in is not somewhere to switch to. Asked before
/// the fence, because it is `Serviceable::Yes` and would otherwise be offered.
#[test]
fn the_current_worktree_is_not_offered_as_a_destination() {
    let mut here = sibling("here", Some("main"), Serviceable::Yes);
    here.is_current = true;
    let rows = rows(drawer_view(observed(vec![here])));
    assert_eq!(rows[0].offer, RowOffer::Current);
}

/// The offer carries the opaque id, never a path — which is why the drawer
/// works with `GIT_VISTA_EXPOSE_PATHS` unset, the default.
#[test]
fn an_openable_row_is_addressed_by_id_and_works_without_path_exposure() {
    let rows = rows(drawer_view(observed(vec![sibling(
        "desk-two",
        Some("feature/x"),
        Serviceable::Yes,
    )])));
    assert_eq!(rows[0].path, None, "this fixture exposes no paths");
    assert_eq!(
        rows[0].offer,
        RowOffer::Open {
            id: "worktree-desk-two".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// What is checked out
// ---------------------------------------------------------------------------

/// A detached HEAD and a bare record both have no branch, and they are not the
/// same thing. A view handed `None` would have to guess.
#[test]
fn a_detached_head_and_a_bare_record_are_told_apart() {
    let detached = sibling("detached", None, Serviceable::Yes);
    let mut bare = sibling("hub.git", None, Serviceable::Yes);
    bare.bare = true;
    bare.head = None;

    let rows = rows(drawer_view(observed(vec![detached, bare])));
    assert_eq!(rows[0].branch, BranchCell::Detached);
    assert_eq!(rows[1].branch, BranchCell::Bare);
    assert_ne!(rows[0].branch.label(), rows[1].branch.label());
    assert_eq!(rows[1].head, None, "a bare record names no commit");
}

/// The branch the collision refusal names is the branch the drawer shows, so a
/// user following M11.02's sentence to this drawer finds the row it meant.
#[test]
fn a_branch_row_shows_the_branch_by_name() {
    let rows = rows(drawer_view(observed(vec![sibling(
        "desk-two",
        Some("feature/x"),
        Serviceable::Yes,
    )])));
    assert_eq!(rows[0].branch.label(), "feature/x");
    assert_eq!(rows[0].head.as_deref(), Some("abcdef1"));
}

// ---------------------------------------------------------------------------
// An unread census lists nothing and claims nothing
// ---------------------------------------------------------------------------

/// Both ways of learning nothing — the request failed, and the server said it
/// could not read the list — become the same visible statement, and neither
/// becomes an empty list of rows. An empty list would say "this repository has
/// no other worktrees", which is the fail-open `WorktreeCensus` exists to
/// prevent.
#[test]
fn neither_way_of_learning_nothing_becomes_an_empty_drawer() {
    for fetched in [
        Err("network error".to_string()),
        Ok(WorktreeCensus::CensusFailed {
            reason: "git worktree list exited 128".to_string(),
            detail: None,
        }),
    ] {
        match drawer_view(fetched.clone()) {
            DrawerView::Unreadable { reason, .. } => assert!(!reason.is_empty(), "{fetched:?}"),
            DrawerView::Rows(rows) => {
                panic!(
                    "{fetched:?} became {} rows rather than a stated failure",
                    rows.len()
                )
            }
        }
    }
}

/// A census that genuinely observed one worktree — the repository has no
/// linked siblings — is a real answer and renders as one row, not as a
/// failure. The paired positive for the test above.
#[test]
fn a_repository_with_no_linked_worktrees_is_one_row_not_a_failure() {
    let mut here = sibling("here", Some("main"), Serviceable::Yes);
    here.is_current = true;
    assert_eq!(rows(drawer_view(observed(vec![here]))).len(), 1);
}

// ---------------------------------------------------------------------------
// The wasm-only view (ADR 0115): `cargo test` never compiles `view.rs`, so
// every mapping in it is unreachable by every test above. It is read back as
// source instead — the mechanism `features::preview::core` and
// `features::dialogs::core` already use, and what `wasm_module_census` counts
// as watching this file at all.
//
// These pin the mappings the acceptance criteria are about. They cannot prove
// the drawer renders; `ci/browser/tests/worktree-drawer.spec.mjs` is what
// proves that.
// ---------------------------------------------------------------------------

const VIEW: &str = include_str!("view.rs");

/// `view.rs` with `//`-prefixed lines dropped.
///
/// Needed because the checks below are about what the file *does*, and the
/// file's own doc comment names the things it deliberately does not do — it
/// says, in as many words, that the refusal sentence comes from
/// `Serviceable::refusal` rather than from this file. A scan that could not
/// tell a comment from code would read that sentence as the violation it
/// exists to warn against, which is the most annoying possible false positive:
/// it would punish the documentation for being accurate.
fn view_code() -> String {
    VIEW.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Acceptance 2, structurally: git's facts and the app's verdict reach two
/// **different** CSS modifiers. One badge covering both is the failure this
/// criterion names, and it would present exactly as these two strings being
/// the same.
#[test]
fn the_two_fact_sources_render_as_two_different_pills() {
    let git = VIEW
        .split_once("FactSource::Git => ")
        .expect("view.rs no longer maps FactSource::Git to a class")
        .1
        .lines()
        .next()
        .unwrap()
        .trim_end_matches(',')
        .trim();
    let app = VIEW
        .split_once("FactSource::App => ")
        .expect("view.rs no longer maps FactSource::App to a class")
        .1
        .lines()
        .next()
        .unwrap()
        .trim_end_matches(',')
        .trim();
    assert_ne!(
        git, app,
        "git's flags and this app's verdict render through the same class, so a \
         reader cannot tell a fact git reported from a verdict this app reached"
    );
    assert!(
        git.contains("act-pill") && app.contains("act-pill"),
        "{git} / {app}"
    );
}

/// Acceptance 3: the reason is rendered as text. #65's finding is that a
/// reason carried only in `title=` never surfaces on a tap and is never
/// announced — and the stash drawer beside this one does use `title=`, so
/// "follow the neighbouring file" would have reintroduced it.
#[test]
fn a_refused_row_renders_its_reason_as_text_not_a_tooltip() {
    let arm = VIEW
        .split_once("RowOffer::Refused { reason } =>")
        .expect("view.rs no longer renders the refused arm")
        .1;
    let end = arm.find("RowOffer::Open").unwrap_or(arm.len());
    let arm = &arm[..end];
    assert!(
        arm.contains("{reason}"),
        "the refused arm does not render the reason at all:\n{arm}"
    );
    assert!(
        !arm.contains("title="),
        "the refused arm carries its reason in a tooltip:\n{arm}"
    );
}

/// The view decides nothing: it must not re-derive the fence itself. A
/// `Serviceable` match here would be a second opinion about whether a row can
/// be opened, and the one that disagrees with the server would win on screen.
#[test]
fn the_view_asks_the_core_rather_than_matching_on_serviceable() {
    let code = view_code();
    assert!(
        code.contains("drawer_view("),
        "view.rs no longer calls `drawer_view`, so every test above proves a rule \
         nothing uses"
    );
    assert!(
        !code.contains("Serviceable::"),
        "view.rs matches on `Serviceable` itself — the fence is decided in two \
         places now, and they can disagree"
    );
}

/// The switch goes to the worktree route, not the catalog-only one. Routing it
/// to `/api/select` is exactly the gap #651's body named: a serviceable
/// sibling nobody scanned answers `404 No such repository.`
#[test]
fn the_open_button_uses_the_worktree_route() {
    let code = view_code();
    assert!(
        code.contains("select_worktree_request("),
        "the drawer no longer switches through `/api/select-worktree`"
    );
    assert_eq!(
        code.matches("select_request(").count(),
        0,
        "the drawer reaches the catalog-only `select_request`, which cannot \
         resolve a worktree nobody registered"
    );
}

/// #65's 44x44 floor. `.act-undo` already carries it and its `:focus-visible`
/// twin; a new class would need its own entry in `features::a11y::audit`'s
/// `INTERACTIVE_CENSUS`, and this asserts the drawer did not quietly grow one.
#[test]
fn the_one_control_reuses_a_class_that_already_meets_the_touch_floor() {
    let button = VIEW
        .split_once("<button class=")
        .expect("the drawer has no button")
        .1;
    assert!(
        button.starts_with("\"act-undo\""),
        "the drawer's button uses a class with no recorded 44x44 decision: {}",
        &button[..button.len().min(40)]
    );
}

/// Switching desks must never escalate the session's posture — the same rule
/// M11.02's "Open Worktree" offer follows, and the reason a refused-or-awkward
/// state must not become a route to Active mode.
#[test]
fn opening_a_worktree_never_escalates_the_session_mode() {
    let code = view_code();
    assert!(
        code.contains("session_state::ui_mode().unwrap_or(RepoMode::Visualize)"),
        "the drawer no longer inherits the session's mode, so switching desks may \
         change the posture the user chose"
    );
    assert_eq!(
        code.matches("RepoMode::Active").count(),
        0,
        "the drawer names Active mode somewhere; switching desks must not grant it"
    );
}
