//! A structural reachability census over `crates/git-vista/src/` and
//! `crates/git-vista-core/src/`: every `pub`/`pub(crate)` `fn` (free function
//! or method) declared there must have at least one statement-shaped call
//! site (`name(`) somewhere in the whole `crates/` tree, outside its own
//! `#[cfg(test)]` blocks and outside dedicated test files/directories — or be
//! listed in [`EXEMPT`] with an argued reason. A small, explicitly bounded
//! exception: three functions ([`EXEMPT`]'s `App`, `not_connected_view`,
//! `progress_line` entries) are passed *by value* to another function
//! instead of ever being called with parens — see
//! [`contains_real_call`]'s doc for why that gap is handled by exemption
//! rather than by loosening the census's own matching.
//!
//! # Why this exists
//!
//! `cargo test --workspace` runs on the host target only. Every wasm32-gated
//! view module in `git-vista` (`app`, `activity`, `detail`, `menu`, `api`,
//! `session`, `picker`, …) is invisible to it — not compiled, not linked, not
//! executed (`main.rs:57-99`). The pure-logic modules those views are
//! *supposed* to call (`features/*/core.rs`, `git-vista-core`'s public
//! surface) DO compile and get tested on the host — but only by tests
//! co-located in the same file, calling the function directly. A function can
//! therefore be fully covered by its own unit tests and have **zero**
//! real callers anywhere a browser would ever reach, and nothing in
//! `./dev gate` notices: `dead_code` structurally exempts `pub` items (and
//! `git-vista-core` is a real `[lib]` crate, doubly exempt), and the two
//! checks that DO touch wasm32 code (`clippy --target wasm32-unknown-unknown`,
//! `trunk build`) only compile/lint/bundle — neither loads the result into a
//! runtime, so neither can observe whether anything is ever called.
//!
//! Three real regressions had exactly this shape: `StatusSections`/`StatusRow`
//! (#68d), `CumulativeHeights` (#69c), and `scroll_to_reveal` (#350) — each
//! shipped, fully host-tested, with zero production call sites, until a later
//! change wired the view layer to them. This module is a permanent,
//! automated version of the by-hand census that found those three (and 16
//! genuine, already-argued-dead functions besides — see [`EXEMPT`]) so the
//! next one is caught by `cargo test`, not by a manual grep sweep.
//!
//! Modeled directly on the `include_str!`-census precedent this repo already
//! ships three times for the identical "wasm-gated / cross-crate, can't link
//! on host" problem shape: [`crate::offline_guard_audit`],
//! `features::a11y::audit`, and `git-vista-server`'s `route_authz`. Unlike
//! those three, this module scans the filesystem at *test run time*
//! (`std::fs`, not `include_str!`) rather than naming every file up front —
//! the set of files under the two scanned directories is too large and too
//! churny to hand-list, and Cargo cannot track a directory glob as a build
//! dependency the way it tracks a single `include_str!` path. The tradeoff:
//! an edit inside the scanned tree does not force this test to re-run the way
//! editing `api.rs` forces `offline_guard_audit` to — it re-runs whenever
//! `cargo test` re-runs this binary at all, same as every other `#[test]`.
//!
//! # What this proves, and what it does not
//!
//! Proves: every declared `pub`/`pub(crate)` `fn` in the two scanned
//! directories (excluding the [`GENERIC_NAME_SKIPLIST`] and [`EXEMPT`]) has
//! at least one textual, statement-shaped call site somewhere in the
//! `crates/` tree outside test-only code.
//!
//! Does NOT prove:
//!
//! 1. That the call site itself is reachable from a real user action —
//!    reachability here is "named at a real call site", not "provably on a
//!    path from `main()`". A function called only by another function that
//!    is itself dead would still read as "reachable" one hop early. (Every
//!    hand-verified instance of this shape in the source census turned out
//!    to resolve correctly one hop further out — see the module-level
//!    comment on `resolve_release`/`drag_released` in
//!    `features/shell/signals.rs` — but this module does not walk that
//!    chain itself.)
//! 2. Anything about `struct`/`enum`/`trait`/`const`/`static`/`type` items.
//!    The source census found ~87% of its raw candidates in those kinds were
//!    false positives of one shape — a type consumed by field access or
//!    method call at the call site, never by writing the type's name — and
//!    resolving that reliably needs the two-hop transitive check the source
//!    census did by hand. Scoping this module to `fn`/method items only
//!    sidesteps that false-positive class entirely, at the cost of not
//!    covering those other four item kinds at all.
//! 3. Anything about a name in [`GENERIC_NAME_SKIPLIST`] (`new`, `default`,
//!    `fmt`, …) — those are excluded because a single genuinely-orphaned
//!    `new()` would be invisible against every *other* type's real `new()`
//!    calls scattered across the tree; this census cannot tell them apart by
//!    name alone.
//! 4. Anything inside a block comment (`/* … */`) — only line comments
//!    (`//`, `///`, `//!`) are stripped, matching every other census in this
//!    repo; none of the scanned directories use block comments today
//!    (checked: `grep -rn '/\*' crates/git-vista/src crates/git-vista-core/src`
//!    finds none outside string literals).
//! 5. Runtime behavior of any kind — like its siblings, this is a text
//!    census, not an execution. A call site that is itself unreachable code
//!    (e.g. behind a condition that can never be true) still counts.
//! 6. A function passed *by value* rather than called (`.then(f)`,
//!    `.map(f)`, `mount_to_body(f)`) never matches `name(` and reads as an
//!    orphan. Confirmed to happen for real: `App`, `not_connected_view`, and
//!    `progress_line` all ship this way. A looser "any bare word" match was
//!    tried and reverted — it hid a genuinely orphaned `sheet.rs` method
//!    behind an unrelated struct field of the same name elsewhere in the
//!    tree (see [`contains_real_call`]'s doc). The three known instances are
//!    exempted individually, with their real call site cited; a *new*
//!    function shipped this way will misreport as an orphan until someone
//!    notices and adds it to [`EXEMPT`] the same way.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ── Shared text-scanning primitives ─────────────────────────────────────────
// `word_boundary_before` is a byte-for-byte copy of
// `crate::offline_guard_audit`'s helper of the same name, per this repo's
// established posture of each census carrying its own self-contained
// scanning helpers rather than factoring out a shared crate.

