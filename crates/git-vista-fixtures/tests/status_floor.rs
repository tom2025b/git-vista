//! #365 — the status parser, run against the **documented git floor** and not
//! only against whatever git the developer happens to have.
//!
//! # What was actually wrong
//!
//! `crates/git-vista-protocol/src/status.rs` parses `git status
//! --porcelain=v2 --branch -z`, and its fifteen unit tests feed it hand-written
//! byte strings — good tests of the parser, and no evidence at all about
//! `git`, because no git runs in any of them. The module said so itself:
//! every record shape was *captured* from one real 2.43.0 by hand and encoded.
//!
//! Meanwhile `docs/SUPPORTED_VERSIONS.md` sets the floor at **2.32**, and
//! `.github/workflows/ci.yml` rejects a runner *older* than that. Nothing
//! provisioned the floor, so the check enforced "not older than 2.32" while
//! every fixture had only ever met 2.43.0.
//!
//! # What this test does
//!
//! Builds the real repositories in [`git_vista_fixtures::status`], reads each
//! with `git status --porcelain=v2 --branch -z` — three ways, see below —
//! parses the bytes with the production parser, and asserts the result equals
//! a **named expected value written out below**. Not "the two versions agree":
//! two identical wrong answers compare equal, so agreement is checked *as well
//! as* correctness, never instead of it.
//!
//! When `GV_GIT_FLOOR` names a second binary, every read is done twice and
//! both are held to the same expectations.
//!
//! # How "mandatory" is enforced, and why not here
//!
//! This test cannot be the thing that decides whether the floor leg ran. A
//! test that skips when its binary is missing reports the same green as one
//! that passed, which is the failure mode this repository has written down
//! repeatedly — and a test that *fails* when the binary is missing would make
//! `cargo test --workspace` impossible for a contributor who has one git.
//!
//! So it does neither. It writes a **report** to `GV_STATUS_FLOOR_REPORT`
//! recording which binaries actually ran, and CI asserts over that file in
//! shell — the same anti-vacuity shape `ci/` already uses for the escape
//! battery (`GV_ESCAPE_REPORT`), where an unset variable fails closed. The
//! decision lives in ADR 0082.

