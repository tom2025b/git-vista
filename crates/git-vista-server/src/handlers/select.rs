//! `POST /api/select` (ADR 0007), `POST /api/select-worktree` (M11.03, #548)
//! and `POST /api/rescan` (ADR 0009).
//!
//! Select moves the process-global current selection to a repository the catalog
//! already holds — addressed by opaque id, resolved fail-closed — and records the
//! Visualize/Active mode the operator chose. Rescan re-reads the configured repo
//! root without a restart, so a repo created after launch can be picked. Both sit
//! behind the full M1.04 auth gate (session + CSRF + Host/Origin) like every
//! other mutation.

use std::path::Path;

use axum::http::StatusCode;
use axum::Json;

use git_vista_core::identity::WorktreeId;
use git_vista_protocol::{SelectRequest, SelectWorktreeRequest, WorktreeCensus};

use crate::state::{
    register_repo_list, scan_clones_root, scan_repo_root, scan_worktrees_root, select_registered,
};

/// Make the repository addressed by `worktree` the current selection, in the
/// requested mode. Unknown/forged id → 404, the same fail-closed contract as
/// the `?repo=` reads; a string that isn't even id-shaped → 400.
pub(crate) async fn select_repo(Json(req): Json<SelectRequest>) -> (StatusCode, String) {
    let worktree: WorktreeId = match req.worktree.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()),
    };
    if select_registered(worktree, req.mode) {
        (StatusCode::OK, "Selected.".to_string())
    } else {
        (StatusCode::NOT_FOUND, "No such repository.".to_string())
    }
}

/// Switch to a linked worktree of the currently served repository, addressed
/// by the opaque id the census reports (M11.03, #548).
///
/// # The gap this closes
///
/// M11.02 (#547) made a checkout of a branch held by another worktree refuse
/// with an offer: *open that worktree instead*. That offer went to
/// `POST /api/select`, which resolves ids through the catalog — and a linked
/// worktree nobody ever scanned is not in the catalog, so a perfectly
/// serviceable sibling answered `404 No such repository.` The offer was honest
/// about failing, which is not the same as working. See
/// [`SelectWorktreeRequest`]'s doc for why this is a second door rather than a
/// widening of the first.
///
/// # Order of questions, and why the catalog is asked first
///
/// 1. **Already registered?** Then select it and stop. A sibling that is
///    already a catalog entry — the common case on this machine, where the
///    repo root scan registers every worktree under `~/projects` — costs no
///    subprocess at all, and the well-tested `/api/select` path is reused
///    verbatim rather than reimplemented.
/// 2. Otherwise take a **fresh census** of the served repository and look the
///    id up in it. A census that could not be read establishes nothing and
///    refuses; an id no sibling carries is a `404`, the same fail-closed
///    answer a forged id gets from `/api/select`.
/// 3. A sibling that is not [`Serviceable::Yes`] is refused **in
///    `Serviceable::refusal`'s own words** — the same sentence the drawer
///    already showed beside the row, so the two cannot drift.
/// 4. Only then is it admitted, by `Catalog::register`, which re-checks the
///    allowed roots itself.
///
/// # The fence is enforced twice and never widened
///
/// `Serviceable::Yes` already means "this canonical path lies inside an
/// allowed root" — that is how `worktree_census` computes it. Registration
/// then asks the same question again, independently, in the code that has
/// always owned it (`register_fails_closed_outside_the_allowed_roots` is its
/// test). Neither check is skipped on the strength of the other, and no path
/// here is taken from the request: the request carries an opaque id, and every
/// path comes from `git worktree list --porcelain` run inside the repository
/// the session already has selected.
///
/// # `CensusPaths::rows_for_local_use` on the internal census — precisely
///
/// The census is taken with **row** paths on because registration takes a
/// path. What that discloses differs by arm, and the honest statement has to
/// say so:
///
/// * **`Observed`** — the sibling rows are read locally and never serialized.
///   This handler answers with a status and a sentence it composes itself, so
///   nothing the operator's `GIT_VISTA_EXPOSE_PATHS` opt-in would have
///   withheld reaches the client.
/// * **`CensusFailed`** — the `reason` **is** returned to the client. It is
///   now the client-safe half by construction, and the path-bearing `detail`
///   follows the operator's flag rather than this route's local need, which
///   is why the two are separate arguments to `CensusPaths` at all.
///
/// The second bullet used to read the other way, and that was the defect:
/// `GET /api/worktrees` (M11.01, #546) answered `CensusFailed.reason`
/// verbatim with `expose_paths` off, so a control whose stated guarantee is
/// "absolute paths do not leave the process unless the operator opts in" held
/// on the success arm and not the failure one (Grok, round 6, finding 4;
/// #657; ADR 0119). ADR 0117 §2a records the state before the fix.
pub(crate) async fn select_discovered_worktree(
    Json(req): Json<SelectWorktreeRequest>,
) -> (StatusCode, String) {
    let worktree: WorktreeId = match req.worktree.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "Not a worktree id.".to_string()),
    };
    // 1. The catalog already holds it: reuse the existing, well-tested path.
    if select_registered(worktree, req.mode) {
        return (StatusCode::OK, "Selected.".to_string());
    }

    // 2. A fresh census of the repository this session has selected.
    let (repo, read_only) = crate::state::current();
    let census = crate::worktree_census::worktree_census(
        &repo,
        crate::worktree_census::CensusPaths::rows_for_local_use(crate::state::expose_paths()),
        &crate::state::path_is_allowed,
    )
    .await;
    let siblings = match census {
        WorktreeCensus::Observed { siblings } => siblings,
        WorktreeCensus::CensusFailed { reason, .. } => {
            return (StatusCode::CONFLICT, census_failure_body(&reason));
        }
    };
    let Some(sibling) = siblings.iter().find(|s| s.id == req.worktree) else {
        // Fail-closed, and worded exactly like `/api/select`'s: an id this
        // repository's census does not name is not a worktree of it.
        return (StatusCode::NOT_FOUND, "No such worktree.".to_string());
    };

    // 3. Refused, in the sentence the drawer already showed.
    if let Some(why) = sibling.serviceable.refusal() {
        return (StatusCode::CONFLICT, why.to_string());
    }

    // 4. Admit it. `path` is present because this census was taken with
    //    `expose_paths: true`; its absence would be a bug in the census rather
    //    than a state to guess at, so it refuses rather than inventing one.
    let Some(path) = sibling.path.as_deref() else {
        eprintln!("git-vista: /api/select-worktree got a serviceable sibling with no path");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Couldn't determine where that worktree lives, so nothing was selected.".to_string(),
        );
    };
    // `read_only` is inherited from the repository currently open: a linked
    // worktree shares its provenance exactly — a worktree of a read-only URL
    // clone is no less a clone than the tree it was made from.
    if let Err(e) = crate::state::register_discovered_worktree(Path::new(path), read_only) {
        eprintln!("git-vista: /api/select-worktree refused to register {path}: {e}");
        return (
            StatusCode::CONFLICT,
            "That worktree could not be opened by this app.".to_string(),
        );
    }
    if select_registered(worktree, req.mode) {
        (StatusCode::OK, "Selected.".to_string())
    } else {
        // Registered, and still unresolvable: the id the census derived and the
        // id registration produced disagree, which is a real defect rather than
        // a user error. Say so instead of pretending the worktree is gone.
        eprintln!("git-vista: /api/select-worktree registered {path} but could not resolve its id");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "That worktree was admitted but could not be opened.".to_string(),
        )
    }
}