fn word_boundary_before(code: &str, idx: usize) -> bool {
    idx == 0 || {
        let prev = code.as_bytes()[idx - 1];
        !(prev.is_ascii_alphanumeric() || prev == b'_')
    }
}

fn word_boundary_before_chars(chars: &[char], idx: usize) -> bool {
    idx == 0 || !(chars[idx - 1].is_ascii_alphanumeric() || chars[idx - 1] == '_')
}

/// Strips `//`/`///`/`//!` line comments and `/* … */` block comments (any
/// nesting depth) to end-of-comment, and blanks the *interior* of every
/// string-shaped literal — plain `"…"`, raw `r"…"`/`r#"…"#`/`r##"…"##`/…,
/// and `'x'`/`'\n'`/`'\u{1F600}'` char literals — leaving delimiters (`"`,
/// `r#`, `'`) and structural punctuation (braces, parens) exactly where they
/// were. After this, every remaining occurrence of a needle this module
/// searches for is real code: never a comment quoting a name, never a
/// string or char literal containing one.
///
/// A single combined pass, unlike `offline_guard_audit`'s two-pass
/// `strip_line_comments` + `blank_string_interiors` (which only tracks plain
/// `"…"` strings): this census scans the *whole* `crates/` tree rather than
/// one hand-picked file, and raw strings (`r#"…"#`, common in fixtures
/// building JSON bodies) and char-literal braces/quotes (`'{'`, `'"'`) both
/// appear across it. Confirmed empirically: `crates/git-vista/src/features/
/// dialogs/commit.rs:1553` has `r#"{"branch":"main","#` — a raw string
/// whose content contains an unescaped `{` and `"` — which the two-pass
/// approach misreads as a plain string boundary, desynchronizing every
/// downstream brace-balance scan for the rest of the file. A lifetime
/// (`'a`) is distinguished from a char literal by a bounded lookahead (14
/// chars) for a closing `'`; Rust char literals are always short, so a
/// lifetime — which is never followed by another `'` within that window —
/// is left untouched.
fn neutralize(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;

    while i < n {
        let c = chars[i];

        // Line comment: `//` to end of line (keeps the newline itself).
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            i += 2;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // Block comment: `/* ... */`, tracking nesting depth (Rust block
        // comments nest).
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let mut depth = 1i32;
            i += 2;
            while i < n && depth > 0 {
                if chars[i] == '/' && i + 1 < n && chars[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                } else {
                    if chars[i] == '\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            }
            continue;
        }

        // Raw string: `r`/`r#`/`r##`/… immediately followed by `"`. A bare
        // `r` not followed by `#`*`"` is either a raw identifier (`r#name`,
        // which has no `"` after the hashes) or just a variable named `r` —
        // both fall through to the plain-character path below untouched.
        if c == 'r' && word_boundary_before_chars(&chars, i) {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while j < n && chars[j] == '#' {
                hashes += 1;
                j += 1;
            }
            if j < n && chars[j] == '"' {
                out.push('r');
                for _ in 0..hashes {
                    out.push('#');
                }
                out.push('"');
                let mut k = j + 1;
                loop {
                    if k >= n {
                        break; // unterminated — copy nothing further
                    }
                    if chars[k] == '"' {
                        let mut hcount = 0usize;
                        let mut m = k + 1;
                        while m < n && hcount < hashes && chars[m] == '#' {
                            hcount += 1;
                            m += 1;
                        }
                        if hcount == hashes {
                            out.push('"');
                            for _ in 0..hashes {
                                out.push('#');
                            }
                            k = m;
                            break;
                        }
                    }
                    out.push(if chars[k] == '\n' { '\n' } else { ' ' });
                    k += 1;
                }
                i = k;
                continue;
            }
            // Not a raw string — normal handling below.
        }

        // Plain string literal.
        if c == '"' {
            out.push('"');
            i += 1;
            while i < n {
                if chars[i] == '\\' && i + 1 < n {
                    out.push(' '); // the backslash itself
                    i += 1;
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                    continue;
                }
                if chars[i] == '"' {
                    out.push('"');
                    i += 1;
                    break;
                }
                out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            continue;
        }

        // Char literal or lifetime: bounded lookahead for a closing `'`.
        // Char literals are always short (`'x'`, `'\n'`, `'\u{1F600}'`); a
        // lifetime (`'a`) never has a second `'` within the window, so it
        // falls through untouched.
        if c == '\'' {
            let bound = (i + 14).min(n);
            let mut k = i + 1;
            let mut close = None;
            while k < bound {
                if chars[k] == '\'' {
                    close = Some(k);
                    break;
                }
                if chars[k] == '\\' && k + 1 < bound {
                    k += 2;
                    continue;
                }
                k += 1;
            }
            if let Some(close) = close {
                out.push('\'');
                for &ch in &chars[(i + 1)..close] {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
                out.push('\'');
                i = close + 1;
                continue;
            }
            // No close within bound — treat as a lifetime, fall through.
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Remove every `#[cfg(test)]`-attributed item's braced body from `code`
/// (already [`neutralize`]d) — `mod tests { .. }`, or a single
/// `#[cfg(test)] fn helper() { .. }` — leaving the rest of the file
/// untouched. Panics naming `label` (the file being processed) on unbalanced
/// braces, fail-closed, matching this repo's other censuses: a scan that
/// silently swallowed a parse failure would make every result downstream of
/// it vacuous.
fn strip_cfg_test_blocks(code: &str, label: &str) -> String {
    let marker = "#[cfg(test)]";
    let mut out = String::with_capacity(code.len());
    let mut idx = 0usize;
    while let Some(rel) = code[idx..].find(marker) {
        let at = idx + rel;
        if !word_boundary_before(code, at) {
            out.push_str(&code[idx..at + 1]);
            idx = at + 1;
            continue;
        }
        out.push_str(&code[idx..at]);
        let Some(brace_rel) = code[at..].find('{') else {
            // No item body follows (e.g. a trailing `#[cfg(test)]` with
            // nothing after it) — keep the rest verbatim rather than guess.
            out.push_str(&code[at..]);
            idx = code.len();
            break;
        };
        let open = at + brace_rel;
        let mut depth = 0i32;
        let mut close = None;
        for (off, ch) in code[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + off);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.unwrap_or_else(|| {
            panic!(
                "reachability_census: unbalanced braces stripping a #[cfg(test)] block in \
                 {label} — this file's #[cfg(test)] content could not be safely excluded, \
                 so every result from scanning it is untrustworthy"
            )
        });
        idx = close + 1;
    }
    out.push_str(&code[idx..]);
    out
}

/// Whether the `fn ` at byte offset `fn_at` in `code` (the position of the
/// `f` in `fn `) is `pub`/`pub(...)` — i.e. immediately preceded (skipping
/// whitespace, `async`, `unsafe`, `const`, `extern "C"`) by that visibility
/// keyword. Used only during declaration *extraction*
/// ([`extract_pub_fn_decls`]), where the scan is already positioned on the
/// `fn ` token itself.
fn is_pub_fn_declaration(code: &str, fn_at: usize) -> bool {
    let before = &code[..fn_at];
    let mut window_start = before.len().saturating_sub(160);
    while window_start > 0 && !before.is_char_boundary(window_start) {
        window_start += 1;
    }
    let window = &before[window_start..];
    let skip = ["async", "unsafe", "const", "extern", "\"C\"", "\"Rust\""];
    let tokens: Vec<&str> = window.split_whitespace().collect();
    let mut i = tokens.len();
    while i > 0 && skip.contains(&tokens[i - 1]) {
        i -= 1;
    }
    if i == 0 {
        return false;
    }
    let vis = tokens[i - 1];
    vis == "pub" || vis.starts_with("pub(")
}

/// Whether the identifier occurrence at byte offset `name_at` in `code` is
/// itself sitting right after a `fn ` token — i.e. this occurrence of
/// `name(` IS the function's own declaration (`pub fn toggle_line(`), not a
/// call to it (`self.toggle_line(`, `Foo::toggle_line(`). Both contain the
/// literal substring `toggle_line(`; only whether `fn` immediately precedes
/// the identifier (skipping whitespace) tells them apart. Used by
/// [`contains_real_call`], where the scan is positioned on the identifier
/// itself, not on `fn ` — a different offset than [`is_pub_fn_declaration`]
/// expects, which is why this is a separate function rather than a shared
/// one called with two different meanings of its parameter.
fn is_declaration_site(code: &str, name_at: usize) -> bool {
    let before = code[..name_at].trim_end();
    if !before.ends_with("fn") {
        return false;
    }
    let word_start = before.len() - 2;
    word_start == 0 || {
        let prev = before.as_bytes()[word_start - 1];
        !(prev.is_ascii_alphanumeric() || prev == b'_')
    }
}

/// The identifier immediately following `fn ` at `name_start`, or `None` if
/// `fn ` isn't followed by one (a `Fn`/`FnOnce`/`FnMut` trait bound can't
/// reach here — those need a capital `Fn`, and the caller only ever searches
/// for lowercase `"fn "`).
fn identifier_after(code: &str, name_start: usize) -> Option<&str> {
    let mut name_end = name_start;
    for (off, ch) in code[name_start..].char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            name_end = name_start + off + ch.len_utf8();
        } else {
            break;
        }
    }
    if name_end == name_start {
        None
    } else {
        Some(&code[name_start..name_end])
    }
}

// ── Declaration extraction ──────────────────────────────────────────────────

/// One declared item this census tracks.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Declaration {
    /// Path relative to the `crates/` root, forward-slash separated
    /// (e.g. `git-vista/src/features/diff/selection.rs`).
    rel_path: String,
    name: String,
    /// 1-based line number of the declaration, for readable failure output.
    line: usize,
}

/// Names deliberately excluded from extraction entirely: common
/// constructor/trait-method names likely to collide with an unrelated type's
/// identically-named, genuinely-called method elsewhere in the tree. A
/// census that can't tell `OrphanType::new()` apart from `WiredType::new()`
/// by name alone would either miss a real orphan (false green) or flag a
/// wired one (false red) depending on which type it happens to check first —
/// excluding the whole name class is more honest than guessing.
const GENERIC_NAME_SKIPLIST: &[&str] = &[
    "new",
    "default",
    "from",
    "try_from",
    "into",
    "try_into",
    "as_ref",
    "as_mut",
    "clone",
    "fmt",
    "eq",
    "ne",
    "cmp",
    "partial_cmp",
    "hash",
    "drop",
    "deref",
    "deref_mut",
    "next",
    "iter",
    "len",
    "is_empty",
    "id",
    "get",
    "set",
    "with",
];

/// Whether byte offset `at` in `code` (already [`neutralize`]d, NOT
/// [`strip_cfg_test_blocks`]ed) falls inside any `#[cfg(test)]`-attributed
/// item's braced body. Same brace-matching as `strip_cfg_test_blocks`, but
/// checks membership instead of removing the span — removing would shift
/// every later byte offset, which is exactly the bug this function exists to
/// avoid: [`extract_pub_fn_decls`] needs line numbers that match the real,
/// on-disk file, and stripping text before counting newlines desyncs them
/// the moment an earlier `#[cfg(test)]` block in the same file is removed.
/// Caught empirically: `features/graph/core.rs` has a `#[cfg(test)]` block
/// at line 100, and `label_occupancy`'s declaration at its real line 538
/// was first mis-reported as line 456 — the exact size of the removed block
/// — before this function replaced the remove-then-count approach.
fn is_within_cfg_test_block(code: &str, at: usize, label: &str) -> bool {
    let marker = "#[cfg(test)]";
    let mut idx = 0usize;
    while let Some(rel) = code[idx..].find(marker) {
        let m_at = idx + rel;
        if !word_boundary_before(code, m_at) {
            idx = m_at + 1;
            continue;
        }
        let Some(brace_rel) = code[m_at..].find('{') else {
            break;
        };
        let open = m_at + brace_rel;
        let mut depth = 0i32;
        let mut close = None;
        for (off, ch) in code[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + off);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.unwrap_or_else(|| {
            panic!(
                "reachability_census: unbalanced braces scanning a #[cfg(test)] block in \
                 {label} — this file's #[cfg(test)] content could not be safely identified, \
                 so every result from scanning it is untrustworthy"
            )
        });
        if at >= m_at && at <= close {
            return true;
        }
        idx = close + 1;
    }
    false
}

/// Every `pub`/`pub(crate)` `fn` declaration in `code` — a whole file's
/// contents, [`neutralize`]d but deliberately **not**
/// [`strip_cfg_test_blocks`]ed, so `Declaration::line` matches the real
/// on-disk file exactly (see [`is_within_cfg_test_block`]'s doc for why) —
/// excluding anything inside a `#[cfg(test)]` block and excluding
/// [`GENERIC_NAME_SKIPLIST`] names.
fn extract_pub_fn_decls(code: &str, rel_path: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    while let Some(rel) = code[idx..].find("fn ") {
        let at = idx + rel;
        if !word_boundary_before(code, at) {
            idx = at + 3;
            continue;
        }
        if !is_pub_fn_declaration(code, at) {
            idx = at + 3;
            continue;
        }
        let Some(name) = identifier_after(code, at + 3) else {
            idx = at + 3;
            continue;
        };
        if !GENERIC_NAME_SKIPLIST.contains(&name) && !is_within_cfg_test_block(code, at, rel_path) {
            let line = code[..at].matches('\n').count() + 1;
            out.push(Declaration {
                rel_path: rel_path.to_string(),
                name: name.to_string(),
                line,
            });
        }
        idx = at + 3;
    }
    out
}

// ── Consumer search ──────────────────────────────────────────────────────────

/// Whether `path` (relative to `crates/`) is dedicated test scaffolding that
/// should never count as a real caller, mirroring the source census's own
/// exclusions: a `tests/` path component, or a filename stem of `tests`,
/// `*_test`, or `*_tests`. Also `*_suite` (added when `features/graph/core.rs`
/// and `durable.rs` split their inline `#[cfg(test)] mod tests` into child
/// modules, M-current): this repo's other, older test-extraction convention
/// — `git-vista-server/src/planner/*_suite.rs` — names the exact same
/// category of file, just with the suffix this census didn't yet recognize.
/// A `#[cfg(test)] mod foo_suite;` declaration in the parent already gates
/// the whole file's compilation on the test profile; nothing inside
/// `foo_suite.rs` itself carries a per-item `#[cfg(test)]` for
/// [`strip_cfg_test_blocks`] to find, so without this the file's test-only
/// calls (e.g. `graph_core_suite.rs`'s use of `GraphCore::at_generation`)
/// were misread as real, non-test call sites.
fn is_dedicated_test_path(rel_path: &str) -> bool {
    if rel_path.split('/').any(|seg| seg == "tests") {
        return true;
    }
    let stem = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .trim_end_matches(".rs");
    stem == "tests"
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with("_suite")
}

/// Whether `code` (already [`neutralize`]d + [`strip_cfg_test_blocks`]ed)
/// contains a real, non-declaration, statement-shaped call to `name` —
/// `name(` at a word boundary, that is not itself `name`'s own `pub fn`
/// declaration line.
///
/// Deliberately requires the trailing `(`, even though a first version of
/// this function tried the looser "any whole-word occurrence" reading (to
/// match named-function-passed-by-value call sites — see [`EXEMPT`]'s
/// `App`/`not_connected_view`/`progress_line` entries for that real, if
/// narrow, gap). That looser version was reverted after it produced its own
/// false negatives in the other direction, caught empirically on this
/// census's own first broadened run: `features/shell/sheet.rs`'s
/// `height_px` collided with an unrelated struct field of the same name at
/// `features/a11y/core.rs:36,59` (`TapTarget { width_px, height_px }`,
/// struct-init shorthand — a real, non-comment, non-call occurrence of the
/// bare word that has nothing to do with the sheet), silently marking a
/// genuinely orphaned sheet.rs method "reachable". A struct field or local
/// variable sharing a function's name is common enough in a codebase this
/// size that "any bare word" is worse than the narrower gap it was meant to
/// close. The by-value-passing gap is bounded and known (three functions,
/// listed in [`EXEMPT`] with their real call sites cited) — a name
/// collision with an unrelated field is not bounded at all.
fn contains_real_call(code: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    let mut from = 0usize;
    while let Some(rel) = code[from..].find(needle.as_str()) {
        let at = from + rel;
        if word_boundary_before(code, at) && !is_declaration_site(code, at) {
            return true;
        }
        from = at + needle.len();
    }
    false
}

// ── Filesystem walk ──────────────────────────────────────────────────────────

/// The `crates/` directory to census: `GIT_VISTA_CENSUS_ROOT` if set
/// (used to point this exact test at a scratch copy for the red-state
/// demonstration — see `design-docs/2026-08-08-reachability-census-red-proof.md`),
/// otherwise computed from `CARGO_MANIFEST_DIR` (`crates/git-vista`'s
/// parent is `crates/`).
fn crates_root() -> PathBuf {
    if let Ok(over) = std::env::var("GIT_VISTA_CENSUS_ROOT") {
        return PathBuf::from(over);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap_or_else(|| {
            panic!(
                "reachability_census: {} has no parent directory",
                manifest.display()
            )
        })
        .to_path_buf()
}

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "reachability_census: cannot read dir {}: {e}",
            dir.display()
        )
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            panic!(
                "reachability_census: dir entry error under {}: {e}",
                dir.display()
            )
        });
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "target" | "dist" | ".git" | "node_modules") {
                continue;
            }
            walk_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn to_rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ── The exempt table ─────────────────────────────────────────────────────────