use git_vista_fixtures::status::{self, BATTERIES};
use git_vista_protocol::{
    parse_porcelain_v2_z, ChangeKind, ChangeSides, ConflictKind, StatusEntry, SubmoduleState,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The production argv, plus the two variants that reach records production
/// never asks for. See `status::status_battery`'s doc for why those two exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Exactly what `/api/status/v2` runs.
    Production,
    /// `--ignored`, the only way a `!` record appears.
    Ignored,
    /// `status.renames=copies`, the only way a `C` record appears.
    Copies,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::Production, Mode::Ignored, Mode::Copies];

    fn name(self) -> &'static str {
        match self {
            Mode::Production => "production",
            Mode::Ignored => "ignored",
            Mode::Copies => "copies",
        }
    }

    fn config(self) -> &'static [&'static str] {
        match self {
            Mode::Copies => &["status.renames=copies"],
            _ => &[],
        }
    }

    fn extra_args(self) -> &'static [&'static str] {
        match self {
            Mode::Ignored => &["--ignored"],
            _ => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Running git
// ---------------------------------------------------------------------------

/// Run `git status --porcelain=v2 --branch -z` under `mode`, with `binary`.
///
/// The argv's fixed part is written out here rather than borrowed from a
/// helper on purpose: this test's whole claim is about what the *product*
/// runs, and `crates/git-vista-server/src/handlers/read.rs` spells the same
/// four arguments. A shared constant would let both move together and the
/// claim would quietly stop being about production.
fn status_bytes(binary: &Path, repo: &Path, mode: Mode) -> Vec<u8> {
    let mut cmd = Command::new(binary);
    for kv in mode.config() {
        cmd.arg("-c").arg(kv);
    }
    cmd.arg("-C").arg(repo);
    cmd.args(["status", "--porcelain=v2", "--branch", "-z"]);
    cmd.args(mode.extra_args());
    let out = cmd
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|e| panic!("could not run {binary:?} in {repo:?}: {e}"));
    assert!(
        out.status.success(),
        "{binary:?} status failed in {repo:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn version_of(binary: &Path) -> String {
    let out = Command::new(binary)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| panic!("could not run {binary:?} --version: {e}"));
    assert!(out.status.success(), "{binary:?} --version failed");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .strip_prefix("git version ")
        .unwrap_or_else(|| panic!("{binary:?} does not report a git version"))
        .to_string()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has a workspace root two levels up")
        .to_path_buf()
}

/// The floor, parsed out of `docs/SUPPORTED_VERSIONS.md`.
///
/// Parsed, never retyped: that document is the single source of truth by
/// construction, and `ci.yml`'s existing version-floor step reads the same
/// heading the same way. Hardcoding `2.32` here would create the second place
/// it can be wrong — which is the drift the heading-parsing pattern exists to
/// prevent.
fn documented_floor() -> String {
    let path = repo_root().join("docs/SUPPORTED_VERSIONS.md");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let floor = text
        .lines()
        .find_map(|l| l.strip_prefix("## Git: "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| {
            panic!("no '## Git: X.Y or later' heading in {path:?} — the doc and this test have drifted apart")
        });
    assert!(
        floor
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        "the floor heading in {path:?} is not a version number: {floor:?}"
    );
    floor.to_string()
}

// ---------------------------------------------------------------------------
// The named expectations
// ---------------------------------------------------------------------------

fn changed(path: &str, sides: ChangeSides) -> StatusEntry {
    StatusEntry::Changed {
        path: path.to_string(),
        sides,
        submodule: None,
        binary: false,
    }
}

fn submodule(path: &str, commit_changed: bool) -> StatusEntry {
    StatusEntry::Changed {
        path: path.to_string(),
        sides: ChangeSides::UnstagedOnly {
            unstaged: ChangeKind::Modified,
        },
        submodule: Some(SubmoduleState {
            commit_changed,
            has_tracked_changes: true,
            has_untracked_changes: true,
        }),
        binary: false,
    }
}

fn renamed(path: &str, origin_path: &str) -> StatusEntry {
    StatusEntry::Renamed {
        path: path.to_string(),
        origin_path: origin_path.to_string(),
        score: 100,
        unstaged: None,
        submodule: None,
        binary: false,
    }
}

fn conflicted(path: &str, kind: ConflictKind) -> StatusEntry {
    StatusEntry::Conflicted {
        path: path.to_string(),
        kind,
        submodule: None,
    }
}

/// What every supported git must report for `shape` under `mode`, in git's own
/// order (path-sorted within a run).
fn expected(shape: &str, mode: Mode) -> Vec<StatusEntry> {
    match shape {
        "battery" => {
            let mut v = vec![changed(
                "both.txt",
                ChangeSides::Both {
                    staged: ChangeKind::Modified,
                    unstaged: ChangeKind::Modified,
                },
            )];
            // A copy is only a copy when git is asked to look for one; under
            // the production argv the destination is an ordinary staged add.
            if mode == Mode::Copies {
                v.push(renamed("copy-dst.txt", "copy-src.txt"));
            } else {
                v.push(changed(
                    "copy-dst.txt",
                    ChangeSides::StagedOnly {
                        staged: ChangeKind::Added,
                    },
                ));
            }
            v.push(changed(
                "copy-src.txt",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Modified,
                },
            ));
            v.push(renamed("rename-dst.txt", "rename-src.txt"));
            v.push(changed(
                "staged.txt",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Modified,
                },
            ));
            v.push(changed(
                "unstaged.txt",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            ));
            v.push(submodule("vendor/dirty", false));
            v.push(submodule("vendor/moved", true));
            v.push(StatusEntry::Untracked {
                path: "untracked.txt".to_string(),
                binary: false,
            });
            if mode == Mode::Ignored {
                v.push(StatusEntry::Ignored {
                    path: "build.log".to_string(),
                });
            }
            v
        }
        "conflicts-rename" => vec![
            conflicted("f.txt", ConflictKind::BothDeleted),
            conflicted("main-name.txt", ConflictKind::AddedByUs),
            conflicted("side-name.txt", ConflictKind::AddedByThem),
        ],
        "conflicts-merge" => vec![
            conflicted("both-add.txt", ConflictKind::BothAdded),
            conflicted("both-mod.txt", ConflictKind::BothModified),
            conflicted("they-del.txt", ConflictKind::DeletedByThem),
            conflicted("we-del.txt", ConflictKind::DeletedByUs),
        ],
        other => panic!("no expectation written for shape {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
fn the_status_vocabulary_parses_identically_on_every_supported_git() {
    assert!(
        !BATTERIES.is_empty(),
        "an empty battery table would make this test pass over nothing"
    );

    let current = PathBuf::from("git");
    let current_version = version_of(&current);

    // The floor binary, if one was provisioned. Its identity is checked
    // against the documented floor BEFORE it is trusted for anything: a
    // harness pointed at a second copy of the current git would otherwise
    // compare a version with itself and report the whole thing green.
    let floor_binary = std::env::var_os("GV_GIT_FLOOR").map(PathBuf::from);
    let floor_version = floor_binary.as_ref().map(|b| {
        let v = version_of(b);
        let want = documented_floor();
        assert!(
            v == want || v.starts_with(&format!("{want}.")),
            "GV_GIT_FLOOR={b:?} reports git {v}, which is not the documented \
             floor {want} from docs/SUPPORTED_VERSIONS.md. Comparing the \
             current git against itself would prove nothing."
        );
        v
    });

    let mut report = String::new();
    report.push_str(&format!("current={current_version}\n"));
    report.push_str(&format!(
        "floor={}\n",
        floor_version.clone().unwrap_or_else(|| "unrun".to_string())
    ));

    for (shape, build) in BATTERIES {
        let fixture = build();
        let repo = status::repo_of(&fixture);

        for mode in Mode::ALL {
            let want = expected(shape, mode);
            assert!(
                !want.is_empty(),
                "the expectation for {shape}/{} is empty, which would make \
                 every assertion below vacuous",
                mode.name()
            );

            let got = parse_porcelain_v2_z(&status_bytes(&current, repo, mode));
            assert_eq!(
                got.entries,
                want,
                "git {current_version} disagrees with the expectation for \
                 {shape} under {}",
                mode.name()
            );
            report.push_str(&format!(
                "shape={shape} mode={} git={current_version} entries={}\n",
                mode.name(),
                got.entries.len()
            ));

            let Some(binary) = floor_binary.as_ref() else {
                continue;
            };
            let floor_version = floor_version.as_deref().expect("checked above");
            let floor_got = parse_porcelain_v2_z(&status_bytes(binary, repo, mode));

            // Held to the same named expectation, not merely to `got`: two
            // identical wrong answers compare equal.
            assert_eq!(
                floor_got.entries,
                want,
                "git {floor_version} disagrees with the expectation for \
                 {shape} under {}",
                mode.name()
            );
            // And then to each other, including the branch headers, which the
            // expectation above does not cover.
            assert_eq!(
                floor_got,
                got,
                "git {floor_version} and git {current_version} parse {shape} \
                 differently under {} — this is a FINDING, not a flake: record \
                 it in the PR body rather than relaxing the assertion",
                mode.name()
            );
            report.push_str(&format!(
                "shape={shape} mode={} git={floor_version} entries={}\n",
                mode.name(),
                floor_got.entries.len()
            ));
        }
    }

    if let Some(path) = std::env::var_os("GV_STATUS_FLOOR_REPORT") {
        std::fs::write(&path, &report)
            .unwrap_or_else(|e| panic!("write the floor report to {path:?}: {e}"));
    }
}
