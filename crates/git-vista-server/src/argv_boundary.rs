//! #144 (M1.06c): proof the browser cannot smuggle arbitrary argv into
//! server-side git execution — and a tripwire so it stays that way.
//!
//! The audit behind this file found no raw-command or freeform-args route:
//! every write body deserializes into a closed `deny_unknown_fields` DTO (or
//! the typed `UndoAction` enum), five write routes take no body at all, and
//! the only client string that ever becomes a git argument travels as its own
//! argv entry after validation (`validate_clone_url`). These tests pin that
//! posture down in three layers:
//!
//!  1. A source scan asserting every process-spawn site in this crate and the
//!     native git crate lives in an allowlisted module and spawns only `git`,
//!     never a shell. A new spawn site fails the scan until it is reviewed
//!     and deliberately allowlisted here.
//!  2. Serde-level adversarial fixtures: every write-request DTO refuses
//!     unknown fields (no `"args": [...]` smuggled beside legitimate fields)
//!     and non-object shapes (no raw argv arrays).
//!  3. Wire-level adversarial fixtures through the real auth/CSRF middleware
//!     and the real DTO extractors: hostile bodies die at the API boundary
//!     with a client error before any handler logic — let alone git — runs.

use std::path::{Path, PathBuf};

/// Files allowed to construct a process `Command`, relative to their crate
/// root. Everything else is a regression. Keep the list short and deliberate:
/// the planner's executor is where every *client-requested* mutation's argv is
/// built (#143); `durable.rs`'s `update-ref` (#62) is the one other mutating
/// site, and it is narrow by construction rather than by review alone — fixed
/// subcommand, a ref name built only from the server-minted `OperationId`
/// (token-shaped, never client free text) under a fixed app-owned prefix, and
/// an oid that only ever comes from an already-validated `CommitOid`. Every
/// other entry here is a read-only helper or `#[cfg(test)]` fixture setup.
const ALLOWED_SPAWN_SITES: &[&str] = &[
    // git-vista-server
    // The executor — every client-requested mutation's argv is built here.
    // Its production spawn (`run_git`) now goes through
    // `crate::git_cmd::git_output`, the sealed sandbox launcher (#66 Task 6);
    // this entry now covers only its `#[cfg(test)]` fixture setup.
    "src/planner.rs",
    // `git update-ref` for recovery refs (#62) — see the module doc above.
    // The production call (`write_recovery_ref`) now goes through
    // `crate::git_cmd::git_output` (#66 Task 6); this entry now covers only
    // its `#[cfg(test)]` fixture setup — specifically `read_recovery_ref`,
    // which stayed in this file rather than moving with its two callers (see
    // `durable/recovery_ref_suite.rs`'s doc comment for why).
    "src/durable.rs",
    // The recovery-ref write/read tests extracted verbatim from durable.rs's
    // inline `mod tests` (M-current test-extraction). `#[cfg(test)]` fixture
    // setup only, same posture as `src/durable.rs` above: throwaway
    // repositories built to prove `write_recovery_ref` writes the right ref
    // and never the working branch of the same name.
    "src/durable/recovery_ref_suite.rs",
    // The shared read-only git helpers, including the sealed `git_output`
    // launcher (#66 Task 6). Note what this entry does *not* cover any more:
    // `git_output` builds no `Command` of its own — it goes through
    // `sandbox::spawn::command_async`, which is the chokepoint — so every
    // `Command::new` left in this file is `#[cfg(test)]` fixture setup.
    "src/git_cmd.rs",
    // `src/handlers/clone.rs` was here for its raw `git clone` spawn. It is
    // gone: the production call goes through `crate::git_cmd::git_output`, the
    // sealed sandbox launcher (#66 Task 6, plan step 6.7), and unlike the other
    // migrated entries below the file has **no** `#[cfg(test)]` `Command::new`
    // left either — its tests exercise pure helpers (`clone_dir_name`,
    // `unique_dest`) and the delete handler. So the entry was removed rather
    // than re-commented: an allowlist entry for a file that constructs no
    // `Command` is a standing permission nobody needs, and the scan below only
    // consults this list for files that *do* spawn, so a stale entry would
    // silently pre-authorise a future raw spawn there — in the one handler that
    // fetches attacker-chosen content.
    // `git status --porcelain=v2` (static args). The production call
    // (`worktree_status`) now goes through `crate::git_cmd::git_output`
    // (#66 Task 6). Its `#[cfg(test)]` fixtures used to be this entry's only
    // remaining reason to exist; a mechanical test-extraction moved every
    // inline `#[cfg(test)]` module out to `src/handlers/read/<topic>_suite.rs`
    // child files (this crate's own `planner/*_suite.rs` convention), so
    // `read.rs` itself now constructs no `Command` at all — see the three
    // entries below for where those fixtures actually live now.
    //
    // #63's paged-history suites, split out of the single `read.rs` test
    // module above: `content_suite.rs` (diff/file-read fixtures: `run`/`out`/
    // `stdout_len`), `graph_suite.rs` (history/paging fixtures: `run`/`out`/
    // `run_env`, plus the `git fast-import`/`git daemon` fixtures for the
    // deep/adversarial repos), and `status_suite.rs` (worktree-status
    // fixtures: a duplicated `run`, matching the planner-suite convention of
    // each suite carrying its own private copy rather than sharing one).
    // `routing_suite.rs` — the fourth split-out file, covering route
    // registration and the `?repo=` selector — constructs no `Command` and is
    // deliberately not listed here.
    "src/handlers/read/content_suite.rs",
    "src/handlers/read/graph_suite.rs",
    "src/handlers/read/status_suite.rs",
    // M2.21b (#236): `#[cfg(test)]` fixture setup only. No handler in this
    // file runs a subprocess in production. `GET /api/tags` runs none at all —
    // `git_vista_git::read_tags` opens the repository with `gix` and decodes
    // tag objects out of the mapped object database — and M2.21d's (#238)
    // `POST /api/tag` / `POST /api/delete-tag` build a typed `GitOperation`
    // and hand it to the planner, so their `git tag` / `git tag -d` argvs are
    // constructed in `planner.rs` like every other client-requested mutation
    // (ADR 0016). Every `Command::new` in this file therefore builds a
    // throwaway tagged repository for the tests. It lives here rather than in
    // `main.rs` precisely because `main.rs` must keep constructing no
    // `Command` at all (see `every_allowlist_entry_names_a_live_spawn_site`,
    // which uses it as its negative control) and because
    // `sandbox::escape_contract`'s R7 pins `main.rs`'s `GIT_*` surface to two
    // variables.
    "src/handlers/tags.rs",
    // #327: `#[cfg(test)]` fixture setup only. The production probe —
    // `revert_would_conflict`'s `git merge-tree --write-tree`, which
    // establishes whether a revert can actually apply before the UI offers it
    // — goes through `crate::git_cmd::git_output`, the sealed sandbox
    // launcher, exactly like every other production read in this crate. The
    // two `Command::new("git")` calls left in this file build throwaway
    // repositories for the tests that pin that probe's classification.
    //
    // This entry exists because the tripwire below caught the addition and
    // refused the build until it was reviewed — which is the mechanism
    // working, not a formality. Do not widen it to cover a production spawn:
    // if `revert_would_conflict` ever stops going through `git_output`, that
    // is a boundary change needing its own decision, and this comment is the
    // record that it was not one when the entry was added.
    "src/activity.rs",
    // M3.25 (#78): `#[cfg(test)]` fixture setup only — `git init`/`commit`/
    // `rev-parse`, building throwaway repositories for the live
    // recovery-classification tests. The module's production reads all go
    // through `crate::git_cmd::git_output`, the sealed sandbox launcher
    // (`resolve_ref_exact`), and its one *mutating* path builds no argv at all:
    // `recover_operation` hands a typed `GitOperation` to the planner like
    // every other write (ADR 0016), so the recovery's git argv is constructed
    // in `planner.rs` with all the others.
    //
    // Do not widen this entry to cover a production spawn. If classification
    // ever stops going through `git_output`, that is a boundary change needing
    // its own decision — this comment is the record that it was not one when
    // the entry was added, and the tripwire refusing the build is what forced
    // the review.
    "src/recovery_center.rs",
    // `#[cfg(test)]` fixture setup only — `git init -q`, to build repositories
    // `read_repo_facts` can classify. This entry used to read "static-arg read
    // at registration", which stopped being true when registration moved to the
    // sealed helpers: the file's only `Command::new` today sits inside
    // `#[cfg(test)] mod tests`. A comment that describes a production spawn the
    // file no longer performs is how a later reviewer talks themselves into
    // leaving a permission wider than the file needs.
    "src/catalog.rs",
    // D2 (#66, Task 7): `#[cfg(test)]` fixture setup only (`git init`, to
    // build real repos for the hostile-geometry/managed-root tests) — no
    // production spawn.
    "src/sandbox/repo_paths.rs",
    "src/sandbox/hostile.rs", // same: `#[cfg(test)]` fixture setup only
    // Task 9's boot probe (#66, INV-13/GC15). Its ONE `Command::new("git")`
    // is `boot_probe_fixture`'s unsandboxed `git init` for a throwaway
    // scratch repo — fixture construction, not the thing under test, run
    // outside the sandbox on purpose so a fixture failure is never
    // misreported as a capability-absent verdict (see that function's doc
    // comment). Unlike the other fixture-only entries above, this one runs
    // in production (every real boot), not only under `#[cfg(test)]` — the
    // whole point of a boot gate is that it runs for real.
    "src/sandbox/probe.rs",
    "src/journal.rs", // #[cfg(test)] fixture setup
    // `git rev-parse --absolute-git-dir` (static args, read-only). The
    // production call (`absolute_git_dir`) now goes through
    // `crate::git_cmd::git_output` (#66 Task 6); this entry now covers only
    // its `#[cfg(test)]` fixture setup.
    "src/coordinator.rs",
    "src/planner/contract_suite.rs", // #[cfg(test)] git fixtures for the #146 pipeline suite
    "src/planner/coordination_suite.rs", // #[cfg(test)] git fixtures for the #60 coordination suite
    "src/planner/lifecycle_suite.rs", // #[cfg(test)] git fixtures for the #61 lifecycle suite
    // #[cfg(test)] git fixtures for the M2.20c (#229) fetch suite: building
    // the repository + its in-tree bare remote, and one deliberate
    // *unsandboxed* `git fetch` that asserts the redaction test's premise (the
    // fixture really does leak a credential when nothing redacts it).
    "src/planner/fetch_suite.rs",
    // #[cfg(test)] git fixtures for the M2.20d (#230) pull suite: the diverged
    // and conflicting repositories plus their in-tree bare remote, and the
    // read-only `rev-parse`/`rev-list`/`merge-base`/`ls-files` calls that make
    // the *repository* the referee for what a pull did — deliberately plain
    // git, outside the harness under test, so an assertion can never be
    // satisfied by the same code it is checking.
    "src/planner/pull_suite.rs",
    // #[cfg(test)] git fixtures for the M2.20e (#231) push suite: the served
    // repository, its bare remote and the `git daemon` that serves it over
    // `git://` (a path remote cannot receive a push under the sandbox), the
    // third-party clone whose push makes a lease lose, and the deliberately
    // *unsandboxed* `git push` that asserts the redaction test's premise. Every
    // read that decides whether an assertion passes — the remote's
    // `for-each-ref`, `rev-parse` and `merge-base --is-ancestor` — is plain git
    // outside the harness under test, so no assertion can be satisfied by the
    // same code it is checking.
    "src/planner/push_suite.rs",
    // #[cfg(test)] git fixtures for the remote-target boundary suite (ADR
    // 0047): a repository, an in-tree bare target the server must refuse to
    // fetch from, and `git remote add` for the paired positive control.
    "src/planner/remote_boundary_suite.rs",
    // #[cfg(test)] git fixtures for the #72 (M2.19) hook-timeout suite: plain
    // `git init`/`commit`/`rev-list --count` to build and inspect fixture
    // repositories, deliberately outside the sandboxed harness under test —
    // the same "referee is not the code being checked" posture `pull_suite`
    // and `push_suite` document above.
    "src/planner/hook_timeout_suite.rs",
    // #[cfg(test)] git fixtures for the #327 defect B revert-conflict suite:
    // plain `git init`/`commit`/`rev-parse` to build fixture repositories,
    // outside the sandboxed harness under test.
    "src/planner/revert_suite.rs",
    // #[cfg(test)] git fixtures for the M2.21d/e (#238/#239) tag-argv-shape
    // and signed-tag-execution suite: plain `git init`/`commit`/`rev-parse`,
    // plus the failed-signing attempt's own `git rev-parse --verify` check
    // that no tag was left behind.
    "src/planner/tag_signing_suite.rs",
    // #[cfg(test)] git fixtures for the #145 staleness-contract suite:
    // plain `git init`/`commit`/`add`/`branch`/`remote` to build and drift
    // fixture repositories, outside the sandboxed harness under test.
    "src/planner/staleness_suite.rs",
    // #[cfg(test)] git fixtures for the #214 (M2.17c) hunk/line-staging
    // suite: plain `git init`/`commit`/`add`/`diff` to build fixture
    // repositories and read back their state, outside the sandboxed harness
    // under test.
    "src/planner/hunk_staging_suite.rs",
    // #[cfg(test)] git fixtures for the M2.19a/b (#222/#223) amend/commit
    // failure-classification suite: plain `git init`/`commit`/`add` to build
    // fixture repositories, outside the sandboxed harness under test.
    "src/planner/commit_classification_suite.rs",
    // #[cfg(test)] git fixtures for the M2.20a/M2.21a/f (#227/#235/#240)
    // remote-operation-shape suite: plain `git init`/`commit`, plus a real
    // in-tree bare remote (`git init --bare`) so `RemoteConfigured`
    // preconditions genuinely hold.
    "src/planner/remote_operation_shape_suite.rs",
    "src/state.rs",         // #[cfg(test)] fixture setup
    "src/argv_boundary.rs", // this file (the scan reads its own source)
    // The M1.13b spawn chokepoint (#66, Task 5). It builds a git Command from
    // `sandbox_argv(policy)`, so `argv[0]` is the shim (or bare `git` in the
    // unsandboxed tier), never a literal chosen here. Also in
    // LAUNCHER_SPAWN_SITES: it is the whole point of the sandbox that this is
    // where git is spawned, and Task 6 routes the existing sites through it.
    "src/sandbox/spawn.rs",
    // The M1.13b sandbox shim (#66). It is the *blessed launcher*: the one
    // process that applies Landlock and seccomp and then replaces its own image
    // with git. It qualifies for this list more strongly than most entries —
    // it names `git` literally AND its `validate()` refuses, with exit 90, any
    // argv whose program is not exactly `git`, so it cannot exec anything else
    // even if the literal rule below were relaxed. It uses `.exec()`, never
    // `.spawn()`/`.output()`/`.status()`: it never becomes a parent.
    "src/bin/gv-sandbox/main.rs",
    // (An orphaned paragraph describing `shim_cli.rs` — "the `#[cfg(test)]`
    // harness that drives the composed launcher" — used to sit here, left
    // behind when that entry was removed. `shim_cli.rs` constructs no
    // `Command`: it composes argv and hands it to `sandbox::spawn`. A comment
    // with no entry under it is worse than none, because the next reader
    // attaches it to whichever entry follows.)
    // The `#[cfg(test)]` escape battery. It launches the composed launcher via
    // shim_cli, and separately runs the C compiler to build the adversarial
    // probes it feeds in as hostile hooks. Also in LAUNCHER_SPAWN_SITES.
    "src/sandbox/escape_suite.rs",
    // #188: the real-SSH end-to-end fixture. `#[cfg(test)]` only. It spawns
    // `ssh-keygen`, `sshd`, `ssh-agent`, `ssh-add` and plain `git` to build a
    // throwaway loopback SSH server and drive a real `ls-remote` against it
    // — every one of those, unsandboxed setup, never the thing under test
    // (which spawns through `spawn::command_async` like everything else).
    // Also in LAUNCHER_SPAWN_SITES, for the same reason `cc` is carved out
    // for escape_suite.rs above: none of the five is a shell.
    "src/sandbox/ssh_remote.rs",
    // #66 Task 25 (step 3): the anti-vacuity contract's harness. Its baseline
    // leg spawns plain `git` outside the sandbox (literal), and its CI
    // preflight (`ci_preflight_host_meets_the_declared_minimum`) runs `cc
    // --version` to confirm the compiler the escape battery needs is present
    // — it never compiles or execs anything client-influenced. Also in
    // LAUNCHER_SPAWN_SITES for the same `cc` reason as escape_suite.rs above.
    "src/sandbox/escape_contract.rs",
    // The live-clone check for `policy_for_clone`. `#[cfg(test)]` and
    // `#[ignore]`d. It constructs exactly one `Command`: the **baseline** leg,
    // a plain unsandboxed `git clone` of a hardcoded literal public HTTPS URL,
    // which exists so that a failure of the sandboxed leg is attributable to
    // the policy rather than to the network. The leg under test spawns through
    // `spawn::command_async` like every other sandboxed git. Program is the
    // literal `"git"`; no argument is client-influenced.
    "src/sandbox/clone_live.rs",
    // #228 (M2.20b): the shared Network-tier exec harness. Its production
    // function (`network_command`) builds no `Command` of its own — it
    // calls `spawn::command_async`, the same chokepoint every other spawn
    // site in this crate already goes through. The only
    // `Command::new` literal in this file is `#[cfg(test)]` fixture setup
    // (`run()`, `git init`/`git config` for the askpass/credential-helper
    // fixtures) — same posture as `repo_paths.rs`/`hostile.rs` above.
    "src/sandbox/network_exec.rs",
    // Deliberately absent: `src/sandbox/documented_gaps.rs` and
    // `src/sandbox/lifecycle.rs`, the suites #66's Task 15 round adds beside
    // this one. `documented_gaps.rs` exists and constructs no `Command` at all,
    // so an entry for it would be a permission with nothing behind it;
    // `lifecycle.rs` did not exist when this list was last reviewed, so nobody
    // has read the spawn it may or may not contain. Neither was pre-added on
    // purpose — see `every_allowlist_entry_names_a_live_spawn_site` below for
    // why an entry written ahead of its file is the failure mode this list is
    // supposed to prevent, not a convenience.
    //
    // #63's paged-history module. `#[cfg(test)]` fixture setup only: its two
    // `Command::new("git")` sites are the suite's private `run_env`/`out`
    // helpers, below `mod tests`.
    //
    // **This entry is new, and why it was missing is the point.** The list used
    // to be one flat set of crate-relative paths shared by both crates, and
    // `git-vista-git`'s `src/history.rs` has been on it since long before this
    // module existed. When #63 added a *second* `src/history.rs` under this
    // crate, the scan stripped the same relative path off it, found that path
    // already allowlisted, and passed — so a spawn site in the server crate was
    // authorised by a review of a different crate's file, and stayed that way
    // from 2026-07-26 until the liveness guard below turned the collision up.
    // The lists are per-crate now, so one crate can no longer bless another's
    // same-named file, and this entry records the review that never happened.
    "src/history.rs",
];

