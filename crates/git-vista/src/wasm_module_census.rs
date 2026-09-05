//! #612 slice 4: an inventory over `crates/git-vista/src`'s `#[cfg(target_arch =
//! "wasm32")]`-gated source, answering the question the first three slices
//! answered by hand: **which wasm-only modules has nobody pinned with a host
//! test?**
//!
//! # Why this exists, and why it is not a lint
//!
//! Slices 1-3 moved four specific decisions (`operations::signals`'s send
//! table, `app::mod`'s `HistoryPhase`, `dialogs::confirm`'s preview wiring, and
//! their siblings) out of wasm-only code and into `features::*::core`, because
//! `cargo test --workspace` never compiles a wasm32-gated module and so can
//! never run a test against a decision left inside one. Finding those four
//! took a manual read of the whole tree (`design-docs/2026-09-02-lane3-612-wasm-census.md`).
//! This module is that manual census turned into a standing `#[test]`, so the
//! next large decision left in wasm-only code is named the moment it exists,
//! not the next time someone happens to reread everything.
//!
//! `#645`'s PR body considered and rejected a clippy lint for this: a
//! wasm-only module is *full* of functions that are pure by signature — every
//! `#[component]`, every markup helper — so a lint keyed on purity would be
//! almost entirely false positives. What this module checks instead is
//! structural and much cheaper to state honestly: does **some** host test's
//! `include_str!` read this file's bytes at all? That is a proxy for "someone
//! thought this file mattered enough to watch", not a claim that the file's
//! decisions are correct, or even that they are tested — [`crate::offline_guard_audit`]
//! and [`crate::features::a11y::audit`] are the modules that make *that* kind
//! of claim, each about one narrow slice of `include_str!`'d source. This
//! module only tracks whether a file is on *anyone's* `include_str!` list.
//!
//! # What this proves, and what it does not
//!
//! Proves: every `#[cfg(target_arch = "wasm32")]`-gated `.rs` file under
//! `crates/git-vista/src` at or above [`THRESHOLD_LINES`] either (a) has its
//! source text read by an `include_str!` somewhere a host test compiles, or
//! (b) is named in [`EXEMPT`] with an argued reason.
//!
//! Does NOT prove:
//!
//! 1. That the host test reading a pinned file's bytes checks anything true,
//!    useful, or even non-vacuous about them — a file could be `include_str!`'d
//!    into a test that only asserts it's non-empty. This module has no way to
//!    tell a meaningful census (`offline_guard_audit`'s guard-ordering check)
//!    from a decorative one. Coverage bookkeeping, not correctness — see the
//!    module doc above.
//! 2. Anything about a file below [`THRESHOLD_LINES`] — a threshold this
//!    module applies precisely so a two-line re-export doesn't need its own
//!    census entry. See [`THRESHOLD_LINES`]'s doc for where the number came
//!    from.
//! 3. That an `include_str!` call this module counts as "pinning" a file is
//!    itself reachable from a real test run — a `const` built from
//!    `include_str!` but never read by any `#[test]` would still count. No
//!    instance of this shape exists in the tree today (checked by hand while
//!    writing this module); a future one would silently under-count nothing,
//!    since the const still costs a real filesystem read the compiler tracks,
//!    but it would give false credit for a census nobody actually runs.
//! 4. Anything about a file that *is itself wasm-only* containing an
//!    `include_str!` of *another* wasm-only file — such a call would never
//!    execute under `cargo test --workspace` and would pin nothing, but this
//!    module's [`include_str_targets`] scan does not distinguish where an
//!    `include_str!` call lives before crediting its target. No such call
//!    exists in the tree today (every `include_str!` target found while
//!    writing this module lives in a file `cargo test --workspace` compiles),
//!    so the gap is real but currently unexercised.
//! 5. Runtime behavior of any kind, like every other census in this repo —
//!    this reads source bytes at test-compile-and-run time, never loads a
//!    browser.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ── Filesystem walk ──────────────────────────────────────────────────────────

