//! The running git's version — established once, in one place (#581).
//!
//! # Why this module exists
//!
//! `docs/SUPPORTED_VERSIONS.md` documents a **product** floor of git 2.32,
//! derived from `GIT_CONFIG_GLOBAL`. Two features need more than that, and both
//! need the *same* fact to decide it: what version is the git this process
//! actually execs?
//!
//! Before #581 that fact was established in exactly one place —
//! [`crate::preview`], for the graph preview (#576, ADR 0099) — and
//! [`crate::activity::revert_would_conflict`] did not consult it at all. It ran
//! `git merge-tree --write-tree` (git 2.38) against a documented 2.32 floor and
//! let the failure fall out as an unexplained `Err`. That degrades fail-closed,
//! which is why nobody noticed: the revert offer simply never appeared, and the
//! user was told nothing.
//!
//! **Measured, 2026-09-02**, running the exact argv `revert_would_conflict`
//! builds against two real gits in containers:
//!
//! | git | exit | output |
//! |---|---|---|
//! | 2.34.1 (Ubuntu 22.04) | **129** | `usage: git merge-tree <base-tree> <branch1> <branch2>` |
//! | 2.43.0 (Ubuntu 24.04) | 0 | the merged tree oid |
//!
//! 129 is neither 0 nor 1, so the match arm that means "the check itself did
//! not produce an answer" absorbed a fact we could have stated precisely.
//!
//! # What this module does and does not decide
//!
//! It answers *what version is this*, and offers a pure comparison. It holds no
//! feature's floor: each caller owns its own constant
//! ([`crate::preview::MIN_GIT_FOR_PREVIEW`],
//! [`crate::activity::MIN_GIT_FOR_MERGE_TREE`]) so that reading a call site
//! tells you what that feature needs without chasing a table. A shared *number*
//! would be a second product floor by the back door; a shared *measurement* is
//! what was actually missing.

use std::path::Path;

/// The git version, probed once per process.
///
/// Per process, not per call and not at boot. Not per call because the git
/// binary a process execs is a property of that process's `PATH`, not of the
/// repository or the request — `crate::sandbox` spawns a bare `"git"`, resolved
/// from `PATH`. Not at boot because `sandbox::probe`'s gate has exactly one
/// non-fatal outcome by design ("There is no degrade: a verdict other than
/// `Contained` means no server, full stop" — ADR 0029), and putting a
/// *capability* question into a fatal gate is how a degrade gets bolted onto a
/// gate whose whole argument is that it has none.
///
/// The honest limit, stated rather than hidden: an operator who upgrades git
/// under a running server does not get the gated features until restart. That
/// is the same posture `sandbox::capabilities::current()` already takes toward
/// host capability.
///
/// Only a *success* is cached ([`tokio::sync::OnceCell::get_or_try_init`]), so
/// a transient failure to run git does not permanently disable a feature.
static GIT_VERSION: tokio::sync::OnceCell<(u32, u32, u32)> = tokio::sync::OnceCell::const_new();

/// Probe the host's git version, cached per process (see [`GIT_VERSION`]).
///
/// Goes through `crate::git_cmd::git_output` — the sealed sandbox launcher (#66
/// Task 6) — so this adds no process-spawn site for `crate::argv_boundary`'s
/// scan to review. `sandbox::network_need` classifies an argv with no
/// subcommand token at all as `NetworkNeed::Local`, so it needs no special
/// declaration.
///
/// The error is a plain message because the two callers wrap it in their own
/// vocabulary: `PreviewUnavailable::CheckFailed` for the preview,
/// [`crate::activity::RevertCheckError::CheckFailed`] for the revert offer.
/// Neither may read it as "old enough" or "new enough" — a version we could not
/// read is no fact at all.
pub(crate) async fn current(repo: &Path) -> Result<(u32, u32, u32), String> {
    GIT_VERSION
        .get_or_try_init(|| async {
            let out = crate::git_cmd::git_output(repo, &["--version"])
                .await
                .map_err(|e| format!("could not run git --version: {e}"))?;
            if !out.status.success() {
                let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(if said.is_empty() {
                    String::from("git --version did not produce an answer")
                } else {
                    format!("git --version did not produce an answer: {said}")
                });
            }
            let line = String::from_utf8_lossy(&out.stdout);
            parse(&line).ok_or_else(|| {
                format!(
                    "could not read a version out of git's own output: {:?}",
                    line.trim()
                )
            })
        })
        .await
        .copied()
}

