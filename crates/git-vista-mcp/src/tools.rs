//! The MCP tool surface (M2.23a #245 + M2.23b #246): the read-only six —
//! `list_repositories` (#245) plus `select_repository`, `get_graph`,
//! `get_commit_detail`, `get_commit_diff`, `get_status`, and `get_activity`
//! (#246), all built on 153a's authenticated client.
//!
//! Every tool round-trips the *existing* `git-vista-protocol`/`git-vista-core`
//! wire types verbatim — no ad hoc reshaping. See each tool's doc comment
//! below for exactly which DTO it carries and why.
//!
//! # Why this crate may depend on `git-vista-core` but never `git-vista-server`
//!
//! `git-vista-protocol`'s paged-history envelopes ([`git_vista_protocol::HistoryFrame`],
//! [`git_vista_protocol::HistoryPage`]) are generic over the row/edge/ref/stub
//! types on purpose (see `history.rs`'s module doc) — the server instantiates
//! them with `git_vista_core::model::{GitRef, GraphRow, Edge, FrameStub}` via
//! its own *private* `handlers::read::{Frame, Page}` aliases. This crate
//! cannot import those aliases (they are `pub(crate)` to `git-vista-server`,
//! and #246 forbids linking that crate at all — see
//! `tests/no_write_dependency.rs`), so [`Frame`] and [`Page`] below redeclare
//! the identical instantiation using `git-vista-core`'s own public types —
//! the same DTOs, the same generic parameters, just named locally. That is a
//! type alias, not a reshaping: deserializing the server's JSON into it is
//! exactly as faithful as deserializing into the server's own alias would be.
//! `git-vista-core` is safe to depend on here because it is *pure domain
//! logic* ("No UI dependencies", wasm-safe, no `axum`, no write handlers, no
//! dependency on either `git-vista-server` or `git-vista-git`) — it is the
//! same crate the wasm frontend links for the identical reason.

use crate::auth::{self, Session};
use crate::http::{self, HttpResponse};

/// This crate's local instantiation of the paged-history Frame envelope —
/// see the module doc for why this mirrors, rather than imports,
/// `git-vista-server`'s private `handlers::read::Frame` alias.
pub type Frame = git_vista_protocol::HistoryFrame<git_vista_core::model::GitRef>;

/// This crate's local instantiation of the paged-history Page envelope —
/// see the module doc for why this mirrors, rather than imports,
/// `git-vista-server`'s private `handlers::read::Page` alias.
pub type Page = git_vista_protocol::HistoryPage<
    git_vista_core::model::GraphRow,
    git_vista_core::model::Edge,
    git_vista_core::model::FrameStub,
>;

/// Why a tool call failed — a **protocol** failure (the client asked for a
/// tool that doesn't exist: JSON-RPC `-32602`) versus an **execution** failure
/// of a real tool (auth, HTTP, parse: MCP's `isError` result). The MCP spec
/// separates these, and this dispatcher is the template the rest of the #153
/// chain copies, so the taxonomy is typed from the first slice.
#[derive(Debug)]
pub enum ToolError {
    /// No such tool. The name goes back to the client in a `-32602` error.
    Unknown(String),
    /// A known tool ran and failed; the message is for the client's eyes.
    Execution(String),
}

/// The catalog of tools this bridge advertises to `tools/list` — the
/// read-only surface here, then M2.23d's (#248) `plan_*` build-only tools
/// appended from [`crate::plan_tools`].
pub fn tool_catalog() -> serde_json::Value {
    let mut catalog = read_tool_catalog();
    let array = catalog
        .as_array_mut()
        .expect("the read catalog is a JSON array");
    array.extend(crate::plan_tools::plan_tool_catalog());
    // M2.23e (#249): the one write tool, appended last so it is always the
    // catalog's final entry — see `tools::tests::the_tool_catalog_lists_exactly_the_six_read_tools`.
    array.extend(crate::execute_tool::execute_tool_catalog());
    catalog
}

/// The read-only six (#245/#246), kept as their own function so
/// [`tool_catalog`] above reads as "reads, then plans" rather than one
/// 500-line literal.
fn read_tool_catalog() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "list_repositories",
            "description": "List every repository and clone the running git-vista server \
                            knows about, exactly as its own picker sees them (GET /api/catalog).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "select_repository",
            "description": "Make a repository the server's current selection, opened in the \
                            given mode (POST /api/select). Non-mutating to the repository \
                            itself, but registered on the server's write-gated route table \
                            (ADR 0007), so this tool authenticates and signs with CSRF exactly \
                            like a real write. Returns the server's plain-text confirmation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "worktree": {
                        "type": "string",
                        "description": "The opaque worktree id from list_repositories' catalog."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["visualize", "active"],
                        "description": "Visualize is look-only; Active allows later writes."
                    }
                },
                "required": ["worktree", "mode"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_graph",
            "description": "The commit graph for the current (or selected) repository: the \
                            once-per-view Frame (refs, branch colours, resolved-target \
                            metadata — GET /api/frame) plus one cursor-paginated Page of rows/ \
                            edges/stubs (GET /api/commits). Returns only the requested page — \
                            omit `cursor` for page 1, and pass back `page.cursor` from a prior \
                            call to fetch the next page; `page.cursor` is `null` once history \
                            is exhausted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Opaque repository/worktree id (list_repositories). \
                                        Omit to use the server's current selection."
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Opaque page cursor from a prior get_graph call's \
                                        page.cursor. Omit for page 1."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Rows per page (server default 250, clamped to 1000)."
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "get_commit_detail",
            "description": "Full metadata for one commit — message, author/committer, parents, \
                            exact remote reachability (GET /api/commit/{id}). Does not include \
                            the diff/patch; call get_commit_diff for that.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Commit hex id (full or abbreviated)." },
                    "repo": {
                        "type": "string",
                        "description": "Opaque repository/worktree id. Omit to use the \
                                        server's current selection."
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_commit_diff",
            "description": "One commit's diff — per-file change list plus the unified patch \
                            text (GET /api/diff/{id}). Kept separate from get_commit_detail: \
                            the patch is capped and truncatable (200,000 chars by default, \
                            5,000,000 with `full`), a cost callers of plain metadata should \
                            never pay implicitly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Commit hex id (full or abbreviated)." },
                    "repo": {
                        "type": "string",
                        "description": "Opaque repository/worktree id. Omit to use the \
                                        server's current selection."
                    },
                    "full": {
                        "type": "boolean",
                        "description": "Lift the patch cap to 5,000,000 characters (mirrors \
                                        the full-screen diff viewer's `?full=1`)."
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_status",
            "description": "The generation-tagged working-tree status — branch, upstream, \
                            ahead/behind, and every staged/unstaged/untracked/conflicted entry \
                            (GET /api/status/v2, the v2 WorktreeStatus DTO — not the legacy v1 \
                            shape).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Opaque repository/worktree id. Omit to use the \
                                        server's current selection."
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "get_activity",
            "description": "The chronological activity feed for the server's current \
                            repository — journal + reflogs + snapshot diffs, folded and \
                            attributed, newest first (GET /api/activity).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Max events returned (server default 100, capped at 500)."
                    }
                },
                "additionalProperties": false
            }
        }
    ])
}