/// `crates/git-vista/src`: `GIT_VISTA_WASM_CENSUS_ROOT` if set (for pointing
/// this test at a scratch copy — same escape hatch `reachability_census` and
/// `offline_guard_audit` use), otherwise computed from `CARGO_MANIFEST_DIR`
/// (`crates/git-vista`, whose `src/` this module censuses).
fn git_vista_src_root() -> PathBuf {
    if let Ok(over) = std::env::var("GIT_VISTA_WASM_CENSUS_ROOT") {
        return PathBuf::from(over);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("wasm_module_census: cannot read dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            panic!(
                "wasm_module_census: dir entry error under {}: {e}",
                dir.display()
            )
        });
        let path = entry.path();
        if path.is_dir() {
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

// ── Discovery: which files are wasm32-only ──────────────────────────────────

/// A line, after trimming, that is exactly this attribute gates the `mod`
/// declaration one non-blank, non-comment line below it. This is a text
/// match, not a parse — it is deliberately exact (no whitespace variation
/// tolerated inside the attribute) so it fails loudly rather than silently if
/// the repo's own formatting of this attribute ever changes; `cargo fmt`
/// keeps every occurrence in the tree in this exact shape today (checked:
/// `grep -c` for the literal string below equals the count `main.rs` and the
/// `features/*/mod.rs` files declare together).
const WASM32_CFG_LINE: &str = "#[cfg(target_arch = \"wasm32\")]";

/// Given a file's full text, the identifiers of every `mod NAME;` /
/// `pub mod NAME;` declaration immediately gated (module-doc rules above) by
/// [`WASM32_CFG_LINE`].
fn wasm32_gated_mod_names(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut names = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == WASM32_CFG_LINE {
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.is_empty() || t.starts_with("//") {
                    j += 1;
                    continue;
                }
                if let Some(name) = parse_mod_decl(t) {
                    names.push(name);
                }
                break;
            }
            i = j;
        }
        i += 1;
    }
    names
}

