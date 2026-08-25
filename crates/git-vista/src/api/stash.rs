//! The stash drawer's endpoints (M3.24, #77).
//!
//! Two reads — `GET /api/stashes`, `GET /api/stash/show` — and four writes:
//! `POST /api/stash/push`, `/apply`, `/drop`, `/branch`.
//!
//! # There is no pop request here, and that is the design
//!
//! The server has no `/api/stash/pop` route. That is deliberate and argued in
//! `crates/git-vista-server/src/main.rs`: pop is apply-then-drop, and one
//! durable operation row cannot distinguish "nothing ran" from "your changes
//! were applied and the entry is still there". Two operations produce two rows,
//! and two rows can tell the truth.
//!
//! So a pop is composed by the caller from [`apply_stash_request`] and
//! [`drop_stash_request`], with
//! [`crate::features::stash::core::drop_gate`] deciding whether the second one
//! is sent at all. This module deliberately exposes no function that would let
//! a caller skip that gate.
//!
//! # Every write sends the selector AND the oid
//!
//! Both, always. The selector is the address and is what reaches git; the oid
//! is the witness the server compare-and-swaps against a fresh resolve
//! immediately before mutating. A selector alone renumbers on every drop, so
//! acting on a stale one would eventually delete a stash nobody chose. Neither
//! value is ever *computed* here — both are echoed back exactly as
//! `GET /api/stashes` handed them over.
//!
//! # These request bodies have one author (#495, ADR 0079)
//!
//! Every shape this module sends is a `git-vista-protocol` DTO the server
//! deserializes — [`PushStashRequest`], [`StashTarget`] for apply and drop,
//! [`BranchFromStashRequest`] for the escape hatch. Until #495 each was
//! declared here *and* in `crates/git-vista-server/src/handlers/stash.rs`, so
//! every field name existed twice with nothing forcing the copies to agree,
//! and a rename on either side was silent: `PushStashRequest` and
//! `StashEntryRequest` there tolerated unknown fields, so a drifted name was a
//! value dropped on the floor rather than a 400.
//!
//! # The selectors and oids are typed, and that is where the argument lands
//!
//! The functions below still take `&str`, because the drawer's view and its
//! signals hold selectors as strings. But nothing leaves this module as a
//! string: each is passed through [`StashSelector`]/[`CommitOid`], which are
//! the same validators the server's wire boundary runs. So a malformed
//! selector cannot be *sent*, not merely cannot be served — and the failure
//! arrives as a sentence in the drawer instead of a round trip.

use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{
    BranchFromStashRequest, BranchName, CommitOid, PlanFieldError, PushStashRequest, StashEntry,
    StashMessage, StashSelector, StashTarget,
};