/// Enforce, at call time, the `additionalProperties: false` that every
/// advertised `inputSchema` declares — for the tool's own arguments and for
/// every nested object schema inside them.
///
/// # Why this has to be code and not just schema text
///
/// Nothing between an MCP client and this dispatcher validates arguments
/// against the advertised schema: the client sends whatever it sends, and each
/// tool body then reads only the specific keys it knows about
/// (`args.get("cursor")`, `annotation_arg(args, "annotation")`, …). Without
/// this check the `additionalProperties: false` in `tools/list` is decorative
/// — `every_tool_schema_is_a_closed_object` below pins the advertised *text*,
/// but the *behaviour* was the opposite of what the text promises.
///
/// A misspelled **required** key was already caught, incidentally, by being
/// "missing". A misspelled **optional** key was not caught at all, and that is
/// the dangerous half, because the tool then proceeds as if the argument had
/// never been given:
///
/// - `plan_create_tag` with `"anotation"` built a bare **lightweight** tag —
///   no message, no GPG signature — for a caller who asked for a signed
///   annotated one, and the review digest cannot tell the two apart
///   (`delete_created_tag` either way), so an agent reading only the digest
///   sees no sign the request was silently downgraded.
/// - `get_graph` with `"curser"` silently re-fetches page 1 for ever.
/// - A typo inside `force` escapes `force_arg`'s paired mode/tip check, which
///   only cross-validates the two spellings it knows.
///
/// Unknown *tool* names are deliberately passed through untouched: they are
/// [`ToolError::Unknown`]'s business (JSON-RPC `-32602`), not a schema
/// complaint.
fn reject_undeclared_arguments(name: &str, args: &serde_json::Value) -> Result<(), ToolError> {
    let catalog = tool_catalog();
    let Some(tool) = catalog
        .as_array()
        .expect("the catalog is a JSON array")
        .iter()
        .find(|t| t["name"].as_str() == Some(name))
    else {
        return Ok(());
    };
    reject_undeclared_in(&tool["inputSchema"], args, name)
}

/// [`reject_undeclared_arguments`]'s recursion: one schema node against one
/// JSON value. Only object-shaped values with a `properties` block are
/// inspected — a wrong-*typed* value is the typed extractors' business to
/// report (they name the field and say what shape they wanted), and
/// duplicating that here would only produce two different errors for one
/// mistake.
fn reject_undeclared_in(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), ToolError> {
    let (Some(object), Some(properties)) = (
        value.as_object(),
        schema.get("properties").and_then(|p| p.as_object()),
    ) else {
        return Ok(());
    };
    let closed = schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false));
    for (key, sub_value) in object {
        match properties.get(key) {
            Some(sub_schema) => {
                reject_undeclared_in(sub_schema, sub_value, &format!("{path}.{key}"))?;
            }
            None if closed => {
                let mut known: Vec<&str> = properties.keys().map(String::as_str).collect();
                known.sort_unstable();
                let accepted = if known.is_empty() {
                    "(this tool takes no arguments)".to_string()
                } else {
                    known.join(", ")
                };
                return Err(ToolError::Execution(format!(
                    "`{path}` has no argument named `{key}`; its schema is closed \
                     (additionalProperties: false). Accepted: {accepted}. Refused rather \
                     than dropped: a misspelled optional argument would otherwise leave \
                     the call proceeding as if it had never been given."
                )));
            }
            None => {}
        }
    }
    Ok(())
}

