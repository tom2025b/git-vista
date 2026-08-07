//! #340: a structural gate over the offline write-guard, `refuse_if_offline()`
//! (`api.rs:197`).
//!
//! `mod api` is `#[cfg(target_arch = "wasm32")]`-gated in `main.rs`, and
//! `gloo-net`/`leptos`/`wasm-bindgen` (everything the module imports) live
//! under Cargo.toml's `[target.'cfg(target_arch = "wasm32")'.dependencies]`
//! block. `cargo test --workspace` therefore never compiles a line of
//! `api.rs`, on host or in CI. Twenty write functions in there call
//! `refuse_if_offline()` as (or effectively as) their first action today. If
//! someone deleted that call from one of them tomorrow, nothing in this repo
//! would notice: not one test exercises the guard, because none can link
//! against the module it lives in. `offline_refusal_text()`
//! (`git-vista-core/src/net.rs:43`) is tested, but it only pins the
//! *wording* of the refusal message — it proves nothing about whether
//! anything ever calls the function that would return it.
//!
//! This module is a ratchet in the same shape as
//! [`crate::features::a11y::audit`] (styles.css / render / dialogs source
//! censused as bytes, M1.12 #65) and `git-vista-server`'s `route_authz`
//! (main.rs's route table censused as bytes, M1.14 #67): it reads `api.rs`'s
//! own source text with `include_str!` and checks structural facts about the
//! bytes that ship, because it cannot execute the code that ships them. Two
//! layers:
//!
//!  1. **Discovery** ([`every_write_reaching_function_is_classified`]): find
//!     every function whose body calls one of the low-level transport
//!     helpers (`req_post`, `send_write`, `send_write_with_key`,
//!     `write_json`, `write_json_with_timeout`, `write_json_with_key`,
//!     `write_empty`) and require it to be classified — in [`OFFLINE_GUARDED`]
//!     (must call the guard), [`TRANSPORT_HELPERS`] itself (the layer *below*
//!     the guard; a write-reaching function joining this pinned list is a
//!     design decision, not a default), or [`EXEMPT_UNGUARDED`] (an argued
//!     exception). This is the direction that catches a brand-new mutation
//!     function added without ever wiring the guard in — the regression class
//!     the issue itself names.
//!  2. **Ordering and shape** ([`every_guarded_function_consults_the_guard_before_it_sends`]):
//!     for every name in `OFFLINE_GUARDED`, that its body calls
//!     `refuse_if_offline()` at **statement level** (not nested inside a
//!     dead branch, a closure, or a `match` arm that never runs — see
//!     [`at_statement_level`]), **before** any transport-helper call, and in
//!     a shape that actually **aborts on `Err`** — `refuse_if_offline()?`,
//!     or the `if let Err(...) = refuse_if_offline().and_then(...)` shape
//!     `amend_commit_request` uses — rather than merely being evaluated and
//!     discarded (`let _ = refuse_if_offline();` compiles and guards
//!     nothing).
//!
//! # What this census cannot prove
//!
//! This census pins decisions in api.rs's source bytes; it observes no
//! runtime behavior. Specifically:
//!
//! 1. It proves the guard is **called** before the send at statement level,
//!    not that its `Err` actually aborts. The consultation-shape check pins
//!    today's two shapes (`()?` and `if let Err(` / `match` scrutinee), but
//!    an `if let Err` arm that logs and falls through *without returning*
//!    would still satisfy it.
//! 2. It cannot prove `refuse_if_offline()` itself works.
//!    [`the_guard_itself_still_consults_the_online_signal`] pins that its
//!    body mentions `shell_state::is_online()` and `offline_refusal_text()`,
//!    but an inverted condition (`if !is_online() { Ok(()) }`) would pass
//!    every census here, and whether `is_online()` actually tracks
//!    `navigator.onLine` is wasm wiring only a device test can observe.
//! 3. It sees only `api.rs`. A future write issued from another module
//!    calling `gloo_net` directly never enters the discovery set — this
//!    rests on the (unpinned) convention that every server write goes
//!    through `api.rs`'s transport helpers.
//! 4. It cannot prove any guarded function is *reachable* from the UI, or
//!    that no early `return` precedes the guard in a caller — the same
//!    limit `audit.rs`'s own module doc states for `on_amend`.
//! 5. `offline_banner.rs` (the UI half named in #340, which has zero
//!    `#[test]`/`#[cfg(test)]` blocks of its own) remains completely
//!    untested by this module. This closes the guard boundary, not the
//!    banner.
//! 6. Textual ordering is not temporal ordering in the presence of code
//!    motion into earlier-constructed closures — though no function censused
//!    here has that shape today.