/// Parse the `major.minor.patch` at the front of git's own `--version` line.
///
/// git prints `git version 2.43.0`, and vendor builds append suffixes
/// (`2.39.5 (Apple Git-154)`, `2.43.0.windows.1`), so this takes the first
/// three dot-separated integer runs after the `git version ` prefix and stops
/// at the first component that is not all digits. `None` means the line did not
/// look like git's — which is a check failure, never a silent "old enough" or
/// "new enough".
///
/// Moved here from `crate::preview` by #581, unchanged, so both callers parse
/// identically rather than growing a second parser that disagrees at the edges.
pub(crate) fn parse(line: &str) -> Option<(u32, u32, u32)> {
    let rest = line.trim().strip_prefix("git version ")?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    // A two-component version (`git version 2.38`) is a real, readable answer;
    // the patch level is the only part allowed to be missing or non-numeric.
    let patch: u32 = parts
        .next()
        .and_then(|p| {
            let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether `found` is at or above `floor`.
///
/// The comparison is on `(major, minor)` alone: the plumbing both callers gate
/// on arrived in a `.0` release, so every patch level of the floor minor is new
/// enough and no patch level below it is.
///
/// Pure and separate from [`current`] so the *decision* can be tested with
/// literal versions on both sides of a floor, rather than only on whatever git
/// the host running the tests happens to have.
pub(crate) fn meets(found: (u32, u32, u32), floor: (u32, u32)) -> bool {
    let (major, minor, _patch) = found;
    (major, minor) >= floor
}

/// `found` rendered `major.minor.patch` for a caller-facing message — never
/// git's raw line, whose vendor suffixes are not the caller's business.
pub(crate) fn render(found: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", found.0, found.1, found.2)
}

/// A floor rendered `major.minor`, matching how the floors are written down.
pub(crate) fn render_floor(floor: (u32, u32)) -> String {
    format!("{}.{}", floor.0, floor.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`parse`] against real and vendor-shaped lines, one literal expectation
    /// per case.
    ///
    /// Moved here from `crate::preview`'s suite by #581, unchanged, because the
    /// parser moved: a test for a shared parser that lives in one caller's suite
    /// is a test the other caller's reader never finds.
    #[test]
    fn parse_reads_real_and_vendor_shaped_lines() {
        /// One `--version` line and the triple it must parse to. A named alias
        /// because clippy refuses the inline tuple type, and because naming it
        /// makes the table below read as data.
        type VersionCase = (&'static str, Option<(u32, u32, u32)>);

        let cases: &[VersionCase] = &[
            ("git version 2.43.0", Some((2, 43, 0))),
            ("git version 2.43.0\n", Some((2, 43, 0))),
            // #581: the exact bytes Ubuntu 22.04's git prints, captured from
            // `git --version | od -c` in a container on 2026-09-02 —
            // `git version 2.34.1\n`. This is the version whose `merge-tree
            // --write-tree` exits 129, so it is the one input the gate must
            // read correctly or the whole issue's fix reads the wrong number.
            ("git version 2.34.1\n", Some((2, 34, 1))),
            ("git version 2.39.5 (Apple Git-154)", Some((2, 39, 5))),
            ("git version 2.43.0.windows.1", Some((2, 43, 0))),
            ("git version 2.38", Some((2, 38, 0))),
            ("git version 2.37.3", Some((2, 37, 3))),
            // Not git's line: no fact, never a guess in either direction.
            ("gix version 0.66.0", None),
            ("2.43.0", None),
            ("git version banana", None),
            ("", None),
        ];
        for (line, expected) in cases {
            assert_eq!(parse(line), *expected, "for input {line:?}");
        }
    }

    /// The comparison, on both sides of a floor and exactly at it.
    ///
    /// Written with literals rather than the host's git precisely so it pins
    /// the decision on machines that will never have an old git installed.
    #[test]
    fn meets_is_inclusive_at_the_floor_and_ignores_the_patch_level() {
        const FLOOR: (u32, u32) = (2, 38);
        // Below, including the whole band #581 is about.
        assert!(!meets((2, 32, 0), FLOOR));
        assert!(!meets((2, 34, 1), FLOOR)); // Ubuntu 22.04, measured
        assert!(!meets((2, 37, 9), FLOOR));
        // Exactly at the floor, at every patch level.
        assert!(meets((2, 38, 0), FLOOR));
        assert!(meets((2, 38, 7), FLOOR));
        // Above.
        assert!(meets((2, 43, 0), FLOOR)); // Ubuntu 24.04, measured
        assert!(meets((3, 0, 0), FLOOR));
    }

    #[test]
    fn rendering_drops_the_vendor_suffix_and_keeps_the_shape() {
        assert_eq!(render((2, 34, 1)), "2.34.1");
        assert_eq!(render_floor((2, 38)), "2.38");
    }
}