/// The same list for the **git-vista-git** crate.
///
/// Kept separate from [`ALLOWED_SPAWN_SITES`] rather than merged into it
/// because the entries are crate-*relative*. A single shared list made
/// `src/history.rs` mean whichever crate's file the scan happened to be
/// walking, which is exactly how this workspace ended up with an unreviewed
/// spawn site: #63 added `git-vista-server/src/history.rs`, and the entry
/// written years-of-commits earlier for `git-vista-git/src/history.rs` covered
/// it silently. Two lists, one per root, makes that collision unrepresentable.
const ALLOWED_GIT_CRATE_SPAWN_SITES: &[&str] = &[
    // `#[cfg(test)]` fixture setup only. This entry used to read "read-side
    // reflog/stash reads, static args"; that is stale — the crate's production
    // history reads (`walk_history`, `read_commit`, `remote_membership`,
    // `read_remote_commits`) do not spawn a process at all, and every
    // `Command::new` left in the file is below its `#[cfg(test)] pub(crate) mod
    // tests` line, building real repositories for the suites to read.
    "src/history.rs",
    // M2.21b (#236): `#[cfg(test)]` fixture setup only, same posture as
    // `src/history.rs` above. `read_tags` itself spawns nothing — it opens the
    // repository with `gix::open_opts(.., isolated())` and decodes each tag
    // object out of the mapped object database — so the file's only
    // `Command::new` sites build tagged fixture repositories (`git tag`,
    // `git mktag`, `git pack-refs`) for the suite to read back.
    "src/tags.rs",
    // M3.24 (#77): `#[cfg(test)]` fixture setup only, same posture as the two
    // above. `read_stashes` spawns nothing — it opens the repository with
    // `gix::open_opts(.., isolated())` and walks `refs/stash`'s reflog out of
    // the mapped ref store. The file's only `Command::new` sites build stashed
    // fixture repositories for the suite to read back, including two that pin
    // properties of *git itself* (one commit can occupy two stash slots;
    // dropping renumbers the entries below) rather than of this code.
    "src/stash.rs",
];