/// Argued exceptions — a `(rel_path, name)` pair landing here is a decision,
/// same posture as `offline_guard_audit::EXEMPT_UNGUARDED`: widening this
/// table is the visible act of arguing a function should stay unreferenced,
/// not something that happens implicitly. Every entry traces to the 2026-08
/// orphan census (this task) and is checked against api.rs-precedent
/// discipline by [`the_exempt_table_does_not_rot`] below: an entry whose
/// function has since GAINED a real caller, or been deleted, must be removed
/// from here, not left stale.
const EXEMPT: &[(&str, &str)] = &[
    // gestures.rs:250's own doc comment: Enter/Space handling reads the DOM
    // event's own target rather than asking GraphFocus which row is
    // focused, making both `GraphFocus::focused_row` AND `::activate`
    // redundant by design. Neither is listed here, for two DIFFERENT
    // reasons this census's own rules produce, worth recording so a future
    // reader doesn't "fix" either by re-adding it:
    //   - `focused_row` has a real call site: `activate`'s own body
    //     (focus.rs:188). `activate` itself has none — but this census only
    //     walks one hop (module doc limitation #1), so `focused_row` reads
    //     as reachable even though its only caller is itself unreached.
    //   - `activate` reads as reachable too, but for a WRONG reason: a
    //     completely unrelated local closure in gestures.rs (captured
    //     earlier in that function, nothing to do with `GraphFocus`) is
    //     also named `activate` and is called at gestures.rs:313 with a
    //     matching call shape (`activate(x, y)` vs. `GraphFocus::activate`'s
    //     own `(&self)` — different signatures, same text). A genuine name
    //     collision this text census cannot see past (module doc
    //     limitation #6's sibling case: collision, not by-value passing).
    //     `GraphFocus::activate` is, by the same reasoning that made
    //     `focused_row` redundant, still real dead code — this census just
    //     cannot currently prove it, so it is not listed as exempt either
    //     (exempting it would be arguing a false reason: "no reference
    //     found" is not this case).
    // selection.rs:50-63's own module doc: no per-line UI wiring exists yet;
    // issue #215's own scope box explicitly permitted splitting it out.
    ("git-vista/src/features/diff/selection.rs", "toggle_line"),
    (
        "git-vista/src/features/diff/selection.rs",
        "is_line_selected",
    ),
    (
        "git-vista/src/features/diff/selection.rs",
        "select_all_in_hunk",
    ),
    // Only matters at extreme zoom-out (MIN_ZOOM=0.2); #65's 44px floor is
    // met at scale 1.0 by construction (audit.rs's own tripwire test), and
    // no production path clamps camera scale using this value today.
    (
        "git-vista/src/features/a11y/core.rs",
        "min_camera_scale_for_guidance",
    ),
    // signals.rs:506's own doc says it exists "for the submit handler that
    // must not subscribe" — but dialogs/commit.rs's real submit_commit
    // closure takes the intent as a parameter instead, so this was never
    // called the way its own doc anticipated.
    (
        "git-vista/src/features/shell/signals.rs",
        "commit_dialog_untracked",
    ),
    // menu/remote_items.rs's `remote_op_running` reimplements the same
    // underlying .in_flight() iteration directly rather than calling this
    // accessor (moved from menu.rs by the menu.rs split, refactor/split-menu-rs).
    (
        "git-vista/src/features/operations/signals.rs",
        "in_flight_count",
    ),
    // status/core.rs:186's own doc: StatusSections::headline() "matches
    // RepoStatus::change_count()'s identical semantics, which this
    // replaces" — self-documented as deliberately superseded.
    ("git-vista-core/src/status.rs", "change_count"),
    // main.rs:42-46: the whole `graph` module is `#[cfg(test)] mod graph;`
    // at the declaration site in main.rs — test-only by construction.
    ("git-vista/src/graph.rs", "fake_graph"),
    // geometry.rs's own doc: "Superseded in the views by
    // label_x_per_row... kept as the documented old behaviour, pinned by
    // its test below."
    ("git-vista/src/geometry.rs", "label_x"),
    // geometry.rs's own doc: "M1.10 (#63): the paged views no longer call
    // this... stays as the documented reference the incremental one
    // mirrors."
    ("git-vista/src/geometry.rs", "label_x_per_row"),
    // geometry.rs's own doc: paged views call stub_headroom_for instead;
    // "this form stays as the whole-Graph reference its tests below pin."
    ("git-vista/src/geometry.rs", "stub_headroom"),
    // GraphCore derives Default for production start state; at_generation
    // is a pub test-convenience constructor, never marked #[cfg(test)]
    // itself but only ever called from the file's own test module.
    ("git-vista/src/features/graph/core.rs", "at_generation"),
    // dialogs/signals.rs:519-522's own doc: the signals wrapper
    // "deliberately exposes no open_dialog() reader" — the core's record of
    // which dialog is up can lag reality.
    ("git-vista/src/features/dialogs/core.rs", "open_dialog"),
    // ── By-value passing (see contains_real_call's doc and limitation #6
    // above): never spelled `name(` in production, only passed by name to
    // another function. Each cited to its real, verified call site. ────────
    //
    // main.rs:114: `leptos::mount_to_body(app::App);` — the Leptos entry
    // point, mounted by reference, never invoked with parens anywhere.
    ("git-vista/src/app/mod.rs", "App"),
    // app/mod.rs:621: `{move || needs_sign_in().then(not_connected_view)}` —
    // `Option::then` takes the function itself.
    ("git-vista/src/session.rs", "not_connected_view"),
    // features/operations/view.rs:55: `e.progress.as_ref().map(progress_line)`.
    ("git-vista/src/features/operations/core.rs", "progress_line"),
    // features/activity/signals.rs:44: `self.core.update(ActivityCore::toggle);`
    // — `RwSignal::update` takes the function itself, not a call.
    ("git-vista/src/features/activity/core.rs", "toggle"),
    // ── Self-documented dead-by-design, found by this census, not in the
    // original 15-item manual list ─────────────────────────────────────────
    //
    // graph/core.rs:531-535's own doc: "The view never needs this... the
    // accessor exists for the host tests that pin the monotonic-growth
    // rule... on the browser target it genuinely has no caller, and that is
    // by design rather than a loose end." Same wasm-only dead_code shape as
    // the geometry.rs entries above, and says so explicitly.
    ("git-vista/src/features/graph/core.rs", "label_occupancy"),
    // layout/mod.rs's own doc on `layout()`: "Use layout_with_refs to also
    // attach branch/tag/HEAD badges." Production (git-vista-server's
    // handlers/read.rs) builds graphs through `layout::stream::StreamLayout`
    // exclusively (confirmed: no `layout(`/`layout_with_refs(` call anywhere
    // under crates/git-vista-server or crates/git-vista-git outside their
    // own #[cfg(test)] blocks). Both batch functions survive as the
    // differential-testing reference the streaming implementation is
    // checked against — `layout/tests/stream.rs` literally names its
    // `layout(...)` result `oracle`.
    ("git-vista-core/src/layout/mod.rs", "layout"),
    ("git-vista-core/src/layout/mod.rs", "layout_with_refs"),
    // sheet.rs's own module doc (lines 12-17): "nothing in this module is
    // consumed by crate::features::shell::signals yet... the model is
    // settled and tested; the sheet does not exist on screen. Wiring it is
    // the next slice." The whole file's public surface is pre-wiring by
    // design; these six are every non-generic pub fn this census found in it.
    ("git-vista/src/features/shell/sheet.rs", "taller"),
    ("git-vista/src/features/shell/sheet.rs", "shorter"),
    ("git-vista/src/features/shell/sheet.rs", "height_px"),
    ("git-vista/src/features/shell/sheet.rs", "flick_threshold"),
    ("git-vista/src/features/shell/sheet.rs", "expand"),
    ("git-vista/src/features/shell/sheet.rs", "collapse"),
    // app/mod.rs:797-800's own comment, right at the one place that could
    // have called this: "Through the shell, not `activity.toggle()`: the
    // overlay stack is what keeps the right edge to one panel... " —
    // `shell.toggle_activity()` is called instead, explicitly bypassing
    // this wrapper. `ActivityCore::toggle` (the type this wraps) IS reached,
    // by value, from this exact function's own body — see the by-value
    // section above — so this is the wrapper being skipped, not the whole
    // mechanism being dead.
    ("git-vista/src/features/activity/signals.rs", "toggle"),
    // ── Found by this census, no self-documentation located anywhere in the
    // surrounding module — genuinely new candidates the original manual
    // 15-item census did not include. Verified: `grep -rn "\bNAME("
    // --include=*.rs crates` finds no hit anywhere in the workspace (all 7
    // crates) outside the defining file's own #[cfg(test)] module. Exempted
    // here so the census is green against today's tree; each is a real,
    // reportable finding for a human to either wire up or remove — NOT an
    // argued-dead decision the way every entry above this line is. ────────
    (
        "git-vista/src/features/status/core.rs",
        "discardable_tracked_paths",
    ),
    (
        "git-vista/src/features/status/core.rs",
        "deletable_untracked_paths",
    ),
    ("git-vista/src/features/tags/core.rs", "tag_row"),
    ("git-vista/src/features/dialogs/commit.rs", "staged_breadth"),
    // `git-vista-core/src/request_generation.rs`'s `issue` used to sit here.
    // The census asked whoever found it to "wire it up or remove"; ADR 0053
    // answered *remove* — Leptos 0.6.15's own resource already drops
    // out-of-order completions, and every diff/detail response echoes its id
    // for the view to re-check before painting, so a third layer defended
    // nothing. The module is gone, so the exemption has nothing to exempt.
    ("git-vista-core/src/identity.rs", "hex_len"),
    ("git-vista-core/src/identity.rs", "algorithm"),
    ("git-vista-core/src/identity.rs", "from_raw"),
];

