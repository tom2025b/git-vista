//! The stash drawer's HTTP surface (M3.24, #77).
//!
//! Six endpoints: two reads and four writes.
//!
//! | endpoint | body | operation |
//! |---|---|---|
//! | `GET  /api/stashes` | → `Vec<`[`StashEntry`]`>` | list the drawer — unconditionally safe |
//! | `GET  /api/stash/show` | → patch text | read one entry, and only read it |
//! | `POST /api/stash/push` | [`PushStashRequest`] | [`GitOperation::PushStash`] |
//! | `POST /api/stash/apply` | [`StashTarget`] | [`GitOperation::ApplyStash`] |
//! | `POST /api/stash/drop` | [`StashTarget`] | [`GitOperation::DropStash`] |
//! | `POST /api/stash/branch` | [`BranchFromStashRequest`] | [`GitOperation::BranchFromStash`] |
//!
//! # Every shape in that table is a `git-vista-protocol` DTO (#495, ADR 0079)
//!
//! Not one of them used to be. The listing was built by hand with
//! `serde_json::json!` right here, each write body was declared here and again
//! in the frontend's `api/stash.rs`, and every field name existed twice in the
//! workspace with nothing forcing the copies to agree. A rename on either side
//! did not fail: the field deserialized as absent and the drawer rendered
//! empty — "no stashes", which is the one thing `git-vista-git`'s
//! `read_stashes` refuses to say when it means "couldn't look".
//!
//! What follows from sharing them is that **this file has no validation left**.
//! Every field arrives as the type its operation wants, checked by the
//! newtype's own `Deserialize`, so there is no `::new` call here for someone to
//! delete and no test that could pass by testing the newtype instead of the
//! endpoint.
//!
//! **There is no `/api/stash/pop`.** Pop is apply-then-drop, and a single
//! operation row cannot tell the truth about the half-done state: apply
//! succeeds, drop fails, and the record says only `Failed` — indistinguishable
//! from "nothing happened" while the user's changes are actually in the tree.
//! Two independent operations produce two rows, and two rows can say "applied,
//! then the drop failed". See `GitOperation`'s comment on the absent
//! `PopStash` in `plan.rs`.
//!
//! # The selector/oid split, restated here because this is where clients meet it
//!
//! Every write takes [`StashTarget`]'s two fields — `entry` (a positional
//! `stash@{n}`) **and** `expected_oid`.
//! The selector is the address and is what reaches git; the oid is the witness
//! and is compare-and-swapped against a fresh resolve immediately before the
//! mutation runs. A client that sends only one of them cannot be served: an oid
//! alone is not a valid argument to `git stash drop`, and a selector alone
//! renumbers on every drop, so acting on it would eventually delete a stash
//! nobody chose.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{
    BranchFromStashRequest, CommitOid, GitOperation, PushStashRequest, StashEntry, StashSelector,
    StashTarget,
};

use crate::planner;
use crate::state::reject_if_read_only;

