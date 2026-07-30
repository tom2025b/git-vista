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
    // its `#[cfg(test)]` fixture setup.
    "src/durable.rs",
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
    // (#66 Task 6); this entry now covers only its `#[cfg(test)]` fixtures.
    "src/handlers/read.rs",
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
    "src/state.rs",                  // #[cfg(test)] fixture setup
    "src/argv_boundary.rs",          // this file (the scan reads its own source)
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
    // #66 Task 25 (step 3): the anti-vacuity contract's harness. Its baseline
    // leg spawns plain `git` outside the sandbox (literal), and its CI
    // preflight (`ci_preflight_host_meets_the_declared_minimum`) runs `cc
    // --version` to confirm the compiler the escape battery needs is present
    // — it never compiles or execs anything client-influenced. Also in
    // LAUNCHER_SPAWN_SITES for the same `cc` reason as escape_suite.rs above.
    "src/sandbox/escape_contract.rs",
    // Deliberately absent: `src/sandbox/documented_gaps.rs` and
    // `src/sandbox/lifecycle.rs`, the suites #66's Task 15 round adds beside
    // this one. `documented_gaps.rs` exists and constructs no `Command` at all,
    // so an entry for it would be a permission with nothing behind it;
    // `lifecycle.rs` did not exist when this list was last reviewed, so nobody
    // has read the spawn it may or may not contain. Neither was pre-added on
    // purpose — see `every_allowlist_entry_names_a_live_spawn_site` below for
    // why an entry written ahead of its file is the failure mode this list is
    // supposed to prevent, not a convenience.
    // git-vista-git
    // `#[cfg(test)]` fixture setup only. This entry used to read "read-side
    // reflog/stash reads, static args"; that is stale — the crate's production
    // history reads (`walk_history`, `read_commit`, `remote_membership`,
    // `read_remote_commits`) do not spawn a process at all, and every
    // `Command::new` left in the file is below its `#[cfg(test)] pub(crate) mod
    // tests` line, building real repositories for the suites to read.
    "src/history.rs",
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
/// the tripwire. It stands at three: one production chokepoint (`spawn.rs`) and
/// two `#[cfg(test)]` harnesses that need `cc`. (This doc used to say "keep
/// this list at one entry", written when it held one; leaving that in while the
/// list grew to three turned a live budget into a slogan nobody could act on.)
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

    for root in [&server_root, &git_root] {
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
                ALLOWED_SPAWN_SITES.contains(&rel.as_str()),
                "NEW PROCESS-SPAWN SITE: {rel} constructs a Command but is not \
                 allowlisted in argv_boundary.rs. Review it — a mutating git argv \
                 belongs in the planner's executor, nowhere else."
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

    for (i, rel) in ALLOWED_SPAWN_SITES.iter().enumerate() {
        assert!(
            !ALLOWED_SPAWN_SITES[..i].contains(rel),
            "{rel} is listed twice; two reviews of one file cannot disagree \
             usefully, and a duplicate hides which comment is current"
        );
        // Entries are relative to *their own* crate root, so resolve against
        // both and insist on exactly one hit — an ambiguous entry would grant
        // one file's permission to another crate's same-named file.
        let candidates: Vec<PathBuf> = [&server_root, &git_root]
            .iter()
            .map(|root| root.join(rel))
            .filter(|p| p.is_file())
            .collect();
        assert_eq!(
            candidates.len(),
            1,
            "{rel}: expected exactly one of the two crate roots to contain this \
             file, found {}. A dead entry pre-authorises a spawn nobody has \
             read; an ambiguous one authorises the wrong file.",
            candidates.len()
        );
        let text = std::fs::read_to_string(&candidates[0]).expect("readable source file");
        assert!(
            text.contains(&spawn),
            "{rel} is allowlisted but constructs no Command. Remove the entry \
             rather than leaving it: the scan only consults this list for files \
             that spawn, so this one grants a permission that is invisible \
             until someone adds a raw spawn to that file."
        );
    }

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
    let tests_at = code
        .find("mod tests")
        .expect("handlers/read.rs has a test module");
    assert!(
        at < tests_at,
        "`{marker}` was found inside `mod tests`, not in production code"
    );

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

/// Layer 1b (M1.10, #63): the *streaming* source boundary. Every git read the
/// two bounded read handlers perform must go through the capped, killable
/// primitive — proved structurally, on the source, not inferred from the size
/// of a returned buffer.
///
/// Exactly one production body is extracted for each of `commit_diff_for_repo`
/// and `file_at_commit_for_repo`; across only those two bodies there must be
/// exactly five `git_stdout_capped(` call sites (three diff reads, two file
/// reads) and no escape hatch — no uncapped `git_stdout(`, no `.output()`, no
/// `.wait_with_output()`, no direct `Command` construction, each of which would
/// buffer whatever git chose to print.
///
/// `file_at_commit_for_repo` went from one call site to two in #168: a
/// `git cat-file -t <spec>` type check now runs — through the same capped
/// primitive, so this file's guarantee still holds — *before* the `git show`
/// content read, so a tree or submodule entry is rejected without ever
/// reading (or serving) its git-show output. See that function's doc comment.
///
/// The scope is deliberately narrow. The unrelated `worktree_status` read in
/// the very same file legitimately buffers a whole (tiny, static-arg) git
/// output — since Task 6 through the sealed `git_cmd::git_output` helper rather
/// than a raw `Command` — and the assertion below that the *file* still
/// contains that call while the two extracted *bodies* do not is what proves
/// the extractor cut where it claims to, instead of quietly matching nothing.
#[test]
fn bounded_read_source_boundary_is_streaming_and_exactly_five() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handlers/read.rs");
    let src = std::fs::read_to_string(&path).expect("readable handlers/read.rs");
    let code = code_only(&src);

    let capped = ["git_stdout", "_capped("].concat();
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
    let file_calls = file_body.matches(&capped).count();
    assert_eq!(
        diff_calls, 3,
        "commit_diff_for_repo must perform exactly three bounded reads \
         (--name-status -z, --numstat -z, --patch), found {diff_calls}"
    );
    assert_eq!(
        file_calls, 2,
        "file_at_commit_for_repo must perform exactly two bounded reads \
         (the cat-file -t type check, then the show content read), found {file_calls}"
    );
    assert_eq!(
        diff_calls + file_calls,
        5,
        "exactly five target callers cross the capped boundary"
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
        DeleteCloneRequest, SelectRequest,
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
    ] {
        assert!(err.is_some(), "{what}: unknown field was accepted");
    }

    // A raw argv array where an object is expected: refused.
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
        validate_clone_url, BranchRequest, CloneRequest, CreateBranchRequest, SessionInfo,
        CSRF_HEADER,
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