use super::{
    network_error, receipt, refuse_if_offline, refuse_if_visualize, req_get, send_write_with_key,
    user_facing_error, write_json, WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// The `(selector, expected_oid)` pair every write echoes back, validated
/// through the shared newtypes before anything is sent.
///
/// Neither value is ever *computed* here — both are exactly what
/// `GET /api/stashes` handed over, and both are already `StashSelector` and
/// `CommitOid` on that listing. Re-validating them costs nothing and means the
/// view's `String` round trip cannot be where a bad value enters. In practice
/// this never fails; it is checked rather than unwrapped because "the server
/// said so" is an assumption, and a panic in the client is a blank screen —
/// the same posture `api::conflicts`'s `WorktreePath` build takes.
fn target(entry: &str, expected_oid: &str) -> Result<StashTarget, String> {
    Ok(StashTarget {
        entry: StashSelector::new(entry).map_err(|e| e.to_string())?,
        expected_oid: CommitOid::new(expected_oid).map_err(|e| e.to_string())?,
    })
}

/// Every entry in the drawer, newest first (`GET /api/stashes`).
///
/// A live read like the tag list and the event feed beside it: a stash can
/// appear or vanish from a terminal at any moment, so it is fetched fresh
/// whenever the panel opens and cache-busted the same way.
///
/// An `Err` here is *"could not read the drawer"*, and the caller must not
/// render it as an empty list. The server keeps those apart on purpose — its
/// own handler comment says so — and collapsing them in the client would undo
/// that: "no stashes" and "could not look" authorise different UI.
pub async fn fetch_stashes() -> Result<Vec<StashEntry>, String> {
    let url = format!("/api/stashes?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<StashEntry>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// The patch one entry holds (`GET /api/stash/show`) — **A1's endpoint**:
/// *"stash content is inspectable before apply or drop."*
///
/// Plain text, not JSON: the server returns `git stash show --patch`'s stdout
/// verbatim (with `--no-color` and `--no-textconv`, so neither a `color.ui`
/// setting nor a repository's own `.gitattributes` textconv filter can inject
/// escapes or get executed to render it).
///
/// `entry` is sent as a query parameter and percent-encoded. The server's
/// `StashSelector` newtype refuses anything that is not `stash@{<digits>}`, so
/// there is no byte here that encoding protects against — it is encoded anyway
/// because `{` and `}` are not legal in a query string unescaped, and relying
/// on every intermediary to tolerate them is a bet with no upside.
pub async fn fetch_stash_patch(entry: &str) -> Result<String, String> {
    // The server's `ShowStashQuery` deserializes this field as a
    // `StashSelector`, so this is the same gate one process earlier — a query
    // string is the one stash shape that is not a shared DTO (see that type's
    // doc for why), and running its validator here is what keeps the two ends
    // from drifting on what `entry` may hold.
    let entry = StashSelector::new(entry).map_err(|e| e.to_string())?;
    let encoded = js_sys::encode_uri_component(entry.as_str())
        .as_string()
        .unwrap_or_default();
    let url = format!("/api/stash/show?entry={encoded}&t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.text().await.map_err(|e| e.to_string())
    } else {
        // The server's own sentence, which for a 404 explains that entries
        // renumber on every drop and the list should be re-read. That is the
        // part worth showing; a generic "failed" would throw it away.
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// What a caller is told when it asks to stash under a blank message.
///
/// A blank message is the caller's bug, and normalising it to `None` would
/// hide that the user typed something and it went nowhere. Before #495 the
/// server owned this sentence, because the wire carried `Option<String>` and
/// only a handler could tell the two apart. The wire now carries
/// `Option<StashMessage>`, which cannot spell a blank at all — so the refusal
/// moves to the one place a user could act on it, and this line exists rather
/// than the newtype's own terser "stash message can't be empty".
pub const BLANK_STASH_MESSAGE: &str =
    "Stash message can't be blank — omit it entirely to let git write its own.";

/// Put the working tree in the drawer (`POST /api/stash/push`).
///
/// `keep_index` and `include_untracked` are taken as plain required arguments
/// for the same reason the shared DTO has no `#[serde(default)]` on them: a
/// caller must have decided. The decision is shown to the user first by
/// [`crate::features::stash::core::push_preview`].
pub async fn push_stash_request(
    message: Option<&str>,
    keep_index: bool,
    include_untracked: bool,
) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let message = match message {
        None => None,
        // A blank is the one refusal worth its own sentence; the other
        // (16 KiB of "message") already says exactly what is wrong.
        Some(m) => Some(StashMessage::new(m).map_err(|e| match e {
            PlanFieldError::Empty(_) => BLANK_STASH_MESSAGE.to_string(),
            other => other.to_string(),
        })?),
    };
    let body = PushStashRequest {
        message,
        keep_index,
        include_untracked,
    };
    let (resp, _key) = write_json("/api/stash/push", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/stash/push", resp).await)
    }
}

/// Restore a stash's changes, keeping the entry (`POST /api/stash/apply`).
///
/// Not operation-tracked: an apply keeps the entry whatever happens, so its
/// worst outcome is a messy worktree with the stash still safe in the drawer —
/// the same posture `create_tag_request` takes for the non-destructive half of
/// its pair. The destructive half of a pop goes through
/// [`drop_stash_request`], which is.
///
/// Both halves of the identity go out together; see the module doc.
pub async fn apply_stash_request(entry: &str, expected_oid: &str) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = target(entry, expected_oid)?;
    let (resp, _key) = write_json("/api/stash/apply", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/stash/apply", resp).await)
    }
}

/// Discard an entry (`POST /api/stash/drop`).
///
/// Operation-tracked and idempotency-keyed like `delete_tag_request`, because
/// it is the destructive one: the entry's commit becomes unreachable and only
/// the recovery pin keeps it alive. Reached from the danger-styled confirm
/// modal, or as the second half of a composed pop — and in that second case
/// only after [`crate::features::stash::core::drop_gate`] has returned
/// `DropGate::Run`.
pub async fn drop_stash_request(
    entry: &str,
    expected_oid: &str,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = target(entry, expected_oid)?;
    let json = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let (resp, _key) =
        send_write_with_key("/api/stash/drop", Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}

/// Create a branch at the stash's own base commit, check it out, apply the
/// stash there, and drop the entry if that succeeded
/// (`POST /api/stash/branch`).
///
/// The recovery path for "my stash won't come back": git creates the branch at
/// the commit the stash was *taken from*, so the apply happens in the context
/// the changes were written in, where by construction they fit. A stash that
/// conflicts on pop will usually go in without complaint this way.
pub async fn branch_from_stash_request(
    name: &str,
    entry: &str,
    expected_oid: &str,
) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = BranchFromStashRequest {
        // The name is the user's own typing, so unlike the selector/oid pair
        // this really can be malformed — and refusing it here means the drawer
        // says so without a round trip. `git stash branch <name> <selector>`
        // puts it straight after the subcommand, which is why option-shaped is
        // the shape that matters.
        name: BranchName::new(name).map_err(|e| e.to_string())?,
        target: target(entry, expected_oid)?,
    };
    let (resp, _key) = write_json("/api/stash/branch", &body).await?;
    if resp.ok() {
        // The success body's one sentence of substance — "The stash entry has
        // been removed." — is discarded here, so the caller's own notice must
        // disclose the removal itself; silently dropping it left the user with
        // a vanished stash and no explanation (#516).
        Ok(())
    } else {
        Err(user_facing_error("/api/stash/branch", resp).await)
    }
}