/// The one carve-out from "every spawn site names `git` literally": sites that
/// launch the **sandbox launcher** rather than git itself.
///
/// The literal rule exists so no spawn site can be talked into running a
/// program chosen at runtime. A launcher site necessarily breaks it — the
/// program it runs is `Policy::shim`, a path rather than a literal — so the
/// rule is replaced here by a narrower one, asserted in
/// `launcher_sites_name_no_interpreter` below: a launcher site may name a
/// non-literal program, but it must never name a shell or interpreter.
///
/// Keep this list as short as possible. Every addition widens the only hole in
/// the tripwire. It stands at four: one production chokepoint (`spawn.rs`) and
/// three `#[cfg(test)]` harnesses that need a non-git external tool. (This doc
/// used to say "keep this list at one entry", written when it held one;
/// leaving that in while the list grew turned a live budget into a slogan
/// nobody could act on.)
const LAUNCHER_SPAWN_SITES: &[&str] = &[
    // (As in `ALLOWED_SPAWN_SITES` above, an orphaned paragraph about
    // `shim_cli.rs` was left here when that entry was removed. It is gone: the
    // file constructs no `Command`.)
    // The `#[cfg(test)]` escape battery. It launches the composed launcher and
    // also runs `cc` to compile the adversarial probes it feeds in as hooks.
    // `cc` is not an interpreter of *its* arguments the way a shell is — it
    // compiles a source file this test wrote — so it is permitted here while
    // shells remain forbidden.
    "src/sandbox/escape_suite.rs",
    // The contract harness's CI preflight runs `cc --version` (a capability
    // check, not a compile). Its inside leg spawns through
    // `spawn::command_async` — a real `Command` (`tokio::process::Command`),
    // but built inside `spawn.rs`, not by a `Command::new(` in this file, so
    // it does not itself need this carve-out for that path.
    "src/sandbox/escape_contract.rs",
    // #188's real-SSH fixture. `ssh-keygen`, `sshd`, `ssh-agent` and `ssh-add`
    // are each a fixed-purpose tool that does not reinterpret an argument as a
    // second command the way a shell does — `sshd`'s own path is chosen from
    // a small, fixed, reviewed candidate list at runtime
    // (`["/usr/sbin/sshd", "/usr/bin/sshd"]`), the identical pattern
    // `BWRAP_CANDIDATES` already uses for the strict tier's launcher, never
    // resolved through `PATH`.
    "src/sandbox/ssh_remote.rs",
    // The Task 5 spawn chokepoint. Its `Command::new` program is
    // `sandbox_argv(policy)[0]` — the resolved shim, or bare `git` in the
    // unsandboxed tier — a value this crate produced, never a runtime string.
    "src/sandbox/spawn.rs",
];

/// A launcher site may name a non-literal program, but never a **shell**.
///
/// The literal-`git` rule exists so no spawn site can be talked into running a
/// program chosen at runtime; a shell is the sharpest form of that, because it
/// re-interprets a string as a command. A launcher site is exempt from the
/// literal rule (it runs `Policy::shim`, a resolved path) but must not reopen
/// the shell hole. `cc` is deliberately *not* on this list: it compiles a file,
/// it does not interpret an argument as a command, and the escape battery needs
/// it to build the C probes that make the seccomp assertions real.
#[test]
fn launcher_sites_name_no_shell() {
    let server_root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    let spawn = ["Command", "::new("].concat();
    for rel in LAUNCHER_SPAWN_SITES {
        let path = server_root.join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("launcher site {rel} must exist"));
        assert!(
            text.contains(&spawn),
            "{rel} is listed as a launcher site but constructs no Command; \
             remove it from LAUNCHER_SPAWN_SITES rather than leaving the \
             carve-out open"
        );
        for shell in [
            "\"sh\"",
            "\"bash\"",
            "\"/bin/sh\"",
            "\"/bin/bash\"",
            "\"zsh\"",
            "\"env\"",
        ] {
            let needle = [&spawn, shell].concat();
            assert!(
                !text.contains(&needle),
                "{rel}: a launcher site must never name a shell ({shell})"
            );
        }
    }
}

