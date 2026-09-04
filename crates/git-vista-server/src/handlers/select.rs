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

use crate::state::{register_repo_list, scan_clones_root, scan_repo_root, select_registered};

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
/// `expose_paths: true` on that internal census is **not a disclosure**.
/// Nothing from it is serialized to the client — this handler answers with a
/// status and a sentence it composes itself — and the path is needed locally
/// because registration takes a path. The operator's `GIT_VISTA_EXPOSE_PATHS`
/// opt-in governs what leaves the process, not what the process may know about
/// its own repository.
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
    let census =
        crate::worktree_census::worktree_census(&repo, true, &crate::state::path_is_allowed).await;
    let siblings = match census {
        WorktreeCensus::Observed { siblings } => siblings,
        WorktreeCensus::CensusFailed { reason } => {
            eprintln!("git-vista: /api/select-worktree could not read the census: {reason}");
            return (
                StatusCode::CONFLICT,
                format!(
                    "Couldn't read this repository's worktrees, so nothing was selected: {reason}"
                ),
            );
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

/// Re-scan the configured repo root and the clones root (ADR 0009/0008).
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
    let listed_note = if listed > 0 || listed_skipped > 0 {
        format!(" {listed} listed repo(s) registered, {listed_skipped} skipped;")
    } else {
        String::new()
    };
    let summary = match repo_result {
        Some((registered, skipped)) => format!(
            "Rescanned: {registered} repos registered, {skipped} skipped;\
            {listed_note} {clones_registered} clone(s) re-registered."
        ),
        None => format!(
            "No repo root configured;{listed_note} \
             {clones_registered} clone(s) re-registered."
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

    /// The body of `register_discovered_worktree`.
    fn register_body() -> String {
        let after = STATE
            .split_once("pub(crate) fn register_discovered_worktree(")
            .expect("state.rs no longer defines `register_discovered_worktree`")
            .1;
        let end = after
            .find("\n}\n")
            .expect("`register_discovered_worktree` is no longer a closed block");
        after[..end].to_string()
    }

    /// **The security property of M11.03 (#548), stated where it can be
    /// checked.**
    ///
    /// `register_explicit` allows a root and then registers under it, which is
    /// right for a path an operator named on the command line. Doing the same
    /// for a *discovered* worktree would make "git listed this directory"
    /// sufficient to widen the fence — and creating a worktree would become a
    /// way to make this app serve any directory on the filesystem. That is the
    /// second of the three options `docs/superpowers/specs/m3.23-worktrees.md`
    /// §1 weighs, and the one it rejects in as many words.
    ///
    /// The whole guarantee is therefore an **omission**: this function must
    /// call `register` and must not call `allow_root`. An omission has no
    /// runtime signature — adding the call makes the app serve *more*, never
    /// less, so no existing test goes red and nothing looks wrong. Reading the
    /// source is the only place this can be caught.
    #[test]
    fn admitting_a_discovered_worktree_never_widens_the_allowed_roots() {
        let body = register_body();
        assert!(
            body.contains(".register("),
            "`register_discovered_worktree` no longer registers anything:\n{body}"
        );
        assert!(
            !body.contains("allow_root"),
            "`register_discovered_worktree` allows a new root. Discovering a \
             worktree would then be enough to widen the fence, which is how \
             `git worktree add` becomes a way to make this app serve any \
             directory:\n{body}"
        );
    }

    /// The paired positive: the fence exists to be hit, and the function that
    /// enforces it is the one this calls. Without this, the assertion above is
    /// satisfied by a `register_discovered_worktree` that does nothing at all.
    #[test]
    fn the_registration_it_calls_is_the_one_that_fails_closed() {
        const CATALOG: &str = include_str!("../catalog.rs");
        assert!(
            CATALOG.contains("return Err(CatalogError::OutsideAllowedRoots);"),
            "`Catalog::register` no longer refuses a path outside the allowed \
             roots, so the second half of this route's defence is gone"
        );
    }
}
