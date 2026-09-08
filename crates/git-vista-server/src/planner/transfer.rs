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

/// `for-each-ref` only reads refs and dispatches no hook (#753) — it reaches
/// no remote regardless of which operation calls it. Declared here rather
/// than threaded from the caller for the same reason `pull.rs`'s
/// `INTEGRATION_NEED` gives for `unmerged_paths`: a `need` parameter on this
/// function would be one more place an operation's `Remote` declaration
/// could leak into a spawn that has no business holding it, and `run_fetch`'s
/// and `exec_push`'s own `debug_assert_eq!(need, NetworkNeed::Remote)` proves
/// the operation-level value really is `Remote` at every one of this
/// function's three call sites — reusing it here would be reusing the wrong
/// thing, not the convenient one.
///
/// `reconcile_need` agrees: `for-each-ref` is not in `REMOTE_SUBCOMMANDS`, so
/// declaring `Local` here never trips the D3 cross-check.
const REF_READ_NEED: NetworkNeed = NetworkNeed::Local;

/// Every `refs/remotes/<remote>/*` ref and the object it points at.
///
/// `Err` is "we could not observe", which is a refusal reason and never
/// silently an empty map — a transfer whose before-state is unknown cannot
/// honestly answer "did anything move?" afterwards, and that answer is the
/// whole contract of a cancelled fetch and of a refused push (D5's posture: we
/// did not observe anything, so we may not act as though we did).
pub(super) async fn remote_tracking_refs(
    repo: &Path,
    remote: &RemoteName,
) -> Result<BTreeMap<String, String>, String> {
    let prefix = format!("refs/remotes/{}/", remote.as_str());
    let output = run_git(
        repo,
        REF_READ_NEED,
        &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
    )
    .await
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(stderr_or(&output, "git for-each-ref failed."));
    }
    // `refs/remotes/<remote>/HEAD` is excluded, and that exclusion is
    // load-bearing rather than cosmetic. It is a *symbolic* ref pointing at
    // one of the branch refs already in this map, so counting it reports a
    // single branch movement twice — once as `origin/main`, once as
    // `origin/HEAD` shadowing it. Worse, whether it appears at all is a
    // git-version difference: git 2.54 writes it during `fetch`, git 2.43
    // does not, so including it makes the observed result depend on the git
    // on the host. CI (2.54) failed five fetch tests that passed locally
    // (2.43) for exactly this reason. A remote-tracking *branch* is what a
    // transfer moves; the symref is bookkeeping about which branch is default.
    //
    // This filter arrived on `main` in `e80d647` while this function still
    // lived in `fetch.rs`. Hoisting the function into this shared module (so
    // fetch, pull and push observe refs identically) moved the code out from
    // under that fix, and the merge presented it as an add/add conflict whose
    // HEAD side was empty — resolving it the obvious way would have dropped
    // the filter silently. It is restored here so all three transfer paths
    // inherit it, which is the point of sharing the function at all.
    let head_symref = format!("{prefix}HEAD");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.trim().split_once(' ')?;
            if name == head_symref {
                return None;
            }
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

    // -----------------------------------------------------------------
    // #753: `for-each-ref` must run under a tier that denies `AF_UNIX`,
    // on every route that reaches `remote_tracking_refs` — proven by
    // observing the *actual* Linux network namespace of the spawned
    // process, not by re-deriving the tier from the constant that
    // declares it (`tier_for(REF_READ_NEED, ..)` would only prove the
    // classifier agrees with itself).
    // -----------------------------------------------------------------

    use git_vista_fixtures::seeded as seeded_repo;

    /// The probe the two tests below compare against: a *host* fact, so a
    /// spawn reporting this string ran with the host's network — no bwrap,
    /// no `--unshare-net` — and a spawn reporting anything else did not.
    /// Same technique as `pull_suite::host_netns`, duplicated because that
    /// one is private to its own test module.
    fn host_netns() -> String {
        std::fs::read_link("/proc/self/ns/net")
            .expect("Linux: /proc/self/ns/net")
            .to_string_lossy()
            .into_owned()
    }

    /// A `PATH` containing nothing but a fake `git` that prints its own
    /// network namespace, dressed as one valid `for-each-ref --format=%(refname)
    /// %(objectname))` output line so [`remote_tracking_refs`]'s own parser
    /// keeps it rather than discarding it.
    ///
    /// `for-each-ref` dispatches no hook — that is #753's whole finding — so
    /// there is no user-code seam a probe can ride the way `pull_suite`'s
    /// installed hooks do for `merge`/`rebase`. Standing in for the real
    /// binary is the only way to observe this spawn's confinement at all.
    /// Same technique `git_cmd.rs`'s `fake_git_dumper` uses, duplicated for
    /// the same reason that one gives: private to its own test module.
    /// Written inside `repo` (already rw-granted by the policy under test, at
    /// every tier) since a path outside every grant a policy makes cannot be
    /// exec'd at all under Landlock.
    fn fake_git_netns_probe(repo: &Path) -> String {
        let dir = repo.join("fake-bin");
        std::fs::create_dir_all(&dir).expect("mkdir fake-bin");
        let bin = dir.join("git");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho \"refs/remotes/origin/probe $(readlink /proc/self/ns/net)\"\n",
        )
        .expect("write fake git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&bin).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&bin, perm).unwrap();
        }
        // The fake `git` shells out to the real `readlink`, so PATH must still
        // resolve it — `fake-bin` first, so `git` itself still hits the fake,
        // then the real system directories for everything the script calls.
        format!("{}:/usr/bin:/bin", dir.display())
    }

    const PROBE_MODE: &str = "GV753_PROBE_MODE";
    const PROBE_REPO: &str = "GV753_PROBE_REPO";

    /// **The child half of the netns proof.** Gated behind `PROBE_MODE` so it
    /// is a no-op under an ordinary `cargo test` run — it only does anything
    /// when [`observed_netns_in_subprocess`] re-execs this same test binary
    /// with that variable set.
    ///
    /// Runs in an isolated *process*, not a thread, specifically so its `PATH`
    /// override cannot race any other test's real `git` spawn — the same
    /// reasoning `couldnt_run_suite`'s `subprocess_probe` gives for its own
    /// process-global env mutation (#666).
    ///
    /// Two modes, both driving a **real, unmodified production entry point**
    /// rather than reimplementing `git_cmd::sandboxed`'s three calls by hand:
    /// * `"remote_tracking_refs"` calls [`remote_tracking_refs`] itself — the
    ///   exact function `run_fetch`/`push.rs` call, unmodified — so this
    ///   exercises `remote_tracking_refs -> run_git -> git_output_for ->
    ///   sandboxed` end to end. A mutation to `remote_tracking_refs`'s own
    ///   call site (reverting it to a caller-supplied or `Remote` need) is
    ///   caught here, not merely by the source-scan test below.
    /// * `"git_output_for_remote"` calls the *same* `git_output_for` that
    ///   `run_git` is a one-line wrapper around, with the identical argv
    ///   declared `Remote` — the operation-level need `for-each-ref`
    ///   inherited before this fix. This is the paired negative: it reaches
    ///   the same `sandboxed` chokepoint the first mode does, so it proves
    ///   the probe technique discriminates tiers rather than measuring
    ///   nothing (a test runner already inside a netns, a fake binary that
    ///   silently never ran).
    #[test]
    fn subprocess_probe() {
        let Ok(mode) = std::env::var(PROBE_MODE) else {
            return;
        };
        let repo = std::path::PathBuf::from(
            std::env::var_os(PROBE_REPO).expect("GV753_PROBE_REPO set by the parent"),
        );
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let netns = rt.block_on(async {
            match mode.as_str() {
                "remote_tracking_refs" => {
                    let remote = RemoteName::new("origin").expect("valid remote name");
                    let map = remote_tracking_refs(&repo, &remote)
                        .await
                        .expect("remote_tracking_refs must succeed against the fake-git probe");
                    map.into_values()
                        .next()
                        .expect("fake git's synthetic ref line must have parsed into the map")
                }
                "git_output_for_remote" => {
                    let out = crate::git_cmd::git_output_for(
                        &repo,
                        &[
                            "for-each-ref",
                            "--format=%(refname) %(objectname)",
                            "refs/remotes/origin/",
                        ],
                        NetworkNeed::Remote,
                    )
                    .await
                    .expect("git_output_for must succeed against the fake-git probe");
                    assert!(
                        out.status.success(),
                        "fake git exited nonzero: stderr={}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .rsplit(' ')
                        .next()
                        .expect("fake git's synthetic line must carry a namespace field")
                        .to_string()
                }
                other => panic!("unknown {PROBE_MODE} {other:?}"),
            }
        });
        println!("\nGV753_NETNS={netns}");
    }

    /// Re-exec this test binary (same technique `couldnt_run_suite::probe`
    /// uses for #666, and for the identical reason: an env override must not
    /// race another test's real `git` spawn in the same process) with only
    /// [`subprocess_probe`] selected, `PATH` pointed at the fake-git probe,
    /// and `mode` chosen. Returns the network namespace the probe reported.
    fn observed_netns_in_subprocess(mode: &str) -> String {
        let (_dir, repo) = seeded_repo();
        let dumper = fake_git_netns_probe(&repo);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "planner::transfer::tests::subprocess_probe",
                "--nocapture",
            ])
            .env(PROBE_MODE, mode)
            .env(PROBE_REPO, &repo)
            .env("PATH", dumper)
            .env("HOME", std::env::var("HOME").unwrap())
            .output()
            .expect("spawn self as subprocess");
        assert!(
            output.status.success(),
            "subprocess probe (mode={mode}) failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        stdout
            .lines()
            .find_map(|l| l.strip_prefix("GV753_NETNS="))
            .expect("the child must call the real helper and print GV753_NETNS=...")
            .to_string()
    }

    /// **The need `remote_tracking_refs` actually declares confines
    /// `for-each-ref`** — its spawn's network namespace differs from the
    /// host's, i.e. it runs `Tier::Strict` (bwrap `--unshare-net`), not the
    /// `Tier::Network` the surrounding fetch/pull/push operation's own
    /// `Remote` need would give it. Drives the real, unmodified
    /// `remote_tracking_refs` — see `subprocess_probe`'s doc for why that
    /// matters and how.
    #[test]
    fn for_each_ref_runs_confined_from_the_hosts_network_namespace() {
        let host = host_netns();
        let observed = observed_netns_in_subprocess("remote_tracking_refs");
        assert_ne!(
            observed, host,
            "for-each-ref, run through the real remote_tracking_refs, must NOT \
             share the host's network namespace — seeing {host} here means it is \
             running Tier::Network (or unsandboxed), and #753's excess AF_UNIX \
             authority is back."
        );
    }

    /// The paired negative: the identical argv, declared `Remote` through the
    /// same `sandboxed` chokepoint — the operation-level need `for-each-ref`
    /// inherited before this fix — DOES share the host's namespace.
    ///
    /// Without this leg, the assertion above could pass for a reason that has
    /// nothing to do with the tier: a test runner already inside a netns, or
    /// a fake binary that silently never ran. With it, the same probe
    /// mechanism is shown reporting both answers, so the first leg's
    /// "confined" reading is real. This is also the `declared` mutation arm
    /// by construction: reverting `REF_READ_NEED` to `NetworkNeed::Remote`
    /// makes the positive test above assert exactly what this one already
    /// does, and it would then fail.
    #[test]
    fn the_same_spawn_declared_remote_would_share_the_hosts_namespace() {
        let host = host_netns();
        let observed = observed_netns_in_subprocess("git_output_for_remote");
        assert_eq!(
            observed, host,
            "sanity leg: the same for-each-ref argv declared Remote must run \
             Tier::Network and share the host's namespace, or the probe above \
             proves nothing. Saw {observed:?}, host is {host:?}."
        );
    }

    /// **Wiring, not just mechanism.** The two netns tests above prove that
    /// *if* `REF_READ_NEED` is used as `run_git`'s second argument, the
    /// spawn is confined — this proves `remote_tracking_refs` really passes
    /// it there: its signature must carry no `need` a caller could thread a
    /// `Remote` declaration through, and its body must call `run_git` with
    /// `REF_READ_NEED` in exactly that position — not merely mention the
    /// name (a dead reference or a doc comment would satisfy a bare
    /// `contains` and prove nothing).
    ///
    /// Same source-scan technique `sandbox::spawn`'s
    /// `the_sandboxed_command_exposes_no_way_to_change_what_runs` uses, for
    /// the same reason: this is cheaper and more honest to assert against
    /// the source text than to contrive a runtime probe for, and a runtime
    /// probe of `remote_tracking_refs` itself cannot inject the fake `git`
    /// binary the two tests above need (it has no environment-override
    /// seam — deliberately, per `SandboxedCommand`'s own "no env" rule).
    #[test]
    fn remote_tracking_refs_takes_no_need_from_its_caller() {
        let src = include_str!("transfer.rs");
        let start = src
            .find("pub(super) async fn remote_tracking_refs(")
            .expect("remote_tracking_refs moved or was renamed");
        let sig_end = src[start..]
            .find(") -> Result<BTreeMap<String, String>, String> {")
            .expect("signature shape changed");
        let signature = &src[start..start + sig_end];
        assert!(
            !signature.contains("need"),
            "remote_tracking_refs's signature must not accept a `need` from its \
             caller — that would be one more place an operation's Remote \
             declaration could leak into this local-only read. signature={signature:?}"
        );
        let body_start = start + sig_end;
        let body_end = src[body_start..]
            .find("\n}\n")
            .map(|i| body_start + i)
            .expect("unterminated function body");
        let body = &src[body_start..body_end];
        // Pins REF_READ_NEED as `run_git`'s *second positional argument* at
        // this call site — not a bare substring match, which a dead
        // reference or a comment mentioning the name would also satisfy.
        assert!(
            body.contains("run_git(\n        repo,\n        REF_READ_NEED,\n"),
            "remote_tracking_refs must call `run_git(repo, REF_READ_NEED, ..)` — \
             REF_READ_NEED must be the need actually passed to the spawn, not \
             merely referenced somewhere in this function. body={body:?}"
        );
    }

    /// **Every route that reaches `remote_tracking_refs` passes no `need`.**
    /// The wiring test above pins what the helper itself does with
    /// `REF_READ_NEED`; this pins that its three call sites — before and
    /// after `run_fetch` (which `exec_pull`'s fetch half also goes through,
    /// so pull is covered transitively) and before and after `push.rs`'s
    /// push executor — cannot thread an operation's own `need` into it,
    /// because the call shape `remote_tracking_refs(repo, need, remote)`
    /// would no longer compile once the parameter was removed. This test
    /// exists so a regression that reintroduces the parameter *and*
    /// updates every call site consistently — leaving the crate compiling
    /// but the parameter alive again — still fails here.
    #[test]
    fn every_call_site_uses_the_two_argument_shape() {
        for (file, src) in [
            ("fetch.rs", include_str!("fetch.rs")),
            ("push.rs", include_str!("push.rs")),
        ] {
            let two_arg = src.matches("remote_tracking_refs(repo, remote)").count();
            let three_arg = src
                .matches("remote_tracking_refs(repo, need, remote)")
                .count();
            assert_eq!(
                three_arg, 0,
                "{file} calls remote_tracking_refs with a caller-supplied `need` — \
                 that thread must not exist"
            );
            assert_eq!(
                two_arg, 2,
                "{file} must call remote_tracking_refs(repo, remote) exactly twice \
                 (before and after the transfer); saw {two_arg}"
            );
        }
    }
}
