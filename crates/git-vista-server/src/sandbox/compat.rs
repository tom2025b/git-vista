//! #66 (M1.13b, plan Task 14): the **compatibility** battery — the half that
//! proves real git workflows still work *under the policy*.
//!
//! A sandbox that breaks real workflows gets turned off, so compatibility is a
//! security property and not a nicety. The escape battery
//! (`escape_suite.rs`/`hook_mode_suite.rs`) proves things are denied; this file
//! proves the things that must still work still work.
//!
//! # The vacuity mode here is the mirror image of the escape battery's
//!
//! An escape case is vacuous when a pass proves nothing was actually tried. A
//! **compat** case is vacuous when the pass would be **byte-identical if the
//! sandbox had never been applied at all** — because then it proves that git
//! works, which it always does, and says nothing about the policy. That is
//! exactly how the old `blocked_hooks` case went wrong: it was satisfied by
//! `Tier::Unsandboxed`, with no Landlock, seccomp or bwrap present anywhere.
//!
//! So every case here is **two legs of one differential**, and the sandbox has
//! to be visible in the difference:
//!
//! | | baseline leg | inside leg |
//! |---|---|---|
//! | tier | `Tier::Unsandboxed` | `Tier::Strict` |
//! | built by | `policy_for(repo, false, NetworkNeed::Local)` | the same call, via [`strict_baseline`] |
//! | spawned by | `spawn::command_async` | `spawn::command_async` |
//! | environment | `production_env_profile()` | `production_env_profile()` |
//! | hook's `/proc/self/status` | `Seccomp: 0` — asserted | `Seccomp: 2`, `NoNewPrivs: 1` — asserted |
//!
//! The two legs differ in **one** input: whether the repository carries an
//! operator-trust marker. Everything else — the fixture, the argv, the
//! environment, the spawn seam, the policy constructor — is identical. That is
//! what makes a green inside leg attributable to the policy rather than to git.
//!
//! # Capability is established by execution, never by asking the host (R4')
//!
//! No case here calls `strict_available()`, `capabilities::probe()`,
//! `bwrap_path()` or stats a path to decide what it will attempt. The baseline
//! leg *performs the operation* and must produce the case's declared outcome; a
//! baseline that misses it is a `CapabilityAbsent` naming what was missing. Per
//! this task's instruction, capability absence here is a **hard, loud failure**,
//! not a silent green skip — see "Deviations" below for why that is stronger
//! than the escape battery's record-and-let-the-job-decide posture *for this
//! file*, which no CI job consumes yet.
//!
//! # How the sandbox is observed from the inside
//!
//! Every case is a real `git commit` carrying a `pre-commit` hook, because a
//! hook is the only in-sandbox observer available: it runs *after* the shim has
//! applied Landlock and seccomp and exec'd git, in the process tree git itself
//! created. The hook writes three facts into files inside the repository (the
//! one tree the policy already grants read-write, so no policy is bent to let
//! the observation through):
//!
//! - `gv-compat-ran` — proof the hook executed at all (INV-11);
//! - `gv-compat-status` — its own `/proc/self/status` `Seccomp:`/`NoNewPrivs:`
//!   lines, the kernel's own statement about the process (acceptance evidence
//!   (F) of the anti-vacuity contract);
//! - `gv-compat-interp` — `readlink -f /proc/$$/exe`, the interpreter the `#!`
//!   line actually resolved to (INV-12).
//!
//! Nothing is printed. A `SKIPPED`/`WARNING` line on a *passing* libtest test is
//! swallowed by the harness and is byte-identical in CI whether or not it
//! happened — the exact false-green channel the contract's skip-policy section
//! documents.
//!
//! # Contract binding (`docs/sandbox/escape-battery-anti-vacuity-contract.md`)
//!
//! - **R1 (declarative)** — every case is a `const CASE_X: CompatCase` with every
//!   field written out, and every `#[test]` body is exactly
//!   `run_compat_case(&CASE_X);`. All assertions, control flow, fixture setup
//!   and environment access live in `mod harness`; all source tripwires live in
//!   `mod contract`. `contract::r1_the_case_region_is_declarative` greps the
//!   region outside both for `if`/`match`/`return`/`assert*!`/`eprintln!`.
//! - **R2 (exact observation)** — every observation is a named value compared
//!   with `assert_eq!` (`Seccomp: 2`, `NoNewPrivs: 1`, an exact exit code, an
//!   exact commit subject). A missing observation is a typed `Err`, never an
//!   `Option` that a negation could satisfy: `Leg` is only ever built by
//!   `observe`, which returns `Result<Leg, String>`.
//! - **R3', reinterpreted** — there is no exclude list under test, so "sibling
//!   under the same granted tree" does not apply. What survives is *same policy,
//!   same run, same probe*: the inside leg's policy comes from
//!   [`strict_baseline`], which delegates to `shim_cli::production_policy`, the
//!   identical constructor the escape battery's Strict cases use. This file adds
//!   **no** policy builder (R6) — a compat case that only passed under a policy
//!   the escape battery does not also use would prove nothing about what ships.
//! - **R5 (report file)** — `run_compat_case` appends one
//!   `GV-COMPAT case=<id> result=… class=functional` line to `$GV_COMPAT_REPORT`
//!   when that variable is set. See "The census" below for what actually gates.
//! - **R6 (production seam)** — both legs spawn through
//!   `sandbox::spawn::command_async`. This file constructs no `Command` at all
//!   (not even in prose — `argv_boundary.rs`'s scan reads raw text, so the bare
//!   `Command::new` + `(` pattern is never written here, the same discipline
//!   that file applies to its own source), no `Policy {` literal and no second
//!   policy constructor;
//!   `contract::r6_every_leg_goes_through_the_production_seam` asserts it.
//! - **R7 (one environment)** — every spawn, in both legs and in fixture setup,
//!   uses `escape_contract::production_env_profile()`. This file contains no
//!   `env_clear` and no `.env(`.
//! - **R8 (expiring exemption)** — **not needed, and deliberately absent.** The
//!   plan text says every case here must carry
//!   `Exemption::NotProductionReachable` because `policy_for_repo` hard-codes
//!   `Tier::Network`/`HookMode::Run`. That was true when the plan was written
//!   and is false now: #197 made the tier a function of the declared
//!   `NetworkNeed` and #206 retired the escape battery's seven Strict
//!   exemptions for exactly this reason (see `escape_contract::policy_for_case`).
//!   A `Strict` policy is production-constructible today, so an exemption here
//!   would be a permission with nothing behind it. The tier is not asserted by
//!   construction either — [`strict_baseline`] reads it back off the policy
//!   production returned, so a re-tiering of the dispatch fails these tests
//!   loudly instead of silently re-pointing them at a tier with no namespaces.
//! - **R9 (mutation matrix)** — does not bind this file and no case declares
//!   `dies_under`. M1–M10 all *remove or weaken a denial*; weakening a denial
//!   cannot break a functional claim, it can only make one easier to satisfy.
//!   Forcing a `dies_under` here would manufacture a claim the mutation
//!   semantics cannot support. (What is worth having, at near-zero marginal
//!   cost once the mutation driver exists, is running these cases against every
//!   mutant and requiring them to stay green as a regression-freedom sweep —
//!   that belongs to the driver, which this lane does not own.)
//! - **R11 (self-binding)** — `contract::RULES` pairs every rule this file
//!   claims to honour with the test that enforces it, and
//!   `contract::r11_every_rule_names_a_live_test` reads this file's own source
//!   and fails if one of those tests is deleted or renamed.
//!
//! # The census, and what actually gates
//!
//! `docs/sandbox/compat-census.txt` is this battery's own case-id census, kept
//! **separate** from `docs/sandbox/escape-census.txt` rather than folded into
//! it. Two reasons, one of them mechanical:
//!
//! 1. `escape_contract::r5_census_names_exactly_the_declared_cases` asserts that
//!    the escape census equals the set of `EscapeCase` ids scanned from the
//!    battery files **in both directions**. Adding a compat id there breaks that
//!    tripwire; writing `$GV_ESCAPE_REPORT` records without census entries
//!    breaks the CI gate instead. The route is closed in both directions and
//!    forcing it open would need edits to files this lane does not own.
//! 2. The vocabulary does not fit. `result=escaped` is a security-breach word
//!    and none of the cases here makes a denial claim it could be true or false
//!    of.
//!
//! **How it is consumed, today:** by `contract::the_census_names_exactly_the_declared_cases`,
//! a source tripwire in this file that asserts the census's id set equals the
//! set of `CompatCase` ids declared below, in both directions. That is
//! deliberately stronger than the shell gate the plan describes: a renamed or
//! deleted case breaks the **build**, where a `cargo test <filter>` gate exits 0
//! on "0 filtered out" and a renamed module empties it silently. The file holds
//! bare ids, one per line, with no comments — so the `sort | diff` gate plan
//! step 14 assigns to the CI lane (`.github/workflows/ci.yml`, not owned here)
//! can consume it verbatim when it lands, against `$GV_COMPAT_REPORT`.
//!
//! # Deviations from the plan text, stated rather than absorbed
//!
//! 1. **Capability absence is a hard failure, not a green record.** The escape
//!    battery may record and continue because a CI job asserts zero
//!    `capability-absent` records. No job consumes `$GV_COMPAT_REPORT` yet
//!    (`ci/*` is another lane's file), so recording-and-passing here would be a
//!    silent green skip — the failure mode this milestone has now found six
//!    times. `sandbox::lifecycle` took the same deviation for the same reason
//!    and documents it as deviation 2 in its module doc. The record is still
//!    written first, so a future job loses nothing.
//! 2. **Every case is a `git commit`, not a `git status`.** The plan spells the
//!    worktree and submodule cases as `status --short`. A `status` runs no hook,
//!    and with no hook there is no in-sandbox observer — the case would then be
//!    green under `Tier::Unsandboxed`, which is the vacuity this file exists to
//!    avoid. A commit is also strictly the stronger claim: it reads *and*
//!    writes the resolved git directory, which is the whole point of the
//!    linked-worktree and submodule geometries.
//! 3. **No `node` case.** Plan gap 1 asks for a husky-shaped `node` hook beside
//!    the `sh` ones. Because capability absence is a hard failure here
//!    (deviation 1), a `node` case would turn every host without node red for a
//!    non-security reason. The husky *shape* — a repo-local `core.hooksPath`
//!    directory whose hook is the one that runs — is covered by the two `sh`
//!    cases below, including a decoy that proves `core.hooksPath` was honoured.
//!    The interpreter *identity* claim (INV-12) is covered by
//!    `interpreter_identity`. What is genuinely not covered is a hook whose
//!    interpreter is a non-system toolchain (nvm's node, a pyenv python); that
//!    is a named gap, not a silent one.
//! 4. **The `io_uring` positive trio is not here.** `status`/`commit`/`log`
//!    under the filter are INV-5's positive half; `commit` and `log` are
//!    exercised by every case below (the effect check runs `git log` through the
//!    same policy), and `escape_suite`'s io_uring cases already carry the
//!    denial. Three more `plain_fixture` cases whose only difference is the
//!    subcommand would add census rows, not evidence.
//!
//! # Demonstrated reds — the evidence that these cases are not vacuous
//!
//! A gate never observed to fail is not known to be a gate, and the mirror-image
//! vacuity here ("would this pass with the sandbox never applied?") is exactly
//! the question a mutation answers and a review does not. Four single-mechanism
//! mutations were applied to this file and measured on 2026-07-30. Every one was
//! reverted; they are recorded rather than committed because R9's driver
//! (`ci/mutation-matrix.sh`) belongs to another lane and mutates *production*
//! source, which is the right home for a permanent grid.
//!
//! | mutation | measured result |
//! |---|---|
//! | **A** — the inside leg runs under the *baseline* (`Tier::Unsandboxed`) policy: the sandbox is never applied | **all six cases FAIL**, each on `the hook inside the sandbox reports \`Seccomp: 0\`, not 2`. `contract::r6_every_leg_goes_through_the_production_seam` fails independently, off the source. |
//! | **B** — `core.hooksPath` is never written, so the repository keeps ordinary `.git/hooks` | **`husky_hook_runs` and `husky_hook_gates` FAIL** on the decoy (`the decoy hook in .git/hooks fired`); the other four stay green. The husky *shape*, not merely "a hook ran", is what those two cases test. |
//! | **C** — the rejecting hook's `exit 1` becomes `exit 0` | **`husky_hook_gates` alone FAILS**; `husky_hook_runs` stays green. INV-11's two halves are genuinely independent cases, not one test asserting two things. |
//! | **D** — the linked-worktree case runs in the main repository instead of the worktree | **`linked_worktree_commit` FAILS** on `gitdir == commondir … the geometry under test does not exist here`. A case cannot silently degrade into a plain-repository case. |
//!
//! Mutation A is the load-bearing one: it is the exact defect this battery's
//! predecessor shipped (`blocked_hooks`, satisfied by `Tier::Unsandboxed`), and
//! every case here dies under it.
//!
//! # The submodule geometry — a finding, measured, not inferred
//!
//! The case this file carries is `submodule_parent_commit`: a commit in a
//! repository that **contains** a submodule. The obvious case — a commit
//! *inside* the submodule's working directory — is not here, and the reason is
//! not that it was hard. **It cannot run at all.** Measured 2026-07-30 on this
//! host, through the production builder, git 2.x:
//!
//! ```text
//! git -C <outer> -c protocol.file.allow=always submodule add -q <inner> sub   rc=0
//! <outer>/sub/.git            = "gitdir: ../.git/modules/sub\n"
//! repo_paths::resolve(<outer>/sub)
//!     = Err(WorktreeGeometry { why: "`commondir` at
//!           <outer>/.git/modules/sub/commondir is unreadable: No such file or directory" })
//! policy_for(<outer>/sub, false, NetworkNeed::Local)
//!     = Err(RepoPaths(WorktreeGeometry { .. }))          <-- no policy is built
//! policy_for(<outer>, ..)  = Ok(..)   and a real `git commit` there under
//!                                     Tier::Strict exits 0
//! ```
//!
//! So **every git operation whose repository path is a submodule working
//! directory is refused before a sandbox is even constructed.** That is not a
//! regression and not an accident: `worktree::linked_worktree_dirs` requires a
//! `commondir` file, which a linked worktree has and a submodule does not, and
//! that module's doc names the consequence — *"Geometries this deliberately
//! does not support, today: submodule gitdir pointers (no `commondir`) and
//! `--separate-git-dir` repositories (same) — both refuse rather than guess."*
//! Fail-closed is the right posture for an unproven pointer geometry.
//!
//! What is **not** recorded anywhere is the user-visible consequence: a
//! submodule cannot be served as a repository, at all, in any tier. Two things
//! follow, and neither is this lane's file to write:
//!
//! - the inverted claim ("a submodule working directory is refused with a named
//!   error, not a confusing downstream git failure") is an INV-17
//!   documented-non-coverage claim and belongs in `documented_gaps.rs`;
//! - whether to *support* the geometry (grant `<outer>/.git/modules/<name>`
//!   after an equivalent containment proof) is a security decision about a
//!   repository-writable pointer file, which is an ADR, not a test.
//!
//! A case asserting the commit works inside a submodule would be asserting that
//! production should do something it deliberately decided not to do; a case that
//! quietly ran in the parent while claiming the submodule id would be the
//! vacuity this file exists to prevent. So the claim is scoped to the parent,
//! the id says so, and the gap is written down here with its measurement.
//!
//! # Why the baseline leg is `Tier::Unsandboxed` and not a raw `git`
//!
//! The obvious baseline is a raw `std::process::Command` naming `git`, which is
//! what `escape_contract`'s own `run_git_outside` does. This file cannot: a
//! `Command` construction here would need a new entry in `argv_boundary.rs`'s
//! `ALLOWED_SPAWN_SITES`, permanently widening the crate's strongest structural
//! tripwire for a test convenience — and that file belongs to another lane.
//!
//! Going through `Tier::Unsandboxed` is better anyway. It is a **production**
//! tier reached by a **production** builder at production arity, it is genuinely
//! unsandboxed (`sandbox_argv` returns a bare `git` argv for it — no shim, no
//! bwrap, no ruleset), and it makes the two legs differ in exactly one input.
//! Reaching it needs an operator-trust marker, which [`TrustGuard`] writes and
//! revokes: markers are keyed by the *canonicalised* repository path and their
//! content is compared verbatim, so a marker for a throwaway `/tmp` fixture can
//! never trust a real repository. The guard revokes on `Drop`, so a panicking
//! test cleans up too, and the inside leg's own
//! `assert_eq!(policy.tier, Tier::Strict)` (inside [`strict_baseline`]) is what
//! catches a marker that failed to be revoked — a leaked grant makes this file
//! go red, never quietly green.