pub(crate) fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Layer 1: the tripwire. Walk both native crates' sources; every
/// `Command::new` must sit in an allowlisted file and name `git` literally.
/// (The needles are assembled at runtime so this file's own source never
/// contains the bare pattern it scans for.)
#[test]
fn every_process_spawn_site_is_allowlisted_and_spawns_only_git() {
    let server_root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    let git_root = server_root.parent().unwrap().join("git-vista-git");
    let spawn = ["Command", "::new("].concat();
    let spawn_git = ["Command", "::new(\"git\")"].concat();

    // One allowlist per crate root. A single shared list would let an entry
    // reviewed for one crate authorise a same-named file in the other — see
    // `ALLOWED_GIT_CRATE_SPAWN_SITES` for the time that actually happened.
    for (root, allowed) in [
        (&server_root, ALLOWED_SPAWN_SITES),
        (&git_root, ALLOWED_GIT_CRATE_SPAWN_SITES),
    ] {
        let mut files = Vec::new();
        rs_files(&root.join("src"), &mut files);
        assert!(
            !files.is_empty(),
            "source scan found no files under {root:?}"
        );
        for file in files {
            let text = std::fs::read_to_string(&file).expect("readable source file");
            let hits = text.matches(&spawn).count();
            if hits == 0 {
                continue;
            }
            let rel = file
                .strip_prefix(root)
                .expect("file under crate root")
                .to_string_lossy()
                .replace('\\', "/");
            assert!(
                allowed.contains(&rel.as_str()),
                "NEW PROCESS-SPAWN SITE: {rel} (in {root:?}) constructs a Command \
                 but is not allowlisted in argv_boundary.rs. Review it — a \
                 mutating git argv belongs in the planner's executor, nowhere \
                 else. Note the allowlists are per-crate: an entry for the same \
                 path in the *other* crate's list does not cover this file."
            );
            // This file talks *about* spawning without doing it; every other
            // allowlisted site must spawn `git` literally — no shells, no
            // dynamically chosen program names.
            if rel != "src/argv_boundary.rs" && !LAUNCHER_SPAWN_SITES.contains(&rel.as_str()) {
                assert_eq!(
                    text.matches(&spawn_git).count(),
                    hits,
                    "{rel}: a Command::new site does not name \"git\" literally"
                );
            }
        }
    }
}

/// The allowlist is a **review record, not a standing permission**. Every entry
/// must name a file that exists and that actually constructs a `Command`.
///
/// This is the `clone.rs` lesson mechanised. That entry sat on the list after
/// its raw `git clone` had migrated to the sealed launcher, and the only thing
/// that removed it was a human noticing — in the one handler that fetches
/// attacker-chosen content. The scan above structurally *cannot* catch it: it
/// consults the list only for files that already spawn, so a dead entry is
/// invisible to it and silently pre-blesses whatever raw spawn appears in that
/// file next.
///
/// It also settles the question that comes up every time a new suite is written
/// beside this one: may an entry be added *ahead* of its file, so the scan does
/// not go red the moment that file lands? No — this test makes such an entry
/// fail, on purpose. A spawn site is allowlisted after it has been read, never
/// before it exists. If a new file lands carrying a git fixture, one red run is
/// the tripwire doing its job; the fix is to read the file and add the entry
/// with its reason, which is the review this list exists to record.
#[test]
fn every_allowlist_entry_names_a_live_spawn_site() {
    let server_root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    let git_root = server_root.parent().unwrap().join("git-vista-git");
    let spawn = ["Command", "::new("].concat();

    /// Why an entry is not a live spawn site, or `None` if it is. Factored out
    /// of the loop so the same predicate can be pointed at deliberately bad
    /// entries below: a guard that has never been shown to reject anything is
    /// the kind of green test this milestone keeps having to disown.
    fn why_dead(root: &Path, rel: &str, spawn: &str) -> Option<String> {
        let path = root.join(rel);
        if !path.is_file() {
            return Some(format!("no such file under {}", root.display()));
        }
        let text = std::fs::read_to_string(&path).ok()?;
        (!text.contains(spawn)).then(|| "constructs no Command".to_string())
    }

    for (root, allowed) in [
        (&server_root, ALLOWED_SPAWN_SITES),
        (&git_root, ALLOWED_GIT_CRATE_SPAWN_SITES),
    ] {
        assert!(
            !allowed.is_empty(),
            "an empty allowlist would make this test pass over nothing"
        );
        for (i, rel) in allowed.iter().enumerate() {
            assert!(
                !allowed[..i].contains(rel),
                "{rel} is listed twice in {root:?}'s allowlist; two reviews of \
                 one file cannot disagree usefully, and a duplicate hides which \
                 comment is current"
            );
            assert_eq!(
                why_dead(root, rel, &spawn),
                None,
                "{rel} is allowlisted for {root:?} but is not a live spawn site. \
                 Remove the entry rather than leaving it: the scan above only \
                 consults this list for files that already spawn, so a dead entry \
                 grants a permission that stays invisible until someone adds a \
                 raw spawn to that file."
            );
        }
    }

    // The predicate rejects both failure modes it claims to. Without this, the
    // loop above is only evidence that `why_dead` returns `None` a lot.
    assert!(
        why_dead(
            &server_root,
            "src/sandbox/a-file-that-does-not-exist.rs",
            &spawn
        )
        .is_some(),
        "an entry naming a nonexistent file must be rejected — this is what \
         stops an allowlist entry being written ahead of the file it blesses"
    );
    assert!(
        why_dead(&server_root, "src/main.rs", &spawn).is_some(),
        "an entry naming a real file that constructs no Command must be \
         rejected. (If `main.rs` ever starts spawning, this line will fail and \
         it will be pointing at a genuine new spawn site, not at itself.)"
    );

    // The launcher carve-out is a *narrowing* of the main list, never a way
    // around it: a file exempted from the literal-`git` rule that was not on
    // the main allowlist would be flagged by the scan above anyway, so an entry
    // here that is missing there means one of the two lists has drifted.
    for rel in LAUNCHER_SPAWN_SITES {
        assert!(
            ALLOWED_SPAWN_SITES.contains(rel),
            "{rel} carries the launcher carve-out but is not on the main \
             allowlist; the carve-out narrows that list, it does not replace it"
        );
    }
}

