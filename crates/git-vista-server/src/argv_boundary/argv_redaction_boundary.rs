//! #801: a tripwire for the hazard `network_exec::redact_args`'s doc comment
//! names but nothing enforced — a future (or, as this file's own fix shows,
//! an *existing*) logging or panic call site that embeds a Network-tier
//! spawn's raw `args: &[&str]` without redacting it first.
//! `redact_output` only ever sees a spawn's captured stdout/stderr; `args`
//! itself routinely carries a remote URL's `user:token@` userinfo (every SSH
//! test in `network_exec.rs`, and every real caller, passes the remote URL
//! as one of `args`), so a diagnostic that formats `args` directly bypasses
//! `redact_output` entirely. `redact_args` exists to give that diagnostic the
//! same treatment as a first-class primitive — but until this file, nothing
//! made a caller use it.
//!
//! # This tripwire is not hypothetical — it already caught a live one
//!
//! `sandbox::reconcile_need`'s D3 cross-check (the sealed chokepoint's own
//! "declared `Local`, argv looks `Remote`" mismatch guard) formatted the raw
//! `args` slice directly into both its `debug_assert!` panic message and its
//! release `eprintln!`. That branch fires precisely when an operation's argv
//! *looks* remote — the one condition under which `args` is most likely to
//! carry a credential-bearing URL — and it is reached through
//! `git_cmd.rs`'s `sandboxed`, the single seam every git command this server
//! runs goes through. It shipped that way; the issue's own audit ("I checked
//! every tracing/eprintln!/format! site in the server that names `args`, and
//! none does") missed it, most likely because `debug_assert!`/`eprintln!`
//! read as defensive plumbing rather than "a diagnostic path", not because
//! the pattern wasn't there. Fixed in the same commit that adds this file:
//! both call sites now format `redact_args(args)`. See `reconcile_need`'s own
//! doc comment for the full reachability argument.
//!
//! # Scope, and the blind spot that comes with it
//!
//! This does not walk every `.rs` file in the crate. `args` is too common an
//! identifier — CLI parsing, unrelated fixtures, `sandbox_argv`'s own shapes —
//! for a bare crate-wide text match to stay both sound and useful; a
//! throwaway check while writing this file found exactly one real hit
//! (`reconcile_need`, above) against a couple of incidental near-misses
//! (`gv-sandbox-reaper`'s `<args…>` usage string, which never interpolates a
//! variable at all). So instead this follows `argv_boundary.rs`'s own
//! `ALLOWED_SPAWN_SITES` shape: a short, named, hand-reviewed list of the
//! functions that actually receive a Network-tier-capable `args: &[&str]` —
//! [`SINK_FUNCTIONS`] below — each one's *production* body (never its tests)
//! scanned for a logging or panic macro that names `args` without a
//! `redact_args(...)` wrapping it.
//!
//! Stated plainly, because a scan that cannot fail this way is not honest
//! about what it proves: a **new** function added to this call chain leaks
//! silently until it is added to [`SINK_FUNCTIONS`] (the same "reviewed after
//! the fact" posture `ALLOWED_SPAWN_SITES` accepts for spawn sites); and
//! *inside* an already-listed function, aliasing `args` to a differently
//! named binding before logging it defeats the token match, because this is
//! text matching over `code_only`-stripped source, not real data-flow
//! analysis. What it does prove: none of the six functions that constitute
//! this crate's actual Network-tier/chokepoint call chain today log `args`
//! unredacted, and any of them regressing to do so — including the exact
//! shape `reconcile_need` had — fails the build.
//!
//! # Why this needs the *raw* source, not only `code_only`'s
//!
//! The actual hazard lives inside a format string's captured identifier
//! (`{args:?}`), which is a *string literal* — and `code_only` blanks string
//! *contents* on purpose (so a comment or string cannot be mistaken for
//! code). Scanning only the `code_only`-stripped text would make the exact
//! violation this file fixes invisible: `{args:?}` inside the string would
//! already be blanked to spaces. So this scan uses `code_only`'s output only
//! to find *structure* — where a macro call starts and where its balanced
//! closing paren is, so a stray `(`/`)` inside a string like `"(D3)"` cannot
//! desynchronise the count — and then reads the **original** source text at
//! those same offsets to see what the macro actually argues. `code_only`
//! blanks every character it touches one-for-one (a multi-byte character
//! becomes one ASCII space), so its output has exactly as many `char`s as the
//! input; every offset in this file is a `char` index for exactly that
//! reason; `network_argv_sink_functions_never_log_args_unredacted` asserts
//! the two texts' `char` counts still match rather than assuming it.