/// `GET /api/stash/show?entry=stash@{N}` — the patch a stash entry holds
/// (M3.24 #77).
///
/// # The criterion: "stash content is inspectable before apply or drop"
///
/// Before this, the only way to learn what an entry contained was to apply it
/// and look — which is exactly the thing a user wants to avoid deciding
/// blindly, and it is irreversible in the drop case. A stash you cannot read
/// is a stash you cannot safely discard.
///
/// # A read, and only a read
///
/// `git stash show -p` resolves the entry and prints a diff. It writes
/// nothing, touches no index and no worktree, so this needs no plan and no
/// `GitOperation` — the same posture every other diff read in this server
/// takes.
///
/// The flag set matters and is the same one every diff read here uses:
/// `--no-color` so a `color.ui = always` config cannot inject escapes into
/// text rendered as-is, and `--no-textconv` because a repository's own
/// `.gitattributes` can bind a textconv filter that git would then *execute*
/// to render content.
pub(crate) async fn show_stash(
    axum::extract::Query(q): axum::extract::Query<ShowStashQuery>,
) -> (StatusCode, String) {
    let (repo, _read_only) = crate::state::current();
    let entry = q.entry;

    // `--` is not applicable here (the argument is a revision, not a path),
    // but the selector newtype has already refused anything that is not
    // `stash@{N}`, so no argument can be read as an option.
    let out = match crate::git_cmd::git_output(
        &repo,
        &[
            "stash",
            "show",
            "--patch",
            "--no-color",
            "--no-textconv",
            entry.as_str(),
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't run git stash show: {e}"),
            )
        }
    };

    if !out.status.success() {
        // Most often a selector that no longer resolves, because every drop
        // renumbers the list. Say that, rather than passing git's wording
        // through and leaving the user to infer it.
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return (
            StatusCode::NOT_FOUND,
            format!(
                "{} could not be read — {msg}\n\nStash entries renumber on every \
                 drop, so a selector held from an earlier listing may now point \
                 somewhere else or nowhere. Re-read the list and try again.",
                entry
            ),
        );
    }

    (
        StatusCode::OK,
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Query for [`show_stash`].
///
/// Deliberately **not** `deny_unknown_fields`, for the same reason
/// `read.rs`'s `PageQuery` is not: the frontend appends its own
/// `?t=<millis>` cache-buster to every GET it makes (`api/stash.rs`, and
/// nine other call sites in `crates/git-vista/src/api/`), and a read must
/// never answer that with a 400.
///
/// It shipped closed, and the drawer's whole "Show changes" control was dead
/// on arrival — every click answered
/// `Failed to deserialize query string: t: unknown field \`t\`, expected
/// \`entry\``, rendered into the panel where the patch belonged. Nothing in
/// the Rust suite could see it: the handler was only ever called with the
/// query a test composed by hand, never the one the browser sends.
///
/// The closed-DTO rule this looked like it was following is about **write
/// bodies** reaching the argv boundary (`argv_boundary::dto_gates`), where an
/// unknown key is a smuggling attempt. This is a GET query whose one field is
/// already refused by [`StashSelector`] unless it is exactly `stash@{N}`, so an
/// extra key buys an attacker nothing at all.
///
/// # The one stash shape that is not in `git-vista-protocol` (#495, ADR 0079)
///
/// Every stash *body* is now a shared DTO both ends deserialize. A query
/// string is not a body: the frontend builds this URL with
/// `js_sys::encode_uri_component`, and serializing a shared type into a query
/// instead would need `serde_urlencoded` as a dependency of the wasm crate,
/// which is a bigger change than the duplication it removes. What *is* shared
/// is the part that can be wrong in a way nothing catches — the field's
/// **type**. `entry` is a [`StashSelector`], so it deserializes through the
/// same validator the write bodies use and the handler has no `::new` call
/// left to delete. The name is pinned by the three tests below, which send the
/// query string the browser actually sends.
#[derive(serde::Deserialize)]
pub(crate) struct ShowStashQuery {
    /// The `stash@{N}` selector, exactly as the list returned it.
    pub(crate) entry: StashSelector,
}

#[cfg(test)]
mod show_stash_query_tests {
    use super::ShowStashQuery;

    /// The exact query string the browser sends, cache-buster and all.
    ///
    /// Pinned against the live shape rather than a tidy one: `entry` alone
    /// deserialized perfectly well while the feature was broken in the app.
    #[test]
    fn the_frontend_cache_buster_does_not_make_the_read_a_400() {
        let q: ShowStashQuery = serde_urlencoded::from_str("entry=stash%40%7B0%7D&t=1756112884123")
            .expect("the `?t=` cache-buster every frontend GET appends must not 400");
        assert_eq!(
            q.entry.as_str(),
            "stash@{0}",
            "the selector must survive percent-decoding intact"
        );
    }

    /// Order is the client's to choose, and `js_sys::Date::now()` yields a
    /// float — `1756112884123.4` is a legal thing for it to send.
    #[test]
    fn neither_the_parameter_order_nor_a_fractional_millisecond_matters() {
        let q: ShowStashQuery =
            serde_urlencoded::from_str("t=1756112884123.4&entry=stash%40%7B12%7D")
                .expect("a cache-buster is opaque to this handler wherever it sits");
        assert_eq!(q.entry.as_str(), "stash@{12}");
    }

    /// The one field that IS this DTO's business still has to be there.
    #[test]
    fn a_query_with_no_entry_is_still_refused() {
        assert!(
            serde_urlencoded::from_str::<ShowStashQuery>("t=1756112884123").is_err(),
            "`entry` is required — tolerating unknown keys is not tolerating a missing one"
        );
    }
}

/// `GET /api/stashes` — the drawer, newest first.
///
/// A read, so it is not `full_routes`-gated and the LAN router sees it. An app
/// that can *show* the stash list is useful before any write path exists, which
/// is why the read shipped first.
pub(crate) async fn stash_list() -> (StatusCode, String) {
    let (repo, _read_only) = crate::state::current();
    match git_vista_git::stash::read_stashes(&repo) {
        Ok(records) => {
            let entries: Result<Vec<StashEntry>, _> = records.iter().map(listing_entry).collect();
            let entries = match entries {
                Ok(entries) => entries,
                // A record this server cannot express on the wire is a failure
                // to read the drawer, not an entry to leave out: a shorter
                // list renumbers everything below the gap, and the number is
                // the address the user acts on.
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("could not read the stash list: {e}"),
                    )
                }
            };
            match serde_json::to_string(&entries) {
                Ok(json) => (StatusCode::OK, json),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not serialise the stash list: {e}"),
                ),
            }
        }
        // An unreadable drawer is an error, never an empty list. "No stashes"
        // and "could not look" authorise different things in the UI, and the
        // git crate already keeps them apart — this must not re-merge them.
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not read the stash list: {e}"),
        ),
    }
}