/// Run a tool by name with its JSON-RPC `arguments` object. `session` is
/// authenticated lazily on first use and re-established once on a 401 — the
/// server rotates sessions on restart, and the bridge may well outlive one
/// server process.
pub fn call_tool(
    name: &str,
    arguments: &serde_json::Value,
    session: &mut Option<Session>,
) -> Result<serde_json::Value, ToolError> {
    // The advertised schema, enforced — before any argument is read and long
    // before anything authenticates. See [`reject_undeclared_arguments`].
    reject_undeclared_arguments(name, arguments)?;
    match name {
        "list_repositories" => get_json::<serde_json::Value>("/api/catalog", session),
        "select_repository" => select_repository(arguments, session),
        "get_graph" => get_graph(arguments, session),
        "get_commit_detail" => {
            let id = required_url_safe(arguments, "id")?;
            let path = format!("/api/commit/{id}{}", repo_suffix(arguments, "?")?);
            get_json::<git_vista_core::model::CommitDetail>(&path, session)
        }
        "get_commit_diff" => {
            let id = required_url_safe(arguments, "id")?;
            let mut qs = repo_suffix(arguments, "?")?;
            if optional_bool(arguments, "full")?.unwrap_or(false) {
                qs.push_str(if qs.is_empty() { "?full=1" } else { "&full=1" });
            }
            let path = format!("/api/diff/{id}{qs}");
            get_json::<git_vista_core::diff::CommitDiff>(&path, session)
        }
        "get_status" => {
            let path = format!("/api/status/v2{}", repo_suffix(arguments, "?")?);
            get_json::<git_vista_protocol::WorktreeStatus>(&path, session)
        }
        "get_activity" => {
            let mut qs = String::new();
            if let Some(limit) = optional_u64(arguments, "limit")? {
                qs.push_str(&format!("?limit={limit}"));
            }
            let path = format!("/api/activity{qs}");
            get_json::<Vec<git_vista_core::activity::ActivityEvent>>(&path, session)
        }
        // M2.23d (#248): the `plan_*` build-only surface, then M2.23e (#249)'s
        // one write tool. Tried *after* the read tools and before the
        // unknown-tool refusal, so neither can shadow a read tool's name, and
        // an unrecognised name is still `Unknown` rather than silently
        // swallowed.
        other => match crate::plan_tools::call_plan_tool_live(other, arguments, session)
            .or_else(|| crate::execute_tool::call_execute_tool_live(other, arguments, session))
        {
            Some(result) => result,
            None => Err(ToolError::Unknown(other.to_string())),
        },
    }
}

// ---------------------------------------------------------------------------
// Argument extraction — small, typed helpers so every tool's argument
// handling reads the same way and a missing/wrong-shaped field is always an
// Execution error naming the field, never a panic.
// ---------------------------------------------------------------------------

fn required_str(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ToolError::Execution(format!("missing required argument `{key}`")))
}

fn optional_str(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Absent is `Ok(None)`; present-but-wrong-JSON-type is a hard `Err`, not a
/// silent fall-back to "absent" — a caller that sends `"limit": "100"`
/// (a string, not a number) gets told so, the same honesty `required_str`
/// already gives a missing/wrong-typed required argument.
fn optional_u64(args: &serde_json::Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            ToolError::Execution(format!("`{key}` must be a non-negative integer, got {v}"))
        }),
    }
}

fn optional_bool(args: &serde_json::Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| ToolError::Execution(format!("`{key}` must be a boolean, got {v}"))),
    }
}

/// Every byte this crate ever legitimately sends as a URL path/query segment
/// — hex commit ids, opaque uuid-shaped repository/worktree ids, and
/// `URL_SAFE_NO_PAD` base64 history cursors — fits this set. An allow-list
/// rather than an exclude-list on purpose: simpler to reason about than
/// trying to anticipate every dangerous byte one at a time.
fn is_url_segment_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A required string argument, additionally validated against
/// [`is_url_segment_safe`] — for any value about to be spliced into a URL
/// path or query string.
///
/// # Why this exists (security)
///
/// `http.rs`'s hand-rolled client builds the literal HTTP request line by
/// `format!`ing a path string straight onto the wire — there is no request
/// library between this crate and the raw TCP socket to reject an embedded
/// `\r\n`. A JSON string CAN carry literal CR/LF bytes (`"\r\n"` decodes to
/// real `0x0D 0x0A`), so an unchecked tool-call argument spliced into a URL
/// is a classic HTTP request-splitting vector (CWE-93): a crafted `id`
/// ending in `\r\nHost: ...\r\n\r\nPOST /api/branch HTTP/1.1\r\n...` smuggles
/// a second, fully-formed request onto the same connection — one that rides
/// this bridge's own real session cookie and CSRF token, since those are
/// appended to the buffer *after* the (attacker-corrupted) path. That turns
/// a nominally read-only tool into a real write, with the smuggled response
/// silently discarded by `parse_response`'s single-response framing, so
/// nothing in the tool's own JSON-RPC result reveals a mutation happened.
/// Realistic in an MCP setting specifically: a tool argument can originate
/// from an LLM relaying untrusted repository content (a commit message, a
/// file's text, an activity-log entry) rather than a value that legitimately
/// came back from a prior call. Found by #246's own adversarial review.
fn required_url_safe(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    let value = required_str(args, key)?;
    if !is_url_segment_safe(&value) {
        return Err(ToolError::Execution(format!(
            "`{key}` must be a plain id (letters, digits, -, _, .), got {value:?}"
        )));
    }
    Ok(value)
}

/// [`required_url_safe`]'s optional sibling — an absent key is `Ok(None)`, a
/// present-but-unsafe value is still a hard `Err`, never silently ignored
/// (matching `required_str`'s honesty, not `optional_str`'s pass-through).
fn optional_url_safe(args: &serde_json::Value, key: &str) -> Result<Option<String>, ToolError> {
    match optional_str(args, key) {
        None => Ok(None),
        Some(value) if is_url_segment_safe(&value) => Ok(Some(value)),
        Some(value) => Err(ToolError::Execution(format!(
            "`{key}` must be a plain id (letters, digits, -, _, .), got {value:?}"
        ))),
    }
}

