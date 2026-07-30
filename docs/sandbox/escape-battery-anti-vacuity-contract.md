# The escape-battery anti-vacuity contract (#66, M1.13b)

Derived 2026-07-29 from audit C11 (`codex-result-C11.md`: **0 PROVES, 4 VACUOUS, 1 UNCERTAIN**)
by a four-agent design workflow — a contract angle, a feasibility angle, an adversarial
pre-mortem, and a synthesis that cross-checked the first against the third.

**Why a contract and not just a rewrite.** This battery has now failed twice. C8 found it
vacuous; it was competently rewritten; C11 found that for the io_uring test the defect
"merely moved from the inside assertion to the baseline gate." Relocated, not removed. A
third rewrite guided by care rather than by a checkable standard relocates it again.

---

## The rules

### R1-DECLARATIVE  ·  _source-tripwire_

**Rule.** Every case in the battery is a `const CASE_X: EscapeCase` value, and every `#[test]` body is exactly one statement: `run_case(&CASE_X)`. Outside the single `mod harness`, the battery file contains zero `assert*!`, zero `return`, zero `if`/`match`, zero `||`/`&&` in code position, zero `eprintln!`, zero `std::env::var`. `EscapeCase` has no `Default` impl and no `..Default::default()` is permitted, so every field must be written out per case.

**Why.** Every one of the four VACUOUS verdicts is a bespoke acceptance condition an author hand-wrote: `contains("Permission denied") || contains("READ_FAIL")` (escape_suite.rs:203-207), `!= Some(0)` (:425), `match errno { Some(0)=>{} other=>{eprintln;return} }` (:243-248). You cannot grep "this assertion accepts a family of values" out of freeform Rust; you can grep "there are no assertions here at all." This is the enabling rule — R2, R3, R4 are only mechanically checkable because authors have no syntax left in which to express an acceptance condition. It is also the answer to the relocation problem: after R1 there is exactly one place a defect can live, `run_case`, and one place is reviewable. Checked by a new `#[cfg(test)] sandbox/escape_contract.rs` reusing `argv_boundary::code_only` (promoted to `pub(crate)`) and `rs_files`, splitting the file at the `mod harness` marker and asserting forbidden-token counts are 0 in the case region, plus `#[test]` count == `run_case(` count.

### R2-EXACT-OBSERVATION  ·  _source-tripwire_