/// INV-16, the value half asserted **exhaustively over the tier enum**: every
/// argv `sandbox_argv` can produce is one of exactly three shapes, and in the
/// sandboxed shapes the tail is `-- git` with `git` named exactly once.
///
/// `sandbox::argv` already pins each shape against its reviewed constants. What
/// this adds — and the reason it lives in the tripwire file rather than beside
/// those — is coverage that cannot silently miss a case, plus the negative
/// space:
///
///  * The tiers are walked through an exhaustive `match` (`next_tier`), so a
///    new `Tier` variant is a **compile error here** rather than an untested
///    shape. `sandbox::argv` iterates hand-written arrays, which a fourth tier
///    would slip past without a word.
///  * Both hook modes are crossed with every tier. `HookMode::Blocked` is the
///    state a host that failed INV-13 drops to, and the `Unsandboxed`/`Blocked`
///    corner is the one that already shipped broken once (a bare `["git"]` that
///    ran hooks a policy said were blocked).
///  * `git` must appear **exactly once** in a sandboxed argv, as the last
///    entry, and the element before `-- git` must be a reviewed policy flag or
///    its value. Together those say nothing can be appended between the
///    reviewed prefix and the program, and no second program name can ride
///    along — neither of which follows from checking the tail alone.
#[test]
fn sandbox_argv_is_one_of_inv16s_three_shapes_in_every_tier() {
    use crate::sandbox::{sandbox_argv, HookMode, Policy, Tier, DEFAULT_GIT_PORTS};

    /// The tier walk. Exhaustive on purpose: adding a `Tier` variant will not
    /// compile until it is given an arm, and linking it into the chain is what
    /// puts it under every assertion below.
    fn next_tier(tier: Tier) -> Option<Tier> {
        match tier {
            Tier::Strict => Some(Tier::Network),
            Tier::Network => Some(Tier::Unsandboxed),
            Tier::Unsandboxed => None,
        }
    }

    const SHIM: &str = "/opt/gv/gv-sandbox";
    const HOOK_DIR: &str = "/var/lib/gv/no-hooks";

    let policy = |tier: Tier, hook_mode: HookMode| Policy {
        tier,
        shim: PathBuf::from(SHIM),
        // Fake but absolute, like `sandbox::argv`'s: these assertions are about
        // shape, and pinning to wherever bwrap really lives would make them
        // pass or fail for reasons unrelated to the chokepoint.
        bwrap: (tier == Tier::Strict).then(|| PathBuf::from("/usr/bin/bwrap")),
        rw_trees: vec![PathBuf::from("/srv/repos/r")],
        ro_trees: vec![PathBuf::from("/usr"), PathBuf::from("/home/tom")],
        secret_excludes: vec![PathBuf::from("/home/tom/.ssh")],
        // #188 is out of scope for this test (it pins the three INV-16
        // argv shapes across every tier/hook-mode combination, not any one
        // flag's contents) — empty in every tier here on purpose.
        ro_carveouts: Vec::new(),
        net_ports: if tier == Tier::Network {
            DEFAULT_GIT_PORTS.to_vec()
        } else {
            Vec::new()
        },
        hook_mode,
    };

    let mut tiers = Vec::new();
    let mut cursor = Some(Tier::Strict);
    while let Some(tier) = cursor {
        assert!(!tiers.contains(&tier), "the tier walk loops: {tier:?}");
        tiers.push(tier);
        cursor = next_tier(tier);
    }
    assert_eq!(
        tiers.len(),
        3,
        "the tier census drifted — {} tiers walked, three shapes documented in \
         `sandbox_argv`'s INV-16 comment. Either a tier was added without a \
         shape, or the walk lost one.",
        tiers.len()
    );

    for tier in tiers {
        for hook_mode in [
            HookMode::Run,
            HookMode::Blocked {
                empty_dir: PathBuf::from(HOOK_DIR),
            },
        ] {
            let blocked = matches!(hook_mode, HookMode::Blocked { .. });
            let argv: Vec<String> = sandbox_argv(&policy(tier, hook_mode))
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let what = format!("{tier:?}/blocked={blocked}");

            if tier == Tier::Unsandboxed {
                // Shapes 1 and 2. `git` leads here — there is no launcher in
                // front of it — and nothing may follow but the hook
                // suppression, which is the only argument this tier is allowed
                // to add.
                let expected: Vec<String> = if blocked {
                    vec![
                        "git".into(),
                        "-c".into(),
                        format!("core.hooksPath={HOOK_DIR}"),
                    ]
                } else {
                    vec!["git".into()]
                };
                assert_eq!(
                    argv, expected,
                    "{what}: the unsandboxed argv is not one of INV-16's two \
                     unsandboxed shapes"
                );
                continue;
            }

            // Shape 3: a reviewed prefix, then `-- git`.
            assert!(
                argv.len() > 2,
                "{what}: a sandboxed argv is a prefix *and* `-- git`, got {argv:?}"
            );
            assert_eq!(
                &argv[argv.len() - 2..],
                ["--".to_string(), "git".to_string()],
                "{what}: the launcher argv must end in `-- git`"
            );
            assert_eq!(
                argv.iter().filter(|s| *s == "git").count(),
                1,
                "{what}: `git` appears more than once — the program name must be \
                 the single last entry, or a reviewer cannot tell which one runs"
            );
            assert!(
                Path::new(&argv[0]).is_absolute(),
                "{what}: the launcher must be an absolute path ({}); a bare name \
                 resolves against the inherited PATH",
                argv[0]
            );
            assert_eq!(
                argv.iter().filter(|s| *s == SHIM).count(),
                1,
                "{what}: the shim must appear exactly once in the argv"
            );

            // Nothing sits between the reviewed policy flags and `-- git`. The
            // last prefix element is either the net decision or, in the network
            // tier, the final `--net-port` value.
            let terminator = &argv[argv.len() - 3];
            assert!(
                terminator == "--net-deny"
                    || terminator == "--net-allow"
                    || terminator.parse::<u16>().is_ok(),
                "{what}: `{terminator}` was appended after the reviewed policy \
                 flags and before `-- git`; every argument the shim reads must \
                 sit inside the reviewed prefix"
            );

            // Exactly one hook decision reaches the shim. Neither is silence.
            let hook_flags = argv
                .iter()
                .filter(|s| *s == "--hooks-run" || *s == "--hooks-blocked")
                .count();
            assert_eq!(
                hook_flags, 1,
                "{what}: expected exactly one hook decision in the argv, found \
                 {hook_flags}"
            );
            assert_eq!(
                argv.iter().any(|s| s == "--hooks-blocked"),
                blocked,
                "{what}: the argv's hook decision contradicts the policy's"
            );

            // And no interpreter anywhere in it — the same rule
            // `launcher_sites_name_no_shell` applies to the source, applied to
            // the value, because a path that arrived from a policy field is not
            // covered by a source scan.
            for bad in ["sh", "bash", "/bin/sh", "/bin/bash", "zsh", "env", "-c"] {
                assert!(
                    !argv.iter().any(|s| s == bad),
                    "{what}: `{bad}` must never appear in a launcher argv"
                );
            }
        }
    }
}

/// The shim `exec`s and **never forks**.
///
/// `gv-sandbox`'s module doc has asserted this in prose since it was written —
/// "this file must contain `.exec()` and must not contain `.spawn()`,
/// `.output()` or `.status()`" — and named this file as the place that proves
/// it. Nothing did. That prose was the entire guarantee, which is the shape of
/// claim this milestone has been burned by five times.
///
/// It matters because the shim's containment is *inherited through the exec*.
/// Landlock and seccomp are applied to the shim's own process and survive
/// `execve`; they would equally be inherited by a forked child, but a fork
/// gives the shim a second life — it stays resident as a parent, with an
/// argv it has already validated, in a process that could then exec something
/// else. `execve` is what makes the validation final: after it, there is no
/// gv-sandbox process left to run anything, only git wearing its restrictions.
///
/// Scanned on [`code_only`] output rather than raw source, so the module doc's
/// own mention of `.spawn()` is not counted as a use of it — the mistake that
/// makes a prose-driven scan report the file it is quoting.
#[test]
fn the_shim_execs_and_never_forks() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/gv-sandbox/main.rs");
    let src = std::fs::read_to_string(&path).expect("readable gv-sandbox main.rs");
    let code = code_only(&src);

    let spawn = ["Command", "::new("].concat();
    let spawn_git = ["Command", "::new(\"git\")"].concat();

    // Exactly one command is built, and it names `git` literally. The literal
    // is checked against the raw source because `code_only` blanks the contents
    // of string literals — the count is checked against code so a comment
    // quoting the pattern cannot inflate it.
    assert_eq!(
        code.matches(&spawn).count(),
        1,
        "the shim must construct exactly one Command; a second one is a second \
         thing it could exec"
    );
    assert_eq!(
        src.matches(&spawn_git).count(),
        1,
        "the shim's one Command must name `git` literally"
    );

    // It replaces its own image.
    let exec = [".exec", "()"].concat();
    assert_eq!(
        code.matches(&exec).count(),
        1,
        "the shim must `{exec}` exactly once — that call is what makes the \
         validated argv final"
    );
    assert!(
        code.contains("use std::os::unix::process::CommandExt"),
        "`{exec}` comes from `CommandExt`; if that import is gone, the call \
         above is not the exec this test thinks it is"
    );

    // It never becomes a parent.
    for (needle, why) in [
        (
            [".spawn", "()"].concat(),
            "forks a child and leaves the shim resident as its parent",
        ),
        (
            [".output", "()"].concat(),
            "forks a child and waits on it; the shim never waits on anything",
        ),
        (
            [".status", "()"].concat(),
            "forks a child and waits on it; the shim never waits on anything",
        ),
        (
            ["fork", "("].concat(),
            "duplicates the shim process outright",
        ),
        (
            ["daemon", "("].concat(),
            "forks and detaches — the shim must not outlive its exec",
        ),
    ] {
        assert_eq!(
            code.matches(needle.as_str()).count(),
            0,
            "gv-sandbox/main.rs: `{needle}` {why}. The shim applies Landlock and \
             seccomp to *itself* and then becomes git; anything that keeps it \
             alive as a parent keeps a validated-argv process around to exec \
             again."
        );
    }
}