use std::path::Path;

use super::code_only;

/// One function this crate's Network-tier / sealed-chokepoint call chain
/// passes a `args: &[&str]`-shaped parameter through, as `(file relative to
/// this crate's `src/`, function name)`. Reviewed by hand; see the module
/// doc for why this is a named list rather than a crate-wide scan.
const SINK_FUNCTIONS: &[(&str, &str)] = &[
    // The sealed chokepoint's own cross-check. `git_cmd.rs`'s doc calls
    // `sandboxed` "the single seam every git the server runs goes through" —
    // every spawn's args, network-declared or not, passes through here. This
    // is where the live hazard this file fixes was found.
    ("src/sandbox/mod.rs", "reconcile_need"),
    // The two callers of `reconcile_need`, and the only two places a
    // `NetworkNeed` is resolved into an actual spawn.
    ("src/git_cmd.rs", "sandboxed"),
    ("src/git_cmd.rs", "sandboxed_with_grant"),
    // The Network-tier harness itself: composes the argv and builds the
    // launcher from it, in all three forms — no credential, a supplied
    // credential, and (#702/#704/#723) the post-credential-use untrusted
    // checkout phase that still carries the same `args`.
    ("src/sandbox/network_exec.rs", "compose_network_args"),
    ("src/sandbox/network_exec.rs", "network_command"),
    (
        "src/sandbox/network_exec.rs",
        "network_command_with_credential",
    ),
    (
        "src/sandbox/network_exec.rs",
        "network_command_without_credential",
    ),
];

