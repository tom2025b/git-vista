//! Tag endpoints — `POST /api/tag`, `GET /api/tags`, `POST /api/delete-tag`.
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_protocol::dto::TagDetail;
use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{CreateTagRequest, DeleteTagRequest, SignTagError};

use super::{
    network_error, receipt, refuse_if_offline, refuse_if_visualize, req_get, send_write_with_key,
    write_json, WriteReceipt, REQUEST_TIMEOUT_MS,
};

/// Ask the backend to create a tag (M2.21d #238 / M2.21e #239, `POST
/// /api/tag`), mirroring [`create_branch_request`] just above. `message` is
/// what the DTO's own doc comment uses to choose the tag's *kind*: `None` is
/// a lightweight tag, `Some(text)` is annotated with `text` — the caller
/// builds this with `features::graph::core::tag_annotation_from_prompt`
/// rather than deciding it inline, so "cancelled" and "typed nothing" can't
/// diverge from one another by accident. `sign` asks for `git tag -s`
/// (M2.21e wires real execution; it reliably fails against this server's own
/// sandbox today — see [`SignTagError`]'s doc comment for why).
///
/// # Two error shapes, tried in order
///
/// A signing failure carries a typed [`SignTagError`] (`kind` + `message`) —
/// a JSON *object* that is not the `ApiError` envelope every other refusal
/// uses, so it is parsed first; `user_facing_error`'s generic
/// `split_error_response` cannot read it (a JSON object it does not
/// recognise as the envelope falls back to a bare `HTTP <status>`, which
/// would throw away the one part of a `SignTagError` worth showing). Every
/// other refusal — a bad name, an existing tag, git couldn't run — keeps the
/// server-wide prose/envelope contract and falls through to the same
/// `user_facing_error` path every other write in this file uses (#316).
pub async fn create_tag_request(
    name: &str,
    commit: &str,
    message: Option<&str>,
    sign: bool,
) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = CreateTagRequest {
        name: name.to_string(),
        commit: commit.to_string(),
        message: message.map(str::to_string),
        sign,
    };
    let (resp, _key) = write_json("/api/tag", &body).await?;
    if resp.ok() {
        return Ok(());
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if let Ok(refusal) = serde_json::from_str::<SignTagError>(&text) {
        web_sys::console::error_1(
            &format!("git-vista: POST /api/tag signing refused: {refusal:?}").into(),
        );
        return Err(refusal.message);
    }
    let parsed = crate::features::dialogs::core::split_error_response(status, &text);
    if let Some(id) = &parsed.request_id {
        web_sys::console::error_1(
            &format!(
                "git-vista: POST /api/tag failed (request {id}): {}",
                parsed.message
            )
            .into(),
        );
    }
    Err(parsed.message)
}

/// Fetch every tag with its full metadata (`GET /api/tags`, M2.21b #236):
/// lightweight vs annotated, the tagged commit, and — for annotated tags —
/// the tag object, tagger and message.
///
/// A live read like the feed beside it: a tag can appear or vanish from a
/// terminal at any moment, so it is fetched fresh whenever the Activity panel
/// opens and cache-busted the same way.
pub async fn fetch_tags() -> Result<Vec<TagDetail>, String> {
    let url = format!("/api/tags?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<TagDetail>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to delete the **local** tag `tag` (M2.21d, #238, `POST
/// /api/delete-tag`, `git tag -d`). Not [`branch_op_request`]'s `BranchRequest`
/// shape reused: the wire body's key is `tag`, not `branch` — its own DTO,
/// [`DeleteTagRequest`], exists precisely so a tag-delete body can't be typo'd
/// into deleting a branch of the same name (see that type's own doc comment).
///
/// Operation-tracked like `branch_op_request`, so it takes and forwards the
/// same idempotency `key` the confirm-modal dispatch path mints — this is the
/// destructive half of the pair, reached only from the danger-styled confirm
/// modal, never the direct-POST path [`create_tag_request`] takes.
///
/// Local only — deleting a tag already pushed to a remote reaches a different
/// route, still to come (#74), because that one opens a socket with
/// credentials on it.
pub async fn delete_tag_request(tag: &str, key: IdempotencyKey) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = DeleteTagRequest {
        tag: tag.to_string(),
    };
    let json = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let (resp, _key) =
        send_write_with_key("/api/delete-tag", Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}
