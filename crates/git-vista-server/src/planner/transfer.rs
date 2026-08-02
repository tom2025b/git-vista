//! git's `--progress` records, parsed once for every operation that moves
//! objects over a transport.
//!
//! This started life inside `planner::fetch` (M2.20c, #229) and moved here when
//! M2.20e (#231) wired push execution, for the reason ADR 0044 gives for there
//! being exactly one `git fetch` in this server: a second parser would be a
//! second place for the phase vocabulary to drift from git's actual output, and
//! the two would diverge silently — nothing fails when a progress bar is
//! subtly wrong, which is exactly why it needs one owner.
//!
//! A fetch and a push print the **same phase names** for the work each side
//! does; only which side does which work differs. Fetching, the remote
//! enumerates/counts/compresses (git prefixes those `remote:`) and this host
//! receives and resolves. Pushing, this host enumerates/counts/compresses and
//! writes, and the remote resolves (prefixed `remote:` in turn). So one parser
//! covers both, and [`TransferPhase`] carries no direction of its own.
//!
//! The other thing both directions need, and for the same reason, is the
//! **observation of `refs/remotes/<remote>/*` before and after** — the
//! difference between the two listings *is* the answer to "did anything move?",
//! for a fetch that brought refs in and equally for a push that advanced the
//! remote-tracking ref git updates on success. Git's prose is gettext-translated
//! and version-dependent; two listings and a diff are neither.

use std::collections::BTreeMap;

use git_vista_protocol::{RemoteRefUpdate, TransferPhase, TransferProgress};

use super::*;

/// Parse one of git's `--progress` records into a [`TransferProgress`].
///
/// The records this recognises, verified byte-for-byte against git 2.43.0's
/// own output (see the tests below, which are built from a captured real fetch
/// and a captured real push):
///
/// ```text
/// remote: Enumerating objects: 121, done.          (fetch: the remote's side)
/// remote: Counting objects:  37% (45/121)
/// remote: Compressing objects: 100% (120/120), done.
/// Receiving objects:  66% (80/120), 174.40 KiB | 14.53 MiB/s
/// Resolving deltas: 100% (39/39), completed with 1 local object.
///
/// Enumerating objects: 15, done.                   (push: this host's side)
/// Counting objects: 100% (15/15), done.
/// Compressing objects:  92% (13/14)
/// Writing objects: 100% (15/15), 1004.39 KiB | 8.03 MiB/s, done.
/// remote: Resolving deltas: 100% (3/3), done.
/// ```
///
/// `None` for anything else — including git's `From <url>`/`To <url>` headers,
/// its `a1b2c3..d4e5f6  main -> origin/main` summary lines, its
/// `Delta compression using up to 8 threads` and `Total 15 (delta 3), …` notes,
/// and every warning or error. That is deliberate: this function's job is
/// progress, and a record it does not understand must not be turned into a
/// fabricated phase. The error paths have their own readers
/// (`fetch::classify_failure`, `push::classify_failure`) and the ref outcome
/// has its own observation.
///
/// # Locale
///
/// These phase names are gettext-translated: under `LC_ALL=de_DE` git prints
/// `Objekte empfangen`, and the `remote:`-prefixed ones come from the *remote's*
/// locale, not this host's. Unrecognised records simply produce no progress, so
/// a non-English pair degrades to "no progress bar", never to a wrong one.
/// `SandboxedCommand` exposes no `env` setter by construction (#228's C10
/// hazard #1), so this cannot be closed by forcing `LC_ALL=C` here; ADR 0043
/// records that as an accepted, reported gap and ADR 0045 inherits it.
pub(super) fn parse_progress(record: &str) -> Option<TransferProgress> {
    let record = record.strip_prefix("remote:").unwrap_or(record).trim();
    let (phase, rest) = [
        ("Enumerating objects:", TransferPhase::Enumerating),
        ("Counting objects:", TransferPhase::Counting),
        ("Compressing objects:", TransferPhase::Compressing),
        ("Receiving objects:", TransferPhase::Receiving),
        // M2.20e (#231): the push side's transfer phase — `Receiving`'s mirror
        // image, and the only record shape a push prints that a fetch does not.
        ("Writing objects:", TransferPhase::Writing),
        ("Resolving deltas:", TransferPhase::Resolving),
    ]
    .into_iter()
    .find_map(|(needle, phase)| record.strip_prefix(needle).map(|rest| (phase, rest.trim())))?;

    // `Enumerating` reports a bare running count and no percentage; every
    // other phase reports `N% (a/b)`.
    let percent = rest
        .split('%')
        .next()
        .filter(|_| rest.contains('%'))
        .and_then(|p| p.trim().parse::<u8>().ok())
        .filter(|p| *p <= 100);

    let (objects, total_objects) = match rest.split_once('(') {
        Some((_, after)) => {
            let inside = after.split(')').next().unwrap_or("");
            match inside.split_once('/') {
                Some((done, total)) => (
                    done.trim().parse::<u64>().ok(),
                    total.trim().parse::<u64>().ok(),
                ),
                None => (None, None),
            }
        }
        // `Enumerating objects: 121, done.` — the count is the first token.
        None => (
            rest.split(&[',', ' '][..])
                .next()
                .and_then(|n| n.trim().parse::<u64>().ok()),
            None,
        ),
    };

    Some(TransferProgress {
        phase,
        percent,
        objects,
        total_objects,
    })
}