use std::collections::{BTreeMap, BTreeSet};

/// `api.rs`'s own source, read at test-compile time. Cargo tracks
/// `include_str!` as a build dependency, so an edit to `api.rs` recompiles
/// this test — there is no way for the two files to silently drift apart the
/// way a hand-copied fixture could.
const API_SRC: &str = include_str!("api.rs");

/// The low-level functions that actually put bytes on the wire for a write.
/// Everything above this layer reaches the server only by calling one of
/// these, directly or indirectly — this is the set [`is_write_reaching`]
/// scans a body for.
///
/// Membership here is itself a design decision, not a default: these are the
/// primitives `refuse_if_offline`'s own doc comment says the guard
/// deliberately does *not* live behind ("every write function calls this
/// first" — the chokepoint approach was considered and rejected there). A
/// function landing in this list is asserting "I am infrastructure the
/// guarded entry points build on, not an entry point myself" — see
/// [`the_exempt_and_transport_tables_do_not_rot`], which checks that claim.
const TRANSPORT_HELPERS: &[&str] = &[
    "req_post",
    "send_write",
    "send_write_with_key",
    "write_json",
    "write_json_with_timeout",
    "write_json_with_key",
    "write_empty",
];

/// Every function in `api.rs`, in file order, that reaches the write
/// transport and is required to call [`refuse_if_offline`] first. Adding a
/// write-reaching function to `api.rs` without adding it here (or to
/// [`TRANSPORT_HELPERS`]/[`EXEMPT_UNGUARDED`]) fails
/// [`every_write_reaching_function_is_classified`] — deliberately: deciding
/// whether a new write needs the offline guard is a decision, and this table
/// is where a human is forced to make it.
const OFFLINE_GUARDED: &[&str] = &[
    "clone_request",
    "create_branch_request",
    "create_commit_request",
    "amend_commit_request",
    "stage_request",
    "unstage_request",
    "undo_request",
    "discard_tracked_paths_request",
    "delete_untracked_paths_request",
    "rebase_request",
    "reset_test_repo_request",
    // Fans out to FIVE distinct server routes via its caller-supplied `path`
    // argument — /api/merge, /api/push, /api/checkout, /api/delete-branch,
    // /api/force-delete-branch (see features/operations/signals.rs:566-576).
    // One client function, one guard call, covers all five route-level
    // mutations.
    "branch_op_request",
    "fetch_request",
    "pull_request",
    // Not a git write — it sets a server-side cancellation latch, no git
    // object is touched — but api.rs's own doc comment on this function
    // calls the guard here defense-in-depth for an unreachable path, and
    // it IS write-shaped by every other measure this table uses, so it is
    // classified guarded rather than exempted.
    "cancel_operation_request",
    "select_request",
    "rescan_request",
    "delete_clone_request",
    "staging_preview_request",
    "staging_apply_request",
];

/// The pinned, argued exception list — mirrors `route_authz`'s
/// `EXPECTED_UNAUTHENTICATED` posture: a write-reaching function landing
/// here is a decision serious enough that widening this constant is the
/// explicit, visible act of making it, not something that follows implicitly
/// from a function merely existing.
///
/// `post_session` (`POST /api/session`, M1.04): exchanges the one-time
/// bootstrap token for a session cookie. It is not a repository write — no
/// git object, ref, or working-tree byte changes — and it *precedes* the
/// session state `refuse_if_offline`'s sibling guards
/// (`refuse_if_lan_view`/`refuse_if_visualize`) already assume exists.
/// Refusing it while offline would not protect a repo write; it would only
/// make sign-in itself fail on the one call that has to succeed before
/// anything else can. The survey that fed this module's design flagged this
/// exact boundary as needing an explicit human decision rather than a
/// silent default — this comment is that decision, made visibly.
const EXEMPT_UNGUARDED: &[&str] = &["post_session"];

/// The floor `fn_bodies` must clear before any "for every write-reaching
/// function…" assertion below is trusted. `api.rs` has ~70 functions today;
/// if the extractor silently stopped finding most of them (a Rust shape it
/// doesn't handle, a moved file), every downstream census would go green
/// while checking almost nothing.
const MIN_EXPECTED_FUNCTIONS: usize = 40;

/// The literal, zero-argument call text every guarded function is expected
/// to contain. `refuse_if_offline` takes no arguments (see `api.rs:197`), so
/// matching the closed call `"refuse_if_offline()"` — rather than the open
/// `"refuse_if_offline("` — lets every downstream check work directly off
/// what immediately follows the closing paren, with no separate
/// paren-balancing pass needed.
const GUARD_CALL: &str = "refuse_if_offline()";