fn parse_mod_decl(line: &str) -> Option<String> {
    let line = line.strip_prefix("pub ").unwrap_or(line);
    let rest = line.strip_prefix("mod ")?;
    let name = rest.strip_suffix(';')?.trim();
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

/// Resolve a `mod NAME;` declaration found under `base_dir` to every `.rs`
/// file it names: `NAME.rs` if that file exists, every `.rs` file under
/// `NAME/` if that directory exists (both, if both exist — Rust 2018 allows
/// `menu.rs` to declare submodules living in a sibling `menu/` directory, and
/// several modules censused here use exactly that shape). Panics if neither
/// exists: a wasm32-gated `mod` declaration naming nothing on disk means this
/// module's own text scan has drifted from the compiler's view of the tree,
/// which is exactly the class of silent gap this census exists to prevent —
/// in itself.
fn resolve_module_files(base_dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let single = base_dir.join(format!("{name}.rs"));
    let dir = base_dir.join(name);
    let mut found = false;
    if single.is_file() {
        out.push(single);
        found = true;
    }
    if dir.is_dir() {
        walk_rs_files(&dir, out);
        found = true;
    }
    if !found {
        panic!(
            "wasm_module_census: `{WASM32_CFG_LINE}` gates `mod {name};` under {}, but \
             neither {name}.rs nor {name}/ exists there",
            base_dir.display()
        );
    }
}

/// Every `.rs` file under `src_root` that is `#[cfg(target_arch = "wasm32")]`-only,
/// with its line count. Two sources, both discovered by scanning source text
/// at test-run time rather than by a hand-maintained list — the same reason
/// [`crate::reachability_census`] walks the filesystem instead of naming every
/// file up front (module doc, "Modeled directly on..."): the set is exactly
/// the kind of thing a hand-list silently drifts from.
///
/// 1. Top-level modules named in `main.rs`, gated there directly. A gated name
///    resolving to a directory (`app`, `api`, `dialogs`, `menu`, `render`)
///    contributes every `.rs` file under that directory — everything nested
///    inside a wasm32-only module tree is itself wasm32-only, since it cannot
///    be reached from a non-wasm32 build at all.
/// 2. `features/*/signals.rs` and `features/*/view.rs` (and any sibling given
///    the same treatment later), gated inside their own feature's `mod.rs` —
///    `features/mod.rs` itself gates nothing (M1.11 D1: `core.rs` files are
///    framework-free by convention, not by `cfg`), so the gate lives one
///    level deeper and this walks every `features/*/mod.rs` to find it.
fn discover_wasm_only_files(src_root: &Path) -> BTreeMap<String, usize> {
    let mut files = Vec::new();

    let main_rs = src_root.join("main.rs");
    let main_text = std::fs::read_to_string(&main_rs)
        .unwrap_or_else(|e| panic!("wasm_module_census: cannot read {}: {e}", main_rs.display()));
    for name in wasm32_gated_mod_names(&main_text) {
        resolve_module_files(src_root, &name, &mut files);
    }

    let features_dir = src_root.join("features");
    let entries = std::fs::read_dir(&features_dir).unwrap_or_else(|e| {
        panic!(
            "wasm_module_census: cannot read dir {}: {e}",
            features_dir.display()
        )
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            panic!(
                "wasm_module_census: dir entry error under {}: {e}",
                features_dir.display()
            )
        });
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let mod_rs = path.join("mod.rs");
        let Ok(text) = std::fs::read_to_string(&mod_rs) else {
            continue;
        };
        for name in wasm32_gated_mod_names(&text) {
            resolve_module_files(&path, &name, &mut files);
        }
    }

    let mut out = BTreeMap::new();
    for file in files {
        let rel = to_rel_path(src_root, &file);
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("wasm_module_census: cannot read {}: {e}", file.display()));
        out.insert(rel, text.lines().count());
    }
    out
}

// ── Discovery: which files a host test reads with `include_str!` ───────────