/// The mapping boundary `git-vista-git`'s module doc names: git-shaped facts
/// in, the shared wire DTO out (#495, ADR 0079).
///
/// `git-vista-git` deliberately does not depend on `git-vista-protocol`, so
/// this is where a [`StashRecord`](git_vista_git::stash::StashRecord) becomes
/// something both ends of the wire deserialize. Two things happen here and
/// nowhere else:
///
/// - the position becomes a selector, through [`StashSelector::at`] — the one
///   author of the `stash@{N}` spelling. The wire no longer carries the
///   position beside it; see [`StashEntry`] for why a derivable field was the
///   worse of the two options.
/// - the oid is validated. `gix` gives back a hex id and this cannot fail in
///   practice, but "cannot fail in practice" is not a reason to `unwrap` in a
///   read that a whole panel depends on — the caller turns an `Err` into a
///   500, which is the honest answer for a drawer that could not be read.
fn listing_entry(
    record: &git_vista_git::stash::StashRecord,
) -> Result<StashEntry, git_vista_protocol::PlanFieldError> {
    Ok(StashEntry {
        entry: StashSelector::at(record.index),
        oid: CommitOid::new(record.oid.0.clone())?,
        message: record.message.clone(),
        time: record.time,
    })
}

#[cfg(test)]
mod listing_tests {
    use super::listing_entry;
    use git_vista_core::model::Oid;
    use git_vista_git::stash::StashRecord;
    use git_vista_protocol::StashEntry;

    fn record(index: usize, oid: &str) -> StashRecord {
        StashRecord {
            index,
            oid: Oid(oid.to_string()),
            message: "WIP on main: 1a2b3c4 tidy the parser".to_string(),
            time: 1_700_000_000,
        }
    }

