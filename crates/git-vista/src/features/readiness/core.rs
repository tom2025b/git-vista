//! Whether the full-screen viewer is currently showing a placeholder instead
//! of content (#387) — the predicate `viewer.rs` stamps onto `aria-busy` at
//! the viewer's outer `<div>`.
//!
//! `viewer.rs`'s `body` closure renders `"Loading…"` in six arms: one
//! genuine "no data yet" (the resource has not resolved at all), and five
//! **staleness echoes** — a resource resolved successfully, but for a
//! document that is no longer the one open. ADR 0053's rule is that such a
//! late answer to a superseded request is dropped rather than painted, and
//! each of those five arms is that rule applied once per document kind. The
//! readiness predicate below is a derived boolean over exactly that same
//! decision — not a new source of truth — which is why it takes the same two
//! facts the `body` match already has: what document is open, and what the
//! fetch settled on.
//!
//! Framework-free and host-tested, per the `features/*/core.rs` convention:
//! no Leptos, no `crate::state`, no `#[cfg(target_arch = "wasm32")]`. The
//! types below deliberately do **not** hold the full response payloads
//! (`CommitDiff`, `FileContent`, `ConflictPanes`, `StagingDiff`) — the
//! staleness check `viewer.rs` makes never looks past each payload's
//! identity (an id, a path, a spec), so [`DocIdentity`] carries only that.
//! Reducing a live payload down to its `DocIdentity`, and reading
//! `crate::state::ViewerDoc`/the resource to build a [`FetchOutcome`], is
//! data-only marshalling with no decision in it — that wiring lives in
//! `viewer.rs` itself and is the one part the browser leg, not this module,
//! proves.

/// The identity a document carries — reduced from `crate::state::ViewerDoc`
/// to exactly the fields `viewer.rs`'s own staleness check compares.
/// Serves double duty as both "what's open" and "what a fetch resolved to",
/// which is what makes the equality in [`is_viewer_busy`] the same
/// comparison `viewer.rs`'s `matches!` guards already make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocIdentity {
    Diff {
        id: String,
    },
    File {
        id: String,
        path: String,
    },
    /// Carries no direction, deliberately. `viewer.rs`'s own staleness
    /// check for this arm is:
    ///
    /// ```ignore
    /// let ViewerDoc::Staging { direction } = which_for_body else {
    ///     return /* Loading… */;
    /// };
    /// ```
    ///
    /// — it only asks "is a Staging document still open at all", because
    /// `StagingDiff` (the response payload) has no direction field to echo
    /// back for comparison. A resolved Staging response is therefore
    /// treated as matching *either* direction the viewer might currently
    /// have open. That is a real gap in `viewer.rs` (switching Stage↔Unstage
    /// while a fetch for the old direction is in flight can paint the wrong
    /// diff under the new direction's label) but it is not this predicate's
    /// job to close it: the design's own contract is "derive readiness from
    /// the same information the match uses", and adding a direction
    /// comparison here that the match itself doesn't make would report busy
    /// for a state `viewer.rs` renders as settled — a readiness signal that
    /// disagrees with what actually painted.
    Staging,
    Spec {
        spec: git_vista_protocol::diff::DiffSpec,
    },
    Conflict {
        path: String,
    },
    /// M5.33 (#86): echoes `BlamePage::path`/`BlamePage::rev`, the same
    /// response-echoes-request posture `File` and `Spec` already use.
    Blame {
        path: String,
        rev: String,
    },
}

/// What the viewer's resource has settled on for the currently-open
/// document, at the granularity the readiness predicate needs.
///
/// `Err` deliberately carries no [`DocIdentity`]: `viewer.rs` renders every
/// error the same way (`"Couldn't load: {e}"`) regardless of which document
/// it was fetched for, so an error is never a placeholder and staleness
/// never enters into it — see [`is_viewer_busy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The resource has not resolved at all yet (`doc.get().flatten()` is
    /// `None`). The one genuine "no data yet" arm.
    Pending,
    /// The fetch failed.
    Err,
    /// The fetch succeeded, for the document named by this identity.
    Ok(DocIdentity),
}