/// The floor the discovered-declaration count must clear before any
/// assertion below is trusted — guards against the walker silently finding
/// nothing (a moved directory, a `crates_root()` miscalculation) and every
/// downstream census going green while checking zero files. `git-vista/src`
/// and `git-vista-core/src` combined declare well over 100 non-generic
/// `pub`/`pub(crate)` `fn`s today.
const MIN_EXPECTED_DECLARATIONS: usize = 80;

/// The two directories this census scans for declarations, relative to
/// `crates/` — exactly the source census's own stated scope.
const DECLARATION_DIRS: &[&str] = &["git-vista/src", "git-vista-core/src"];

/// One preprocessed consumer file: its relative path and its
/// neutralize+strip_cfg_test_blocks'd text, built once and reused for every
/// name lookup.
struct ConsumerFile {
    rel_path: String,
    text: String,
}

fn load_consumer_files(root: &Path) -> Vec<ConsumerFile> {
    let mut paths = Vec::new();
    walk_rs_files(root, &mut paths);
    paths
        .into_iter()
        .map(|p| {
            let rel_path = to_rel_path(root, &p);
            let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| {
                panic!("reachability_census: cannot read {}: {e}", p.display())
            });
            let text = strip_cfg_test_blocks(&neutralize(&raw), &rel_path);
            ConsumerFile { rel_path, text }
        })
        .filter(|f| !is_dedicated_test_path(&f.rel_path))
        .collect()
}