**Rule.** An expectation is a single named errno constant compared with `assert_eq!` — never a set, predicate, closure, negation, or string match. The probe-output parser returns `Result<i32, MissingObservation>`, never `Option<i32>`. Every probe emits `GVPROBE <nonce> BEGIN` and `GVPROBE <nonce> END` in a format the harness owns and substitutes; the runner requires both lines with the matching nonce in both legs before evaluating any expectation. `expect_carrier_code: i32` (the commit's exit status) is a mandatory field asserted in both legs.

**Why.** `errno_for` returns `None` when the tag is absent (escape_suite.rs:111-120), and `assert_ne!(.., Some(0))` at :425 is satisfied by `None` — which means the hook never ran, `cc` produced a broken binary, the shim exited 90 on argv, bwrap failed, `socket()` failed, or the commit died before hook discovery. Six ways to learn nothing, all scored as containment. Only `blocked_hooks` checks `commit_code` (:335, :351), which is why it is the one sound functional test — R2 generalises the single thing the last rewrite got right. `Result` cannot be compared to `Errno` without handling the error arm, so the compiler enforces the hard part; the tripwire only has to assert `-> Option<i32>` appears nowhere and that no `assert_ne!`/`.contains(`/`is_some()` survives in the case region. Clippy `-D warnings` independently catches the declared-but-unused `EACCES = 13` at :36.

### R3-PAIRED-POSITIVE  ·  _source-tripwire_

**Rule.** Every case declares BOTH a denial expectation and a grant expectation, observed by the same probe binary, in the same run, under the same policy, and both are asserted. For a filesystem case the grant must be a sibling entry under the SAME granted tree as the denied path. `expect_granted: Errno(0)` is a mandatory field with no default.

**Why.** This is the rule the pre-mortem produced and the other two angles missed, and I verified it is decisive. `enumerate()` (bin/gv-sandbox/main.rs:386-441) implements exclusion by *skipping entries during enumeration* — Landlock is deny-by-default and no deny rule is ever added. Therefore EACCES-on-an-excluded-path and EACCES-on-a-never-granted-path are literally the same kernel event. Angle 1's FIX-1 ("move the secret to a controlled temp tree") would, on its own, make the test pass with `secret_excludes_for_home()` returning `Vec::new()` — it destroys the one property that currently makes the assertion attributable (that `$HOME` IS granted, so a missing exclude set would leak the secret). A denial claim without a paired positive in the same granted tree does not test the exclude list at all; it tests that `--exclude` parses, which `shim_cli.rs:171-181` already covers. Missing positive is a failure, never a skip. Tripwire checks the field exists and is asserted in `run_case`; M3 in R9 proves it bites.

### R4-CAPABILITY-BY-EXECUTION  ·  _source-tripwire_

**Rule.** Capability is established only by a baseline leg that actually performed the operation in this process run and produced its exact declared `expect_baseline: Errno`. Querying the host is forbidden in the battery: no `strict_available`, `bwrap_path`, `capabilities::probe`, `.exists()`, `.is_dir()`, `env::var("HOME")`, `var_os("HOME")`. A baseline that misses its declared value yields `Outcome::CapabilityAbsent{case, missing}` as a RETURN VALUE from `run_case`; the inside leg does not run; nothing is printed and nothing returns early.

**Why.** `shim_cli::strict_available()` (shim_cli.rs:65-67) returns true when a bwrap binary merely exists on disk, while `capabilities::strict_available()` (capabilities.rs:53-55) correctly requires Landlock floor AND bwrap AND userns — on stock Ubuntu 24.04 the weak check says yes, bwrap then fails to launch, no CONNECT line is emitted, and `assert_ne!(None, Some(0))` passes. That is a false green available today, not a hypothesis. Note the trap the pre-mortem caught: simply pointing the helper at the honest prober *widens* the skip, converting an executing test into a permanently green non-test on the most common developer distro. Hence the rule is not "use a better probe" but "delete probing and make the baseline the only capability evidence," with R5 making absence non-green in the job. The high-bit test's baseline (`is_some() && != Some(EPERM)`, :291-297) accepts 0, ENOSYS, EINVAL and EACCES while its comment promises EFAULT — R4's exact-value field is what closes that, and R9/M7 is what stops it being widened back when the kernel disagrees.

### R5-REPORT-FILE-CENSUS  ·  _ci-step_

**Rule.** `run_case` appends exactly one line per case to the file named by `$GV_ESCAPE_REPORT`: `GV-ESCAPE case=<id> result=contained|escaped|capability-absent:<missing> class=<containment|functional>`. Never to stdout or stderr. The gating job asserts three things about that file: it exists; its case-id multiset EQUALS `docs/sandbox/escape-census.txt` in both directions; and zero records have `result=capability-absent`. A source tripwire asserts the census file's id set equals the set of `EscapeCase` constants in the battery.

**Why.** Two verified facts kill the obvious alternatives. (a) libtest swallows stdout AND stderr of PASSING tests and prints them only on failure, so `cargo test --workspace` (ci.yml:151-152) plus `grep SKIPPED` produces byte-identical output whether or not a skip happened — the plan's Task 17 gate is vacuous the day it lands, and worse, it greps for `[skip] capability_absent` while the shipped tests print `SKIPPED:`. (b) `cargo test <filter>` exits 0 on "0 passed; 0 filtered out", so a renamed module empties any filter-based gate silently — the repo already learned this and carries a `-lt 35` floor in the `contract` job for exactly this reason (ci.yml:190-195). Equality in both directions beats a floor because a floor catches deletion but not substitution. The census↔source tripwire is what makes a rename break the BUILD rather than empty the GATE. Crucially, this rule needs no in-Rust severity switch: the tests always record and never hard-fail on absence, and the JOB fails on skip records — so there is no `GV_SANDBOX_REQUIRED` that a workflow can forget to export. If the variable is unset in CI, no file is written and the "file must exist" check fails closed.

### R6-PRODUCTION-SEAM  ·  _source-tripwire_

**Rule.** Every inside leg spawns through `sandbox::spawn::command_async` — not `command_sync`, not `shim_cli::launch`. Where the configuration is production-constructible, the policy comes from `sandbox::policy_for_repo`; otherwise R8 applies. The battery contains zero `launch(`, zero `workable(`, zero `Policy {` literals. `shim_cli::launch` is deleted and `src/sandbox/shim_cli.rs` is removed from both `ALLOWED_SPAWN_SITES` and `LAUNCHER_SPAWN_SITES` in argv_boundary.rs; in `escape_suite.rs` every `Command::new(` must be immediately followed by `"cc"` or `"git"`.

**Why.** Three corrections to the other angles, all verified. (1) Angles 1 and 2 both named `command_sync` as the production seam. It is `#[cfg_attr(not(test), allow(dead_code))]` (spawn.rs:68); production calls `command_async` (git_cmd.rs:138-140). Routing tests through `command_sync` is routing through a wrapper production does not use while looking like the strongest possible composition claim. (2) Angle 2 verified, and I re-verified, that `policy_for_repo(repo)` and `workable(Tier::Network, repo, shim)` build the identical policy — same `default_system_trees(Network)`, same `rw += repo`, `ro += $HOME`, same `secret_excludes_for_home`, same `DEFAULT_GIT_PORTS`, same `HookMode::Run` — so three of five cases reach the production builder with ZERO production change. Angle 1's proposal to split `policy_for(repo, tier, hook_mode)` loses: it invents a builder production does not call, which is the same category of error as `command_sync`, and it is a production change made to satisfy a test. (3) The carve-out shrink is the part that carries evidence: a migration that leaves `escape_suite.rs` and `shim_cli.rs` exempt from argv_boundary's literal-`git` rule has moved code without moving the boundary.

### R7-ONE-ENVIRONMENT  ·  _source-tripwire_

**Rule.** Both legs receive an identical environment map built by exactly one harness function, `production_env_profile()`, which is a pinned reviewed constant set. The battery contains zero `env_clear` and zero `.env(` outside that one function. A separate source tripwire asserts the server sets only `GIT_TERMINAL_PROMPT` and `GIT_EDITOR` (main.rs:123-132) and no other `GIT_*`. At least one case runs with a deliberately hostile addition (`GIT_CONFIG_GLOBAL` pointing at a config carrying `core.hooksPath` and `core.fsmonitor`) and asserts the boundary holds.

**Why.** The audit's under-appreciated finding, and the one where routing through the seam provably does not help: `spawn.rs` deliberately does not touch env (spawn.rs:41-45), the CALLER supplies it, and spawn.rs's own existing tests go through the seam and still call `.env_clear().env("PATH").env("HOME")` (spawn.rs:117-119, 143-146, 156-158, 179-181). So the migrated battery would satisfy the composition complaint in the diff while preserving byte-for-byte the divergence that was the complaint's stated reason. Angle 1 and Angle 2 both said "inherit the real environment." That loses: it is nondeterministic — a developer with `GIT_DIR` or `GIT_CONFIG_GLOBAL` exported gets a red suite for a non-security reason, which is precisely the "trains people to ignore it" failure the task names — and it will be reverted to `env_clear` within a week under flake pressure. A pinned profile plus one deliberately hostile case tests the actual risk (environment-selected git helpers) without importing the developer's shell into the security signal.

### R8-EXPIRING-EXEMPTION  ·  _source-tripwire_

**Rule.** A case whose configuration is not production-constructible carries `exemption: NotProductionReachable{ blocker: &'static str }` as a typed field, and a source tripwire asserts the named blocker still exists in production source. When the blocker disappears the tripwire fails and forces the port. Today: `Tier::Strict` and `HookMode::Blocked` are exempt, blocker = `policy_for_repo` hard-coding `Tier::Network`/`HookMode::Run`.

**Why.** Obstacle 1, confronted rather than wished away or papered over with a production change. `policy_for_repo` (mod.rs:451-468) hard-codes `Tier::Network` and `HookMode::Run`, and `tier_for`/`trust::is_trusted` have zero callers outside their own test modules — the classifier is built, audited and wired to nothing. Wiring it is Task 8, and Task 8 is gated on Task 9's INV-13 security judgement (what happens when Strict is selected and bwrap/userns is absent — `Policy` cannot even represent Strict-without-bwrap, `shim_argv` panics at mod.rs:537). That decision is Tom's, not an agent's, and it must not be smuggled into a test-contract task: the diff is small and the consequence is that every `git log` the live server runs starts spawning bwrap. The exemption must EXPIRE mechanically because a comment saying "port this when dispatch lands" is exactly the artefact this battery has twice proved does not survive a rewrite.

### R9-MUTATION-MATRIX  ·  _ci-step_

**Rule.** A committed set of single-mechanism patch files is applied to a throwaway copy of the tree, one at a time, and the full battery is run against each; the job emits a mutant x case grid. `patch --forward` failing to apply is a job failure (a mutant that silently no-ops manufactures evidence). Every case declares `dies_under: &[MutantId]` with at least one entry; every mutant must be named by at least one case. M0 (unmodified) must be all-pass; every declared cell must be FAIL. Minimum mutant set: M1 `apply_seccomp` body emptied; M2 `apply_landlock` returns before `landlock_restrict_self`; M3 `secret_excludes_for_home` returns `Vec::new()`; M4 `--unshare-net` removed from `STRICT_BWRAP_ARGS`; M5 `--net-deny` emitted as `--net-allow` with all `DEFAULT_GIT_PORTS`; M6 shim ignores `hooks_blocked_dir`; M7 high-bit comparison widened to 64 bits. **Added 2026-07-29:** M8 removes only the strict tier's `socket`/`socketpair` AF_UNIX rules from `seccomp_filter::rules_for`, leaving the rest of the filter installed — M1 would kill an AF_UNIX case too, but only M8 shows the case notices its own mechanism rather than the filter's existence.

**Why.** This is the only rule that answers the task's actual demand — detection without a human re-deriving the analysis — and it is the strongest idea any of the three angles produced. Every one of C11's five verdicts is a matrix cell that currently reads PASS where it must read FAIL: M3 against case 1 (verified: exclusion is omission-during-enumeration, so an empty exclude set is indistinguishable to any test lacking R3's paired positive), M1 against case 2, M4 against case 5 (verified: `apply_landlock` declares `NET_CONNECT_TCP` handled in BOTH tiers and adds no port rule under `--net-deny`, main.rs:458-470 — so Landlock denies TCP independently and removing `--unshare-net` leaves the test green, which is why the comment at escape_suite.rs:363-368 is false), M6 against case 4. It subsumes Angle 1's DISC-1/DISC-2 in a less gameable form: DISC-1 mutates the argv with `retain`, which the pre-mortem correctly showed can silently match nothing and then be written up as confirming the mechanism. A source patch that fails to apply is loud. Honest cost: N+1 rebuilds of two crates; bind it to the gating job on PRs touching `sandbox/` plus nightly, not to every push.

### R10-FLAG-ROUND-TRIP  ·  _source-tripwire_

**Rule.** Every `"--..."` literal any argv builder in `sandbox/mod.rs` emits must have a matching arm in the shim's `parse()`, and every terminal mode the shim accepts must be reachable from some builder. Resolution today: DELETE `probe_argv` (mod.rs:519-526) and its two tests (argv.rs:268, :287), and amend the plan's Global Constraint 2 to name ONE sanctioned route.

**Why.** Obstacle 2 made mechanical, and the place where I overrule Angle 2 hardest. Verified three ways: `parse()` has no `--self-probe` arm and its catch-all is `die(EXIT_ARGV, "unknown flag")`; `validate()` separately requires `program_args.first() == Some("git")`, which a probe argv can never satisfy; `grep -rn 'self.probe' crates/` returns only `argv.rs:272` and `mod.rs:524`. So the repo ships two green tests certifying a route that exits 90 on contact. Angle 2 argued for implementing a minimal `--self-probe` because it removes the None-ambiguity. It loses on three counts. (1) A self-probe runs in the shim's own process without exec'ing anything, while production ALWAYS crosses `execve` into git (main.rs:547-566) — execve is precisely where a policy can be lost. A perfect self-probe truth table is evidence about a process configuration production never runs, and the battery's containment count would go UP while its evidentiary value went to zero. (2) It adds a second argv-reachable terminal mode to the binary whose `validate()` is the crate's strongest structural invariant, in the same diff as a test rewrite — how the last relocated defect got through review. (3) By Angle 2's own measurement the plan-faithful version is 250-350 lines and silently contains implementing INV-4 (`seccomp_filter.rs` has no `SYS_socket`/`SYS_socketpair` rule at all) plus an unmeasured AF_UNIX/git compatibility risk. R2's `Result` + BEGIN/END nonce removes the None-ambiguity structurally with no shim change at all. A dead sanctioned route is worse than a missing one because it makes a reviewer believe two independent routes exist.

### R11-SELF-BINDING  ·  _source-tripwire_

**Rule.** `escape_contract.rs` carries `const RULES: &[(&str, &str)]` pairing every rule id above with the name of the test that enforces it, and one test reads its own source and asserts each named `fn <name>(` exists. A rule whose enforcement is deleted fails the build.

**Why.** This battery has failed twice, each time after a rewrite believed to address the previous audit. The failure mode is not carelessness — it is that the standard lived in a report, and reports are not open during the next rewrite. `argv_boundary.rs` is the working precedent in this repo: a scan that reads its own source (`ALLOWED_SPAWN_SITES` includes `src/argv_boundary.rs`, line 47) and refuses to be quietly narrowed. Cheap, and it is the only thing that makes the contract survive its own authors.

---

## Skip policy — the mechanism, not the principle

The mechanism, not the principle: **the tests never decide severity; the job does, by asserting over an artifact both sides produce identically.**

`run_case` always appends one record per case to the file named by `$GV_ESCAPE_REPORT` and always returns `Outcome`; capability absence is `Outcome::CapabilityAbsent{case, missing}` recorded as `result=capability-absent`, and it is never a panic and never an early return (R1 has banned `return`; R4 makes it a value). Locally, `cargo test` therefore exits 0 with a report file saying which cases were not exercised. The gating `sandbox` job then makes three assertions in shell, outside Rust:

1. the report file exists;
2. `sort` of its case-ids `diff`s clean against `docs/sandbox/escape-census.txt` — equality in both directions;
3. `grep -c 'result=capability-absent'` is 0.

Why each piece is the way it is, against three verified false-green channels:

- **Not stderr, not stdout.** libtest swallows both streams for PASSING tests. A `SKIPPED` line emitted by a passing test never reaches the CI log without `--nocapture`, so `grep SKIPPED` produces byte-identical output whether or not a skip occurred — the gate's "all clear" and its "I can see nothing" are the same bytes. This is not hypothetical: the shipped tests print `SKIPPED:` (escape_suite.rs:162, :246, :372), the plan's Task 17 gate greps for `[skip] capability_absent`, and `ci.yml:151-152` runs `cargo test --workspace` with no consumer for either string. A file written by the test process is immune to capture.
- **Not `GV_SANDBOX_REQUIRED`.** Angle 1 proposed a one-read-site env var with a counted-string tripwire. It is better than a per-test convention but still has the failure the pre-mortem names: nothing verifies the workflow exports it, so deleting one YAML line makes every case green-by-skipping with a passing CI. The report-file design has no in-Rust severity switch to mis-set. If `$GV_ESCAPE_REPORT` is unset in CI, no file is written and assertion (1) fails closed.
- **Equality, not a floor.** `cargo test <filter>` exits 0 on "0 passed; 0 filtered out", so a renamed module or a moved file empties any filter-based gate silently. A floor catches deletion but not substitution. The repo already learned the first half of this — the `contract` job carries `-lt 35` for exactly this reason (ci.yml:190-195). A source tripwire additionally asserts the census id set equals the set of `EscapeCase` constants in the battery, so a rename breaks the BUILD rather than emptying the GATE.
- **Two independent mechanisms, not one.** The job's first step is a preflight asserting `capabilities::probe()` satisfies the real three-part `Capabilities::strict_available()` (landlock floor AND bwrap AND userns, capabilities.rs:53-55) plus `io_uring_disabled == 0` and `cc` present, failing with `::error::` naming the missing field before any test runs. This makes host-inadequacy a *differently named* red check from a containment failure, so nobody learns to click past the security one — the failure mode the task explicitly warns about. A preflight alone is insufficient (it cannot anticipate every capability a case needs); a report gate alone is insufficient (a mis-provisioned runner produces a security-shaped failure for an infrastructure cause). With both, a false green needs two independent mistakes.

Net effect on the developer laptop with no unprivileged userns (stock Ubuntu 24.04, where `apparmor_restrict_unprivileged_userns=1` by default): green suite, report file naming the strict case as not exercised. On the gating job: red, with the reason on the check name.

---

## Acceptance evidence — what makes the battery sound

The battery is sound when all six hold. (A)-(D) are Angle 3's demand, kept because they are the only form that survives a third rewrite; (E) and (F) are additions I confirmed necessary against source.

**(A) A green mutation matrix with a demonstrated red.** M0 all-pass; every declared `dies_under` cell FAIL; `patch --forward --strict` applied cleanly for every mutant (a mutant that silently no-ops manufactures positive evidence). This is the acceptance criterion, not a supplement to one: relocating the defect a third time now requires producing a matrix cell that says PASS, and the matrix prints that cell. Every C11 verdict is a cell that today reads PASS and must read FAIL — M3/case 1, M1/case 2, M4+M5/case 5, M6/case 4.

**(B) Claim-to-mutant closure, both directions.** Every case names at least one mutant it must die under; every mutant is named by at least one case. A claim with no mutant is an unproven assertion; a mutant no case names is undeclared coverage. Checked by a source scan, the pattern this repo already runs and trusts.

**(C) A structured report file, and a census that matches the source.** `$GV_ESCAPE_REPORT` exists; its case-id set diffs clean against the census in both directions; zero `result=capability-absent` records in the gating job; a tripwire asserts census ids == `EscapeCase` constants in the battery.

**(D) Three recorded red runs, linked.** (1) A runner without unprivileged userns: strict case records `capability-absent` and the job exits non-zero. (2) A case renamed: the census tripwire fails the build. (3) Mutant M3: case 1 fails. A gate never observed to fail is not known to be a gate — and as far as the record shows, this battery has never been red for a containment reason.

**(E) A paired positive in every denial claim, same run, same policy, same probe binary.** A sibling entry under the same granted `$HOME` tree read successfully while the excluded one returns EACCES; a granted port connectable while the denied one is not. Non-negotiable and verified necessary: `enumerate()` (bin/gv-sandbox/main.rs:386-441) implements exclusion by omission, so without this the exclude list is not under test at all and M3 cannot bite. A missing positive is a failure, never a skip.

**(F) The kernel's own self-report, across the execve.** Each probe prints its `/proc/self/status` `Seccomp:` and `NoNewPrivs:` fields beside its errno. The inside leg must show `Seccomp: 2` and `NoNewPrivs: 1`; the baseline leg, under the identical outer environment, must show `Seccomp: 0`. That pair is stronger provenance than any errno — it is the kernel stating that the exact post-exec process git actually runs in is under a filter, and the baseline comparison rules out an outer container's default profile (Docker's returns EPERM for `io_uring_setup`, the precise value case 2 demands) being credited to Git-Vista. It is also the one observation `--self-probe` structurally cannot produce, which is the affirmative case for R10's deletion rather than merely the absence of a reason to keep it.

**Honest limits, stated so nobody reads more into a green matrix than is there.**
- R9 proves a case dies when a named mechanism is removed. It does not prove that mechanism is the security property in the claim's English sense. A large improvement over a comment; not a proof.
- The census makes coverage explicit, not complete. Nothing here says these are the right five cases.
- Until step 8, production only ever builds `Network`/`Run`. Everything proven about Strict is proven under an R8 exemption. The contract makes the battery honest about what it tests; it does not widen what ships.
- `Tier::Unsandboxed` is reachable only through operator trust (mod.rs:420-425) and by design has no boundary. The mode with the most damage potential is the one the battery is definitionally unable to test.
- Unverified because this phase ran no cargo, no git write, no server: whether the five tests are green today (taken on trust from the audit and handoff.md); whether Strict can carry a real `git log`/`cat-file` workload (only `git add` under bwrap is evidenced anywhere, at escape_suite.rs:424, and that assertion does not check `commit_code`); bwrap's per-spawn cost; whether `/tmp` fixtures survive Strict's bind mounts; and whether INV-4's absence from `seccomp_filter.rs` is a deliberate deferral or an unnoticed gap.

**Signed:** thomas2025 · 2026-07-29

---

## Where the three angles disagreed, and who won

Five real disagreements. In each I picked, and in three the pre-mortem beat both constructive angles because it predicted a satisfying-the-letter implementation that I then confirmed against source.

1. `--self-probe`: DELETE (Angle 1) beats implement-minimal (Angle 2). Angle 2's case is that self-probe structurally kills the `None`-means-pass class. True, but R2's `Result` + nonce kills the same class with zero shim change. Against it: a self-probe never crosses `execve`, and execve is where production always goes and where a policy can be lost; it adds a second terminal mode to the binary whose `validate()` guarantees "execs only git" (verified: `program_args.first() != Some("git")` -> die); and Angle 2's own sizing says the faithful version smuggles in INV-4 (`seccomp_filter.rs` has no `SYS_socket` rule) plus unmeasured git/AF_UNIX compatibility risk. Angle 3 named it "the strongest vacuity available, and the most attractive one" — containment count up, evidentiary value zero. Delete `probe_argv` and its two tests; amend Global Constraint 2 to one route.

2. Production seam function: BOTH Angle 1 and Angle 2 said `command_sync`. Both are wrong. `git_cmd.rs:138-140` uses `command_async`; `command_sync` is `#[cfg_attr(not(test), allow(dead_code))]` (spawn.rs:68). A test routed through `command_sync` is routed through a wrapper production does not call while looking like the strongest possible composition claim. R6 names `command_async`. This was the pre-mortem's catch and I verified it directly.

3. Reaching the production policy builder: Angle 2 beats Angle 1. Angle 1 wants `policy_for_repo` split into `policy_for(repo, tier, hook_mode)` so all five cases route through "production." Angle 2 established, and I re-verified line by line, that `policy_for_repo(repo)` and `workable(Tier::Network, repo, shim)` build the identical policy — so three of five cases reach the real builder today with no production change at all. Angle 1's split loses for the same reason `command_sync` loses: it manufactures a builder production does not call, to satisfy a test. The residue (Strict, Blocked) gets R8's expiring exemption instead of a speculative refactor. This also keeps a security-critical change (tier dispatch) out of a test-contract diff.

4. The Landlock secret fixture: Angle 3 beats Angle 1 decisively, and this is the single most important correction in the document. Angle 1's FIX-1 says move the secret out of `~/.ssh` into "a unique controlled temp tree included as a read grant with a test exclusion beneath it" — which is also the audit's own minimum repair. I read `enumerate()` (bin/gv-sandbox/main.rs:386-441) and confirmed the pre-mortem's claim: exclusion is implemented as *omission during enumeration*, Landlock is deny-by-default, and no deny rule is ever added. EACCES-from-exclusion and EACCES-from-never-granted are therefore the same kernel event. If the engineer moves the secret and forgets the enclosing grant — or the grant is later dropped — the denial comes from absence-of-grant and `secret_excludes_for_home()` can return `Vec::new()` with the test still green. The audit's own recommended repair is vacuously satisfiable. Fix: R3 requires a paired positive sibling under the SAME granted tree, and R9/M3 mutates `secret_excludes_for_home` to `Vec::new()` and requires the case to go red. Without R3, tightening `"Permission denied"` to "exactly EACCES" reads as a strict improvement while silently discarding the only property that made the assertion attributable.

5. The environment: Angle 3 beats Angles 1 and 2. Both said "drop `env_clear`, inherit like production." Verified against source, that is satisfiable-in-the-letter and self-defeating in practice: `spawn.rs` deliberately does not set env, the caller does, and spawn.rs's OWN tests go through the seam and still `.env_clear().env("PATH").env("HOME")` (spawn.rs:117-119, 143-146, 156-158, 179-181) — so the migrated battery closes the composition complaint in the diff while preserving the exact divergence that was the complaint's reason. And true inheritance is nondeterministic: a developer with `GIT_DIR` exported gets a red suite for a non-security reason, generating flake pressure that restores `env_clear` within a week. R7 takes Angle 3's pinned profile plus one deliberately hostile `GIT_CONFIG_GLOBAL` case, which tests the actual risk without importing the developer's shell into the security signal.

Cut without replacement: Angle 1's DISC-3 ("no prose claim that is not a checked field") — it reduces to "a reviewer should notice," which is what the task forbids. Its two live instances (the false `--unshare-net` comment, the unenforced EFAULT provenance) are covered by R9/M4 and R4 respectively; new prose is simply not checked and the document says so rather than pretending. Angle 1's FIX-2 (nonce-derived sentinels) survives only inside R2's nonce, and only paired with R3, because the pre-mortem is right that uniqueness makes an absence assertion (`!out.contains(sentinel)`) trivially true by construction — absence assertions are strengthened by a paired positive, never by uniqueness.

---

## Ordered work

**1.** Land the gating `sandbox` CI job: preflight step asserting `capabilities::probe()` meets the real three-part `strict_available()` plus `io_uring_disabled==0` and `cc` present (fail with `::error::` naming the missing field, before any test); then the battery run exporting `GV_ESCAPE_REPORT`; then the three report assertions (exists / census diff both directions / zero `capability-absent`). Also add `docs/sandbox/escape-census.txt` as an empty-but-present file and document the job in RELEASE_GATES. No Rust. Do this FIRST — until the job exists there is nowhere for any capability rule to bind, and the current `cargo test --workspace` (ci.yml:151-152) consumes nothing.

- Files: `/home/tom/projects/Git-Vista/.github/workflows/ci.yml, /home/tom/projects/Git-Vista/docs/RELEASE_GATES.md, /home/tom/projects/Git-Vista/docs/sandbox/escape-census.txt`

**2.** R10: delete `probe_argv` (crates/git-vista-server/src/sandbox/mod.rs:519-526) and its two tests (argv.rs:268-279, :281-288); add the flag round-trip tripwire (every `"--..."` literal emitted by `shim_argv`/`sandbox_argv` has a `parse()` arm in bin/gv-sandbox/main.rs). Independent, cheap, and it closes a live falsehood: the repo currently ships two green tests certifying a route that exits 90 on contact.

- Files: `/home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/mod.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/argv.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/escape_contract.rs`

**3.** Build `escape_contract.rs` and the harness+runner BEFORE rewriting any case: promote `argv_boundary::code_only` and `rs_files` to `pub(crate)`; write the tripwires for R1, R2, R4, R6, R7, R8, R11; write `EscapeCase`, `Errno`, `Outcome`, the `Result`-returning parser, the nonce BEGIN/END substitution, `production_env_profile()`, and `run_case`. Land every `mod` declaration the later steps need in `sandbox/mod.rs` in this one commit so no later lane touches the module list. The tripwires must be able to REJECT the rewrite before the rewrite exists — otherwise this is a third rewrite reviewed by its own author.

- Files: `/home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/escape_contract.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/argv_boundary.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/mod.rs`
- Blocked by: Step 2 (shares escape_contract.rs — same writer, sequential).

**4.** Delete `shim_cli::launch` and `shim_cli::workable`; port their non-battery callers (`spawn.rs` tests, `argv.rs`) onto `policy_for_repo`/`command_async`; remove `src/sandbox/shim_cli.rs` from `ALLOWED_SPAWN_SITES` and `LAUNCHER_SPAWN_SITES` in argv_boundary.rs, shrinking the tripwire's only hole from three entries to two. This is the part of the composition fix that carries evidence — a migration that leaves the carve-out open has moved code without moving the boundary.

- Files: `/home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/shim_cli.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/argv_boundary.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/spawn.rs`
- Blocked by: Step 3.

**5.** Rewrite the five cases as `EscapeCase` declarations. Landlock secret: `policy_for_repo`, secret under the granted `$HOME`, paired positive on a sibling under the same tree (R3), numeric open/read probe, `expect_inside: EACCES`, `expect_granted: Errno(0)`, `dies_under: [M2, M3]`. io_uring: `expect_baseline: Errno(0)`, absence -> `CapabilityAbsent`, `dies_under: [M1]`. High-bit prctl: `expect_baseline: EFAULT` exactly, `dies_under: [M1, M7]`. Blocked hooks: `class = functional`, moved to `hook_mode_suite.rs`, `dies_under: [M6]`, R8 exemption blocker = `HookMode::Run` hard-coded. Strict listener: R8 exemption blocker = `Tier::Network` hard-coded, false comment at :363-368 deleted, claim rewritten as composed-Strict containment, `dies_under: [M4, M5]` — note M4 alone will NOT kill it (verified: `--net-deny` makes Landlock deny TCP independently), which is the correct forcing function to either widen the declared claim or pair M4 with M5.

- Files: `/home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/escape_suite.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/hook_mode_suite.rs`
- Blocked by: Step 4. Single writer for both files — the three sub-rewrites are sequential commits by one agent, never three agents.

**6.** R9: commit the seven mutant patch files plus the matrix driver script; wire it into the gating job (PRs touching `crates/git-vista-server/src/sandbox/**` or `src/bin/gv-sandbox/**`, plus nightly). Driver copies the tree to a throwaway dir, applies each patch with `patch --forward --strict` (non-application = job failure), runs the battery, emits the grid, asserts M0 all-pass and every declared `dies_under` cell FAIL. Fill in the census with the observed ids and classes.

- Files: `/home/tom/projects/Git-Vista/ci/mutants/*.patch, /home/tom/projects/Git-Vista/ci/mutation-matrix.sh, /home/tom/projects/Git-Vista/.github/workflows/ci.yml, /home/tom/projects/Git-Vista/docs/sandbox/escape-census.txt`
- Blocked by: Steps 1 and 5. Same writer as step 1 for ci.yml.

**7.** BLOCKED ON TOM — not an agent's call. ADR deciding INV-13: what happens when Strict is selected and bwrap or userns is absent. Degrade Strict->Network (which gives local read operations network access — the best-effort downgrade C5 forbids), or hard-fail every local git operation on a host without bubblewrap. `Policy` cannot represent Strict-without-bwrap (`shim_argv` panics, mod.rs:537), so the decision determines the type-level shape of the fix. `capabilities.rs` deliberately refuses to hold this judgement.

- Files: `/home/tom/projects/Git-Vista/docs/adr/`
- Blocked by: Tom's decision.

**8.** BLOCKED ON TIER DISPATCH. Wire dispatch: `git_cmd.rs:138` `sandboxed(repo)` -> `sandboxed(repo, args)` and its five callers (:149, :253, :271, :284, :304 — all already hold their args); `policy_for_repo` calls `tier_for(network_need(args), trust::is_trusted(canonical))`; fill `bwrap` for Strict; apply the ADR's degradation. Note the trap: the current wrapper builds the policy with an EMPTY args slice, so wiring the classifier without the restructure classifies every operation `Local` and routes `git push` (planner.rs, via `git_ok`) into Strict where `--unshare-net` breaks it. Carries real operational risk — every local git the live server runs starts spawning bwrap, and bwrap's per-spawn cost is unmeasured anywhere in this repo.

- Files: `/home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/mod.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/git_cmd.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/dispatch.rs`
- Blocked by: Step 7 (the ADR).

**9.** BLOCKED ON TIER DISPATCH. Retire the R8 exemptions: the strict and blocked-hooks cases move onto `policy_for_repo`. No manual trigger needed — step 8 removes the hard-coded `Tier::Network`/`HookMode::Run`, which fails R8's tripwire and forces this step. Hand back to step 5's writer; lane D must not edit escape_suite.rs.

- Files: `/home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/escape_suite.rs, /home/tom/projects/Git-Vista/crates/git-vista-server/src/sandbox/hook_mode_suite.rs`
- Blocked by: Step 8.

---

## Open items

1. **INV-13, the only true blocker (step 7).** When Strict is selected and bwrap or unprivileged userns is absent: degrade Strict->Network, or hard-fail every local git operation? Degrading gives local *read* operations network access, which is the best-effort downgrade C5 forbids; hard-failing makes git-vista unusable on any host without bubblewrap. `Policy` cannot represent Strict-without-bwrap (`shim_argv` panics at mod.rs:537), so this decides the type-level shape of the fix. `capabilities.rs` deliberately refuses to hold this judgement, and handoff.md names it as the reason the Network-for-all interim stands. Nothing in steps 8-9 can start without it.

2. **Amending the plan's Global Constraint 2.** R10 deletes `probe_argv` and leaves ONE sanctioned composition route rather than two. That is a change to a plan document you own. Confirm before step 2 lands, since it also deletes two currently-green tests (argv.rs:268, :287).

3. **Mutation-matrix cost.** R9 rebuilds two crates eight times. My proposal binds it to PRs touching `sandbox/**` plus nightly, not every push. If you want it on every push, that is a runner-minutes decision, not a correctness one.

4. **Step 8 operational risk, on the machine with your live iPad session.** Wiring dispatch puts a bwrap launch in front of every local git operation the server runs, and `git_cmd.rs`'s streaming path spawns per request. bwrap's per-spawn cost is unmeasured anywhere in this repo. Someone should measure it and run the five `git_cmd.rs` helpers under a Strict policy before that lands — I could not, under this phase's constraints.

5. **INV-4.** `seccomp_filter.rs` has no `SYS_socket`/`SYS_socketpair` rule, while the plan's Architecture section and Task 4 both state the filter denies AF_UNIX. Nothing in source, handoff or either audit says whether that is deliberate deferral or an unnoticed gap. R10's deletion of `--self-probe` means this is no longer surfaced as a side effect of test work, so it needs its own tracked issue rather than disappearing.

Process note: **the next stretch after this document is docs-only** — the ADR in step 7 and the RELEASE_GATES prose in step 1. Good point to switch to a cheaper model. Steps 3, 4, 5 and 9 are the ones that want a strong model.

> **Two of these are now DECIDED (Tom, 2026-07-29) — see Global Constraints 2 and 15 in the plan:**
> **INV-13 → hard-fail.** Strict selected but bwrap/userns absent ⇒ the operation refuses to run.
> No degrade to Network, no degrade-and-block-hooks. Accepted cost: unusable without bubblewrap.
> **R10 → approved.** Delete `probe_argv`; Global Constraint 2 now names one composition route.

**Signed:** thomas2025 · 2026-07-29T09:45:00-04:00