/// `?repo=<id>` (or `&repo=<id>` if `sep` is `"&"`) when the caller passed
/// one, else the empty string — shared by every tool that accepts the
/// optional `?repo=` selector every read endpoint understands.
fn repo_suffix(args: &serde_json::Value, sep: &str) -> Result<String, ToolError> {
    Ok(match optional_url_safe(args, "repo")? {
        Some(repo) => format!("{sep}repo={repo}"),
        None => String::new(),
    })
}

// ---------------------------------------------------------------------------
// Tool bodies
// ---------------------------------------------------------------------------

/// `POST /api/select` (ADR 0007). Registered inside `main.rs`'s
/// `full_routes`-gated block alongside the write routes — non-mutating to
/// repository *content*, but the server still requires the full write auth
/// gate (session + CSRF), so this reuses [`authed_post`] rather than
/// [`authed_fetch`]: `security.rs`'s gate keys entirely on HTTP method
/// (`is_state_changing`), not on any per-route "is this really a write"
/// judgment, so a `POST` here needs the same cookie+CSRF pairing any other
/// `POST` does — the existing authenticated client already carries both in
/// [`Session`], with no separate auth flow to build.
///
/// The endpoint's own response is plain confirmation text ("Selected."), not
/// a JSON DTO — `git-vista-protocol` has no response type for `/api/select`
/// to reuse, so the tool result is that text verbatim, wrapped as a JSON
/// string. That is the wire type here; nothing is invented.
fn select_repository(
    args: &serde_json::Value,
    session: &mut Option<Session>,
) -> Result<serde_json::Value, ToolError> {
    let worktree = required_str(args, "worktree")?;
    let mode_str = required_str(args, "mode")?;
    let mode = match mode_str.as_str() {
        "visualize" => git_vista_protocol::RepoMode::Visualize,
        "active" => git_vista_protocol::RepoMode::Active,
        other => {
            return Err(ToolError::Execution(format!(
                "`mode` must be \"visualize\" or \"active\", got {other:?}"
            )))
        }
    };
    let body = serde_json::to_vec(&git_vista_protocol::SelectRequest { worktree, mode })
        .map_err(|e| ToolError::Execution(format!("could not encode the select request: {e}")))?;

    let text = authed_post(
        "/api/select",
        &body,
        session,
        &mut |path, body, cookie, csrf| http::post_json(path, body, Some(cookie), Some(csrf)),
        &mut auth::authenticate,
    )
    .map_err(ToolError::Execution)?;
    Ok(serde_json::Value::String(
        String::from_utf8_lossy(&text).into_owned(),
    ))
}

/// `GET /api/frame` + one page of `GET /api/commits`. See [`tool_catalog`]'s
/// entry for the pagination decision: this tool answers exactly one page per
/// call (server default 250 rows, clamped to 1000) and hands the wire
/// `cursor` straight back for the caller to pass into the next call — the
/// natural MCP shape here is "the DTO already has a cursor field," not a
/// bespoke pagination convention layered on top.
fn get_graph(
    args: &serde_json::Value,
    session: &mut Option<Session>,
) -> Result<serde_json::Value, ToolError> {
    // Validate every argument before either network call: a malformed
    // cursor/repo/limit means this whole request fails regardless, so there
    // is no reason to authenticate and fetch `/api/frame` first only to
    // reject the request on `cursor` afterward — cheap, local, structural
    // checks come before any network round trip, not interleaved with it.
    let repo = repo_suffix(args, "?")?;
    let cursor = optional_url_safe(args, "cursor")?;
    let limit = optional_u64(args, "limit")?;

    let frame_path = format!("/api/frame{repo}");
    let frame: Frame = get_json_typed(&frame_path, session)?;

    let mut commits_qs = repo;
    if let Some(cursor) = cursor {
        commits_qs.push_str(if commits_qs.is_empty() { "?" } else { "&" });
        commits_qs.push_str("cursor=");
        commits_qs.push_str(&cursor);
    }
    if let Some(limit) = limit {
        commits_qs.push_str(if commits_qs.is_empty() { "?" } else { "&" });
        commits_qs.push_str(&format!("limit={limit}"));
    }
    let page_path = format!("/api/commits{commits_qs}");
    let page: Page = get_json_typed(&page_path, session)?;

    Ok(serde_json::json!({ "frame": frame, "page": page }))
}

/// GET `path`, authenticate lazily / retry once on 401, and parse the body as
/// `T`. The shared tail every simple GET-shaped tool uses.
fn get_json_typed<T: serde::de::DeserializeOwned>(
    path: &str,
    session: &mut Option<Session>,
) -> Result<T, ToolError> {
    let body = authed_fetch(
        path,
        session,
        &mut |p, cookie| http::get(p, Some(cookie)),
        &mut auth::authenticate,
    )
    .map_err(ToolError::Execution)?;
    serde_json::from_slice(&body)
        .map_err(|e| ToolError::Execution(format!("{path} did not return valid JSON: {e}")))
}

/// [`get_json_typed`], returning a `serde_json::Value` — for tools whose
/// output IS the parsed-then-reserialized DTO with no further composition.
fn get_json<T: serde::de::DeserializeOwned + serde::Serialize>(
    path: &str,
    session: &mut Option<Session>,
) -> Result<serde_json::Value, ToolError> {
    let value: T = get_json_typed(path, session)?;
    serde_json::to_value(value)
        .map_err(|e| ToolError::Execution(format!("could not re-encode {path}'s response: {e}")))
}

