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

// Split out of this file (was 1553 lines) so each real seam — the allowlist
// and its scanner below, and the argv-shape/shim/streaming/DTO proofs in the
// children — lives in its own file. `ALLOWED_SPAWN_SITES`,
// `ALLOWED_GIT_CRATE_SPAWN_SITES`, `LAUNCHER_SPAWN_SITES` and the scan that
// reads them stay in *this* file on purpose: separating a security list from
// the code that enforces it makes the pairing easier to break later.
// `code_only` stays here too — it is the scanning machinery every child
// module (and `sandbox::escape_contract`, `sandbox::compat`,
// `planner::contract_suite`, via `crate::argv_boundary::code_only`) reuses,
// not a test-specific helper. The three-layer numbering in the module doc
// above still holds; layers 2 and 3 now live in `dto_gates` below.
//
// Every child is a private submodule under `#[cfg(test)] mod argv_boundary;`
// (declared in `main.rs`), so none of them need their own `#[cfg(test)]`.
mod bounded_read; // Layer 1b: the streaming/bounded-read source boundary.
mod dto_gates; // Layers 2, 2b, 3: DTO, clone-URL and wire adversarial fixtures.
mod sandbox_argv_shapes; // INV-16: `sandbox_argv`'s three shapes, every tier.
mod shim; // The `gv-sandbox` shim execs and never forks.

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
    // #[cfg(test)] git fixtures for M4.31e (#431)'s reconnect/crash suite:
    // `git init`/`commit`/`merge` to build a two-path conflict, then
    // `checkout --ours|--theirs` + `add` to resolve one path BY HAND — the
    // point of that suite being that a reconnected client shares no state with
    // whatever produced the conflict, including this fixture. The mutating
    // argv under test still goes through the planner's executor
    // (`plan_and_execute_in`); these are the fixture and the hand-resolution
    // standing in for a user at a terminal, never the path being proven.
    "src/planner/reconnect_suite.rs",
    // #[cfg(test)] git fixtures for the M2.21d/e (#238/#239) tag-argv-shape
    // and signed-tag-execution suite: plain `git init`/`commit`/`rev-parse`,
    // plus the failed-signing attempt's own `git rev-parse --verify` check
    // that no tag was left behind.
    "src/planner/tag_signing_suite.rs",
    // #[cfg(test)] git fixtures for the #145 staleness-contract suite:
    // plain `git init`/`commit`/`add`/`branch`/`remote` to build and drift
    // fixture repositories, outside the sandboxed harness under test.
    "src/planner/staleness_suite.rs",
    // #[cfg(test)] git fixtures for the M11.02 (#547) checkout-collision
    // suite: plain `git branch` and `git worktree add` to build a repository
    // that genuinely has a second desk open on a branch. The `worktree add`
    // is the one shape this suite needs and the app itself does not have
    // (M11's write path is a later slice) — it stands in for a user at a
    // terminal, never the path being proven.
    "src/planner/worktree_collision_suite.rs",
    // #[cfg(test)] git fixtures for the M11.04 (#549) worktree-add suite:
    // plain `git branch` to make a branch that is not checked out, and
    // `git worktree list --porcelain` to read back what the executor actually
    // created — outside the sandboxed harness under test.
    "src/planner/worktree_add_suite.rs",
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
    // #[cfg(test)] git fixtures for the M4.32 (#85) advisory suite: plain
    // `git init`/`commit`/`push`/`symbolic-ref` to build a repository with a
    // real bare remote, so the presence or absence of refs/remotes/origin/HEAD
    // — the variable the whole suite turns on — is genuine rather than mocked.
    "src/planner/advisory_suite.rs",
    // #448 removed `src/conflicts.rs` from this list: its `#[cfg(test)]` git
    // fixtures now come from the `git-vista-fixtures` catalogue, so the file
    // constructs no `Command` at all and the entry had become a permission
    // granted to nothing — invisible until someone added a raw spawn back.
    //
    // #[cfg(test)] git fixtures for the M4.31a (#428) inspect-a-conflict
    // handlers: `blob_content_for_repo` and `worktree_file_for_repo` are
    // proven against git's actual index and working-tree state, not a mock.
    "src/handlers/conflicts.rs",
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