/// Blank out comments and the *contents* of string/char literals, so a
/// structural scan of source text sees code and nothing else. Delimiters and
/// newlines are kept, so offsets stay meaningful and a blanked region never
/// merges two lines together.
///
/// Without this, a prose sentence in a comment ("we no longer call
/// `git_stdout(`…") would be counted as a call site — and a brace inside a
/// string or comment would desynchronise the body extractor.
/// `#66` Task 25 (step 3) promotes this from private to `pub(crate)` so
/// `sandbox::escape_contract`'s tripwires can reuse the same comment/string
/// blanking this file's own scans rely on, rather than re-implementing it and
/// risking the two copies drifting apart.
pub(crate) fn code_only(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    fn blank(out: &mut String, ch: char) {
        out.push(if ch == '\n' { '\n' } else { ' ' });
    }
    let mut i = 0usize;
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        // Line comment: blank to (not including) the newline.
        if ch == '/' && next == Some('/') {
            while i < c.len() && c[i] != '\n' {
                blank(&mut out, c[i]);
                i += 1;
            }
            continue;
        }
        // Block comment, nesting as Rust's do.
        if ch == '/' && next == Some('*') {
            let mut depth = 0usize;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    blank(&mut out, c[i]);
                    blank(&mut out, c[i + 1]);
                    i += 2;
                    continue;
                }
                if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    blank(&mut out, c[i]);
                    blank(&mut out, c[i + 1]);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                blank(&mut out, c[i]);
                i += 1;
            }
            continue;
        }
        // Raw string: r"…", r#"…"#, r##"…"##. Only when `r` starts a token.
        let prev_is_ident = i > 0 && (c[i - 1].is_alphanumeric() || c[i - 1] == '_');
        if ch == 'r' && !prev_is_ident {
            let mut hashes = 0usize;
            while c.get(i + 1 + hashes) == Some(&'#') {
                hashes += 1;
            }
            if c.get(i + 1 + hashes) == Some(&'"') {
                out.push('r');
                for _ in 0..hashes {
                    out.push('#');
                }
                out.push('"');
                i += hashes + 2;
                loop {
                    if i >= c.len() {
                        break;
                    }
                    if c[i] == '"' && (1..=hashes).all(|h| c.get(i + h) == Some(&'#')) {
                        out.push('"');
                        for _ in 0..hashes {
                            out.push('#');
                        }
                        i += hashes + 1;
                        break;
                    }
                    blank(&mut out, c[i]);
                    i += 1;
                }
                continue;
            }
        }
        // Ordinary string literal, honouring backslash escapes.
        if ch == '"' {
            out.push('"');
            i += 1;
            while i < c.len() {
                if c[i] == '\\' {
                    blank(&mut out, c[i]);
                    if i + 1 < c.len() {
                        blank(&mut out, c[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                if c[i] == '"' {
                    out.push('"');
                    i += 1;
                    break;
                }
                blank(&mut out, c[i]);
                i += 1;
            }
            continue;
        }
        // `'` is a char literal only when it closes within two chars; otherwise
        // it is a lifetime (`&'a str`) and must be passed through untouched.
        if ch == '\'' {
            let escaped = next == Some('\\');
            let closes = if escaped {
                (2..=8).find(|&k| c.get(i + k) == Some(&'\''))
            } else if c.get(i + 2) == Some(&'\'') {
                Some(2)
            } else {
                None
            };
            if let Some(k) = closes {
                out.push('\'');
                for j in 1..k {
                    blank(&mut out, c[i + j]);
                }
                out.push('\'');
                i += k + 1;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// The body of the one **production** `fn <name>` in `code` (already passed
/// through [`code_only`]), matched brace-for-brace.
///
/// Deliberately strict: exactly one definition must exist, and it must sit
/// ahead of `mod tests`, so a same-named test helper can neither be picked up
/// instead of the real thing nor make the scan ambiguous.
fn production_body<'a>(code: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}");
    let defs = code.matches(&marker).count();
    assert_eq!(
        defs, 1,
        "expected exactly one `{marker}` definition in handlers/read.rs, found {defs}"
    );
    let at = code.find(&marker).expect("counted above");
    // The ordering check exists to stop a same-named *test helper* being read
    // instead of the real definition. Once every test moved out into
    // `handlers/read/*_suite.rs` child modules there is no inline `mod tests`
    // left, so there is no helper to confuse it with and the guard is
    // vacuously satisfied — the `defs == 1` assertion above already pins
    // uniqueness. Kept conditional rather than deleted so the check comes back
    // the moment an inline test module reappears.
    if let Some(tests_at) = code.find("mod tests") {
        assert!(
            at < tests_at,
            "`{marker}` was found inside `mod tests`, not in production code"
        );
    }

    let open = at
        + code[at..]
            .find('{')
            .expect("a function signature is followed by its body brace");
    let mut depth = 0usize;
    for (offset, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let body = &code[open + 1..open + offset];
                    assert!(
                        body.len() > 200,
                        "extracted body for `{marker}` is implausibly small ({} bytes)",
                        body.len()
                    );
                    return body;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces while extracting `{marker}`");
}

/// Layer 1b (M1.10, #63; collapsed to one file spawn in #221): the
/// *streaming* source boundary. Every git read the two bounded read handlers
/// perform must go through a primitive that owns its child process end to
/// end and bounds what it reads — proved structurally, on the source, not
/// inferred from the size of a returned buffer.
///
/// Exactly one production body is extracted for each of `commit_diff_for_repo`
/// and `file_at_commit_for_repo`; across only those two bodies there must be
/// exactly four such calls: three `git_stdout_capped(` (the diff's
/// `--name-status`, `--numstat` and `--patch` reads) plus exactly one
/// `git_cat_file_batch(` (#221: the file read's single `cat-file --batch`
/// spawn, which does the #168 type check and, when it resolves to a blob,
/// the content read, on the one still-open process — including through the
/// `<id>^:<path>` parent-fallback). And no escape hatch — no uncapped
/// `git_stdout(`, no `.output()`, no `.wait_with_output()`, no direct
/// `Command` construction, each of which would buffer whatever git chose to
/// print.
///
/// `file_at_commit_for_repo` went from one call site to two in #168 (a
/// `git cat-file -t <spec>` type check, through the same capped primitive,
/// ran before the `git show` content read) and from two back down to *one*
/// in #221: the type check and the content read are now two possible facts
/// read off one `cat-file --batch` response stream, so a tree or submodule
/// entry is still rejected without ever reading (or serving) content bytes —
/// enforced by the wire's own field order rather than by two separate
/// spawns. See that function's doc comment, and `git_cat_file_batch`'s in
/// `git_cmd.rs`.
///
/// The scope is deliberately narrow. The unrelated `worktree_status` read in
/// the very same file legitimately buffers a whole (tiny, static-arg) git
/// output — since Task 6 through the sealed `git_cmd::git_output` helper rather
/// than a raw `Command` — and the assertion below that the *file* still
/// contains that call while the two extracted *bodies* do not is what proves
/// the extractor cut where it claims to, instead of quietly matching nothing.
#[test]
fn bounded_read_source_boundary_is_streaming_and_exactly_four() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handlers/read.rs");
    let src = std::fs::read_to_string(&path).expect("readable handlers/read.rs");
    let code = code_only(&src);

    let capped = ["git_stdout", "_capped("].concat();
    let batched = ["git_cat_file", "_batch("].concat();
    let uncapped = ["git_stdout", "("].concat();
    // Assembled at runtime, like the spawn scan above, so this file's own source
    // never contains the bare patterns it forbids.
    let banned: [(String, &str); 4] = [
        ([".output", "()"].concat(), "buffers all of git's stdout"),
        (
            [".wait_with", "_output()"].concat(),
            "buffers all of git's stdout",
        ),
        (
            ["Command", "::new"].concat(),
            "spawns git outside the capped primitive",
        ),
        (
            ["git_out", "put("].concat(),
            "buffers all of git's stdout (the sealed helper is for small, \
             fixed-size reads, never for these streams)",
        ),
    ];

    let diff_body = production_body(&code, "commit_diff_for_repo");
    let file_body = production_body(&code, "file_at_commit_for_repo");

    // The two bodies are distinct regions of the same file.
    assert_ne!(
        diff_body.as_ptr(),
        file_body.as_ptr(),
        "the extractor returned the same body twice"
    );

    let diff_calls = diff_body.matches(&capped).count();
    assert_eq!(
        diff_calls, 3,
        "commit_diff_for_repo must perform exactly three bounded reads \
         (--name-status -z, --numstat -z, --patch), found {diff_calls}"
    );

    let file_capped_calls = file_body.matches(&capped).count();
    assert_eq!(
        file_capped_calls, 0,
        "file_at_commit_for_repo must no longer call the two-spawn \
         git_stdout_capped primitive at all — #221 folded its reads into the \
         single-spawn batch primitive below, found {file_capped_calls}"
    );
    let file_batch_calls = file_body.matches(&batched).count();
    assert_eq!(
        file_batch_calls, 1,
        "file_at_commit_for_repo must perform exactly one batched read (the \
         #168 type check and, when applicable, the content read, both off \
         one still-open `cat-file --batch` process, including through the \
         parent-fallback), found {file_batch_calls}"
    );

    assert_eq!(
        diff_calls + file_batch_calls,
        4,
        "exactly four target callers cross the capped/batched boundary"
    );

    for (what, body) in [
        ("commit_diff_for_repo", diff_body),
        ("file_at_commit_for_repo", file_body),
    ] {
        assert_eq!(
            body.matches(&uncapped).count(),
            0,
            "{what}: an uncapped `{uncapped}` read survives — every read here \
             must name its own cap"
        );
        for (needle, why) in banned.iter() {
            assert_eq!(
                body.matches(needle.as_str()).count(),
                0,
                "{what}: `{needle}` {why}; the bounded primitive owns the child"
            );
        }
    }

    // Narrowness, both directions. The file as a whole still contains the
    // unrelated buffering invocation — `worktree_status` runs
    // `git status --porcelain=v2` and buffers its (tiny, static-arg) output,
    // since Task 6 through the sealed `git_cmd::git_output` helper — so the two
    // extractions above cut where they claim to rather than swallowing the
    // whole file and asserting over nothing. Before Task 6 the witness here was
    // a raw `.output()` call; that migrated away, and a witness that quietly
    // degrades to `#[cfg(test)]` fixtures is this guard passing vacuously — so
    // the witness now names the production helper itself. (`porcelain=v2` is
    // checked against the raw source: `code_only` blanks string contents.)
    let sealed_buffered = ["git_out", "put("].concat();
    assert!(
        code.matches(sealed_buffered.as_str()).count() > 0,
        "file-wide `{sealed_buffered}` vanished: either worktree_status changed, \
         or this guard is now passing vacuously"
    );
    assert!(
        src.contains("porcelain=v2"),
        "the unrelated worktree-status read is expected to remain in this file"
    );
    // Each extracted body really is the one under test, not a stray region that
    // happens to be brace-balanced.
    assert!(
        diff_body.contains("patch_cap(full)"),
        "the extracted diff body does not select a patch cap"
    );
    assert!(
        file_body.contains("FILE_CONTENT_CAP"),
        "the extracted file body does not name the file content cap"
    );
}

/// Layer 2: no write DTO tolerates smuggled extras or non-object shapes. The
/// interesting property is *where* these die: at deserialization, before any
/// handler code runs.
#[test]
fn write_dtos_reject_smuggled_args_and_wrong_shapes() {
    use git_vista_protocol::{
        BranchName, BranchRequest, CloneRequest, CreateBranchRequest, CreateCommitRequest,
        CreateTagRequest, DeleteCloneRequest, DeleteTagRequest, SelectRequest, TagName,
    };

    // An extra freeform-args field beside legitimate fields: refused.
    for (what, err) in [
        (
            "branch+args",
            serde_json::from_str::<CreateBranchRequest>(
                r#"{"name":"x","commit":"HEAD","args":["--force"]}"#,
            )
            .err(),
        ),
        (
            "commit+argv",
            serde_json::from_str::<CreateCommitRequest>(
                r#"{"message":"m","allow_empty":false,"argv":["push","--mirror"]}"#,
            )
            .err(),
        ),
        (
            "branch-op+flags",
            serde_json::from_str::<BranchRequest>(r#"{"branch":"b","flags":"--force"}"#).err(),
        ),
        (
            "clone+command",
            serde_json::from_str::<CloneRequest>(
                r#"{"url":"https://x.example/r","command":"rm -rf /"}"#,
            )
            .err(),
        ),
        (
            "select+path",
            serde_json::from_str::<SelectRequest>(
                r#"{"worktree":"w","mode":"active","path":"/etc"}"#,
            )
            .err(),
        ),
        (
            "delete-clone+recursive",
            serde_json::from_str::<DeleteCloneRequest>(r#"{"worktree":"w","recursive":true}"#)
                .err(),
        ),
        // M2.21d (#238): `-f` is the field a tag-create body would most like
        // to smuggle — it turns "create this tag" into "silently repoint an
        // existing one", past the plan's own `RefAbsent` precondition.
        (
            "tag+force",
            serde_json::from_str::<CreateTagRequest>(
                r#"{"name":"v1","commit":"HEAD","force":true}"#,
            )
            .err(),
        ),
        (
            "delete-tag+remote",
            serde_json::from_str::<DeleteTagRequest>(r#"{"tag":"v1","remote":"origin"}"#).err(),
        ),
    ] {
        assert!(err.is_some(), "{what}: unknown field was accepted");
    }

    // A raw argv array where an object is expected: refused.
    //
    // **What this does and does not say.** serde_json can also fill a struct
    // *positionally* from a JSON array, and a body whose element count and
    // types happen to line up with the fields does deserialize — the two
    // arrays above are refused on arity, not on being arrays. That affordance
    // is checked, deliberately and separately, in
    // [`a_positional_array_body_is_the_object_body_and_smuggles_nothing`]:
    // the point of this whole module is that no client string becomes an
    // argv element it was not declared to be, and a positional array cannot
    // reach a field the object form does not already expose, nor skip the
    // validation that field carries. Asserting "arrays are refused" as if it
    // were universal would have been a comfortable falsehood.
    assert!(serde_json::from_str::<CreateBranchRequest>(r#"["git","push","--force"]"#).is_err());
    assert!(serde_json::from_str::<BranchRequest>(r#"["--delete","main"]"#).is_err());

    // An undo body that names no known variant (a smuggled exec request) is
    // refused by the closed `UndoAction` enum.
    assert!(
        serde_json::from_str::<git_vista_core::activity::UndoAction>(r#"{"exec":"rm -rf /"}"#)
            .is_err()
    );

    // Option-shaped and empty ref names die in the typed `BranchName` gate —
    // the same gate the handlers apply before anything reaches the planner.
    assert!(BranchName::new("-force").is_err());
    assert!(BranchName::new("--exec=/bin/sh").is_err());
    assert!(BranchName::new("").is_err());
    // Same gate on the tag namespace (M2.21d, #238): `git tag -d <name>` puts
    // the name straight after a flag, so an option-shaped name is exactly the
    // shape that would turn a delete into something else.
    assert!(TagName::new("-d").is_err());
    assert!(TagName::new("--points-at=HEAD").is_err());
    assert!(TagName::new("").is_err());
}

/// The serde_json affordance the assertion above is careful *not* to claim
/// away: a write DTO can be filled positionally from a JSON array, and that is
/// harmless here — but only for a reason worth writing down and testing,
/// because "the body was an array" reads like an attack and is not one.
///
/// A positional array is fixed by the struct's own field order. It can name no
/// field the object form does not have, add none, reorder none, and skip none
/// of the validation each field carries downstream. So the two forms are the
/// *same request*, which is exactly what is asserted: array and object
/// deserialize to equal values, and the smuggling that would matter — an extra
/// key — is still refused in the object form (an array cannot express one at
/// all, since it has no keys).
///
/// Found while wiring M2.21d (#238): `["tag","-d","v1"]` deserializes into
/// [`CreateTagRequest`] as `name: "tag", commit: "-d", message: Some("v1")`,
/// with `sign` defaulted. It then dies at `resolve_commit_oid` ("-d" is not an
/// object), which is the ordinary path any bad `commit` takes.
#[test]
fn a_positional_array_body_is_the_object_body_and_smuggles_nothing() {
    use git_vista_protocol::{CreateTagRequest, DeleteTagRequest};

    let positional: CreateTagRequest = serde_json::from_str(r#"["v1","HEAD","notes",false]"#)
        .expect("serde_json fills a struct positionally from an array");
    let keyed: CreateTagRequest =
        serde_json::from_str(r#"{"name":"v1","commit":"HEAD","message":"notes","sign":false}"#)
            .unwrap();
    assert_eq!(
        positional, keyed,
        "the positional form must be the very same request, field for field"
    );
    assert_eq!(
        positional.name, "v1",
        "position 0 is `name` — an array cannot choose which field it fills"
    );

    // The one shape that would actually smuggle something is an extra key,
    // and that has no positional spelling: there are four fields, so a fifth
    // element is an arity error, and a key is only expressible in the object
    // form, where `deny_unknown_fields` refuses it.
    assert!(
        serde_json::from_str::<CreateTagRequest>(r#"["v1","HEAD","notes",false,"--force"]"#)
            .is_err(),
        "an array longer than the struct has fields is refused"
    );
    assert!(
        serde_json::from_str::<CreateTagRequest>(r#"{"name":"v1","commit":"HEAD","force":true}"#)
            .is_err(),
        "and the keyed spelling of the same extra is refused too"
    );
    assert!(
        serde_json::from_str::<DeleteTagRequest>(r#"["v1","origin"]"#).is_err(),
        "one field, two elements: refused"
    );
}

/// Layer 2b: the clone URL gate. The URL is the one client string that becomes
/// a git argument, so every smuggling shape must die in `validate_clone_url`.
#[test]
fn hostile_clone_urls_are_refused_by_the_gate() {
    use git_vista_protocol::validate_clone_url;

    for url in [
        "file:///etc/passwd",                           // local filesystem read
        "ssh://evil.example/repo",                      // key-prompting transport
        "git@github.com:owner/repo.git",                // scp-style ssh
        "-oProxyCommand=touch /tmp/pwned",              // option smuggled as the URL
        "--upload-pack=/tmp/evil",                      // ditto
        "ext::sh -c id",                                // git's ext transport = arbitrary exec
        "https://ok.example/r --upload-pack=/tmp/evil", // second token via whitespace
        "https://ok.example/r\tmore",                   // tab counts as whitespace too
        "",                                             // nothing
    ] {
        assert!(
            validate_clone_url(url).is_err(),
            "hostile clone URL was accepted: {url:?}"
        );
    }
}

/// Layer 3: the same refusals observed on the wire, through the real session/
/// CSRF middleware and the real extractors. Stub handler bodies mean a 2xx
/// with the marker text would prove a hostile body *reached* handler logic —
/// every assertion below is that it never does.
mod wire {
    use crate::handlers::session::{create_session, revoke_session, session_status, SessionState};
    use crate::security::{require_auth, AuthState, HostPolicy};
    use crate::session::SessionManager;
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use git_vista_core::activity::UndoAction;
    use git_vista_protocol::{
        validate_clone_url, BranchRequest, CloneRequest, CreateBranchRequest, CreateTagRequest,
        SessionInfo, CSRF_HEADER,
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    const REACHED: &str = "REACHED HANDLER";

    fn app() -> (Router, Arc<SessionManager>) {
        let sessions = Arc::new(SessionManager::new(None));
        let session_state = SessionState {
            manager: sessions.clone(),
            via_lan: false,
            rate_limiter: None,
        };
        let auth_state = AuthState {
            manager: sessions.clone(),
            hosts: HostPolicy::loopback(8080),
        };
        let router = Router::new()
            .route(
                "/api/session",
                get(session_status)
                    .post(create_session)
                    .delete(revoke_session),
            )
            .route(
                "/api/branch",
                post(|Json(_): Json<CreateBranchRequest>| async { REACHED }),
            )
            .route(
                "/api/checkout",
                post(|Json(_): Json<BranchRequest>| async { REACHED }),
            )
            .route(
                "/api/undo",
                post(|Json(_): Json<UndoAction>| async { REACHED }),
            )
            // M2.21d (#238): the tag-create body, whose extra fields are the
            // interesting attack surface (`force`, `annotated`).
            .route(
                "/api/tag",
                post(|Json(_): Json<CreateTagRequest>| async { REACHED }),
            )
            // Mirrors the real clone handler's order: the gate runs before any
            // spawn could (clone.rs validates, then passes the URL as its own
            // argv entry).
            .route(
                "/api/clone",
                post(|Json(req): Json<CloneRequest>| async move {
                    match validate_clone_url(&req.url) {
                        Ok(_) => (StatusCode::OK, REACHED.to_string()),
                        Err(reason) => (StatusCode::BAD_REQUEST, reason),
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                auth_state,
                require_auth,
            ))
            .with_state(session_state);
        (router, sessions)
    }

    fn req(method: &str, path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost:8080")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                55001,
            ))))
    }

    async fn bootstrap(router: &Router, sessions: &SessionManager) -> (String, String) {
        let token = sessions.current_bootstrap();
        let resp = router
            .clone()
            .oneshot(
                req("POST", "/api/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "bootstrap should succeed");
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let info: SessionInfo = serde_json::from_slice(&bytes).unwrap();
        (cookie, info.csrf.unwrap())
    }

    /// POST `body` to `path` with a valid session + CSRF, so the only thing
    /// standing between the payload and handler logic is the API boundary
    /// under test. Returns (status, body text).
    async fn post_json(
        router: &Router,
        cookie: &str,
        csrf: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, String) {
        let resp = router
            .clone()
            .oneshot(
                req("POST", path)
                    .header(header::COOKIE, cookie.to_string())
                    .header(CSRF_HEADER, csrf.to_string())
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn hostile_write_bodies_die_at_the_boundary() {
        let (router, sessions) = app();
        let (cookie, csrf) = bootstrap(&router, &sessions).await;

        for (path, body) in [
            // Raw argv arrays instead of the typed object.
            ("/api/branch", r#"["git","push","--force"]"#),
            ("/api/checkout", r#"["--delete","main"]"#),
            // Freeform args smuggled beside legitimate fields.
            (
                "/api/branch",
                r#"{"name":"x","commit":"HEAD","args":["--force"]}"#,
            ),
            ("/api/checkout", r#"{"branch":"b","extra":"--force"}"#),
            // A smuggled exec request that matches no UndoAction variant.
            ("/api/undo", r#"{"exec":"rm -rf /"}"#),
            ("/api/undo", r#"["sh","-c","id"]"#),
            // M2.21d (#238). `force` would repoint an existing tag past the
            // plan's `RefAbsent` precondition; `annotated` without a message
            // is the request that makes `git tag -a` open an editor a
            // headless server has no way to finish (ADR 0048). Neither key
            // exists on the DTO, so both die here.
            ("/api/tag", r#"{"name":"v1","commit":"HEAD","force":true}"#),
            (
                "/api/tag",
                r#"{"name":"v1","commit":"HEAD","annotated":true}"#,
            ),
            // Not JSON at all.
            ("/api/branch", "name=x; git push --mirror"),
        ] {
            let (status, text) = post_json(&router, &cookie, &csrf, path, body).await;
            assert!(
                status.is_client_error(),
                "{path} accepted hostile body {body:?} (status {status})"
            );
            assert!(
                !text.contains(REACHED),
                "{path}: hostile body {body:?} reached handler logic"
            );
        }
    }

    #[tokio::test]
    async fn hostile_clone_urls_die_at_the_boundary() {
        let (router, sessions) = app();
        let (cookie, csrf) = bootstrap(&router, &sessions).await;

        for body in [
            r#"{"url":"file:///etc/passwd"}"#,
            r#"{"url":"-oProxyCommand=touch /tmp/pwned"}"#,
            r#"{"url":"ext::sh -c id"}"#,
            r#"{"url":"https://ok.example/r --upload-pack=/tmp/evil"}"#,
            r#"{"url":"https://ok.example/r","depth":"--mirror"}"#,
        ] {
            let (status, text) = post_json(&router, &cookie, &csrf, "/api/clone", body).await;
            assert!(
                status.is_client_error(),
                "/api/clone accepted hostile body {body:?} (status {status})"
            );
            assert!(
                !text.contains(REACHED),
                "/api/clone: hostile body {body:?} got past the URL gate"
            );
        }
    }
}