/// Declaration-file text is read and [`neutralize`]d fresh here, deliberately
/// **not** shared with [`ConsumerFile`]'s cache: `ConsumerFile::text` is
/// [`strip_cfg_test_blocks`]ed, which removes spans and shifts every later
/// byte offset — fine for consumer membership checks (only presence matters
/// there), wrong for [`Declaration::line`] (must match the real file). See
/// [`is_within_cfg_test_block`]'s doc for the concrete bug this avoids.
fn declarations_in(root: &Path, scan_dirs: &[&str]) -> Vec<Declaration> {
    let mut out = Vec::new();
    for dir in scan_dirs {
        let mut paths = Vec::new();
        walk_rs_files(&root.join(dir), &mut paths);
        for p in paths {
            let rel_path = to_rel_path(root, &p);
            if is_dedicated_test_path(&rel_path) {
                continue;
            }
            let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| {
                panic!("reachability_census: cannot read {}: {e}", p.display())
            });
            let text = neutralize(&raw);
            out.extend(extract_pub_fn_decls(&text, &rel_path));
        }
    }
    out
}

/// Whether `decl` has a real consumer anywhere in `consumers` — the defining
/// file's own text is a valid consumer too (a same-file caller counts), just
/// never the declaration occurrence itself.
fn has_real_consumer(decl: &Declaration, consumers: &[ConsumerFile]) -> bool {
    consumers
        .iter()
        .any(|c| contains_real_call(&c.text, &decl.name))
}