/// Whether the byte at `idx` in `code` begins a new identifier-ish token —
/// i.e. the preceding byte, if any, is not `[A-Za-z0-9_]`. Guards every
/// needle search below against a coincidental substring hit (the same
/// concern `route_authz::extract_registered_routes` names for its `get(`
/// vs. `target(` example).
fn word_boundary_before(code: &str, idx: usize) -> bool {
    idx == 0 || {
        let prev = code.as_bytes()[idx - 1];
        !(prev.is_ascii_alphanumeric() || prev == b'_')
    }
}

/// Blank `//`, `///`, and `//!` line comments to end-of-line, leaving string
/// literals — including their contents — untouched. A byte-for-byte copy of
/// `git_vista_server::route_authz::strip_line_comments`; kept as a separate,
/// self-contained copy rather than a shared crate helper, matching this
/// repo's existing posture (`route_authz.rs` and `a11y/audit.rs` each carry
/// their own scanning helpers rather than factoring a shared one out).
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.char_indices().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some((_, ch)) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }
        if ch == '/' && chars.peek().map(|&(_, c)| c) == Some('/') {
            for (_, c2) in chars.by_ref() {
                if c2 == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Blank the *interior* of every string literal to spaces, one output
/// character per input character, keeping the boundary `"` characters
/// themselves. Runs on output already passed through [`strip_line_comments`],
/// so it only ever sees genuine string literals, never comment text.
///
/// This is the half of `neutralize` with no precedent in `route_authz`
/// (which needs route *paths*, which live inside string literals, and so
/// must leave string contents alone). This census needs the opposite: no
/// needle it searches for is ever legitimately found inside a string, so a
/// future error message that happens to contain the literal text
/// `"refuse_if_offline"` or `"write_json"` must never be able to satisfy a
/// census or a discovery scan the way a real call would.
fn blank_string_interiors(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in src.chars() {
        if in_string {
            if escaped {
                out.push(' ');
                escaped = false;
            } else if ch == '\\' {
                out.push(' ');
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                out.push('"');
            } else {
                out.push(' ');
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push('"');
            continue;
        }
        out.push(ch);
    }
    out
}

/// The two-step false-positive guard every census below runs on top of:
/// comments stripped, then string interiors blanked. After this pass, every
/// remaining occurrence of a needle this module searches for is either a
/// real piece of code or nothing at all — never a doc comment quoting the
/// function name, never an error message that happens to contain it.
fn neutralize(src: &str) -> String {
    blank_string_interiors(&strip_line_comments(src))
}

/// Confine scanning to the region before a `mod tests` marker, if one exists
/// — `route_authz`'s posture, applied here even though `api.rs` has no test
/// module today, so this module keeps working correctly rather than
/// silently scanning fixture code if one is ever added.
fn scan_region(code: &str) -> &str {
    match code.find("mod tests") {
        Some(at) => &code[..at],
        None => code,
    }
}

/// Every top-level `fn` found in `code` (already [`neutralize`]d), as
/// `(name, body)` — `body` is the brace-balanced text between the function's
/// own `{` and its matching `}`, exclusive.
///
/// Scans for whole-word `fn ` tokens, reads the identifier that follows,
/// skips an optional `<...>` generic parameter list, balances the `(...)`
/// parameter list (so a parameter type like `impl FnOnce()` with its own
/// nested parens doesn't truncate the scan early — the same paren-balancing
/// `route_authz::extract_registered_routes` uses for a `.route(...)` call's
/// argument list), then finds and brace-balances the body
/// (`a11y::audit::braced_body`'s technique).
///
/// Fails closed throughout: an unbalanced `<...>`, `(...)`, or `{...}`
/// panics naming the function being scanned, rather than silently returning
/// a truncated or wrong body. A duplicate function name also panics — this
/// module looks functions up by name in several places, and two functions
/// sharing a name would make every one of those lookups ambiguous.
fn fn_bodies(code: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut idx = 0usize;
    while let Some(rel) = code[idx..].find("fn ") {
        let at = idx + rel;
        if !word_boundary_before(code, at) {
            idx = at + 3;
            continue;
        }

        let name_start = at + 3;
        let mut name_end = name_start;
        for (off, ch) in code[name_start..].char_indices() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                name_end = name_start + off + ch.len_utf8();
            } else {
                break;
            }
        }
        if name_end == name_start {
            // "fn " not followed by an identifier — not a function
            // definition (a `Fn`/`FnOnce`/`FnMut` trait bound can never
            // reach here: it needs a literal lowercase "fn " and none of
            // those spell that).
            idx = at + 3;
            continue;
        }
        let name = code[name_start..name_end].to_string();

        let mut cursor = name_end;
        let trimmed = code[cursor..].trim_start();
        cursor += code[cursor..].len() - trimmed.len();

        // Optional generic parameter list: `fn write_json<T: Serialize>(`.
        if code[cursor..].starts_with('<') {
            let mut depth = 0i32;
            let mut closed = false;
            for (off, ch) in code[cursor..].char_indices() {
                match ch {
                    '<' => depth += 1,
                    '>' => {
                        depth -= 1;
                        if depth == 0 {
                            cursor += off + 1;
                            closed = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            assert!(
                closed,
                "fn_bodies: unbalanced `<...>` generic parameter list scanning `fn {name}`"
            );
        }

        let trimmed = code[cursor..].trim_start();
        cursor += code[cursor..].len() - trimmed.len();
        assert!(
            code[cursor..].starts_with('('),
            "fn_bodies: expected `(` after `fn {name}` (and any generics), found: {:?}",
            &code[cursor..(cursor + 20).min(code.len())]
        );
        let mut depth = 0i32;
        let mut params_end = None;
        for (off, ch) in code[cursor..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        params_end = Some(cursor + off + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let params_end = params_end.unwrap_or_else(|| {
            panic!("fn_bodies: unbalanced parameter-list parens scanning `fn {name}`")
        });

        let Some(brace_rel) = code[params_end..].find('{') else {
            panic!("fn_bodies: `fn {name}(...)` has no body brace following it");
        };
        let open = params_end + brace_rel;
        let mut depth = 0i32;
        let mut body_end = None;
        for (off, ch) in code[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        body_end = Some(open + off);
                        break;
                    }
                }
                _ => {}
            }
        }
        let body_end = body_end.unwrap_or_else(|| {
            panic!("fn_bodies: unbalanced braces scanning the body of `fn {name}`")
        });

        assert!(
            seen.insert(name.clone()),
            "fn_bodies: duplicate function name `{name}` found scanning api.rs — every \
             classification table in this module looks functions up by name, and a \
             duplicate makes that ambiguous"
        );
        out.push((name, code[open + 1..body_end].to_string()));
        idx = body_end + 1;
    }
    out
}

/// [`neutralize`] + [`scan_region`] + [`fn_bodies`] + the anti-vacuity floor,
/// as a map every test below builds once and looks functions up in by name.
fn bodies_map() -> BTreeMap<String, String> {
    let code = neutralize(API_SRC);
    let bodies = fn_bodies(scan_region(&code));
    assert!(
        bodies.len() >= MIN_EXPECTED_FUNCTIONS,
        "only {} functions were parsed out of api.rs — the extractor in \
         offline_guard_audit.rs has lost the file or choked on a shape it does not \
         handle, and every census in this module is now vacuous",
        bodies.len()
    );
    bodies.into_iter().collect()
}

/// The first byte offset in `body` where `name(` occurs as a whole word, or
/// `None` if it never does.
fn first_word_call(body: &str, name: &str) -> Option<usize> {
    let needle = format!("{name}(");
    let mut from = 0usize;
    while let Some(rel) = body[from..].find(needle.as_str()) {
        let idx = from + rel;
        if word_boundary_before(body, idx) {
            return Some(idx);
        }
        from = idx + needle.len();
    }
    None
}

/// Whether `body` calls `name(` anywhere, as a whole word.
fn word_call(body: &str, name: &str) -> bool {
    first_word_call(body, name).is_some()
}

/// The first byte offset in `body` where any [`TRANSPORT_HELPERS`] name is
/// called as a whole word, or `None` if the body never reaches the wire at
/// all.
fn first_transport_call_position(body: &str) -> Option<usize> {
    TRANSPORT_HELPERS
        .iter()
        .filter_map(|h| first_word_call(body, h))
        .min()
}

/// Whether `name`'s `body` reaches the write transport: either `name` *is*
/// one of [`TRANSPORT_HELPERS`] (those functions ARE the write path, by
/// definition, whether or not their own body happens to call a sibling
/// helper), or `body` calls one of them.
fn is_write_reaching(name: &str, body: &str) -> bool {
    TRANSPORT_HELPERS.contains(&name) || first_transport_call_position(body).is_some()
}

/// Whether `needle` occurs in `body` at `body`'s **own statement level** —
/// brace depth zero within it. A byte-for-byte copy of
/// `crate::features::a11y::audit::at_statement_level`: `if false { .. }`, a
/// `match` arm, or a closure all raise the depth, and an occurrence under
/// any of them is text the block does not unconditionally reach — the exact
/// gap that module's doc comment records a real regression slipping past a
/// whole-file `contains()` check before this technique replaced it.
fn at_statement_level(body: &str, needle: &str) -> bool {
    let mut depth = 0i32;
    for (i, c) in body.char_indices() {
        if depth == 0 && body[i..].starts_with(needle) {
            return true;
        }
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    false
}

/// [`at_statement_level`], but returning *where* the needle was found rather
/// than merely whether it was — every check below that needs to reason about
/// what comes before or after the guard call needs the position, not just
/// the yes/no.
fn statement_level_position(body: &str, needle: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in body.char_indices() {
        if depth == 0 && body[i..].starts_with(needle) {
            return Some(i);
        }
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Whether the guard call found at `gpos` (a [`statement_level_position`] hit
/// for [`GUARD_CALL`]) is consulted in a shape that actually aborts on
/// `Err`, rather than being evaluated and thrown away.
///
/// Three shapes are accepted — api.rs uses the first everywhere except
/// `amend_commit_request`, which uses the second, folded into the third:
///
/// 1. `refuse_if_offline()?` — the `?` propagates `Err` out of the function
///    immediately; the compiler enforces the abort, this only has to spot
///    the `?` immediately following the call.
/// 2. `refuse_if_offline().and_then(...)` — a chain that composes with the
///    next guard, immediately followed by `.and_then(`.
/// 3. The call sits as (or inside) the scrutinee of an `if let Err(` or
///    `match ` on the same statement — found by walking backward from
///    `gpos` to the nearest such keyword with no statement terminator (`;`
///    or `}`) between it and the call.
///
/// `let _ = refuse_if_offline();` matches none of the three: it compiles,
/// runs the guard, and discards the answer. That shape is exactly the
/// regression this function exists to catch.
fn consultation_shape_ok(body: &str, gpos: usize) -> bool {
    let after = &body[gpos + GUARD_CALL.len()..];
    if after.starts_with('?') || after.starts_with(".and_then(") {
        return true;
    }
    let before = &body[..gpos];
    for keyword in ["if let Err(", "match "] {
        if let Some(pos) = before.rfind(keyword) {
            let between = &before[pos..];
            if !between.contains(';') && !between.contains('}') {
                return true;
            }
        }
    }
    false
}

/// The three-way outcome [`check_guard`] reaches for one function body —
/// kept as a named enum rather than a bare `bool` so
/// [`every_guarded_function_consults_the_guard_before_it_sends`] can name
/// exactly which of the three failure modes fired, instead of an
/// undifferentiated "assertion failed".
#[derive(Debug, PartialEq, Eq)]
enum GuardVerdict {
    /// `refuse_if_offline()` never appears at statement level — deleted, or
    /// nested somewhere that never runs.
    MissingCall,
    /// The guard is called at statement level, but only after the body has
    /// already reached the write transport.
    WrongOrder,
    /// The guard runs before the send, but not in a shape that actually
    /// aborts on `Err`.
    WrongShape,
    /// Called, before the send, in a shape that aborts on `Err`.
    Ok,
}

/// Runs the full ordering+shape check described on [`consultation_shape_ok`]
/// over one function body (already [`neutralize`]d).
fn check_guard(body: &str) -> GuardVerdict {
    let Some(gpos) = statement_level_position(body, GUARD_CALL) else {
        return GuardVerdict::MissingCall;
    };
    match first_transport_call_position(body) {
        Some(tpos) if gpos < tpos => {}
        _ => return GuardVerdict::WrongOrder,
    }
    if consultation_shape_ok(body, gpos) {
        GuardVerdict::Ok
    } else {
        GuardVerdict::WrongShape
    }
}

/// Whether `src` contains a Rust raw-string prefix (`r"`, `r#"`, `r##"`, …)
/// or a raw identifier (`r#name`) as a whole word — any of which would
/// confuse [`strip_line_comments`]'s/[`blank_string_interiors`]'s
/// escape-aware `"`-tracking, which assumes every `"` is either a plain
/// string boundary or an escaped `\"` inside one.
fn contains_raw_string_prefix(src: &str) -> bool {
    let bytes = src.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'r' && word_boundary_before(src, i) {
            if let Some(&next) = bytes.get(i + 1) {
                if next == b'"' || next == b'#' {
                    return true;
                }
            }
        }
    }
    false
}

// ── Anti-vacuity / precondition ─────────────────────────────────────────────

/// Preconditions [`neutralize`] depends on. If either ever fires, api.rs has
/// grown a string shape the comment-stripper/string-blanker above cannot
/// track correctly — a raw string has no escape-aware closing `"` to look
/// for, and a `'"'` char literal contains an unescaped `"` that is not a
/// string boundary at all, either of which would desynchronise the
/// `in_string` tracking for everything that follows it in the file. Today
/// there are neither (verified directly against the file this test reads).
#[test]
fn api_rs_has_no_string_shapes_the_neutralizer_cannot_handle() {
    assert!(
        !contains_raw_string_prefix(API_SRC),
        "api.rs now contains a raw string (`r\"...\"`/`r#\"...\"#`) or a raw identifier \
         (`r#name`) — offline_guard_audit.rs's comment/string scan does not understand \
         raw strings and will misread every byte after one. Extend `neutralize()` \
         (crates/git-vista/src/offline_guard_audit.rs) before trusting any other test \
         in this module."
    );
    assert!(
        !API_SRC.contains("'\"'"),
        "api.rs now contains a `'\"'` char literal — neutralize()'s string-interior scan \
         tracks every `\"` as a string boundary unconditionally and will misread this \
         one. Extend `neutralize()` before trusting any other test in this module."
    );
    let defs = API_SRC.matches("fn refuse_if_offline(").count();
    assert_eq!(
        defs, 1,
        "expected exactly one `fn refuse_if_offline(` definition in api.rs, found {defs} — \
         every lookup by that name in this module (GUARD_CALL scans, the direct \
         bodies.get(\"refuse_if_offline\") in the test below) is ambiguous otherwise"
    );
}

// ── Discovery: every write-reaching function is classified ─────────────────

/// This is the assertion that catches #340's actual regression class: a
/// *new* function is added to api.rs, reaches the write transport, and is
/// never wired to the offline guard. Nothing about the 20 functions already
/// classified would notice that — only re-running discovery over the whole
/// file does.
///
/// Checked both directions, `route_authz`-style: every function this module
/// *finds* reaching the transport must be classified somewhere, and every
/// name classified somewhere must still be found — the second half catches
/// [`OFFLINE_GUARDED`]/[`TRANSPORT_HELPERS`]/[`EXEMPT_UNGUARDED`] rotting
/// (a function renamed or deleted, leaving a stale entry nobody trusts).
#[test]
fn every_write_reaching_function_is_classified() {
    let bodies = bodies_map();

    let expected: BTreeSet<&str> = OFFLINE_GUARDED
        .iter()
        .chain(TRANSPORT_HELPERS.iter())
        .chain(EXEMPT_UNGUARDED.iter())
        .copied()
        .collect();

    let discovered: BTreeSet<&str> = bodies
        .iter()
        .filter(|(name, body)| is_write_reaching(name, body))
        .map(|(name, _)| name.as_str())
        .collect();

    for name in &discovered {
        assert!(
            expected.contains(name),
            "OFFLINE GUARD DECISION MISSING: `{name}` reaches the write transport in \
             api.rs but is not classified in offline_guard_audit.rs — add \
             `refuse_if_offline()?;` as its first action and list it in \
             OFFLINE_GUARDED, or argue an exemption in EXEMPT_UNGUARDED"
        );
    }
    for name in &expected {
        assert!(
            bodies.contains_key(*name),
            "STALE TABLE ENTRY: `{name}` is classified in offline_guard_audit.rs \
             (OFFLINE_GUARDED/TRANSPORT_HELPERS/EXEMPT_UNGUARDED) but api.rs no longer \
             defines a function by that name — remove the entry"
        );
        assert!(
            discovered.contains(name),
            "STALE TABLE ENTRY: `{name}` is classified in offline_guard_audit.rs but its \
             body no longer reaches the write transport — if that's intentional, remove \
             it from whichever table lists it rather than leaving a decision nothing \
             still applies to"
        );
    }
}

// ── Ordering and shape: the guard runs first, and is actually consulted ────

/// The core claim this module makes, and the one the module doc's "what
/// this cannot prove" section qualifies most heavily: every
/// [`OFFLINE_GUARDED`] function calls `refuse_if_offline()` at statement
/// level, before it reaches the write transport, in a shape that aborts on
/// `Err`. See [`check_guard`]/[`consultation_shape_ok`] for exactly what
/// "shape" means, and the module doc's numbered limitations list — item 1
/// in particular — for what this specifically does *not* prove about that
/// shape.
#[test]
fn every_guarded_function_consults_the_guard_before_it_sends() {
    let bodies = bodies_map();
    for name in OFFLINE_GUARDED {
        let body = bodies.get(*name).unwrap_or_else(|| {
            panic!(
                "STALE TABLE ENTRY: OFFLINE_GUARDED lists `{name}` but api.rs no longer \
                 defines it — remove it from offline_guard_audit.rs"
            )
        });
        match check_guard(body) {
            GuardVerdict::Ok => {}
            GuardVerdict::MissingCall => panic!(
                "OFFLINE GUARD MISSING: `{name}` is listed in OFFLINE_GUARDED but its \
                 body never calls refuse_if_offline() at statement level (deleted, or \
                 moved inside a branch that never runs). Add `refuse_if_offline()?;` as \
                 its first action in crates/git-vista/src/api.rs."
            ),
            GuardVerdict::WrongOrder => panic!(
                "OFFLINE GUARD OUT OF ORDER: `{name}` calls refuse_if_offline() at \
                 statement level, but only AFTER it has already reached the write \
                 transport (a call to one of {TRANSPORT_HELPERS:?}). Move the guard call \
                 in crates/git-vista/src/api.rs to before the send."
            ),
            GuardVerdict::WrongShape => panic!(
                "OFFLINE GUARD NOT CONSULTED: `{name}` calls refuse_if_offline() before \
                 the send, but not in a shape that aborts on Err — expected \
                 `refuse_if_offline()?` or an `if let Err(...)`/`match` over it (the \
                 amend_commit_request `.and_then(...)` shape). `let _ = \
                 refuse_if_offline();` compiles and guards nothing; fix it in \
                 crates/git-vista/src/api.rs."
            ),
        }
    }
}

// ── The tables themselves must not rot ──────────────────────────────────────

/// [`TRANSPORT_HELPERS`] and [`EXEMPT_UNGUARDED`] each carry an implicit
/// claim `every_write_reaching_function_is_classified` does not check on its
/// own: a transport helper is supposed to sit *below* the guard (never call
/// it), and an exemption is supposed to have an argued reason it never needs
/// to. If either starts calling `refuse_if_offline()`, that claim is now
/// false, and the entry needs to move — visibly, not silently keep working
/// either way.
#[test]
fn the_exempt_and_transport_tables_do_not_rot() {
    let bodies = bodies_map();

    for name in TRANSPORT_HELPERS {
        let body = bodies.get(*name).unwrap_or_else(|| {
            panic!(
                "STALE TABLE ENTRY: TRANSPORT_HELPERS lists `{name}` but api.rs no \
                 longer defines it"
            )
        });
        assert!(
            !word_call(body, "refuse_if_offline"),
            "`{name}` is classified in TRANSPORT_HELPERS — the layer BELOW the offline \
             guard — but its body now calls refuse_if_offline(). The guard has moved \
             down into the chokepoint; if that's deliberate, move `{name}` to \
             OFFLINE_GUARDED instead of leaving it classified as infrastructure that \
             happens to also guard itself."
        );
    }

    for name in EXEMPT_UNGUARDED {
        let body = bodies.get(*name).unwrap_or_else(|| {
            panic!(
                "STALE TABLE ENTRY: EXEMPT_UNGUARDED lists `{name}` but api.rs no \
                 longer defines it"
            )
        });
        assert!(
            is_write_reaching(name, body),
            "`{name}` is listed in EXEMPT_UNGUARDED (an argued exception to the offline \
             guard) but no longer reaches the write transport at all — if it isn't a \
             write anymore, delete the entry instead of leaving a stale exemption"
        );
        assert!(
            !word_call(body, "refuse_if_offline"),
            "`{name}` is listed in EXEMPT_UNGUARDED with an argued reason it skips the \
             offline guard, but its body now calls refuse_if_offline() — it is guarded \
             after all. Move it to OFFLINE_GUARDED and delete the exemption argument."
        );
    }
}

// ── The guard itself ─────────────────────────────────────────────────────────

/// Closes part of the module doc's limitation #2: that `refuse_if_offline`
/// still consults `shell_state::is_online()` (the browser's `navigator.onLine`
/// signal) rather than having been hollowed out to an unconditional `Ok(())`,
/// and still answers with [`offline_refusal_text`] — the pinned wording
/// `git-vista-core::net`'s own host test protects — on the offline path.
///
/// What it does NOT close: an inverted condition
/// (`if !is_online() { Ok(()) } else { Err(...) }`) reads identically to
/// this census, and whether `is_online()` actually tracks the real browser
/// signal is wasm wiring no host test can observe.
///
/// [`offline_refusal_text`]: git_vista_core::net::offline_refusal_text
#[test]
fn the_guard_itself_still_consults_the_online_signal() {
    let bodies = bodies_map();
    let body = bodies.get("refuse_if_offline").unwrap_or_else(|| {
        panic!(
            "refuse_if_offline() itself was not found by the source scan — has it been \
             renamed or moved out of crates/git-vista/src/api.rs?"
        )
    });
    assert!(
        at_statement_level(body, "shell_state::is_online()"),
        "refuse_if_offline() no longer consults shell_state::is_online() at statement \
         level — the guard has been hollowed out, or the check moved somewhere \
         conditional that can be skipped entirely"
    );
    assert!(
        word_call(body, "offline_refusal_text"),
        "refuse_if_offline() no longer returns offline_refusal_text() on the offline \
         path — the wording pinned by git-vista-core::net's own host test is no longer \
         what a caller actually sees"
    );
}

// ── Fixture proof: every predicate above can actually fail ─────────────────
//
// A predicate that only ever returns the passing answer would satisfy every
// assertion above perfectly while checking nothing (the exact shape this
// whole issue exists to close, one level up). Each test below runs the full
// pipeline — neutralize, then fn_bodies, then check_guard — over a small
// synthetic function, proving the pipeline can reach every one of
// check_guard's three failure verdicts as well as its passing one.

/// `neutralize` + `fn_bodies`, asserting the fixture contains exactly one
/// function, and returning its body — the same pipeline `bodies_map` runs
/// over the real file, applied to a hand-written fixture instead.
fn only_fn_body(src: &str) -> String {
    let code = neutralize(src);
    let mut found = fn_bodies(&code);
    assert_eq!(
        found.len(),
        1,
        "fixture must define exactly one function, found {}",
        found.len()
    );
    found.remove(0).1
}

#[test]
fn guard_present_first_is_accepted() {
    let src = r#"
        pub async fn stage_request() -> Result<(), String> {
            refuse_if_offline()?;
            refuse_if_visualize()?;
            let (resp, _key) = write_empty("/api/stage").await?;
            Ok(())
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::Ok);
}

#[test]
fn guard_deleted_is_caught() {
    let src = r#"
        pub async fn stage_request() -> Result<(), String> {
            refuse_if_visualize()?;
            let (resp, _key) = write_empty("/api/stage").await?;
            Ok(())
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::MissingCall);
}

#[test]
fn guard_moved_after_the_send_is_caught() {
    let src = r#"
        pub async fn stage_request() -> Result<(), String> {
            let (resp, _key) = write_empty("/api/stage").await?;
            refuse_if_offline()?;
            Ok(())
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::WrongOrder);
}

#[test]
fn guard_inside_a_dead_branch_is_caught() {
    let src = r#"
        pub async fn stage_request() -> Result<(), String> {
            if false {
                refuse_if_offline()?;
            }
            let (resp, _key) = write_empty("/api/stage").await?;
            Ok(())
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::MissingCall);
}

#[test]
fn guard_mentioned_only_in_a_comment_is_caught() {
    let src = r#"
        pub async fn stage_request() -> Result<(), String> {
            // refuse_if_offline()?;
            let (resp, _key) = write_empty("/api/stage").await?;
            Ok(())
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::MissingCall);
}

#[test]
fn guard_mentioned_only_inside_a_string_literal_is_caught() {
    let src = r#"
        pub async fn stage_request() -> Result<(), String> {
            let note = "refuse_if_offline()?;";
            let (resp, _key) = write_empty("/api/stage").await?;
            Ok(())
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::MissingCall);
}

#[test]
fn guard_result_silently_dropped_is_caught() {
    let src = r#"
        pub async fn stage_request() -> Result<(), String> {
            let _ = refuse_if_offline();
            let (resp, _key) = write_empty("/api/stage").await?;
            Ok(())
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::WrongShape);
}

#[test]
fn the_amend_and_then_shape_is_accepted() {
    // The real shape from amend_commit_request (api.rs:1112-1113).
    let src = r#"
        pub async fn amend_commit_request(message: &str, expected_tip: &str) -> AmendOutcome {
            if let Err(refusal) = refuse_if_offline().and_then(|()| refuse_if_visualize()) {
                return AmendOutcome::Unavailable(refusal);
            }
            let body = amend_body(message, expected_tip);
            let resp = match write_json("/api/amend-commit", &body).await {
                Ok((resp, _key)) => resp,
                Err(e) => return AmendOutcome::Unavailable(e),
            };
            classify_amend_response(status, &text)
        }
    "#;
    assert_eq!(check_guard(&only_fn_body(src)), GuardVerdict::Ok);
}