/// GET with the session cookie, authenticating on demand and retrying exactly
/// once on 401 with a fresh session (covers a server restart mid-bridge).
///
/// Generic over the fetch and auth closures so the three legs — lazy first
/// auth, 401 → re-auth → retry with the NEW cookie, 401 → 401 giving up — are
/// unit-testable without a server. Production passes `http::get` and
/// `auth::authenticate`.
fn authed_fetch(
    path: &str,
    session: &mut Option<Session>,
    fetch: &mut dyn FnMut(&str, &str) -> Result<HttpResponse, String>,
    auth: &mut dyn FnMut() -> Result<Session, String>,
) -> Result<Vec<u8>, String> {
    if session.is_none() {
        *session = Some(auth()?);
    }
    let cookie = session.as_ref().expect("just set").cookie.clone();
    let resp = fetch(path, &cookie)?;
    if resp.status == 401 {
        *session = Some(auth()?);
        let cookie = session.as_ref().expect("just set").cookie.clone();
        let retry = fetch(path, &cookie)?;
        if retry.status != 200 {
            return Err(format!(
                "GET {path} answered {} even after re-authenticating: {}",
                retry.status,
                String::from_utf8_lossy(&retry.body)
            ));
        }
        return Ok(retry.body);
    }
    if resp.status != 200 {
        return Err(format!(
            "GET {path} answered {}: {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        ));
    }
    Ok(resp.body)
}

/// POST with the session cookie AND CSRF token, authenticating on demand and
/// retrying exactly once on 401 with a fresh session — the write-shaped
/// sibling of [`authed_fetch`], needed because `/api/select` sits behind the
/// full session+CSRF gate even though it doesn't mutate a repository (see
/// [`select_repository`]'s doc comment). Same three-leg shape, same
/// unit-testability via injected closures.
/// `authed_post`'s injected POST closure: `(path, body, cookie, csrf) ->
/// response`. Named so the signature below reads, rather than clippy's
/// `type_complexity` firing on it inline.
pub(crate) type PostFn<'a> =
    dyn FnMut(&str, &[u8], &str, &str) -> Result<HttpResponse, String> + 'a;

pub(crate) fn authed_post(
    path: &str,
    body: &[u8],
    session: &mut Option<Session>,
    post: &mut PostFn<'_>,
    auth: &mut dyn FnMut() -> Result<Session, String>,
) -> Result<Vec<u8>, String> {
    if session.is_none() {
        *session = Some(auth()?);
    }
    let (cookie, csrf) = {
        let s = session.as_ref().expect("just set");
        (s.cookie.clone(), s.csrf.clone())
    };
    let resp = post(path, body, &cookie, &csrf)?;
    if resp.status == 401 {
        *session = Some(auth()?);
        let (cookie, csrf) = {
            let s = session.as_ref().expect("just set");
            (s.cookie.clone(), s.csrf.clone())
        };
        let retry = post(path, body, &cookie, &csrf)?;
        if retry.status != 200 {
            return Err(format!(
                "POST {path} answered {} even after re-authenticating: {}",
                retry.status,
                String::from_utf8_lossy(&retry.body)
            ));
        }
        return Ok(retry.body);
    }
    if resp.status != 200 {
        return Err(format!(
            "POST {path} answered {}: {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        ));
    }
    Ok(resp.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(cookie: &str) -> Session {
        Session {
            cookie: cookie.to_string(),
            csrf: "csrf".to_string(),
        }
    }

    fn resp(status: u16, body: &[u8]) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    fn no_args() -> serde_json::Value {
        serde_json::json!({})
    }

    #[test]
    fn the_tool_catalog_lists_exactly_the_six_read_tools() {
        // #248 appended the `plan_*` surface after these, so the read tools
        // are now a *prefix* of the catalog rather than the whole of it —
        // still pinned in order, and still pinned to exactly these names, so
        // a read tool silently added, removed or renamed fails here as
        // before. `plan_tools`'s own census owns the rest of the catalog.
        let cat = tool_catalog();
        let names: Vec<&str> = cat
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let expected_reads = [
            "list_repositories",
            "select_repository",
            "get_graph",
            "get_commit_detail",
            "get_commit_diff",
            "get_status",
            "get_activity",
        ];
        assert_eq!(names[..expected_reads.len()], expected_reads);
        // M2.23e (#249) appended exactly one write tool, `execute_plan`, as
        // the catalog's LAST entry — everything between the reads and it is
        // still `plan_*`. Pinning it as the last name (not merely "somewhere
        // after the reads") is what stops a second write tool from sneaking
        // in unnoticed between two plan_* entries.
        let after_reads = &names[expected_reads.len()..];
        let (last, plan_names) = after_reads
            .split_last()
            .expect("the catalog has more than just the read tools");
        assert_eq!(
            *last, "execute_plan",
            "the catalog's last tool must be execute_plan — #249's one write tool"
        );
        assert!(
            plan_names.iter().all(|n| n.starts_with("plan_")),
            "a non-read, non-plan, non-execute_plan tool appeared in the catalog: {names:?}"
        );
        // Nothing else after the reads may be a *write*: this crate's only
        // mutation surface is a reviewable plan built by `plan_*` and
        // executed by the one `execute_plan` tool pinned above.
        for forbidden in ["execute", "submit", "apply", "run"] {
            assert!(
                !plan_names.iter().any(|n| n.contains(forbidden)),
                "‘{forbidden}’ appears in a plan_* tool name — the only execution \
                 capability this crate ships is the single execute_plan tool"
            );
        }
    }

    #[test]
    fn every_tool_schema_is_a_closed_object() {
        // A tool whose schema forgets `additionalProperties: false` would
        // silently accept (and ignore) a misspelled argument instead of
        // telling the caller. Locked here so a future tool can't skip it.
        for tool in tool_catalog().as_array().unwrap() {
            assert_eq!(
                tool["inputSchema"]["additionalProperties"],
                serde_json::json!(false),
                "{} must close its input schema",
                tool["name"]
            );
        }
    }

    /// A value of the shape a schema node declares — used below to feed every
    /// tool exactly its own declared arguments, nested objects included.
    fn placeholder(schema: &serde_json::Value) -> serde_json::Value {
        match schema["type"].as_str() {
            Some("object") => {
                let mut filled = serde_json::Map::new();
                if let Some(props) = schema["properties"].as_object() {
                    for (key, sub) in props {
                        filled.insert(key.clone(), placeholder(sub));
                    }
                }
                serde_json::Value::Object(filled)
            }
            Some("array") => serde_json::json!([placeholder(&schema["items"])]),
            Some("boolean") => serde_json::json!(true),
            Some("integer") | Some("number") => serde_json::json!(1),
            _ => serde_json::json!("x"),
        }
    }

    /// The behavioural half of `every_tool_schema_is_a_closed_object` above.
    /// That test pins the advertised *text*; this one pins that the text is
    /// true — every advertised tool refuses an argument its schema does not
    /// declare, rather than dropping it.
    #[test]
    fn every_advertised_tool_refuses_an_undeclared_argument() {
        let catalog = tool_catalog();
        let tools = catalog.as_array().unwrap();
        assert!(
            tools.len() >= 30,
            "the catalog shrank to {} tools — this census would prove little",
            tools.len()
        );
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            let args = serde_json::json!({ "gv_not_a_real_argument": "x" });
            match reject_undeclared_arguments(name, &args) {
                Err(ToolError::Execution(msg)) => assert!(
                    msg.contains("gv_not_a_real_argument"),
                    "{name} refused without naming the offending key: {msg}"
                ),
                other => panic!("{name} accepted an undeclared argument: {other:?}"),
            }
        }
    }

    /// The paired positive, so the check above is not simply "refuse
    /// everything": every tool's own full set of declared arguments — nested
    /// objects filled out too — passes untouched.
    #[test]
    fn every_tools_own_declared_arguments_pass_the_closed_schema_check() {
        for tool in tool_catalog().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let args = placeholder(&tool["inputSchema"]);
            assert!(
                reject_undeclared_arguments(name, &args).is_ok(),
                "{name} rejected its own declared arguments: {args}"
            );
        }
    }

    /// Nested schemas are closed too, and the refusal says *where*. `force`
    /// and `annotation` are the only nested objects on this surface, and each
    /// carries a field whose silent loss changes what git actually does.
    #[test]
    fn an_undeclared_argument_nested_inside_an_object_is_refused_too() {
        let push = serde_json::json!({
            "branch": "b", "remote": "origin", "set_upstream": false,
            "force": { "mode": "none", "expected_remote_tipp": "x" }
        });
        match reject_undeclared_arguments("plan_push_branch", &push) {
            Err(ToolError::Execution(msg)) => {
                assert!(msg.contains("expected_remote_tipp"), "{msg}");
                assert!(msg.contains("force"), "the refusal must say where: {msg}");
            }
            other => panic!("a typo inside `force` was accepted: {other:?}"),
        }
        let tag = serde_json::json!({
            "name": "v1", "target": "0".repeat(40),
            "annotation": { "message": "release", "signn": true }
        });
        match reject_undeclared_arguments("plan_create_tag", &tag) {
            Err(ToolError::Execution(msg)) => {
                assert!(msg.contains("signn"), "{msg}");
                assert!(
                    msg.contains("annotation"),
                    "the refusal must say where: {msg}"
                );
            }
            other => panic!("a typo inside `annotation` was accepted: {other:?}"),
        }
    }

    /// The finding in its original clothes, driven through the real
    /// dispatcher: `anotation` (one `n`) used to be read by nobody, so
    /// `annotation_arg` answered `Ok(None)` and `plan_create_tag` built a bare
    /// **lightweight** tag — no message, no GPG signature — for a caller who
    /// asked for a signed annotated one. Nothing errored, and the review
    /// digest says `delete_created_tag` for either kind, so an agent reading
    /// the digest could not tell it had been downgraded.
    ///
    /// `target` is omitted deliberately: that keeps the call a purely local
    /// refusal whichever way it fails, so removing the schema check turns this
    /// test red (the message becomes "missing required argument `target`")
    /// instead of turning it into a live network call.
    #[test]
    fn a_typod_annotation_cannot_silently_downgrade_a_signed_tag_to_a_lightweight_one() {
        let mut none = None;
        let args = serde_json::json!({
            "name": "v1",
            "anotation": { "message": "release", "sign": true }
        });
        match call_tool("plan_create_tag", &args, &mut none) {
            Err(ToolError::Execution(msg)) => assert!(
                msg.contains("anotation"),
                "the misspelled key was dropped rather than refused: {msg}"
            ),
            other => panic!("a misspelled `annotation` was accepted: {other:?}"),
        }
        assert!(none.is_none(), "never authenticated for a refused call");
    }

    /// The same enforcement reaches the read tools, which have the same
    /// exposure (`curser` for `cursor` would silently re-fetch page 1 for
    /// ever). `id` is omitted for the same network-safety reason as above.
    #[test]
    fn a_misspelled_read_tool_argument_is_refused_by_call_tool_too() {
        let mut none = None;
        match call_tool(
            "get_commit_detail",
            &serde_json::json!({ "idd": "deadbeef" }),
            &mut none,
        ) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("idd"), "{msg}"),
            other => panic!("a misspelled read-tool argument was accepted: {other:?}"),
        }
        assert!(none.is_none());
    }

    /// The one seam that decides whether #248 works at all: `call_tool`'s
    /// wildcard arm, which is the *only* place production code joins the
    /// JSON-RPC `tools/call` dispatcher to the plan-tool implementation
    /// (`main.rs`'s `tools::call_tool` → this arm →
    /// `plan_tools::call_plan_tool_live`).
    ///
    /// Nothing covered it before: every `plan_tools` unit test calls the
    /// injectable `call_plan_tool` directly, and the one integration test that
    /// would have caught it is `#[ignore]`d behind a live server. Severing the
    /// arm — `call_plan_tool_live` returning `None` unconditionally — left all
    /// 47 tests green while every one of the 23 `plan_*` tools would answer
    /// "Unknown tool" to a real MCP client.
    ///
    /// Each tool is called with **no arguments**, so the refusal is local and
    /// unconditional: a plan tool that is reachable answers `Execution`
    /// ("missing required argument"), and one that is not answers `Unknown`.
    #[test]
    fn every_plan_tool_is_reachable_through_call_tools_dispatcher() {
        let catalog = tool_catalog();
        let mut reached = 0;
        let mut argument_free: Vec<&str> = Vec::new();
        for tool in catalog.as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            if !name.starts_with("plan_") {
                continue;
            }
            if tool["inputSchema"]["required"]
                .as_array()
                .expect("every plan schema declares a `required` list")
                .is_empty()
            {
                argument_free.push(name);
                continue;
            }
            let mut none = None;
            match call_tool(name, &serde_json::json!({}), &mut none) {
                Err(ToolError::Execution(msg)) => {
                    assert!(msg.contains("missing required argument"), "{name}: {msg}");
                    reached += 1;
                }
                Err(ToolError::Unknown(unknown)) => panic!(
                    "`{unknown}` is advertised by tools/list but call_tool's dispatcher \
                     never reaches the plan-tool implementation — every plan_* call would \
                     answer ‘Unknown tool’ to a real MCP client"
                ),
                Ok(_) => panic!("{name} built a plan from no arguments at all"),
            }
            assert!(
                none.is_none(),
                "{name} authenticated for a call it refused locally"
            );
        }
        assert_eq!(
            reached, 21,
            "expected 21 plan tools with a required argument to be exercised here"
        );
        // The two that cannot be probed this way, pinned by name: a tool that
        // silently LOST its required arguments would land here instead of
        // being exercised above, and this is what says so.
        assert_eq!(
            argument_free,
            ["plan_stage_all", "plan_unstage_all"],
            "the set of argument-free plan tools changed"
        );
    }

    #[test]
    fn an_unknown_tool_is_refused_by_name_without_authenticating() {
        let mut none = None;
        match call_tool("drop_tables", &no_args(), &mut none) {
            Err(ToolError::Unknown(name)) => assert_eq!(name, "drop_tables"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        // Crucially: refusing an unknown tool never attempted to authenticate
        // — no session was created for a request that will never be sent.
        assert!(none.is_none());
    }

    #[test]
    fn get_commit_detail_without_an_id_is_an_execution_error_not_a_panic() {
        let mut none = None;
        match call_tool("get_commit_detail", &no_args(), &mut none) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("id")),
            other => panic!("expected Execution, got {other:?}"),
        }
        assert!(
            none.is_none(),
            "never authenticated for a request that can't be sent"
        );
    }

    #[test]
    fn select_repository_rejects_an_unknown_mode_before_authenticating() {
        let mut none = None;
        let args = serde_json::json!({ "worktree": "abc", "mode": "destroy" });
        match call_tool("select_repository", &args, &mut none) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("destroy")),
            other => panic!("expected Execution, got {other:?}"),
        }
        assert!(none.is_none());
    }

    #[test]
    fn repo_suffix_is_empty_when_absent_and_a_query_param_when_present() {
        assert_eq!(repo_suffix(&no_args(), "?").unwrap(), "");
        let with = serde_json::json!({ "repo": "abc-123" });
        assert_eq!(repo_suffix(&with, "?").unwrap(), "?repo=abc-123");
        assert_eq!(repo_suffix(&with, "&").unwrap(), "&repo=abc-123");
    }

    // The finding this exists for: a JSON string can carry literal CR/LF
    // bytes, and this crate's hand-rolled HTTP client (http.rs) splices a
    // path straight onto the wire with no escaping — an unvalidated `id`
    // ending in a smuggled request line would ride the bridge's own real
    // session onto a second, attacker-chosen request (CWE-93). Every
    // URL-destined argument must refuse this before it ever reaches
    // http::get, not just before/after — refuse.
    #[test]
    fn a_crlf_injected_id_is_refused_before_it_ever_reaches_http() {
        let mut none = None;
        let args = serde_json::json!({
            "id": "abc\r\nHost: evil\r\n\r\nPOST /api/branch HTTP/1.1\r\n\r\n{}"
        });
        match call_tool("get_commit_detail", &args, &mut none) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("id"), "{msg}"),
            other => panic!("expected Execution, got {other:?}"),
        }
        assert!(
            none.is_none(),
            "never authenticated for a request that was refused before sending"
        );
    }

    #[test]
    fn a_crlf_injected_repo_is_refused_before_authenticating() {
        let mut none = None;
        let bad_repo = serde_json::json!({ "id": "deadbeef", "repo": "a\r\nEvil: 1" });
        match call_tool("get_commit_detail", &bad_repo, &mut none) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("repo"), "{msg}"),
            other => panic!("expected Execution, got {other:?}"),
        }
        assert!(
            none.is_none(),
            "get_commit_detail checks `repo` before any network call"
        );
    }

    #[test]
    fn a_crlf_injected_cursor_is_refused_before_authenticating() {
        // get_graph validates every argument, cursor included, before its
        // first network call (/api/frame) — so a malformed cursor never
        // triggers even the frame fetch, let alone reaches a query string.
        let mut none = None;
        let bad_cursor = serde_json::json!({ "cursor": "a\r\nEvil: 1" });
        match call_tool("get_graph", &bad_cursor, &mut none) {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("cursor"), "{msg}"),
            other => panic!("expected Execution, got {other:?}"),
        }
        assert!(
            none.is_none(),
            "get_graph checks every argument before its first network call"
        );
    }

    #[test]
    fn ordinary_ids_still_round_trip_through_the_url_safe_check() {
        // No over-rejection: real hex commit ids, uuid-shaped repo/worktree
        // ids, and URL_SAFE_NO_PAD base64 cursors must all still pass.
        for value in [
            "a1b2c3d4",
            "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d",
            "abcXYZ-_09",
        ] {
            assert!(is_url_segment_safe(value), "{value} should be accepted");
        }
        for value in ["", "a\rb", "a\nb", "a b", "a/b", "a;b"] {
            assert!(!is_url_segment_safe(value), "{value} should be refused");
        }
    }

    #[test]
    fn the_first_call_authenticates_lazily_and_sends_that_cookie() {
        let mut sess = None;
        let mut seen = Vec::new();
        let body = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, cookie| {
                seen.push(cookie.to_string());
                Ok(resp(200, b"ok"))
            },
            &mut || Ok(session("gv_session=first")),
        )
        .unwrap();
        assert_eq!(body, b"ok");
        assert_eq!(seen, ["gv_session=first"]);
        assert_eq!(sess.unwrap().cookie, "gv_session=first");
    }

    #[test]
    fn a_401_reauthenticates_once_and_retries_with_the_new_cookie() {
        // The trap this test exists for: a retry that resends the STALE
        // cookie would loop 401 forever in production while looking
        // superficially like a retry.
        let mut sess = Some(session("gv_session=stale"));
        let mut seen = Vec::new();
        let body = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, cookie| {
                seen.push(cookie.to_string());
                if cookie == "gv_session=stale" {
                    Ok(resp(401, b""))
                } else {
                    Ok(resp(200, b"fresh"))
                }
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert_eq!(body, b"fresh");
        assert_eq!(seen, ["gv_session=stale", "gv_session=fresh"]);
    }

    #[test]
    fn a_second_401_gives_up_rather_than_retrying_forever() {
        let mut sess = Some(session("gv_session=stale"));
        let mut fetches = 0;
        let err = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, _| {
                fetches += 1;
                Ok(resp(401, b"no"))
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap_err();
        assert_eq!(fetches, 2, "exactly one retry, never a loop");
        assert!(err.contains("even after re-authenticating"));
    }

    #[test]
    fn authed_post_sends_the_sessions_csrf_token_alongside_its_cookie() {
        let mut sess = Some(session("gv_session=live"));
        let mut seen: Vec<(String, String)> = Vec::new();
        let body = authed_post(
            "/api/select",
            b"{}",
            &mut sess,
            &mut |_, _, cookie, csrf| {
                seen.push((cookie.to_string(), csrf.to_string()));
                Ok(resp(200, b"Selected."))
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert_eq!(body, b"Selected.");
        assert_eq!(seen, [("gv_session=live".to_string(), "csrf".to_string())]);
    }

    /// Every failure string these two helpers build becomes a
    /// `ToolError::Execution` the MCP host may render or log, so neither may
    /// carry the live cookie or CSRF token. The error is assembled from the
    /// path, the status and the *server's* body — never from the session —
    /// and this pins that, in both the first-response and the
    /// retried-after-401 legs.
    #[test]
    fn a_failed_request_never_leaks_the_session_cookie_or_csrf_into_its_error() {
        const COOKIE: &str = "gv_session=CookieSecretABCDEF";
        const CSRF: &str = "CsrfSecret123456";
        let secret_session = || Session {
            cookie: COOKIE.to_string(),
            csrf: CSRF.to_string(),
        };

        let mut sess = Some(secret_session());
        let get_err = authed_fetch(
            "/api/status/v2",
            &mut sess,
            &mut |_, _| Ok(resp(500, b"the server said no")),
            &mut || panic!("500 is not 401 — no re-authentication"),
        )
        .unwrap_err();

        let mut sess = Some(secret_session());
        let post_err = authed_post(
            "/api/plan",
            b"{}",
            &mut sess,
            &mut |_, _, _, _| Ok(resp(401, b"stale")),
            &mut || Ok(secret_session()),
        )
        .unwrap_err();

        for err in [&get_err, &post_err] {
            // Anti-vacuity first: these really are the messages a client sees,
            // carrying the server's own words — not empty strings that would
            // pass the leak check for the wrong reason.
            assert!(err.contains("/api/"), "{err}");
            assert!(!err.contains("CookieSecretABCDEF"), "cookie leaked: {err}");
            assert!(!err.contains("CsrfSecret123456"), "csrf leaked: {err}");
        }
        assert!(get_err.contains("the server said no"), "{get_err}");
        assert!(
            post_err.contains("even after re-authenticating"),
            "{post_err}"
        );
    }

    #[test]
    fn authed_post_reauthenticates_once_on_401_like_authed_fetch() {
        let mut sess = Some(session("gv_session=stale"));
        let mut seen = Vec::new();
        let body = authed_post(
            "/api/select",
            b"{}",
            &mut sess,
            &mut |_, _, cookie, _csrf| {
                seen.push(cookie.to_string());
                if cookie == "gv_session=stale" {
                    Ok(resp(401, b""))
                } else {
                    Ok(resp(200, b"Selected."))
                }
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert_eq!(body, b"Selected.");
        assert_eq!(seen, ["gv_session=stale", "gv_session=fresh"]);
    }
}