/// Macros whose message can end up somewhere a human or a log aggregator
/// reads it — stderr directly, `tracing`/`log`, or a panic message.
/// `panic!`/`assert!`/`debug_assert!` count for the same reason `eprintln!`
/// does: `reconcile_need`'s own hazard shipped inside a `debug_assert!`
/// message, which a crash reporter or CI log captures exactly like any other
/// diagnostic. (Two pairs here are substrings of each other — `debug_assert!(`
/// contains `assert!(`, and `eprintln!(` contains `println!(` — so a single
/// violating call is matched, and reported, twice; a mutation-proof run
/// confirmed exactly this for an `eprintln!` regression. Harmless: it only
/// ever adds a duplicate line to an already-failing assertion's message,
/// never hides one.)
const EMISSION_MACROS: &[&str] = &[
    "eprintln!(",
    "println!(",
    "tracing::error!(",
    "tracing::warn!(",
    "tracing::info!(",
    "tracing::debug!(",
    "tracing::trace!(",
    "log::error!(",
    "log::warn!(",
    "log::info!(",
    "log::debug!(",
    "log::trace!(",
    "panic!(",
    "debug_assert!(",
    "assert!(",
];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// First index at or after `from` where `needle` occurs in `hay`, both as
/// `char` slices.
fn find_seq(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

/// Every index in `hay` where `needle` occurs, non-overlapping, in order.
fn find_all(hay: &[char], needle: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start <= hay.len() {
        match find_seq(&hay[start..], needle) {
            Some(rel) => {
                out.push(start + rel);
                start += rel + 1;
            }
            None => break,
        }
    }
    out
}

/// The index of the delimiter in `chars` (at or after `open_idx`, which must
/// already hold `open_ch`) that balances it, counting nested `open_ch`s.
/// `open_ch` and `close_ch` must differ — this is for parens/braces, never a
/// symmetric delimiter like a quote.
fn matching_close(chars: &[char], open_idx: usize, open_ch: char, close_ch: char) -> Option<usize> {
    assert_ne!(open_ch, close_ch);
    assert_eq!(chars.get(open_idx), Some(&open_ch));
    let mut depth = 0i32;
    let mut i = open_idx;
    loop {
        let c = chars[i];
        if c == open_ch {
            depth += 1;
        } else if c == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        if i + 1 >= chars.len() {
            return None;
        }
        i += 1;
    }
}

/// Whether `ident` occurs in `chars` as a standalone token — not as part of a
/// longer identifier (so `redact_args`'s own name never counts as a bare
/// `args`).
fn contains_bare_ident(chars: &[char], ident: &str) -> bool {
    let needle: Vec<char> = ident.chars().collect();
    let n = needle.len();
    if n == 0 || chars.len() < n {
        return false;
    }
    (0..=chars.len() - n).any(|i| {
        chars[i..i + n] == needle[..]
            && (i == 0 || !is_ident_char(chars[i - 1]))
            && (i + n == chars.len() || !is_ident_char(chars[i + n]))
    })
}

/// The `(start, end)` `char`-index range (exclusive of the braces) of the one
/// **production** `fn <name>`'s body in `code` — `code_only`-stripped source,
/// so brace-counting cannot be confused by a `{`/`}` sitting inside a comment
/// or string.
///
/// Deliberately strict, matching `argv_boundary/bounded_read.rs`'s own
/// `production_body`: exactly one definition must exist, and — when this
/// file has an inline `mod tests` at all — it must sit ahead of it, so a
/// same-named test helper is never picked up instead of the real thing. This
/// is a separate copy rather than a shared one because it works in `char`
/// indices (see the module doc's "why raw source" section) where the shared
/// helper works in bytes; the two would silently disagree on any file
/// carrying a non-ASCII byte in a blanked comment or string.
///
/// A raw substring match on `fn {name}` is not enough by itself: this crate's
/// own sink names are prefixes of other real names in the same file
/// (`sandboxed` of `sandboxed_with_grant`, `network_command` of
/// `network_command_with_credential`/`_without_credential`, and of two
/// `#[test]` fns named `sandboxed_forces_…`/`sandboxed_does_not_force_…`) —
/// `git_cmd.rs` alone raised "found 4" for `sandboxed` before this filter
/// existed. So a hit only counts when the character right after `name` is
/// not itself an identifier character — `(` or whitespace, never `_` or an
/// alphanumeric that would make it a different, longer name.
fn production_fn_range(code_chars: &[char], name: &str, source_path: &str) -> (usize, usize) {
    let marker: Vec<char> = format!("fn {name}").chars().collect();
    let hits: Vec<usize> = find_all(code_chars, &marker)
        .into_iter()
        .filter(|&at| match code_chars.get(at + marker.len()) {
            Some(&c) => !is_ident_char(c),
            None => true,
        })
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one `fn {name}` definition (name not a prefix of a \
         longer one) in {source_path}, found {}",
        hits.len()
    );
    let at = hits[0];
    let tests_marker: Vec<char> = "mod tests".chars().collect();
    if let Some(tests_at) = find_seq(code_chars, &tests_marker) {
        assert!(
            at < tests_at,
            "`fn {name}` in {source_path} was found at/after `mod tests`, not in production code"
        );
    }
    let open = (at..code_chars.len())
        .find(|&i| code_chars[i] == '{')
        .unwrap_or_else(|| panic!("{source_path}: `fn {name}`'s signature has no body brace"));
    let close = matching_close(code_chars, open, '{', '}')
        .unwrap_or_else(|| panic!("{source_path}: unbalanced braces extracting `fn {name}`"));
    assert!(
        close > open + 1,
        "{source_path}: `fn {name}` has an empty body"
    );
    (open + 1, close)
}