use std::path::{Path, PathBuf};

use super::escape_contract::production_env_profile;
use super::escape_suite::hostile_hook_repo;
use super::lifecycle::strict_baseline;
use super::spawn::command_async;
use super::*;

mod harness {
    //! Everything with a decision in it. R1 fences the case region below to
    //! `const` declarations and one-statement `#[test]` bodies precisely so
    //! that this module is the *only* place an acceptance condition can be
    //! written — one place is reviewable, twelve scattered `if`s are not.

    use super::*;

    // -----------------------------------------------------------------------
    // The declarative case vocabulary
    // -----------------------------------------------------------------------

    /// The repository geometry a case runs in. Each variant is a *shape real
    /// users have*, not a test convenience.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Geometry {
        /// A plain repository with its hook in `.git/hooks/pre-commit`.
        PlainHooksDir,
        /// husky's shape: a repo-local `core.hooksPath = .husky`, the real hook
        /// in `.husky/pre-commit`, and a **decoy** left behind in
        /// `.git/hooks/pre-commit`. The decoy is load-bearing: without it a
        /// `core.hooksPath` that was silently ignored would still fire *a* hook,
        /// the witness markers would still appear, and the case would prove
        /// nothing about the husky shape.
        HuskyHooksPath,
        /// A linked worktree (`git worktree add`) — its git directory lives
        /// under the main repository's common directory, outside the worktree
        /// (INV-10 / A14).
        LinkedWorktree,
        /// A repository that **contains** a submodule: `.gitmodules`, a gitlink
        /// entry, and a `sub/.git` *file* pointing into
        /// `<outer>/.git/modules/<name>`.
        ///
        /// The case runs in the **outer** repository, not inside the submodule,
        /// and that is not a convenience — see "The submodule geometry" in the
        /// module doc. `policy_for` **refuses** a submodule working directory
        /// outright, by a decision `worktree.rs` records deliberately, so a case
        /// that ran there would be asserting production should do something it
        /// decided not to do. The parent-side claim is the one the shipped
        /// server actually makes, and it was untested.
        SubmoduleParent,
    }

    /// What the `pre-commit` hook does after writing its witness markers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum HookVerdict {
        /// `exit 0` — the commit proceeds (INV-11's "hooks run" half).
        Accepts,
        /// `exit 1` — the commit is refused (INV-11's "hooks gate" half, the
        /// one round 4 never ran).
        Rejects,
    }

    /// Whether the case's commit must end up in the history. Declared per case
    /// rather than derived from the exit code, so the two are cross-checked
    /// against each other instead of one standing in for the other.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum CommitOutcome {
        Lands,
        DoesNotLand,
    }

    /// INV-2's positive half (A13): the fixture has **no** repo-local identity,
    /// so an author can only come from `~/.gitconfig` reached through the
    /// policy's read-only `$HOME` grant.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum IdentityClaim {
        /// The landed commit's author email must equal the value
        /// `git config --global user.email` returns **through the same policy**,
        /// in the same leg. Equality with the global value is what closes the
        /// hole in a bare "the email is non-empty" check: git will happily
        /// fabricate `user@host` when it cannot read a config, and a fabricated
        /// address would be identical in both legs.
        FromGlobalConfig,
        NotClaimed,
    }

    /// INV-12 / F6: the interpreter a `#!` line resolves to inside the sandbox
    /// must be the same file it resolves to outside. The sharp part of F6 is the
    /// *silent* fall-through — a hook resolving a different interpreter and
    /// reporting success.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InterpreterClaim {
        MustMatchBaseline,
        NotClaimed,
    }

    /// One compatibility case. No `Default`, no `..Default::default()` — every
    /// field is written out per case (R1) so a reviewer sees every claim a case
    /// makes in one literal and never inherits one silently.
    pub(crate) struct CompatCase {
        pub id: &'static str,
        pub geometry: Geometry,
        pub hook: HookVerdict,
        /// R2: the exact exit status git must return, in **both** legs. Never
        /// `assert_ne!(code, 0)` — a negation is satisfied by every way the
        /// commit could have died before hook discovery.
        pub expect_commit_code: i32,
        pub expect_commit: CommitOutcome,
        pub identity: IdentityClaim,
        pub interpreter: InterpreterClaim,
    }

    // -----------------------------------------------------------------------
    // Outcome and the report file (R5)
    // -----------------------------------------------------------------------

    #[derive(Debug)]
    pub(crate) enum Outcome {
        FunctionalOk,
        /// The operation did not work under the policy. This is the finding the
        /// battery exists to produce.
        FunctionalBroken {
            detail: String,
        },
        /// The premise could not be established — the baseline leg, which runs
        /// with no sandbox at all, did not do what the case declares. Recorded,
        /// then raised: it is never a silent pass (deviation 1).
        CapabilityAbsent {
            missing: String,
        },
    }

    impl Outcome {
        fn result_word(&self) -> String {
            match self {
                Outcome::FunctionalOk => "functional-ok".to_string(),
                Outcome::FunctionalBroken { .. } => "functional-broken".to_string(),
                Outcome::CapabilityAbsent { missing } => {
                    format!("capability-absent:{}", one_line(missing))
                }
            }
        }
    }

    /// Collapse a multi-line diagnostic into one report-file field. A record
    /// format is line-oriented; a stray newline would split one case into two
    /// records and quietly change what a census diff compares.
    fn one_line(s: &str) -> String {
        s.replace(['\n', '\r'], " ")
    }

    /// R5: one line per case, to a **file**, never to stdout or stderr — libtest
    /// swallows both streams for passing tests, so a printed record and no
    /// record at all are the same bytes in a CI log.
    fn report(case: &CompatCase, outcome: &Outcome) {
        let Some(path) = std::env::var_os("GV_COMPAT_REPORT") else {
            return;
        };
        let line = format!(
            "GV-COMPAT case={} result={} class=functional\n",
            case.id,
            outcome.result_word()
        );
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        if let Ok(mut f) = opened {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
    }

    // -----------------------------------------------------------------------
    // Operator trust — the one input that differs between the two legs
    // -----------------------------------------------------------------------

    /// Holds operator-trust markers for the duration of the baseline leg and
    /// removes them on `Drop`.
    ///
    /// See the module doc for why the baseline leg is `Tier::Unsandboxed` rather
    /// than a raw `git`. Markers are keyed by canonical path and their content
    /// is compared verbatim by `trust::is_trusted`, so a marker written for a
    /// throwaway `/tmp` fixture cannot trust anything else — and `Drop` means a
    /// panicking test still cleans up.
    pub(crate) struct TrustGuard {
        granted: Vec<PathBuf>,
    }

    impl TrustGuard {
        fn new() -> Self {
            Self {
                granted: Vec::new(),
            }
        }

        fn grant(&mut self, repo: &Path) -> Result<(), String> {
            let canonical = repo
                .canonicalize()
                .map_err(|e| format!("cannot canonicalise {} for trust: {e}", repo.display()))?;
            trust::grant(&canonical)
                .map_err(|e| format!("cannot write a trust marker for {}: {e}", repo.display()))?;
            self.granted.push(canonical);
            Ok(())
        }
    }

    impl Drop for TrustGuard {
        fn drop(&mut self) {
            for p in &self.granted {
                let _ = trust::revoke(p);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Policies — this file builds none of its own (R6)
    // -----------------------------------------------------------------------

    /// The baseline leg's policy: the **production** builder, at the arity
    /// production calls it, on a repository the operator has trusted.
    ///
    /// The tier is read back off what production returned rather than asserted
    /// by construction. A trust marker that failed to be written, or a change to
    /// `tier_for`, therefore fails here with the reason instead of silently
    /// turning the "unsandboxed" leg into a second sandboxed one — which would
    /// destroy the differential every claim in this file rests on.
    fn unsandboxed_policy(repo: &Path, case: &str) -> Policy {
        let policy = policy_for(repo, false, NetworkNeed::Local)
            .unwrap_or_else(|e| panic!("{case}: the baseline policy must build: {e}"));
        assert_eq!(
            policy.tier,
            Tier::Unsandboxed,
            "{case}: the baseline leg must run with no sandbox at all — it is the leg \
             that establishes the operation is possible on this host and supplies the \
             `Seccomp: 0` half of the differential. Getting a sandboxed tier here means \
             the operator-trust marker was not honoured."
        );
        assert!(
            policy.bwrap.is_none(),
            "{case}: an unsandboxed policy must launch no bwrap"
        );
        policy
    }

    // -----------------------------------------------------------------------
    // The hook — the only observer that runs inside the sandbox
    // -----------------------------------------------------------------------

    /// Written by every witness hook: proof it executed at all (INV-11).
    const MARK_RAN: &str = "gv-compat-ran";
    /// The kernel's own statement about the process the hook runs in
    /// (acceptance evidence (F)).
    const MARK_STATUS: &str = "gv-compat-status";
    /// What the `#!` line actually resolved to (INV-12).
    const MARK_INTERP: &str = "gv-compat-interp";
    /// Written **only** by the decoy in `.git/hooks`, and only if
    /// `core.hooksPath` was ignored.
    const MARK_DECOY: &str = "gv-compat-decoy";

    /// The witness hook body.
    ///
    /// Absolute paths, not relative ones: git runs `pre-commit` with the
    /// worktree root as its working directory, but baking the directory in
    /// removes any question of *which* worktree root that is for the linked-
    /// worktree and submodule geometries — and a marker written to the wrong
    /// place would read as "the hook did not run", the single most misleading
    /// failure this file could produce.
    ///
    /// `/proc/self/status` is read by `grep`, a *child* of the hook shell. That
    /// is deliberate and stronger than reading the shell's own entry: seccomp
    /// mode and `no_new_privs` are inherited across `fork`/`exec`, so a child
    /// reporting `Seccomp: 2` is the kernel stating the filter survives into
    /// whatever a hook spawns — which is what a hook actually does.
    fn witness_script(cwd: &Path, verdict: HookVerdict) -> String {
        let ran = cwd.join(MARK_RAN);
        let status = cwd.join(MARK_STATUS);
        let interp = cwd.join(MARK_INTERP);
        let code = match verdict {
            HookVerdict::Accepts => 0,
            HookVerdict::Rejects => 1,
        };
        format!(
            "echo ran > \"{ran}\"\n\
             grep -E '^(Seccomp|NoNewPrivs):' /proc/self/status > \"{status}\"\n\
             readlink -f /proc/$$/exe > \"{interp}\"\n\
             exit {code}\n",
            ran = ran.display(),
            status = status.display(),
            interp = interp.display(),
        )
    }

    /// The husky decoy: a hook in `.git/hooks` that must never run once
    /// `core.hooksPath` points elsewhere. It exits 0, so if `core.hooksPath`
    /// were ignored the commit would still succeed — the *only* thing that
    /// notices is this marker, which is why the marker exists.
    fn decoy_script(cwd: &Path) -> String {
        format!(
            "echo fired > \"{}\"\nexit 0\n",
            cwd.join(MARK_DECOY).display()
        )
    }

    /// Write `body` as an executable `pre-commit` hook in `hooks_dir`, with an
    /// explicit `#!` line (INV-12's subject).
    fn install_hook(hooks_dir: &Path, body: &str) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(hooks_dir)
            .map_err(|e| format!("cannot create {}: {e}", hooks_dir.display()))?;
        let hook = hooks_dir.join("pre-commit");
        std::fs::write(&hook, format!("#!/bin/sh\n{body}"))
            .map_err(|e| format!("cannot write {}: {e}", hook.display()))?;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot chmod {}: {e}", hook.display()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    pub(crate) struct Fixture {
        /// Kept alive for the lifetime of the case; dropping it removes the
        /// repositories.
        _dirs: Vec<tempfile::TempDir>,
        /// Where the case's `git commit` runs — a worktree root in every
        /// geometry.
        cwd: PathBuf,
        /// `Some` only for the husky geometry.
        decoy: Option<PathBuf>,
    }

    struct Built {
        fixture: Fixture,
        trust: TrustGuard,
    }

    /// A repository with a seed commit, no repo-local identity and the canonical
    /// `#!/bin/sh` + `0755` hook wrapper — `escape_suite::hostile_hook_repo`,
    /// the single constructor the lifecycle, non-coverage and compatibility
    /// batteries all name, so "the same fixture" means the same bytes across all
    /// of them.
    ///
    /// Its script argument is a placeholder that is immediately overwritten: the
    /// witness hook's marker paths are absolute and therefore unknowable until
    /// the temporary directory exists. Overwriting rather than reaching past
    /// `hostile_hook_repo` for its private `fixture`/`install_hook` pieces is
    /// what keeps this file on the shared constructor.
    const PLACEHOLDER_HOOK: &str = "exit 0";

    async fn build(case: &CompatCase) -> Result<Built, String> {
        let mut trust = TrustGuard::new();
        let witness = |cwd: &Path| witness_script(cwd, case.hook);

        let built = match case.geometry {
            Geometry::PlainHooksDir => {
                let dir = hostile_hook_repo(PLACEHOLDER_HOOK);
                let cwd = dir.path().to_path_buf();
                install_hook(&cwd.join(".git/hooks"), &witness(&cwd))?;
                Fixture {
                    _dirs: vec![dir],
                    cwd,
                    decoy: None,
                }
            }

            Geometry::HuskyHooksPath => {
                let dir = hostile_hook_repo(PLACEHOLDER_HOOK);
                let cwd = dir.path().to_path_buf();
                // The decoy goes where a non-husky repository keeps its hooks.
                install_hook(&cwd.join(".git/hooks"), &decoy_script(&cwd))?;
                // The real hook goes where husky keeps its.
                install_hook(&cwd.join(".husky"), &witness(&cwd))?;
                // …and the repo-local config is what has to make git choose the
                // second over the first. A relative value is husky's own shape;
                // `command_async` passes `-C <repo>`, so git's working directory
                // is the repository and `.husky` resolves inside it.
                append_config(&cwd.join(".git/config"), "[core]\n\thooksPath = .husky\n")?;
                let decoy = cwd.join(MARK_DECOY);
                Fixture {
                    _dirs: vec![dir],
                    cwd,
                    decoy: Some(decoy),
                }
            }

            Geometry::LinkedWorktree => {
                let dir = hostile_hook_repo(PLACEHOLDER_HOOK);
                let main = dir.path().to_path_buf();
                trust.grant(&main)?;
                let setup = unsandboxed_policy(&main, case.id);
                let added = git(
                    &setup,
                    &main,
                    &["worktree", "add", "-q", "linked", "-b", "gv-compat-wt"],
                )
                .await;
                if added.code != 0 {
                    return Err(format!(
                        "`git worktree add` failed on this host (exit {}): {}",
                        added.code, added.combined
                    ));
                }
                let cwd = main.join("linked");
                // A linked worktree whose git directory is *not* separate is not
                // the geometry this case claims to cover, so proving it is part
                // of establishing the premise, not decoration.
                let paths = repo_paths::resolve(&cwd)
                    .map_err(|e| format!("the linked worktree does not resolve: {e:?}"))?;
                if paths.gitdir == paths.commondir {
                    return Err(format!(
                        "this git creates no separate git directory for a linked worktree \
                         (gitdir == commondir == {}) — the geometry under test does not \
                         exist here",
                        paths.gitdir.display()
                    ));
                }
                // Hooks for a linked worktree live in the common directory.
                install_hook(&paths.commondir.join("hooks"), &witness(&cwd))?;
                Fixture {
                    _dirs: vec![dir],
                    cwd,
                    decoy: None,
                }
            }

            Geometry::SubmoduleParent => {
                let inner = hostile_hook_repo(PLACEHOLDER_HOOK);
                let outer_dir = hostile_hook_repo(PLACEHOLDER_HOOK);
                let outer = outer_dir.path().to_path_buf();
                trust.grant(&outer)?;
                let setup = unsandboxed_policy(&outer, case.id);
                let url = inner.path().display().to_string();
                let added = git(
                    &setup,
                    &outer,
                    &[
                        "-c",
                        "protocol.file.allow=always",
                        "submodule",
                        "add",
                        "-q",
                        url.as_str(),
                        "sub",
                    ],
                )
                .await;
                if added.code != 0 {
                    return Err(format!(
                        "`git submodule add` failed on this host (exit {}): {}",
                        added.code, added.combined
                    ));
                }
                // Without this the case degrades silently into a second
                // `commit_without_repo_identity`: a plain repository, no
                // submodule geometry in it, and a green result that says
                // nothing about submodules at all.
                let pointer = outer.join("sub/.git");
                let text = std::fs::read_to_string(&pointer).map_err(|e| {
                    format!(
                        "`git submodule add` left no `.git` pointer file at {} ({e}) — the \
                         geometry this case is about does not exist here",
                        pointer.display()
                    )
                })?;
                if !text.trim_start().starts_with("gitdir:") {
                    return Err(format!(
                        "the submodule's `.git` is not a `gitdir:` pointer ({text:?}) — this \
                         git kept the submodule's git directory inside the submodule, so the \
                         geometry under test does not exist here"
                    ));
                }
                install_hook(&outer.join(".git/hooks"), &witness(&outer))?;
                Fixture {
                    _dirs: vec![inner, outer_dir],
                    cwd: outer,
                    decoy: None,
                }
            }
        };

        trust.grant(&built.cwd)?;
        Ok(Built {
            fixture: built,
            trust,
        })
    }

    fn append_config(config: &Path, section: &str) -> Result<(), String> {
        let mut text = std::fs::read_to_string(config)
            .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
        text.push_str(section);
        std::fs::write(config, text).map_err(|e| format!("cannot write {}: {e}", config.display()))
    }

    // -----------------------------------------------------------------------
    // Running one leg
    // -----------------------------------------------------------------------

    struct GitRun {
        code: i32,
        stdout: String,
        combined: String,
    }

    /// The one spawn seam (R6) and the one environment (R7). Every git this file
    /// runs — fixture setup included — goes through here.
    async fn git(policy: &Policy, cwd: &Path, args: &[&str]) -> GitRun {
        let out = command_async(policy, cwd, args)
            .pinned_env_for_test(&production_env_profile())
            .output()
            .await;
        match out {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                GitRun {
                    code: out.status.code().unwrap_or(-1),
                    combined: format!("{stdout}{stderr}"),
                    stdout,
                }
            }
            Err(e) => GitRun {
                code: -1,
                stdout: String::new(),
                combined: format!("the launcher could not be spawned: {e}"),
            },
        }
    }

    /// Everything one leg observed. Built only by [`observe`], which returns
    /// `Result` — so a missing observation is a typed error the compiler forces
    /// the caller to handle, never an `Option` a negation could satisfy (R2).
    struct Leg {
        commit_code: i32,
        seccomp: u32,
        no_new_privs: u32,
        interpreter: String,
        subjects: Vec<String>,
        author_email: String,
        global_email: String,
    }

    /// Which leg is running. Only used for diagnostics — the *expectations* are
    /// never a function of this, they are a function of the case declaration and
    /// of the differential.
    #[derive(Clone, Copy)]
    enum Which {
        Baseline,
        Inside,
    }

    impl Which {
        fn label(self) -> &'static str {
            match self {
                Which::Baseline => "baseline (Tier::Unsandboxed)",
                Which::Inside => "inside (Tier::Strict)",
            }
        }
    }

    fn clear_markers(f: &Fixture) {
        for name in [MARK_RAN, MARK_STATUS, MARK_INTERP, MARK_DECOY] {
            let _ = std::fs::remove_file(f.cwd.join(name));
        }
    }

    fn read_marker(f: &Fixture, name: &str, leg: Which) -> Result<String, String> {
        let path = f.cwd.join(name);
        std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .map_err(|e| {
                format!(
                    "{}: the pre-commit hook left no `{name}` marker at {} ({e}). \
                     The hook is the only observer that runs inside the sandbox, so an \
                     absent marker means the hook did not run — never that there was \
                     nothing to observe.",
                    leg.label(),
                    path.display()
                )
            })
    }

    /// Pull one `/proc/self/status` field out of the hook's own capture. R2: an
    /// exact named field, parsed to a number, compared with `assert_eq!` — never
    /// a substring match on the raw text.
    fn status_field(raw: &str, name: &str, leg: Which) -> Result<u32, String> {
        for line in raw.lines() {
            let Some(rest) = line.trim().strip_prefix(name) else {
                continue;
            };
            let Some(value) = rest.trim_start_matches(':').split_whitespace().next() else {
                continue;
            };
            if let Ok(n) = value.parse::<u32>() {
                return Ok(n);
            }
        }
        Err(format!(
            "{}: the hook's /proc/self/status capture has no numeric `{name}` field: {raw:?}",
            leg.label()
        ))
    }

    /// The message this leg's commit carries. Distinct per leg so the effect
    /// check can tell *which* leg's commit landed, on a repository both legs
    /// operate on in turn.
    fn subject(case: &CompatCase, leg: Which) -> String {
        let tag = match leg {
            Which::Baseline => "baseline",
            Which::Inside => "inside",
        };
        format!("gv-compat {} {tag}", case.id)
    }

    /// Run one leg and collect every observation the case could need.
    async fn observe(
        case: &CompatCase,
        f: &Fixture,
        policy: &Policy,
        leg: Which,
    ) -> Result<Leg, String> {
        clear_markers(f);
        let msg = subject(case, leg);
        let commit = git(
            policy,
            &f.cwd,
            &["commit", "--allow-empty", "-m", msg.as_str()],
        )
        .await;

        if let Some(decoy) = &f.decoy {
            if std::fs::read_to_string(decoy).is_ok() {
                return Err(format!(
                    "{}: the decoy hook in `.git/hooks` fired, so `core.hooksPath` was not \
                     honoured. The husky shape is not under test on this host — and a case \
                     that passed on the decoy's markers would be proving that *a* hook ran, \
                     not that the repo-local hooks directory did.",
                    leg.label()
                ));
            }
        }

        read_marker(f, MARK_RAN, leg)?;
        let raw_status = read_marker(f, MARK_STATUS, leg)?;
        let seccomp = status_field(&raw_status, "Seccomp", leg)?;
        let no_new_privs = status_field(&raw_status, "NoNewPrivs", leg)?;
        let interpreter = read_marker(f, MARK_INTERP, leg)?;

        let log = git(policy, &f.cwd, &["log", "--format=%s"]).await;
        if log.code != 0 {
            return Err(format!(
                "{}: `git log` failed under the same policy (exit {}): {}",
                leg.label(),
                log.code,
                log.combined
            ));
        }
        let subjects = log
            .stdout
            .lines()
            .map(str::trim)
            .map(String::from)
            .collect();

        let author = git(policy, &f.cwd, &["log", "-1", "--format=%ae"]).await;
        if author.code != 0 {
            return Err(format!(
                "{}: `git log -1 --format=%ae` failed under the same policy (exit {}): {}",
                leg.label(),
                author.code,
                author.combined
            ));
        }

        let global_email = match case.identity {
            IdentityClaim::NotClaimed => String::new(),
            IdentityClaim::FromGlobalConfig => {
                let cfg = git(policy, &f.cwd, &["config", "--global", "user.email"]).await;
                if cfg.code != 0 {
                    return Err(format!(
                        "{}: `git config --global user.email` failed under the same policy \
                         (exit {}): {}. INV-2's positive half is that ~/.gitconfig is \
                         reachable through the policy's read-only $HOME grant; if the \
                         baseline leg reports this, the host has no global identity and \
                         the case has no premise.",
                        leg.label(),
                        cfg.code,
                        cfg.combined
                    ));
                }
                cfg.stdout.trim().to_string()
            }
        };

        Ok(Leg {
            commit_code: commit.code,
            seccomp,
            no_new_privs,
            interpreter,
            subjects,
            author_email: author.stdout.trim().to_string(),
            global_email,
        })
    }

    // -----------------------------------------------------------------------
    // The two verdicts
    // -----------------------------------------------------------------------

    fn landed(leg: &Leg, case: &CompatCase, which: Which) -> bool {
        leg.subjects.iter().any(|s| *s == subject(case, which))
    }

    /// R4': did the baseline leg actually perform the operation? Everything the
    /// inside leg claims is measured against this, so a baseline that missed its
    /// declared outcome means the case has no premise — reported as
    /// `CapabilityAbsent` naming what was missing, and then raised.
    fn premise(case: &CompatCase, base: &Leg) -> Result<(), String> {
        if base.commit_code != case.expect_commit_code {
            return Err(format!(
                "with no sandbox at all, `git commit` exited {} where the case declares {} \
                 — the operation this case is about does not behave as declared on this \
                 host, so nothing the sandboxed leg does could be attributed to the policy",
                base.commit_code, case.expect_commit_code
            ));
        }
        let landed_now = landed(base, case, Which::Baseline);
        let should_land = case.expect_commit == CommitOutcome::Lands;
        if landed_now != should_land {
            return Err(format!(
                "with no sandbox at all, the commit {} where the case declares it {}",
                if landed_now { "landed" } else { "did not land" },
                if should_land {
                    "lands"
                } else {
                    "does not land"
                },
            ));
        }
        if base.seccomp != 0 {
            return Err(format!(
                "the baseline leg already runs under a seccomp filter (Seccomp: {}). \
                 The whole differential is `Seccomp: 0` outside vs `Seccomp: 2` inside; \
                 with an outer filter already present, a `2` inside cannot be attributed \
                 to this sandbox rather than to whatever container the suite is running \
                 in (acceptance evidence (F) of the anti-vacuity contract)",
                base.seccomp
            ));
        }
        if case.identity == IdentityClaim::FromGlobalConfig {
            if base.global_email.is_empty() {
                return Err(
                    "this host's ~/.gitconfig carries no `user.email`, so `git commit` with \
                     no repo-local identity has nothing to resolve and the case has no \
                     premise"
                        .to_string(),
                );
            }
            if base.author_email != base.global_email {
                return Err(format!(
                    "with no sandbox at all, the commit's author `{}` is not the global \
                     identity `{}` — git resolved an author from somewhere else, so \
                     equality inside the sandbox would prove nothing about ~/.gitconfig",
                    base.author_email, base.global_email
                ));
            }
        }
        Ok(())
    }

    /// The claim. Everything here is about the **inside** leg, and every check
    /// is either an exact declared value or an equality with the baseline — no
    /// predicate, no negation, no "non-empty".
    fn verdict(case: &CompatCase, base: &Leg, inside: &Leg) -> Result<(), String> {
        // (F) The kernel's own self-report, across the execve. This is what
        // makes everything below attributable to the policy: it is the kernel
        // stating that the exact post-exec process git ran in is under a filter,
        // and the baseline's `Seccomp: 0` (checked in `premise`) rules out an
        // outer profile being credited to Git-Vista.
        if inside.seccomp != 2 {
            return Err(format!(
                "the hook inside the sandbox reports `Seccomp: {}`, not 2 \
                 (SECCOMP_MODE_FILTER). The operation may have worked, but it did not work \
                 *under the filter*, so this case proves nothing about the policy",
                inside.seccomp
            ));
        }
        if inside.no_new_privs != 1 {
            return Err(format!(
                "the hook inside the sandbox reports `NoNewPrivs: {}`, not 1",
                inside.no_new_privs
            ));
        }

        if inside.commit_code != case.expect_commit_code {
            return Err(format!(
                "under the policy, `git commit` exited {} where the case declares {} \
                 (the identical command exited {} with no sandbox)",
                inside.commit_code, case.expect_commit_code, base.commit_code
            ));
        }

        let landed_now = landed(inside, case, Which::Inside);
        let should_land = case.expect_commit == CommitOutcome::Lands;
        if landed_now != should_land {
            return Err(format!(
                "under the policy the commit {} where the case declares it {} \
                 (with no sandbox, the same fixture behaved as declared)",
                if landed_now { "landed" } else { "did not land" },
                if should_land {
                    "lands"
                } else {
                    "does not land"
                },
            ));
        }

        if case.identity == IdentityClaim::FromGlobalConfig {
            if inside.global_email != base.global_email {
                return Err(format!(
                    "under the policy `git config --global user.email` reported `{}`, but \
                     with no sandbox it reported `{}` — the policy is not serving the same \
                     ~/.gitconfig",
                    inside.global_email, base.global_email
                ));
            }
            if inside.author_email != inside.global_email {
                return Err(format!(
                    "under the policy the commit's author is `{}` but the global identity \
                     is `{}`. The repository has no local identity, so git invented an \
                     author instead of reading ~/.gitconfig through the read-only $HOME \
                     grant — INV-2's positive half is broken (D5 Option B is what is \
                     supposed to stop the round-4 cascade here)",
                    inside.author_email, inside.global_email
                ));
            }
        }

        if case.interpreter == InterpreterClaim::MustMatchBaseline
            && inside.interpreter != base.interpreter
        {
            return Err(format!(
                "the hook's `#!` line resolved to `{}` under the policy but to `{}` with no \
                 sandbox. INV-12: toolchain paths are declared, not discovered — a hook \
                 silently resolving a different interpreter inside the sandbox and \
                 reporting success is the exact F6 failure this case exists to catch",
                inside.interpreter, base.interpreter
            ));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // The runner
    // -----------------------------------------------------------------------

    async fn execute(case: &CompatCase) -> Outcome {
        let built = match build(case).await {
            Ok(b) => b,
            Err(missing) => return Outcome::CapabilityAbsent { missing },
        };
        let Built { fixture, trust } = built;

        // Leg 1: no sandbox. Establishes the premise by performing the
        // operation (R4'), and supplies the `Seccomp: 0` half of the
        // differential.
        let base_policy = unsandboxed_policy(&fixture.cwd, case.id);
        let base = match observe(case, &fixture, &base_policy, Which::Baseline).await {
            Ok(leg) => leg,
            Err(missing) => return Outcome::CapabilityAbsent { missing },
        };
        if let Err(missing) = premise(case, &base) {
            return Outcome::CapabilityAbsent { missing };
        }

        // The single input that differs between the legs. Dropped explicitly,
        // not at end of scope, so the trust markers are gone *before* the
        // production dispatch is asked for the second policy.
        drop(trust);

        // Leg 2: the same fixture, the same argv, the same environment, the
        // same spawn seam — under the Strict tier. `strict_baseline` delegates
        // to `shim_cli::production_policy` (R6: this file builds no policy of
        // its own), asserts the tier it got back, and proves the composed
        // launcher runs on this host by running `git --version` through it.
        let strict = strict_baseline(&fixture.cwd, case.id).await;
        let inside = match observe(case, &fixture, &strict, Which::Inside).await {
            Ok(leg) => leg,
            Err(detail) => return Outcome::FunctionalBroken { detail },
        };

        match verdict(case, &base, &inside) {
            Ok(()) => Outcome::FunctionalOk,
            Err(detail) => Outcome::FunctionalBroken { detail },
        }
    }

    /// The single statement every `#[test]` in this file consists of.
    ///
    /// The record is written **before** the verdict is raised, so a future CI
    /// job that consumes `$GV_COMPAT_REPORT` sees every case's result including
    /// the failing ones. The raise itself is unconditional for both non-OK
    /// outcomes — see deviation 1 in the module doc for why capability absence
    /// is loud here rather than a green record.
    pub(crate) fn run_compat_case(case: &CompatCase) {
        let rt = tokio::runtime::Runtime::new().expect("a tokio runtime for the compat battery");
        let outcome = rt.block_on(execute(case));
        report(case, &outcome);
        match &outcome {
            Outcome::FunctionalOk => {}
            Outcome::FunctionalBroken { detail } => panic!(
                "COMPAT BROKEN [{}]: a real git workflow does not work under the policy.\n{}\n\
                 This is a finding, not a flake: a sandbox that breaks real workflows gets \
                 turned off. Do not loosen the policy to make this green without recording \
                 the weakening and its reason.",
                case.id, detail
            ),
            Outcome::CapabilityAbsent { missing } => panic!(
                "COMPAT PREMISE ABSENT [{}]: the baseline leg — which runs with no sandbox \
                 at all — could not establish what this case is about.\n{}\n\
                 This is deliberately a hard failure and never a skip: nothing consumes \
                 $GV_COMPAT_REPORT yet, so a green run here would be a case that quietly \
                 stopped testing anything.",
                case.id, missing
            ),
        }
    }
}

use harness::{
    run_compat_case, CommitOutcome, CompatCase, Geometry, HookVerdict, IdentityClaim,
    InterpreterClaim,
};

// INV-11, first half, in husky's own shape: `core.hooksPath` points at a
// repo-local `.husky` directory, and the hook there is the one that runs. A
// decoy in `.git/hooks` proves it — see `Geometry::HuskyHooksPath`.
const CASE_HUSKY_HOOK_RUNS: CompatCase = CompatCase {
    id: "husky_hook_runs",
    geometry: Geometry::HuskyHooksPath,
    hook: HookVerdict::Accepts,
    expect_commit_code: 0,
    expect_commit: CommitOutcome::Lands,
    identity: IdentityClaim::NotClaimed,
    interpreter: InterpreterClaim::NotClaimed,
};
#[test]
fn husky_hook_runs() {
    run_compat_case(&CASE_HUSKY_HOOK_RUNS);
}

// INV-11, second half — "round 4 never ran the gating half". Its own case and
// its own `#[test]`, so a future change to one leg cannot silently stop
// exercising the other. R2: the exact code git returns for a rejecting hook,
// never `assert_ne!(code, 0)`.
const CASE_HUSKY_HOOK_GATES: CompatCase = CompatCase {
    id: "husky_hook_gates",
    geometry: Geometry::HuskyHooksPath,
    hook: HookVerdict::Rejects,
    expect_commit_code: 1,
    expect_commit: CommitOutcome::DoesNotLand,
    identity: IdentityClaim::NotClaimed,
    interpreter: InterpreterClaim::NotClaimed,
};
#[test]
fn husky_hook_gates() {
    run_compat_case(&CASE_HUSKY_HOOK_GATES);
}

// INV-2's positive half and A13: nine of the twenty-four repositories on the
// development host have no local `user.email`, and round 4's blanket-deny
// policy needed three iterations to make one commit work. The author must equal
// what `git config --global user.email` returns *through the same policy* — a
// non-empty address would also be satisfied by git inventing `user@host` when
// it cannot read ~/.gitconfig at all.
const CASE_COMMIT_WITHOUT_IDENTITY: CompatCase = CompatCase {
    id: "commit_without_repo_identity",
    geometry: Geometry::PlainHooksDir,
    hook: HookVerdict::Accepts,
    expect_commit_code: 0,
    expect_commit: CommitOutcome::Lands,
    identity: IdentityClaim::FromGlobalConfig,
    interpreter: InterpreterClaim::NotClaimed,
};
#[test]
fn commit_without_repo_identity() {
    run_compat_case(&CASE_COMMIT_WITHOUT_IDENTITY);
}

// INV-10's positive pair and A14: a linked worktree's git directory lives under
// the main repository's common directory, outside the worktree, and the policy
// has to grant both. A commit rather than a `status`: it reads *and* writes
// both directories, which is the whole geometry.
const CASE_LINKED_WORKTREE_COMMIT: CompatCase = CompatCase {
    id: "linked_worktree_commit",
    geometry: Geometry::LinkedWorktree,
    hook: HookVerdict::Accepts,
    expect_commit_code: 0,
    expect_commit: CommitOutcome::Lands,
    identity: IdentityClaim::NotClaimed,
    interpreter: InterpreterClaim::NotClaimed,
};
#[test]
fn linked_worktree_commit() {
    run_compat_case(&CASE_LINKED_WORKTREE_COMMIT);
}

// INV-10's other positive pair, scoped to the claim production actually makes:
// a repository that CONTAINS a submodule still works under the policy. Running
// *inside* the submodule is refused by `policy_for` — a decision `worktree.rs`
// records deliberately, measured here on 2026-07-30, and written up as a named
// gap in this file's module doc rather than asserted against.
const CASE_SUBMODULE_PARENT_COMMIT: CompatCase = CompatCase {
    id: "submodule_parent_commit",
    geometry: Geometry::SubmoduleParent,
    hook: HookVerdict::Accepts,
    expect_commit_code: 0,
    expect_commit: CommitOutcome::Lands,
    identity: IdentityClaim::NotClaimed,
    interpreter: InterpreterClaim::NotClaimed,
};
#[test]
fn submodule_parent_commit() {
    run_compat_case(&CASE_SUBMODULE_PARENT_COMMIT);
}

// INV-12 / F6: the interpreter the hook's `#!` line resolves to inside the
// sandbox must be the same file it resolves to outside. The sharp part of F6 is
// the silent fall-through — a hook resolving a different interpreter and
// reporting success — so the claim is an equality between the two legs, not a
// "the hook ran" check that a substituted interpreter would also satisfy.
const CASE_INTERPRETER_IDENTITY: CompatCase = CompatCase {
    id: "interpreter_identity",
    geometry: Geometry::PlainHooksDir,
    hook: HookVerdict::Accepts,
    expect_commit_code: 0,
    expect_commit: CommitOutcome::Lands,
    identity: IdentityClaim::NotClaimed,
    interpreter: InterpreterClaim::MustMatchBaseline,
};
#[test]
fn interpreter_identity() { if true {}
    run_compat_case(&CASE_INTERPRETER_IDENTITY);
}

mod contract {
    //! The source tripwires for this file, and the census.
    //!
    //! Same argument as `escape_contract.rs`: a standard that lives in a
    //! document is not open during the next rewrite. These read this file's own
    //! source and the census beside it, so a case that stops being declarative,
    //! a leg that stops going through the production seam, or a case that is
    //! renamed out of the census breaks the **build**.
    //!
    //! Deliberately no `use super::*`: these tests read this file as *text*, and
    //! importing its items would let one of them accidentally exercise the
    //! battery instead of scanning it.

    /// R11: every rule this file claims to honour, paired with the test that
    /// enforces it. A rule whose enforcement is deleted fails
    /// [`r11_every_rule_names_a_live_test`].
    const RULES: &[(&str, &str)] = &[
        ("R1-DECLARATIVE", "r1_the_case_region_is_declarative"),
        (
            "R4-CAPABILITY-BY-EXECUTION",
            "r4_capability_is_never_established_by_asking_the_host",
        ),
        (
            "R5-REPORT-FILE-CENSUS",
            "the_census_names_exactly_the_declared_cases",
        ),
        (
            "R6-PRODUCTION-SEAM",
            "r6_every_leg_goes_through_the_production_seam",
        ),
        (
            "R7-ONE-ENVIRONMENT",
            "r7_both_legs_share_one_pinned_environment_profile",
        ),
        ("R11-SELF-BINDING", "r11_every_rule_names_a_live_test"),
    ];

    const CENSUS: &str = include_str!("../../../../docs/sandbox/compat-census.txt");

    fn source() -> String {
        include_str!("compat.rs").to_string()
    }

    /// This file's source with comments and string literals blanked, reusing
    /// `argv_boundary::code_only` rather than re-implementing the blanking —
    /// two copies of that logic would drift, and the tripwires' own message
    /// strings would otherwise match the patterns they scan for.
    fn code() -> String {
        crate::argv_boundary::code_only(&source())
    }

    /// Everything outside the two fenced modules — the region R1 restricts.
    ///
    /// `escape_contract` splits at a single `mod harness`; this file has a
    /// second fenced module for the tripwires, so both are removed. The split
    /// is by name and brace matching, on comment-blanked source, so a `mod
    /// harness` mentioned in prose cannot move the boundary.
    fn case_region() -> String {
        let mut region = code();
        for name in ["mod harness", "mod contract"] {
            region = strip_block(&region, name);
        }
        region
    }

    fn strip_block(code: &str, marker: &str) -> String {
        let at = code
            .find(marker)
            .unwrap_or_else(|| panic!("`{marker}` not found — the file's fencing changed"));
        let open = at
            + code[at..]
                .find('{')
                .unwrap_or_else(|| panic!("`{marker}` has no body"));
        let mut depth = 0usize;
        let mut close = None;
        for (i, ch) in code[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.unwrap_or_else(|| panic!("unbalanced braces in `{marker}`"));
        format!("{}{}", &code[..at], &code[close..])
    }

    fn tokens(code: &str) -> std::collections::HashSet<&str> {
        code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// The `CompatCase` ids declared in this file, in source order.
    ///
    /// Read off the raw source, because the id *is* a string literal and
    /// `code()` blanks those. The only `id: "…"` lines in the file are the case
    /// constants — the struct's own field is `pub id: &'static str` and matches
    /// nothing here — so this cannot be padded by the harness or the tripwires
    /// without a deliberate edit that a reviewer sees in the same diff as the
    /// census it would be padding.
    fn declared_ids() -> Vec<String> {
        let raw = source();
        let mut ids = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("id: \"") else {
                continue;
            };
            let Some(end) = rest.find('"') else { continue };
            ids.push(rest[..end].to_string());
        }
        ids
    }

    /// R1: outside `mod harness` and `mod contract`, this file may contain only
    /// `const CASE_X: CompatCase` declarations and one-statement `#[test]`
    /// bodies. This is the enabling rule — you cannot grep "this assertion
    /// accepts a family of values" out of freeform Rust, but you can grep
    /// "there is no assertion here at all".
    #[test]
    fn r1_the_case_region_is_declarative() {
        let region = case_region();
        let idents = tokens(&region);
        for banned in ["if", "match", "return", "while", "for", "loop"] {
            assert!(
                !idents.contains(banned),
                "compat.rs: `{banned}` found outside `mod harness`/`mod contract` (R1). \
                 Acceptance conditions belong in the runner, where there is exactly one \
                 of them to review."
            );
        }
        // Substring needles rather than tokens, because these are the shapes an
        // acceptance condition or an escape hatch takes. `.expect(`/`.unwrap(`
        // carry the leading dot so the declarative field names
        // `expect_commit_code`/`expect_commit` are not mistaken for them.
        for banned in ["assert", "eprintln", "println", ".unwrap(", ".expect("] {
            assert!(
                !region.contains(banned),
                "compat.rs: `{banned}` found outside `mod harness`/`mod contract` (R1)"
            );
        }
        // Every `#[test]` body is exactly one `run_compat_case(&CASE_X);`.
        let tests = region.matches("#[test]").count();
        let calls = region.matches("run_compat_case(&CASE_").count();
        assert_eq!(
            tests, calls,
            "compat.rs: {tests} `#[test]`s but {calls} `run_compat_case(&CASE_…)` calls in \
             the case region — a test body that is not exactly one runner call has an \
             acceptance condition of its own (R1)"
        );
        assert!(
            !region.contains("..Default::default()"),
            "compat.rs: a case must write out every field, never inherit one (R1)"
        );
    }

    /// R4': capability is established by performing the operation, never by
    /// asking the host what it can do. `shim_cli::strict_available()` returns
    /// true when a bwrap binary merely exists on disk; the honest prober widens
    /// the skip instead. Neither is available to this file, by scan.
    #[test]
    fn r4_capability_is_never_established_by_asking_the_host() {
        let code = code();
        for banned in [
            "strict_available",
            "capabilities::probe",
            "capabilities::current",
            "bwrap_path",
            ".exists(",
            ".is_dir(",
            ".is_file(",
        ] {
            assert!(
                !code.contains(banned),
                "compat.rs: `{banned}` is a host query, and a case that decides what to \
                 attempt by asking the host can report a pass it never earned (R4'). The \
                 baseline leg performs the operation instead."
            );
        }
        // The one place capability may be concluded, and it is a value, not a
        // control-flow shortcut around the rest of the case.
        assert!(
            code.contains("fn premise"),
            "compat.rs: `premise` is where a missing capability is decided from what the \
             baseline leg actually did — R4' has no other route"
        );
    }

    /// R5/census: the census and the declared cases name the same set, in both
    /// directions. Equality rather than a floor, because a floor catches
    /// deletion but not substitution — and as a *build* failure rather than a
    /// job assertion, because `cargo test <filter>` exits 0 on
    /// "0 filtered out" and a renamed module empties a filter-based gate
    /// silently.
    #[test]
    fn the_census_names_exactly_the_declared_cases() {
        let declared: std::collections::BTreeSet<String> = declared_ids().into_iter().collect();
        assert!(
            !declared.is_empty(),
            "compat.rs: the id scan found no cases — the scan broke, which would make the \
             census check vacuous"
        );
        let census: std::collections::BTreeSet<String> = CENSUS
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        for line in CENSUS.lines() {
            assert!(
                !line.trim_start().starts_with('#'),
                "docs/sandbox/compat-census.txt: bare ids only, one per line, no comments — \
                 the CI gate the plan assigns to another lane consumes it with `sort | diff` \
                 and a comment line would show up as a missing case"
            );
        }
        assert_eq!(
            census, declared,
            "docs/sandbox/compat-census.txt does not match the cases declared in compat.rs. \
             Left is the census, right is the source. A renamed case must be renamed in \
             both, or the gate silently stops covering it."
        );
    }

    /// R6: every leg — both of them, and fixture setup too — reaches git through
    /// `sandbox::spawn::command_async`, the seam production uses. This file
    /// constructs no `Command` at all, which is also why it needs no entry in
    /// `argv_boundary.rs`'s allowlist.
    #[test]
    fn r6_every_leg_goes_through_the_production_seam() {
        let code = code();
        // Assembled at runtime so this file's own source never contains the
        // bare pattern the scan looks for.
        let spawn = ["Command", "::new("].concat();
        assert!(
            !code.contains(&spawn),
            "compat.rs: this file must construct no Command of its own — every git goes \
             through `spawn::command_async`, which is what makes a compat claim a claim \
             about the shipped launcher (R6)"
        );
        for banned in ["shim_cli::launch", "shim_cli::workable"] {
            assert!(
                !code.contains(banned),
                "compat.rs: `{banned}` is not a route this file may take (R6). A compat \
                 case that only passes under a policy the escape battery does not also use \
                 proves nothing about the shipped configuration."
            );
        }
        // A `Policy` *literal* would be a second policy builder wearing a
        // different hat. `-> Policy {` is a function signature, not a
        // construction, and is subtracted rather than special-cased away.
        let literal = ["Policy", " {"].concat();
        let signature = ["-> Policy", " {"].concat();
        assert_eq!(
            code.matches(&literal).count() - code.matches(&signature).count(),
            0,
            "compat.rs: a `Policy` literal here is a compat-only policy — R3'/R6 require \
             the inside leg to run under the same constructor the escape battery uses"
        );
        assert!(
            code.contains("command_async(policy, cwd, args)"),
            "compat.rs: the single spawn helper must call `command_async` (R6)"
        );
        // R3', reinterpreted: the inside leg's policy is the escape battery's,
        // not a second, more permissive, compat-only one.
        assert!(
            code.contains("strict_baseline(&fixture.cwd, case.id)"),
            "compat.rs: the inside leg must take its Strict policy from \
             `lifecycle::strict_baseline`, which delegates to \
             `shim_cli::production_policy` — the identical constructor the escape \
             battery's Strict cases use (R3', R6)"
        );
    }

    /// R7: one pinned environment profile, shared by both legs, and no
    /// incremental environment edits anywhere.
    #[test]
    fn r7_both_legs_share_one_pinned_environment_profile() {
        let code = code();
        assert!(
            !code.contains("env_clear"),
            "compat.rs: the environment is applied as a unit by \
             `pinned_env_for_test(&production_env_profile())`, never cleared here (R7)"
        );
        assert_eq!(
            code.matches("production_env_profile()").count(),
            1,
            "compat.rs: exactly one call site for the pinned profile, so the two legs \
             cannot drift apart in what git sees (R7)"
        );
        assert_eq!(
            code.matches("std::env::var").count(),
            1,
            "compat.rs: the only environment read is `$GV_COMPAT_REPORT` in `report` — a \
             case that read the environment could change what it tests per developer (R4/R7)"
        );
    }

    /// R11: a rule whose enforcement is deleted or renamed fails the build.
    /// `argv_boundary.rs` is the working precedent — a scan that reads its own
    /// source and refuses to be quietly narrowed.
    #[test]
    fn r11_every_rule_names_a_live_test() {
        let src = source();
        for (rule, test_fn) in RULES {
            let marker = format!("fn {test_fn}(");
            assert!(
                src.contains(&marker),
                "compat.rs: {rule} names `{test_fn}`, which no longer exists. Either the \
                 rule is no longer enforced (delete the claim, deliberately) or the test \
                 was renamed (fix the registry) — silence is the one option this registry \
                 removes."
            );
        }
    }
}
