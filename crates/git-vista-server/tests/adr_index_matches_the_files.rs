//! The ADR index and the ADR files must name the same set of numbers (#578).
//!
//! # Why this exists
//!
//! `docs/adr/README.md` is not decoration. It is what a session reads to find
//! out which numbers are taken, and on 2026-08-30 three open pull requests
//! (#560, #563, #567) each added a *different* `docs/adr/0092-*.md`. The
//! repository already held the answer in two places —
//! `0086-a-number-left-deliberately-unused.md` names that collision in its own
//! text, and `0095-the-viewer-says-when-it-is-ready.md` had already reserved
//! "0092 for the worktree census, 0093 for the lesson tool, 0094 for watcher
//! authority" — and three PRs collided anyway.
//!
//! An index that can silently fall behind the files is part of how that
//! happens. When #578 was filed, `0098-a-folded-edge-lands-on-its-marker.md`
//! had existed on `main` since #575 merged and had no row. A reader trusting
//! the index saw a different set of taken numbers than `ls docs/adr/` shows,
//! and the disagreement was invisible.
//!
//! # Both directions, and why they are different claims
//!
//! - **A file with no row** is the #578 bug: the number is taken and the index
//!   does not say so, so the next reader may claim it.
//! - **A row with no file** is worse in a quieter way: a number reads as taken
//!   when nothing was ever written under it, and the link is broken. That is
//!   precisely the habit `0086` diagnoses — "claiming a number when the intent
//!   exists rather than when the file does".
//!
//! They are separate tests so a failure says which side is missing rather than
//! only that the two disagree.
//!
//! # 0086 is not an exception
//!
//! `0086` is a tombstone — a real record saying its number is retired and no
//! decision is filed under it. It has a file and it has a row, so it satisfies
//! these tests the same way every other record does. Nothing here is carved
//! out to make it pass; if it ever needed a carve-out, that would mean the
//! tombstone had stopped being a record, which is the thing it exists to avoid.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repo root resolves from CARGO_MANIFEST_DIR")
}

fn adr_dir() -> PathBuf {
    repo_root().join("docs/adr")
}

/// `NNNN` -> filename, for every `docs/adr/NNNN-*.md` on disk.
///
/// `README.md` is the index itself and carries no number, so it drops out of
/// the four-leading-digits filter rather than needing to be named here.
fn files_by_number() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let dir = adr_dir();
    for entry in std::fs::read_dir(&dir).expect("docs/adr must be readable") {
        let name = entry.expect("a readable dir entry").file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.ends_with(".md") {
            continue;
        }
        let Some(number) = leading_number(&name) else {
            continue;
        };
        if let Some(previous) = out.insert(number.clone(), name.clone()) {
            panic!(
                "two ADR files claim number {number}: {previous} and {name}. \
                 One number, one record — pick a free number for the second."
            );
        }
    }
    assert!(
        !out.is_empty(),
        "no ADR files found under {} — this test would pass vacuously",
        dir.display()
    );
    out
}

/// `NNNN` -> the filename the row links to, for every index row.
///
/// Matches table rows only (`| [NNNN](target) |`). `README.md`'s prose
/// mentions numbers too, and counting those would let a sentence about a
/// number stand in for a record of one.
fn index_rows_by_number() -> BTreeMap<String, String> {
    let path = adr_dir().join("README.md");
    let text = std::fs::read_to_string(&path).expect("docs/adr/README.md must be readable");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("| [") else {
            continue;
        };
        let Some((number, rest)) = rest.split_once("](") else {
            continue;
        };
        let Some((target, _)) = rest.split_once(')') else {
            continue;
        };
        if number.len() != 4 || !number.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Some(previous) = out.insert(number.to_string(), target.to_string()) {
            panic!(
                "docs/adr/README.md has two rows for {number}: -> {previous} and -> {target}. \
                 A duplicated row makes one of the two records unreachable from the index."
            );
        }
    }
    assert!(
        !out.is_empty(),
        "no ADR rows parsed out of {} — the table format changed and this test \
         would pass vacuously, which is worse than the drift it guards",
        path.display()
    );
    out
}

fn leading_number(file_name: &str) -> Option<String> {
    let head: String = file_name.chars().take(4).collect();
    (head.len() == 4 && head.chars().all(|c| c.is_ascii_digit())).then_some(head)
}

#[test]
fn every_adr_file_has_a_row_in_the_index() {
    let files = files_by_number();
    let rows = index_rows_by_number();
    let missing: Vec<_> = files
        .iter()
        .filter(|(number, _)| !rows.contains_key(*number))
        .map(|(number, name)| format!("{number} ({name})"))
        .collect();
    assert!(
        missing.is_empty(),
        "these ADR files have no row in docs/adr/README.md: {missing:?}. \
         The index is what the next session reads to find out which numbers are \
         taken; a file it does not list is a number that reads as free (#578)."
    );
}

#[test]
fn every_index_row_has_an_adr_file() {
    let files = files_by_number();
    let rows = index_rows_by_number();
    let missing: Vec<_> = rows
        .iter()
        .filter(|(number, _)| !files.contains_key(*number))
        .map(|(number, target)| format!("{number} -> {target}"))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/adr/README.md lists rows with no matching file: {missing:?}. \
         The link is broken, and worse, the number reads as taken when nothing \
         was written under it — the habit ADR 0086 diagnoses."
    );
}

#[test]
fn every_index_row_links_to_the_file_its_number_names() {
    let files = files_by_number();
    let rows = index_rows_by_number();
    let wrong: Vec<_> = rows
        .iter()
        .filter_map(|(number, target)| {
            let actual = files.get(number)?;
            (actual != target).then(|| format!("{number}: row links {target}, file is {actual}"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "index rows whose link does not name the file for that number: {wrong:?}. \
         A row can carry the right number and still send the reader nowhere."
    );
}