/// Every path an `include_str!("...")` call anywhere under `src_root` names,
/// resolved to a path relative to `src_root` (forward-slash, matching
/// [`to_rel_path`]) and filtered to `.rs` targets that actually resolve
/// inside `src_root` — an `include_str!` reaching `styles.css`, `index.html`,
/// or a path outside this crate's `src/` (both real, both irrelevant to a
/// census of wasm-only *modules*) is silently skipped.
///
/// Scans every `.rs` file's raw text, line by line, skipping lines whose
/// trimmed text starts with `//` — the module docs quote the literal string
/// `include_str!(...)` several times in prose without a real call following
/// it, and this keeps that prose from ever being mistaken for a target.
fn include_str_targets(src_root: &Path) -> BTreeSet<String> {
    let mut files = Vec::new();
    walk_rs_files(src_root, &mut files);

    let mut targets = BTreeSet::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("wasm_module_census: cannot read {}: {e}", file.display()));
        let parent = file.parent().unwrap_or(src_root);
        for line in text.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            const NEEDLE: &str = "include_str!(\"";
            while let Some(pos) = rest.find(NEEDLE) {
                let after = &rest[pos + NEEDLE.len()..];
                let Some(end) = after.find('"') else { break };
                let rel_literal = &after[..end];
                rest = &after[end + 1..];

                let joined = parent.join(rel_literal);
                let Ok(canon) = joined.canonicalize() else {
                    continue;
                };
                let Ok(canon_root) = src_root.canonicalize() else {
                    continue;
                };
                if canon.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                if let Ok(rel_to_root) = canon.strip_prefix(&canon_root) {
                    targets.insert(rel_to_root.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    targets
}

// ── The threshold, and the exempt table ─────────────────────────────────────

/// A wasm-only file at or above this many lines is large enough to plausibly
/// hold a decision worth pinning, and gets checked; below it, this census
/// stays silent. Picked, not derived: the smallest file any *existing* census
/// in this tree already bothers to pin individually is
/// `features/operations/view.rs` at 218 lines (`features/a11y/audit.rs`'s
/// `OPERATIONS_VIEW`) — 150 sits under that with margin, so nothing already
/// judged worth pinning could ever drift out of this census's view by
/// shrinking slightly, while pure wiring files (`dialogs/mod.rs` at 56 lines,
/// `render/mod.rs` at 43) stay under it without needing an [`EXEMPT`] entry
/// at all. A module hovering near 150 is exactly the case this threshold
/// cannot get right by construction — that is the acknowledged cost of a
/// single number, not an oversight.
const THRESHOLD_LINES: usize = 150;

/// Argued exceptions, same posture as `reachability_census::EXEMPT` and
/// `offline_guard_audit::EXEMPT_UNGUARDED`: a wasm-only file at or above
/// [`THRESHOLD_LINES`] with no host test reading it lands here only with a
/// reason, and [`exempt_entries_still_need_exempting`] below keeps every
/// reason honest — an entry whose file has since been pinned, shrunk under
/// threshold, or deleted is stale and must be removed, not left for someone
/// else to notice.
///
/// These are not all the same kind of gap, and the reasons say so plainly.
/// Several are real, unpinned debt named here for the first time by this
/// census (`gestures.rs`, `features/diff/staging_view.rs`,
/// `dialogs/open_url.rs`) — landing this module does not close those gaps,
/// it is what makes them visible instead of requiring another by-hand read
/// of the tree to rediscover; #653 tracks the ones that remain.
///
/// `dialogs/open_url.rs` is on that list because *this table* says it is.
/// #649's PR body filed it under "argued thin" while the entry it landed
/// called it "unpinned, smaller-scale debt"; the two disagreed for as long
/// as both existed. The entry wins — a reason sitting next to the exemption
/// it justifies is the thing a later reader checks, and the thing
/// [`exempt_entries_still_need_exempting`] is written against. A prose
/// summary elsewhere that drifts from it is the summary that is wrong.
///
/// Four have already left. `app/canvas.rs` went when its 409 handler moved to
/// `features::history::core::drift_reload`; `features/shell/signals.rs` went
/// when its overlay payload map moved to `features::shell::core`; `print.rs`
/// and `render/labels.rs` went together when the GitHub link rule, the ref
/// glyph mapping and the badge palette they each held a copy of moved to
/// `features::graph::core` (#653). None of those entries was removed as a
/// courtesy someone remembered — [`exempt_entries_still_need_exempting`]
/// demanded it the moment a host test started reading the file, which is
/// this table working as intended.
///
/// Others are argued thin on inspection
/// (`state.rs`, `session.rs`, `prefs.rs`, `features/stash/signals.rs`,
/// `update_required.rs`) — the decision they would otherwise hide already
/// lives in a host-tested `core` module, and what remains in the wasm-only
/// file is sequencing or type definitions, not a decision this census's
/// coverage proxy would usefully pin.
const EXEMPT: &[(&str, &str)] = &[
    (
        "gestures.rs",
        "511 lines, 11 free functions classifying pointer count into pan / \
         pinch / wheel gestures — a real decision (which gesture wins) with \
         no host test reading it. Real debt, not fixed here.",
    ),
    (
        "features/diff/staging_view.rs",
        "533 lines: the finger/keyboard hunk-selection view wired to \
         /api/staging. No host test reads it. Real debt, not fixed here.",
    ),
    (
        "dialogs/open_url.rs",
        "218 lines, 1 fn / 1 `view!` — the clone-request modal. The decisions \
         it calls (`clone_dialog_may_dismiss`, `clone_settlement`) are already \
         host-tested in `features/dialogs/core.rs`; the composition in this \
         file is the same shape `dialogs/confirm.rs` was before slice 3 — \
         unpinned, smaller-scale debt, not fixed here.",
    ),
    (
        "state.rs",
        "248 lines, zero free functions — `Settings`/`Features` are \
         Copy signal-bundle *type* definitions, not decisions. \
         `reachability_census`'s own module doc excludes struct/type items \
         from its census for the identical reason (fn-shaped call sites are \
         what it can check); this census counts by file, not by item kind, \
         so the file still needs an entry, but there is no function-shaped \
         content here for a host test to usefully pin.",
    ),
    (
        "session.rs",
        "209 lines, 3 fns. The one real decision here — parsing the \
         `#s=<token>` bootstrap fragment — was already extracted to the \
         host-tested `bootstrap_fragment` module (main.rs's own comment: \
         'lifted out ... so it can be host-tested at all'). What remains is \
         the fetch/redirect sequencing around that decision.",
    ),
    (
        "prefs.rs",
        "199 lines, 14 fns, all `localStorage` get/set pairs for two boolean \
         toggles. Mechanical persistence, not branching decisions — the \
         lowest-risk shape in this table.",
    ),
    (
        "features/stash/signals.rs",
        "177 lines. Its own module doc states the rule directly: 'Every \
         decision in here belongs to core and is host-tested there' and \
         '[compose_pop] does not decide anything the gate decides' — already \
         argued thin, in writing, before this census existed.",
    ),
    (
        "update_required.rs",
        "160 lines, 1 fn / 1 `view!`. The compatibility decision it renders \
         (`git_vista_protocol::Compatibility`) is host-tested in the protocol \
         crate; this file only displays the verdict.",
    ),
    (
        "activity.rs",
        "577 lines, 3 fns / 25 `view!` blocks — predominantly markup routing \
         Activity-panel feed rows into the shared context menu \
         (`menu::menu_view`). A markup-heavy file's actual decision content \
         is not something this census can verify, only its shape; recorded \
         here rather than silently passed because it is well above threshold.",
    ),
];

// ── The tests ────────────────────────────────────────────────────────────────

/// The main census. A wasm-only file at or above [`THRESHOLD_LINES`], not
/// read by any host test's `include_str!`, and not argued in [`EXEMPT`] is a
/// module that could grow — or already holds — a decision nothing in
/// `cargo test --workspace` can ever see. Name it.
#[test]
fn every_large_wasm_only_module_has_a_host_test_reading_it_or_is_exempt() {
    let root = git_vista_src_root();
    let wasm_only = discover_wasm_only_files(&root);
    let pinned = include_str_targets(&root);
    let exempt: BTreeSet<&str> = EXEMPT.iter().map(|&(p, _)| p).collect();

    let mut unpinned: Vec<(String, usize)> = wasm_only
        .into_iter()
        .filter(|(_, lines)| *lines >= THRESHOLD_LINES)
        .filter(|(rel, _)| !pinned.contains(rel))
        .filter(|(rel, _)| !exempt.contains(rel.as_str()))
        .collect();
    unpinned.sort();

    assert!(
        unpinned.is_empty(),
        "WASM MODULE CENSUS FAILED: {} wasm32-only file(s) are >= {THRESHOLD_LINES} lines, \
         read by no host test's include_str!, and not listed in \
         wasm_module_census::EXEMPT:\n{}\n\nFor each: either add a host test that \
         `include_str!`s the file (a new census, or an existing one like \
         `features::a11y::audit`), or — if it is genuinely thin, wiring, or \
         type-definitions-only on inspection — add it to EXEMPT with a comment \
         arguing why, the same way every other entry there is argued.",
        unpinned.len(),
        unpinned
            .iter()
            .map(|(rel, lines)| format!("  {rel} — {lines} lines"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// [`EXEMPT`]'s bidirectional health check, `reachability_census`-style: an
/// entry whose file no longer exists (or is no longer discovered as
/// wasm32-only) is stale and must be removed; an entry whose file has since
/// been pinned by a host test, or has shrunk under [`THRESHOLD_LINES`], is
/// also stale — the argued exception no longer applies, and leaving it in
/// place would hide a gap actually closing behind a note that is now wrong.
#[test]
fn exempt_entries_still_need_exempting() {
    let root = git_vista_src_root();
    let wasm_only = discover_wasm_only_files(&root);
    let pinned = include_str_targets(&root);

    for &(rel, _reason) in EXEMPT {
        let Some(&lines) = wasm_only.get(rel) else {
            panic!(
                "STALE EXEMPT ENTRY: wasm_module_census::EXEMPT lists \"{rel}\" but it is no \
                 longer discovered as a wasm32-only file (moved, deleted, or no longer \
                 cfg-gated) — remove the entry"
            );
        };
        assert!(
            lines >= THRESHOLD_LINES,
            "STALE EXEMPT ENTRY: wasm_module_census::EXEMPT lists \"{rel}\" but it is now only \
             {lines} lines (< {THRESHOLD_LINES}) — below threshold, remove the entry"
        );
        assert!(
            !pinned.contains(rel),
            "STALE EXEMPT ENTRY: wasm_module_census::EXEMPT lists \"{rel}\" but it is now read \
             by a host test's include_str! — the gap it named has been closed, remove the entry"
        );
    }
}

/// A small, fixed set of files that must always classify as wasm32-only —
/// one from each discovery path ([`discover_wasm_only_files`]'s two sources)
/// and one from each directory-resolution shape (`NAME.rs` alone, `NAME/`
/// alone via its `mod.rs`, and both `NAME.rs` + `NAME/` together). If
/// discovery ever silently stops finding one of these — a typo in
/// [`WASM32_CFG_LINE`], a broken walk, a features/ directory no longer
/// scanned — [`every_large_wasm_only_module_has_a_host_test_reading_it_or_is_exempt`]
/// would not notice: it can only report on what discovery hands it, so a
/// discovery collapse reads as "nothing to check" and passes vacuously. This
/// test exists because that failure mode is real (see #612's own history:
/// `api/conflicts.rs` went unseen by `offline_guard_audit::API_SRC` for its
/// entire existence until #77, for the identical reason) and this census's
/// main assertion cannot catch it in itself.
#[test]
fn discovery_finds_every_known_landmark_and_a_plausible_total() {
    let root = git_vista_src_root();
    let wasm_only = discover_wasm_only_files(&root);

    const LANDMARKS: &[&str] = &[
        "main.rs's directory-resolved modules:",
        "app/mod.rs",       // NAME/ only (app.rs does not exist)
        "app/canvas.rs",    // a second file under that same NAME/
        "api.rs",           // NAME.rs, alongside its own NAME/ directory
        "api/conflicts.rs", // a file under that NAME/ directory
        "menu.rs",          // NAME.rs, alongside its own NAME/ directory
        "menu/branch_items.rs",
        "viewer.rs", // NAME.rs alone, no NAME/ directory
        "features/ discovery:",
        "features/shell/signals.rs",
        "features/operations/signals.rs",
        "features/operations/view.rs",
        "features/diff/staging_view.rs",
    ];
    let missing: Vec<&str> = LANDMARKS
        .iter()
        .filter(|p| !p.ends_with(':'))
        .filter(|p| !wasm_only.contains_key(**p))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "wasm_module_census: discovery no longer finds known wasm32-only landmark(s): {missing:?} \
         — something in discover_wasm_only_files (or the cfg-gate text it matches) is broken; \
         the main census would silently under-report while this is true"
    );

    assert!(
        wasm_only.len() >= 40,
        "wasm_module_census: only found {} wasm32-only files under {} — expected several dozen \
         (main.rs alone gates activity/api/app/detail/dialogs/gestures/hook_policy_banner/menu/\
         offline_banner/picker/prefs/print/render/session/state/update_required/viewer, several \
         of which are whole directories); discovery is very likely broken",
        wasm_only.len(),
        root.display(),
    );
}