// ---------------------------------------------------------------------------
// Observing what a transfer did to this repository
// ---------------------------------------------------------------------------

/// Every `refs/remotes/<remote>/*` ref and the object it points at.
///
/// `Err` is "we could not observe", which is a refusal reason and never
/// silently an empty map — a transfer whose before-state is unknown cannot
/// honestly answer "did anything move?" afterwards, and that answer is the
/// whole contract of a cancelled fetch and of a refused push (D5's posture: we
/// did not observe anything, so we may not act as though we did).
pub(super) async fn remote_tracking_refs(
    repo: &Path,
    need: NetworkNeed,
    remote: &RemoteName,
) -> Result<BTreeMap<String, String>, String> {
    let prefix = format!("refs/remotes/{}/", remote.as_str());
    let output = run_git(
        repo,
        need,
        &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
    )
    .await
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(stderr_or(&output, "git for-each-ref failed."));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.trim().split_once(' ')?;
            Some((name.to_string(), oid.to_string()))
        })
        .collect())
}

/// The before/after difference, as the wire type. Sorted by ref name (the
/// `BTreeMap` gives that for free), so two identical transfers report
/// identically.
pub(super) fn diff_refs(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<RemoteRefUpdate> {
    let mut out = Vec::new();
    for (name, new_oid) in after {
        match before.get(name) {
            Some(old) if old == new_oid => {}
            old => out.push(RemoteRefUpdate {
                ref_name: name.clone(),
                old_oid: old.cloned(),
                new_oid: Some(new_oid.clone()),
            }),
        }
    }
    for (name, old_oid) in before {
        if !after.contains_key(name) {
            out.push(RemoteRefUpdate {
                ref_name: name.clone(),
                old_oid: Some(old_oid.clone()),
                new_oid: None,
            });
        }
    }
    out.sort_by(|a, b| a.ref_name.cmp(&b.ref_name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records captured verbatim from a real `git fetch --progress` (git
    /// 2.43.0) against a local remote, `\r`-split the way
    /// `git_cmd::emit_records` splits them.
    #[test]
    fn every_real_fetch_progress_record_shape_parses() {
        let cases: &[(&str, TransferProgress)] = &[
            (
                "remote: Enumerating objects: 121, done.",
                TransferProgress {
                    phase: TransferPhase::Enumerating,
                    percent: None,
                    objects: Some(121),
                    total_objects: None,
                },
            ),
            (
                "remote: Counting objects:  37% (45/121)",
                TransferProgress {
                    phase: TransferPhase::Counting,
                    percent: Some(37),
                    objects: Some(45),
                    total_objects: Some(121),
                },
            ),
            (
                "remote: Compressing objects: 100% (120/120), done.",
                TransferProgress {
                    phase: TransferPhase::Compressing,
                    percent: Some(100),
                    objects: Some(120),
                    total_objects: Some(120),
                },
            ),
            (
                "Receiving objects:  66% (80/120), 174.40 KiB | 14.53 MiB/s",
                TransferProgress {
                    phase: TransferPhase::Receiving,
                    percent: Some(66),
                    objects: Some(80),
                    total_objects: Some(120),
                },
            ),
            (
                "Resolving deltas: 100% (39/39), completed with 1 local object.",
                TransferProgress {
                    phase: TransferPhase::Resolving,
                    percent: Some(100),
                    objects: Some(39),
                    total_objects: Some(39),
                },
            ),
        ];
        for (record, expected) in cases {
            assert_eq!(
                parse_progress(record).as_ref(),
                Some(expected),
                "failed to parse {record:?}"
            );
        }
    }

    /// The push side, captured verbatim from a real `git push --progress` (git
    /// 2.43.0) — the direction M2.20e added.
    ///
    /// Two things this pins that the fetch cases above cannot. **`Writing
    /// objects:` parses at all**: it is the one record shape a push prints and a
    /// fetch never does, so before #231 a pushing user's progress stopped dead
    /// at `Compressing` — the whole transfer, the part that actually takes the
    /// time, reported nothing. And **the unprefixed forms parse**: pushing, the
    /// enumerate/count/compress records come from *this* host with no `remote:`
    /// prefix, where fetching they arrive prefixed.
    #[test]
    fn every_real_push_progress_record_shape_parses() {
        let cases: &[(&str, TransferProgress)] = &[
            (
                "Enumerating objects: 15, done.",
                TransferProgress {
                    phase: TransferPhase::Enumerating,
                    percent: None,
                    objects: Some(15),
                    total_objects: None,
                },
            ),
            (
                "Counting objects:  13% (2/15)",
                TransferProgress {
                    phase: TransferPhase::Counting,
                    percent: Some(13),
                    objects: Some(2),
                    total_objects: Some(15),
                },
            ),
            (
                "Compressing objects:  92% (13/14)",
                TransferProgress {
                    phase: TransferPhase::Compressing,
                    percent: Some(92),
                    objects: Some(13),
                    total_objects: Some(14),
                },
            ),
            (
                "Writing objects:  46% (7/15)",
                TransferProgress {
                    phase: TransferPhase::Writing,
                    percent: Some(46),
                    objects: Some(7),
                    total_objects: Some(15),
                },
            ),
            (
                "Writing objects: 100% (15/15), 1004.39 KiB | 8.03 MiB/s, done.",
                TransferProgress {
                    phase: TransferPhase::Writing,
                    percent: Some(100),
                    objects: Some(15),
                    total_objects: Some(15),
                },
            ),
            (
                "remote: Resolving deltas: 100% (3/3), done.",
                TransferProgress {
                    phase: TransferPhase::Resolving,
                    percent: Some(100),
                    objects: Some(3),
                    total_objects: Some(3),
                },
            ),
        ];
        for (record, expected) in cases {
            assert_eq!(
                parse_progress(record).as_ref(),
                Some(expected),
                "failed to parse {record:?}"
            );
        }
    }

    /// A push's `Writing` and a fetch's `Receiving` are **different tags**, not
    /// two spellings of one.
    ///
    /// The load-bearing negative for the widening: a parser that mapped
    /// `Writing objects:` onto `Receiving` (the "it's a transfer either way"
    /// shortcut) would satisfy every other assertion in this file while telling
    /// a pushing user their data is arriving.
    #[test]
    fn writing_and_receiving_are_not_the_same_phase() {
        let writing = parse_progress("Writing objects:  46% (7/15)").unwrap();
        let receiving = parse_progress("Receiving objects:  46% (7/15)").unwrap();
        assert_eq!(writing.phase, TransferPhase::Writing);
        assert_eq!(receiving.phase, TransferPhase::Receiving);
        assert_ne!(writing.phase, receiving.phase);
        // …and the numbers are read identically, so the difference above is
        // the phase tag and nothing else.
        assert_eq!(writing.percent, receiving.percent);
        assert_eq!(writing.objects, receiving.objects);
        assert_eq!(writing.total_objects, receiving.total_objects);
    }

    /// The paired negative: everything else a fetch or a push prints must
    /// produce **no** progress. Without this, a parser that returned a default
    /// `TransferProgress` for any input would pass the tests above and publish
    /// a fabricated phase for git's ref-summary lines.
    #[test]
    fn non_progress_records_produce_no_progress() {
        for record in [
            "From /tmp/upstream",
            "   fc81d61..43138c2  main       -> origin/main",
            " * [new branch]      feature    -> origin/feature",
            "fatal: Authentication failed for 'https://example.invalid/r.git/'",
            "remote: Total 120 (delta 39), reused 0 (delta 0), pack-reused 0",
            "warning: no common commits",
            "",
            "remote:",
            // Push-side non-progress, verbatim from git 2.43.0.
            "To ./up.git",
            " * [new branch]      main -> main",
            " ! [rejected]        main -> main (stale info)",
            "Delta compression using up to 4 threads",
            "Total 15 (delta 3), reused 0 (delta 0), pack-reused 0",
            "Everything up-to-date",
            "branch 'main' set up to track 'origin/main'.",
            "error: failed to push some refs to './up.git'",
        ] {
            assert_eq!(
                parse_progress(record),
                None,
                "{record:?} must not be read as progress"
            );
        }
    }

    /// A percentage git could not have printed is dropped rather than
    /// clamped: a bar drawn from a fabricated number is worse than no bar.
    #[test]
    fn an_impossible_percentage_is_dropped_not_clamped() {
        let p = parse_progress("Receiving objects: 250% (5/2)").unwrap();
        assert_eq!(p.percent, None);
        assert_eq!(p.objects, Some(5));
    }

    fn refs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_ref_diff_reports_moved_new_and_gone_refs_and_nothing_else() {
        let before = refs(&[
            ("refs/remotes/origin/main", "aaa"),
            ("refs/remotes/origin/stable", "bbb"),
            ("refs/remotes/origin/dropped", "ccc"),
        ]);
        let after = refs(&[
            ("refs/remotes/origin/main", "ddd"),
            ("refs/remotes/origin/stable", "bbb"),
            ("refs/remotes/origin/fresh", "eee"),
        ]);
        let diff = diff_refs(&before, &after);
        assert_eq!(
            diff,
            vec![
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/dropped".into(),
                    old_oid: Some("ccc".into()),
                    new_oid: None,
                },
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/fresh".into(),
                    old_oid: None,
                    new_oid: Some("eee".into()),
                },
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/main".into(),
                    old_oid: Some("aaa".into()),
                    new_oid: Some("ddd".into()),
                },
            ],
            "an unchanged ref must not appear, and the order must be stable"
        );
    }

    #[test]
    fn an_unchanged_listing_diffs_to_nothing() {
        let same = refs(&[("refs/remotes/origin/main", "aaa")]);
        assert!(diff_refs(&same, &same).is_empty());
    }
}