// ── The census tests ─────────────────────────────────────────────────────────

/// The core claim: every non-generic-named `pub`/`pub(crate)` `fn` declared
/// under [`DECLARATION_DIRS`], not listed in [`EXEMPT`], has a real call site
/// somewhere in the whole `crates/` tree. This is the assertion that catches
/// a *new* #68d/#69c/#350-shaped regression: a function that used to have a
/// caller loses its only one, or a brand-new pure-logic function ships with
/// none — nothing about the functions already in [`EXEMPT`] would notice
/// that; only re-scanning the whole tree does.
#[test]
fn every_declared_fn_has_a_real_consumer_or_is_exempt() {
    let root = crates_root();
    let consumers = load_consumer_files(&root);
    assert!(
        !consumers.is_empty(),
        "reachability_census: found zero .rs files under {} — crates_root() is wrong \
         or the tree is missing",
        root.display()
    );

    let declarations = declarations_in(&root, DECLARATION_DIRS);
    assert!(
        declarations.len() >= MIN_EXPECTED_DECLARATIONS,
        "reachability_census: only found {} pub fn declarations under {:?} (root {}) — \
         expected at least {MIN_EXPECTED_DECLARATIONS}. The extractor has lost the tree \
         or choked on a shape it doesn't handle, and this census is now vacuous.",
        declarations.len(),
        DECLARATION_DIRS,
        root.display()
    );

    let exempt: BTreeSet<(&str, &str)> = EXEMPT.iter().map(|&(p, n)| (p, n)).collect();

    let mut orphans = Vec::new();
    for decl in &declarations {
        let key = (decl.rel_path.as_str(), decl.name.as_str());
        if exempt.contains(&key) {
            continue;
        }
        if !has_real_consumer(decl, &consumers) {
            orphans.push(decl.clone());
        }
    }

    assert!(
        orphans.is_empty(),
        "REACHABILITY CENSUS FAILED: {} declared pub fn(s) have no statement-shaped call \
         site anywhere in crates/, outside test-only code, and are not in \
         reachability_census::EXEMPT:\n{}\n\nFor each: either wire a real caller (view code, \
         a signals wrapper, another core fn on a real call path), or — if it is genuinely \
         dead by design — add `(\"{}\", \"{}\")` to EXEMPT with a comment arguing why, the \
         same way every other entry there is argued.",
        orphans.len(),
        orphans
            .iter()
            .map(|d| format!("  {}:{} — pub fn {}", d.rel_path, d.line, d.name))
            .collect::<Vec<_>>()
            .join("\n"),
        orphans.first().map(|d| d.rel_path.as_str()).unwrap_or(""),
        orphans.first().map(|d| d.name.as_str()).unwrap_or(""),
    );
}