/// The readiness predicate: would the viewer's body currently render a
/// `"Loading…"` placeholder for `open`, given that its resource settled on
/// `outcome`?
///
/// Busy exactly when [`FetchOutcome::Pending`] (nothing has come back yet)
/// or when [`FetchOutcome::Ok`] names a document that is **not** `open` (a
/// staleness echo — the subtle case, and the one this predicate exists to
/// get right). Not busy when the fetch succeeded for `open` itself, or when
/// it failed — an error is rendered as its own message, never as a loading
/// placeholder, so it reads as settled either way.
pub fn is_viewer_busy(open: &DocIdentity, outcome: &FetchOutcome) -> bool {
    match outcome {
        FetchOutcome::Pending => true,
        FetchOutcome::Err => false,
        FetchOutcome::Ok(got) => got != open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::diff::DiffSpec;
    use git_vista_protocol::CommitOid;

    fn diff(id: &str) -> DocIdentity {
        DocIdentity::Diff { id: id.to_string() }
    }

    fn file(id: &str, path: &str) -> DocIdentity {
        DocIdentity::File {
            id: id.to_string(),
            path: path.to_string(),
        }
    }

    fn blame(path: &str, rev: &str) -> DocIdentity {
        DocIdentity::Blame {
            path: path.to_string(),
            rev: rev.to_string(),
        }
    }

    #[test]
    fn a_blame_response_for_a_different_path_reads_busy_like_any_other_stale_echo() {
        assert!(is_viewer_busy(
            &blame("a.rs", "HEAD"),
            &FetchOutcome::Ok(blame("b.rs", "HEAD"))
        ));
        assert!(!is_viewer_busy(
            &blame("a.rs", "HEAD"),
            &FetchOutcome::Ok(blame("a.rs", "HEAD"))
        ));
    }

    #[test]
    fn nothing_has_come_back_yet_reads_busy() {
        assert!(is_viewer_busy(&diff("abc123"), &FetchOutcome::Pending));
    }

    #[test]
    fn a_fresh_matching_response_reads_settled() {
        assert!(!is_viewer_busy(
            &diff("abc123"),
            &FetchOutcome::Ok(diff("abc123"))
        ));
    }

    /// The subtle case the design doc calls out by name: a resource
    /// resolved *successfully* (`Ok`, not `Pending`), but for a document
    /// that is no longer the one open — ADR 0053's staleness echo. This
    /// must still read busy, exactly like `Pending`, not like a settled
    /// response, or a late answer to a superseded request would clear
    /// `aria-busy` for content that was never painted.
    #[test]
    fn a_stale_ok_response_for_a_different_id_still_reads_busy() {
        assert!(is_viewer_busy(
            &diff("abc123"),
            &FetchOutcome::Ok(diff("stale-older-id"))
        ));
    }

    /// Staleness spans document *kinds*, not just ids within one kind —
    /// `viewer.rs`'s `matches!` fails just as surely when the variant itself
    /// no longer matches (e.g. the viewer moved from a Diff to a File while
    /// the old Diff fetch was still in flight).
    #[test]
    fn a_stale_response_of_a_different_document_kind_reads_busy() {
        let open = file("abc123", "src/main.rs");
        let stale = diff("abc123");
        assert!(is_viewer_busy(&open, &FetchOutcome::Ok(stale)));
    }

    #[test]
    fn an_error_reads_settled_even_when_it_answers_a_since_superseded_document() {
        // viewer.rs's Err arm renders unconditionally — it never checks
        // identity — so this is settled (not busy) regardless of `open`.
        assert!(!is_viewer_busy(&diff("abc123"), &FetchOutcome::Err));
    }

    /// Documents the gap described on [`DocIdentity::Staging`]: any resolved
    /// Staging response matches an open Staging document, full stop — there
    /// is no direction to disagree about at this predicate's granularity,
    /// because `viewer.rs` itself has none to compare.
    #[test]
    fn any_resolved_staging_response_matches_an_open_staging_document() {
        assert!(!is_viewer_busy(
            &DocIdentity::Staging,
            &FetchOutcome::Ok(DocIdentity::Staging)
        ));
    }

    #[test]
    fn spec_mismatch_reads_busy() {
        let commit = CommitOid::new("a".repeat(40)).expect("40 hex chars is valid");
        let open = DocIdentity::Spec {
            spec: DiffSpec::WorktreeVsIndex,
        };
        let resolved = DocIdentity::Spec {
            spec: DiffSpec::IndexVsCommit { commit },
        };
        assert!(is_viewer_busy(&open, &FetchOutcome::Ok(resolved)));
    }

    #[test]
    fn conflict_path_mismatch_reads_busy() {
        let open = DocIdentity::Conflict {
            path: "src/a.rs".to_string(),
        };
        let resolved = DocIdentity::Conflict {
            path: "src/b.rs".to_string(),
        };
        assert!(is_viewer_busy(&open, &FetchOutcome::Ok(resolved)));
    }
}