/// The plain-text body this route answers a [`WorktreeCensus::CensusFailed`]
/// with — a free function so it can be tested as a *string*, which is the one
/// thing a status-code assertion cannot check (grok, reviewing PR #658).
///
/// # `reason` alone, never `detail`
///
/// The census has already written the full detail to the server's log, and
/// `detail` reaches a client only when the operator opted in. This route
/// answers plain text rather than JSON, so there is no field a client could
/// choose not to read — appending the detail here would be the one place the
/// flag could be bypassed by accident, and it would be bypassed for every
/// operator rather than only for one who asked. An operator who wants the path
/// has the log and `GET /api/worktrees`, both of which honour the flag.
fn census_failure_body(reason: &str) -> String {
    format!("Couldn't read this repository's worktrees, so nothing was selected: {reason}")
}

/// Re-scan the configured repo root, the clones root and the managed
/// worktrees root (ADR 0009/0008/0118).
/// Bodyless POST, like `rebase`. Registered entries and the current selection
/// are untouched; this only adds/refreshes entries.
pub(crate) async fn rescan() -> (StatusCode, String) {
    // Repo-root scan first, clones-root scan second — same order as startup,
    // so the clones-root scan wins any path both roots would register
    // (keeping the `read_only` clone marker accurate) on a rescan too.
    let repo_result = scan_repo_root();
    // ADR 0009 list form: same position as startup — after the root scan, before
    // the clones scan — so a path named by more than one source lands with the
    // same final flags a fresh boot would give it.
    let (listed, listed_skipped) = register_repo_list();
    let (clones_registered, _) = scan_clones_root();
    // ADR 0118: same position as startup. A rescan that skipped this would
    // leave the managed root un-admitted for the rest of the process whenever
    // the root did not exist at boot — which is precisely the fresh-install
    // case the fix is about.
    let (desks_registered, _) = scan_worktrees_root();
    let listed_note = if listed > 0 || listed_skipped > 0 {
        format!(" {listed} listed repo(s) registered, {listed_skipped} skipped;")
    } else {
        String::new()
    };
    let summary = match repo_result {
        Some((registered, skipped)) => format!(
            "Rescanned: {registered} repos registered, {skipped} skipped;\
            {listed_note} {clones_registered} clone(s) re-registered; \
            {desks_registered} linked worktree(s) re-registered."
        ),
        None => format!(
            "No repo root configured;{listed_note} \
             {clones_registered} clone(s) re-registered; \
             {desks_registered} linked worktree(s) re-registered."
        ),
    };
    (StatusCode::OK, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::RepoMode;

    #[tokio::test]
    async fn select_refuses_a_malformed_and_an_unknown_id() {
        let (status, _) = select_repo(axum::Json(SelectRequest {
            worktree: "not-an-id".into(),
            mode: RepoMode::Visualize,
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, msg) = select_repo(axum::Json(SelectRequest {
            // Valid id shape, never registered → fail-closed 404.
            worktree: "99999999-9999-5999-8999-999999999999".into(),
            mode: RepoMode::Active,
        }))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(msg, "No such repository.");
    }
}

#[cfg(test)]
mod worktree_admission_tests {
    /// `state.rs`'s source, read back. The property below is about what a
    /// function does **not** call, and a call that is absent cannot be
    /// asserted by running anything — the mutation that adds it produces a
    /// wider fence, not a wrong value, so every behavioural test still passes.
    const STATE: &str = include_str!("../state.rs");

    /// `register_discovered_worktree`'s body, with `//` comment lines dropped
    /// and whitespace collapsed — so the comparison below is about what the
    /// function *does*, not how it is laid out or documented.
    fn register_body_normalised() -> String {
        let after = STATE
            .split_once("pub(crate) fn register_discovered_worktree(")
            .expect("state.rs no longer defines `register_discovered_worktree`")
            .1;
        let end = after
            .find("\n}\n")
            .expect("`register_discovered_worktree` is no longer a closed block");
        after[..end]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .flat_map(|line| line.split_whitespace())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// **The security property of M11.03 (#548), pinned as an exact body.**
    ///
    /// `register_explicit` allows a root and then registers under it, which is
    /// right for a path an operator named on the command line. Doing the same
    /// for a *discovered* worktree would make "git listed this directory"
    /// sufficient to widen the fence — and creating a worktree would become a
    /// way to make this app serve any directory on the filesystem. That is the
    /// option `docs/superpowers/specs/m3.23-worktrees.md` §1 weighs and
    /// rejects in as many words.
    ///
    /// The whole guarantee is therefore an **omission**: this function must
    /// call `register` and must widen nothing first. An omission has no
    /// runtime signature — adding the call makes the app serve *more*, never
    /// less, so no existing test goes red and nothing looks wrong. Reading the
    /// source is the only place it can be caught.
    ///
    /// # Why exact, and not a list of forbidden calls
    ///
    /// This test used to require `.register(` and forbid the substring
    /// `allow_root`. Grok's round-6 review found the hole: the *other*
    /// widening API in that same file is `allow_repo_root`, and the string
    /// `allow_repo_root` does **not contain** `allow_root` — the shorter name
    /// is not a substring of the longer one. A body calling
    /// `allow_repo_root(path)` and then `.register(path, …)` would have stayed
    /// green while widening the fence, and the paired catalog test would have
    /// stayed green too, because `register` still refuses — *after* the new
    /// root was allowed.
    ///
    /// Lengthening the denylist (`allow_repo_root`, `register_explicit`, …)
    /// would fix that instance and leave the shape intact: a denylist is a
    /// list someone has to remember to extend, and the next widening API to be
    /// added would not be on it. An exact body has nothing to keep complete.
    ///
    /// This test failing is not necessarily a bug. It means a
    /// security-critical function changed, and the change wants a human to
    /// look at it and update the literal deliberately. That is the intended
    /// cost.
    #[test]
    fn admitting_a_discovered_worktree_never_widens_the_allowed_roots() {
        const EXPECTED: &str = concat!(
            "path: &Path, read_only: bool, ",
            ") -> Result<RepositoryHandle, CatalogError> { ",
            "catalog() .write() .expect(\"catalog lock\") .register(path, read_only)",
        );
        assert_eq!(
            register_body_normalised(),
            EXPECTED,
            "\n`register_discovered_worktree` changed. Its guarantee is an OMISSION: it \
             admits a path to the catalog and must widen nothing on the way. Adding any \
             root-allowing call here — `allow_root`, `allow_repo_root`, \
             `register_explicit`, or one that does not exist yet — makes discovering a \
             worktree enough to widen the fence, and no behavioural test will notice. \
             Read the diff, confirm it does not widen, then update this literal \
             deliberately.\n"
        );
    }

    /// The paired positive for the pin above. An exact-body assertion is only
    /// worth having if the body it pins does the safe thing, and a literal
    /// someone updates without thinking would satisfy it either way. This
    /// names the one fact the literal exists to protect, so the update still
    /// has to keep it true.
    #[test]
    fn the_pinned_body_admits_through_the_catalog_and_nothing_else() {
        let body = register_body_normalised();
        assert!(
            body.contains(".register(path, read_only)"),
            "`register_discovered_worktree` no longer admits anything through \
             `Catalog::register`: {body}"
        );
    }

    /// The **second half of "enforced twice"**, which lives in another file and
    /// which the exact-body pin above cannot see.
    ///
    /// The census marks a sibling `Serviceable::Yes` only when its canonical
    /// path is already inside an allowed root; `Catalog::register` then asks
    /// the same question again, independently, and fails closed. Grok's review
    /// confirmed the second check is not weaker than the first — it re-derives
    /// `facts.root` itself rather than trusting the caller's `Serviceable`.
    ///
    /// If that refusal ever goes away, the exact body above is still exactly
    /// right and the fence is gone anyway. So it is asserted here, separately,
    /// and a mutation removing it lands on *this* assertion rather than that
    /// one.
    #[test]
    fn the_registration_it_calls_is_the_one_that_fails_closed() {
        const CATALOG: &str = include_str!("../catalog.rs");
        assert!(
            CATALOG.contains("return Err(CatalogError::OutsideAllowedRoots);"),
            "`Catalog::register` no longer refuses a path outside the allowed \
             roots, so the second half of this route's defence is gone — and the \
             first half is a type, which cannot notice"
        );
    }

    // -----------------------------------------------------------------------
    // #658 follow-up (grok): the failure body, tested as a string
    // -----------------------------------------------------------------------

    use super::census_failure_body;
    use git_vista_protocol::WorktreeCensus;

    /// **Route 2, driven from a real failure rather than a fabricated reason.**
    ///
    /// The census is taken here with [`CensusPaths::rows_for_local_use`], which
    /// is exactly the conflation #657 was about — this route's own local need
    /// for row paths must not decide what a failure discloses. So the fixture
    /// uses that constructor, not `from_flag`: a regression that made
    /// `rows_for_local_use` publish the detail would show up in *this* body,
    /// and this is the body a user reads.
    #[tokio::test]
    async fn the_failure_body_this_route_answers_carries_no_path() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_string_lossy().into_owned();

        let census = crate::worktree_census::worktree_census(
            dir.path(),
            crate::worktree_census::CensusPaths::rows_for_local_use(false),
            &|_: &std::path::Path| true,
        )
        .await;
        let WorktreeCensus::CensusFailed { reason, .. } = census else {
            panic!("the fixture must actually fail the census");
        };

        let body = census_failure_body(&reason);
        assert!(
            body.starts_with("Couldn't read this repository's worktrees"),
            "the sentence a user reads must still say what happened: {body}"
        );
        assert!(
            !body.contains(&here),
            "POST /api/select-worktree's body named `{here}`: {body}"
        );
    }

    /// The paired positive, and the one that keeps the redaction honest: an
    /// opted-in operator's census carries a detail, and this route still does
    /// not put it in the body. Withholding it here is a deliberate choice
    /// (plain text has no field to ignore), so it is asserted rather than left
    /// to a comment.
    #[tokio::test]
    async fn even_an_opted_in_census_detail_stays_out_of_this_routes_body() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_string_lossy().into_owned();

        let census = crate::worktree_census::worktree_census(
            dir.path(),
            crate::worktree_census::CensusPaths::rows_for_local_use(true),
            &|_: &std::path::Path| true,
        )
        .await;
        let WorktreeCensus::CensusFailed { reason, detail } = census else {
            panic!("the fixture must actually fail the census");
        };
        assert!(
            detail.is_some_and(|d| d.contains(&here)),
            "the fixture must have produced a detail, or this proves nothing"
        );
        assert!(!census_failure_body(&reason).contains(&here));
    }
}