/// [`EXEMPT`]'s bidirectional health check, `offline_guard_audit`-style: an
/// entry whose file/name no longer exists at all is stale and should be
/// deleted; an entry whose function has since GAINED a real caller is also
/// stale (the argued exception no longer applies) and should be deleted so
/// the table only ever lists genuinely-dead functions, not ones nobody has
/// re-checked in a year.
#[test]
fn the_exempt_table_does_not_rot() {
    let root = crates_root();
    let consumers = load_consumer_files(&root);
    let declarations = declarations_in(&root, DECLARATION_DIRS);

    for &(rel_path, name) in EXEMPT {
        let decl = declarations
            .iter()
            .find(|d| d.rel_path == rel_path && d.name == name);
        let Some(decl) = decl else {
            panic!(
                "STALE EXEMPT ENTRY: reachability_census::EXEMPT lists (\"{rel_path}\", \
                 \"{name}\") but no such pub fn declaration was found there anymore — \
                 remove the entry (or it was renamed/moved to a name skipped by \
                 GENERIC_NAME_SKIPLIST, which no longer needs an exemption at all)"
            );
        };
        assert!(
            !has_real_consumer(decl, &consumers),
            "STALE EXEMPT ENTRY: reachability_census::EXEMPT lists (\"{rel_path}\", \
             \"{name}\") as a documented-dead function, but it now has a real call site \
             somewhere in crates/ — it has been wired up since the exemption was written. \
             Remove the entry; the argued exception no longer applies."
        );
    }
}