    /// The round trip the two hand-written copies could never make: a record
    /// goes through the real mapping, out as JSON, and back in as the type the
    /// **frontend** parses — with the JSON itself pinned to a literal in
    /// between, so this cannot pass by agreeing with itself.
    ///
    /// The literal is the point. Both ends now share one type, so
    /// `to_string` → `from_str` would round-trip happily through any rename;
    /// what a browser sees is the bytes, and those are asserted here.
    ///
    /// MUTATION 1 (rename): `entry` → `selector` in [`StashEntry`]. RED — the
    ///   serialised bytes stop matching the literal.
    /// MUTATION 2 (retype): `time: i64` → `time: String` in [`StashEntry`].
    ///   RED — this file stops compiling at `time: record.time`, which is the
    ///   same failure one process earlier.
    #[test]
    fn a_record_becomes_the_wire_bytes_the_frontend_parses() {
        let mapped =
            listing_entry(&record(0, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")).expect("maps");

        const WIRE: &str = concat!(
            r#"{"entry":"stash@{0}","#,
            r#""oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","#,
            r#""message":"WIP on main: 1a2b3c4 tidy the parser","#,
            r#""time":1700000000}"#
        );
        assert_eq!(
            serde_json::to_string(&mapped).unwrap(),
            WIRE,
            "these are the bytes the drawer reads; changing them is a wire change"
        );

        let parsed: StashEntry = serde_json::from_str(WIRE).expect("the frontend must parse this");
        assert_eq!(parsed, mapped, "field for field, both directions");
    }

    /// The selector is built from the record's own position, and the drawer's
    /// order is the reflog's order — so entry *k* of the response is
    /// `stash@{k}`, and nothing downstream re-derives it.
    ///
    /// MUTATION: build the selector from the iteration order rather than
    ///   `record.index` — RED here, because these records deliberately carry
    ///   positions the enumeration does not match.
    #[test]
    fn each_entry_is_addressed_by_its_own_recorded_position() {
        // A drawer read mid-drop: the git crate stops at an unreadable line
        // rather than renumbering, so a caller may legitimately see a gap.
        let records = [
            record(0, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            record(3, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ];
        let mapped: Vec<_> = records.iter().map(|r| listing_entry(r).unwrap()).collect();
        assert_eq!(mapped[0].entry.as_str(), "stash@{0}");
        assert_eq!(
            mapped[1].entry.as_str(),
            "stash@{3}",
            "the address is the record's position, not its place in the array"
        );
    }

    /// A record the wire cannot express fails the whole read rather than
    /// vanishing from it. An entry silently dropped from a listing shifts
    /// every entry below it, and the position is what the user's next click
    /// acts on — the same "no stashes" / "couldn't look" merge the git crate
    /// refuses to make.
    #[test]
    fn an_unrepresentable_oid_fails_the_read_instead_of_shortening_the_list() {
        assert!(
            listing_entry(&record(0, "not-a-hex-object-id")).is_err(),
            "a malformed oid must not reach the wire, nor be quietly skipped"
        );
    }
}

/// `POST /api/stash/push` — put the working tree in the drawer.
///
/// Every field of [`PushStashRequest`] is already the type the operation
/// wants, so there is nothing to validate here and nothing to forget to
/// validate: an absent message is git's own `WIP on <branch>` line, a blank
/// one is refused by [`StashMessage`](git_vista_protocol::StashMessage) at the
/// wire boundary, and neither flag has a default a client could stop sending.
///
/// [`StashMessage`]: git_vista_protocol::StashMessage
pub(crate) async fn push_stash(Json(req): Json<PushStashRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::PushStash {
        message: req.message,
        keep_index: req.keep_index,
        include_untracked: req.include_untracked,
    })
    .await
}

/// `POST /api/stash/apply` — restore a stash's changes, keeping the entry.
///
/// Gated on `CleanWorktree` by the plan builder. That is the load-bearing
/// decision of this slice: with a clean tree the abort path is `reset --hard`
/// plus `clean -fd`, and that is provably safe because there is nothing of the
/// user's to destroy.
///
/// The body is [`StashTarget`] — the same one declaration drop and branch take
/// (#495, ADR 0079). Both halves are required and both arrive validated, so
/// this path cannot drift from theirs on what a valid entry looks like: there
/// is no parse step here to drift.
pub(crate) async fn apply_stash(Json(req): Json<StashTarget>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::ApplyStash {
        entry: req.entry,
        expected_oid: req.expected_oid,
    })
    .await
}

/// `POST /api/stash/branch` (M3.24 #77) — the escape hatch for a stash that
/// will not apply where you are now.
///
/// Carries the branch name alongside the usual selector/oid pair — the latter
/// nested as [`StashTarget`] rather than respelled, so this endpoint and
/// apply/drop cannot disagree about what a valid entry is (#495, ADR 0079).
///
/// The name is a [`BranchName`](git_vista_protocol::BranchName), validated by
/// its own newtype at the wire boundary, so a malformed one is refused before
/// a plan exists and without anything being consumed.
pub(crate) async fn branch_from_stash(
    Json(req): Json<BranchFromStashRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    crate::planner::plan_and_execute(GitOperation::BranchFromStash {
        name: req.name,
        entry: req.target.entry,
        expected_oid: req.target.expected_oid,
    })
    .await
}

/// `POST /api/stash/drop` — discard an entry.
///
/// `Destructive`, and the compare-and-swap in the executor is what stands
/// between this and dropping a stash the user never chose: every drop
/// renumbers the list, so a selector planned seconds ago may now address
/// someone else's work.
pub(crate) async fn drop_stash(Json(req): Json<StashTarget>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::DropStash {
        entry: req.entry,
        expected_oid: req.expected_oid,
    })
    .await
}