/// Every argv-redaction violation found in `region` (a `char`-index range
/// into both `code_chars` and `raw_chars`, which must be the same length and
/// position-aligned — see the module doc). One human-readable string per
/// violation, empty when the region is clean.
fn violations_in_region(
    code_chars: &[char],
    raw_chars: &[char],
    region: (usize, usize),
    label: &str,
) -> Vec<String> {
    let (region_start, region_end) = region;
    let redact_marker: Vec<char> = "redact_args(".chars().collect();
    let mut out = Vec::new();

    for macro_name in EMISSION_MACROS {
        let prefix: Vec<char> = macro_name.chars().collect();
        let mut search_from = region_start;
        while search_from < region_end {
            let Some(rel) = find_seq(&code_chars[search_from..region_end], &prefix) else {
                break;
            };
            let m_start = search_from + rel;
            let open = m_start + prefix.len() - 1; // index of the macro's own '('
            let Some(close) = matching_close(code_chars, open, '(', ')') else {
                panic!("{label}: unbalanced parens after `{macro_name}`");
            };

            // Blank out every whole `redact_args(...)` call in a COPY of the
            // raw argument text, so its own inner `args` token is never
            // mistaken for an unwrapped one — `redact_args(args)` is exactly
            // the safe pattern, not a violation of it.
            let code_span = &code_chars[open + 1..close];
            let mut scrubbed: Vec<char> = raw_chars[open + 1..close].to_vec();
            let mut i = 0usize;
            while i < code_span.len() {
                let Some(rel2) = find_seq(&code_span[i..], &redact_marker) else {
                    break;
                };
                let start = i + rel2;
                let paren = start + redact_marker.len() - 1;
                let Some(end) = matching_close(code_span, paren, '(', ')') else {
                    panic!("{label}: unbalanced parens inside a `redact_args(` call");
                };
                for slot in scrubbed.iter_mut().take(end + 1).skip(start) {
                    *slot = ' ';
                }
                i = end + 1;
            }

            if contains_bare_ident(&scrubbed, "args") {
                let snippet: String = raw_chars[open + 1..close].iter().collect();
                out.push(format!(
                    "{label}: `{macro_name}` embeds `args` without routing it \
                     through `redact_args` first — argument text: {snippet:?}"
                ));
            }
            search_from = close + 1;
        }
    }
    out
}

/// The tripwire proper: none of [`SINK_FUNCTIONS`]' production bodies may log
/// or panic with `args` unredacted.
#[test]
fn network_argv_sink_functions_never_log_args_unredacted() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();

    for (rel, name) in SINK_FUNCTIONS {
        let path = root.join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{rel}: readable source file ({e})"));
        let code = code_only(&src);
        let code_chars: Vec<char> = code.chars().collect();
        let raw_chars: Vec<char> = src.chars().collect();
        assert_eq!(
            code_chars.len(),
            raw_chars.len(),
            "{rel}: code_only changed the char count — the char-index mapping \
             this scan relies on no longer holds"
        );
        let region = production_fn_range(&code_chars, name, rel);
        violations.extend(violations_in_region(
            &code_chars,
            &raw_chars,
            region,
            &format!("{rel}::{name}"),
        ));
    }

    assert!(
        violations.is_empty(),
        "found {} argv-redaction violation(s), each a Network-tier `args` \
         reaching a log/panic message without `redact_args`:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// The detector itself, proved non-vacuous: it must flag the exact shape
/// `reconcile_need` shipped with, clear the exact shape it was fixed to, and
/// still flag a body where only *one* of two call sites was fixed — the
/// shape a partial, "weakened" fix would take. Without this, the assertion
/// above is only ever evidence that six specific files happen to pass it
/// today, the same failure mode `argv_boundary.rs`'s own `why_dead` guards
/// against for its allowlist.
#[test]
fn the_detector_flags_unredacted_args_and_clears_redacted_args() {
    let cases: &[(&str, bool, &str)] = &[
        (
            "fn f() {\n    eprintln!(\"leak: {args:?}\");\n}\n",
            true,
            "raw interpolation, unfixed",
        ),
        (
            "fn f() {\n    eprintln!(\"ok: {:?}\", redact_args(args));\n}\n",
            false,
            "wrapped in redact_args, fixed",
        ),
        (
            "fn f() {\n    debug_assert!(false, \"raw {args:?}\");\n    \
             eprintln!(\"{:?}\", redact_args(args));\n}\n",
            true,
            "one site fixed, one still raw — a partial fix must still fail",
        ),
        (
            "fn f() {\n    debug_assert!(false, \"{:?}\", redact_args(args));\n    \
             eprintln!(\"{:?}\", redact_args(args));\n}\n",
            false,
            "both sites fixed",
        ),
    ];

    for (src, expect_violation, label) in cases {
        let code = code_only(src);
        let code_chars: Vec<char> = code.chars().collect();
        let raw_chars: Vec<char> = src.chars().collect();
        assert_eq!(
            code_chars.len(),
            raw_chars.len(),
            "{label}: length mismatch"
        );
        let region = (0, code_chars.len());
        let violations = violations_in_region(&code_chars, &raw_chars, region, label);
        assert_eq!(
            !violations.is_empty(),
            *expect_violation,
            "{label}: expected violation={expect_violation}, got {violations:?}"
        );
    }
}