// ── Fixture proof: the pipeline can actually reach both verdicts ───────────
//
// Runs the full pipeline (neutralize -> strip_cfg_test_blocks -> extract ->
// contains_real_call) over small synthetic "files" held only in memory, never
// touching the filesystem — a permanent regression test that this census's
// own machinery can fail, not just a claim about it.

#[test]
fn fixture_a_real_caller_is_recognized() {
    let def_raw = r#"
        pub fn scroll_to_reveal(target: f64, viewport: f64) -> f64 {
            target.max(viewport)
        }
    "#;
    let caller_raw = r#"
        fn on_hunk_focus(target: f64, viewport: f64) {
            let offset = scroll_to_reveal(target, viewport);
            apply(offset);
        }
    "#;
    let def_text = strip_cfg_test_blocks(&neutralize(def_raw), "fixture/def.rs");
    let caller_text = strip_cfg_test_blocks(&neutralize(caller_raw), "fixture/caller.rs");
    let decls = extract_pub_fn_decls(&def_text, "fixture/def.rs");
    assert_eq!(decls.len(), 1, "fixture should declare exactly one pub fn");
    assert_eq!(decls[0].name, "scroll_to_reveal");

    let consumers = [
        ConsumerFile {
            rel_path: "fixture/def.rs".into(),
            text: def_text,
        },
        ConsumerFile {
            rel_path: "fixture/caller.rs".into(),
            text: caller_text,
        },
    ];
    assert!(
        has_real_consumer(&decls[0], &consumers),
        "fixture: a real call site in a sibling file should be found"
    );
}

#[test]
fn fixture_declaration_alone_is_not_its_own_consumer() {
    // Regression guard for the exact false-positive this module's whole
    // design has to avoid: `pub fn toggle_line(` contains the literal
    // substring `toggle_line(`, immediately preceded by `fn `. If
    // `contains_real_call` ever regressed to a bare substring search, every
    // declared function would trivially "call itself" and this census would
    // silently protect nothing — the same failure class #340/#68d/#69c/#350
    // all share.
    let raw = r#"
        pub fn toggle_line(path: &str, line: u32) {
            // no caller anywhere — issue #215 split the UI wiring out
        }
    "#;
    let text = strip_cfg_test_blocks(&neutralize(raw), "fixture/only.rs");
    let decls = extract_pub_fn_decls(&text, "fixture/only.rs");
    assert_eq!(decls.len(), 1);
    let consumers = [ConsumerFile {
        rel_path: "fixture/only.rs".into(),
        text,
    }];
    assert!(
        !has_real_consumer(&decls[0], &consumers),
        "fixture: a declaration with no other occurrence must NOT be its own consumer"
    );
}

#[test]
fn fixture_call_inside_the_files_own_test_module_does_not_count() {
    let raw = r#"
        pub fn orphaned_helper(x: u32) -> u32 {
            x + 1
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn calls_it_directly() {
                assert_eq!(orphaned_helper(1), 2);
            }
        }
    "#;
    let text = strip_cfg_test_blocks(&neutralize(raw), "fixture/helper.rs");
    let decls = extract_pub_fn_decls(&text, "fixture/helper.rs");
    assert_eq!(decls.len(), 1);
    let consumers = [ConsumerFile {
        rel_path: "fixture/helper.rs".into(),
        text,
    }];
    assert!(
        !has_real_consumer(&decls[0], &consumers),
        "fixture: a call that exists ONLY inside the file's own #[cfg(test)] mod must not \
         count as a real consumer — this is the exact shape that let #68d/#69c/#350 ship \
         fully host-tested with zero production callers"
    );
}

#[test]
fn fixture_dedicated_test_file_is_excluded_from_consumer_search() {
    assert!(is_dedicated_test_path(
        "git-vista/src/features/diff/tests/fixtures.rs"
    ));
    assert!(is_dedicated_test_path(
        "git-vista-core/src/layout/tests/topology.rs"
    ));
    assert!(is_dedicated_test_path("git-vista/src/foo_test.rs"));
    assert!(is_dedicated_test_path("git-vista/src/foo_tests.rs"));
    assert!(is_dedicated_test_path(
        "git-vista/src/features/graph/core/graph_core_suite.rs"
    ));
    assert!(!is_dedicated_test_path(
        "git-vista/src/features/diff/core.rs"
    ));
}

#[test]
fn fixture_generic_names_are_skipped_at_extraction() {
    let raw = r#"
        pub fn new(x: u32) -> Self { Self(x) }
        pub fn genuinely_unique_name(x: u32) -> u32 { x }
    "#;
    let text = strip_cfg_test_blocks(&neutralize(raw), "fixture/generic.rs");
    let decls = extract_pub_fn_decls(&text, "fixture/generic.rs");
    let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
    assert!(
        !names.contains(&"new"),
        "GENERIC_NAME_SKIPLIST should have excluded `new` from extraction"
    );
    assert!(names.contains(&"genuinely_unique_name"));
}
