# 0082 — A version floor is exercised, not asserted; and "mandatory" lives in shell, not in the test

**Status:** Accepted — implemented and tested
**Date:** 2026-08-25
**Issue:** [#365](https://github.com/tom2025b/Git-Vista/issues/365)

---

## Context

`docs/SUPPORTED_VERSIONS.md` puts the supported Git floor at **2.32**, derived from something real: `GIT_CONFIG_GLOBAL`, which the write path depends on to pin the global-config file it reads, arrived in 2.32. Since #67 the `core` job has parsed that heading and failed a runner older than it.

Enforcing a lower bound is not the same as having run anything at it. Nothing provisioned 2.32, so the check said "not older than 2.32" while every fixture in the workspace had only ever met the runner's own 2.43.0.

The parser said so itself. `crates/git-vista-protocol/src/status.rs` recorded, in its own comment, that its record shapes were captured by hand from one 2.43.0 install and that exercising the floor was "a real gap against #68's *results match supported Git versions on fixtures* criterion, named here rather than silently claimed satisfied".

Two further facts shape the decision, and both were measured rather than assumed:

- The parser's twelve porcelain tests all feed it **hand-written byte strings**, with literal oids like `6666…`. Excellent tests of the parser; no evidence at all about `git`.
- Building git 2.32.0 from source takes **about a minute** on four cores, and the result parses this repository's whole status vocabulary **byte-identically** to the current git. Measured at 2.43.0 on a developer box and at **2.55.0** on the CI runner — a twenty-three-minor-version span. The expected answer to #365 is "no difference".

That last point is the trap. When the expected result is agreement, a harness that never invoked the second binary passes exactly as loudly as one that did.

## Decision

**Three decisions, in the order they matter.**

### 1. The floor is provisioned by building it from source, keyed on the documented number

`git clone --depth 1 -b v<floor>.0 https://github.com/git/git`, then `make … install` with `NO_GETTEXT=1 NO_TCLTK=1 NO_CURL=1 NO_EXPAT=1`.

A distro package was refused because no distro ships an arbitrary five-year-old git, and pinning a container image would move the floor number into a second place — a Dockerfile tag — where it can disagree with the document. Building from source is the only source that follows the heading automatically: the job parses `## Git: X.Y or later` for the tag it clones. **Change the document and the test changes with it**, which is the anti-drift property the existing version-floor step already has and the reason this one is written the same way.

`NO_GETTEXT` and `NO_TCLTK` are not incidental: `msgfmt` and `tclsh` are absent from ordinary runner images and are not needed to run `git status`. Requiring them would make provisioning fail for a reason having nothing to do with what is under test.

### 2. The leg is mandatory — and survives a transient fetch failure by a cache plus bounded retry, never by downgrading

#365's acceptance settles that the leg is mandatory ("rather than silently falling back to the runner's ambient Git"), so that was never the open question. The open question was what "mandatory" does when fetching an eleven-year-old git briefly fails, since a gate that goes red on infrastructure noise gets disabled by the third occurrence.

The answer is, in order:

- **A cache keyed on the floor number.** A hit needs no network at all, which is most of the answer: the fetch only happens when the floor moves or the cache expires.
- **Three attempts with backoff** on a miss.
- **Then fail the job**, with an `::error::` that says `PROVISIONING FAILED` and states that it is not a parser regression. A red build nobody can diagnose is how a gate gets deleted; naming the failure mode is what keeps it.

What is explicitly refused: making the leg advisory on fetch failure. "It could not fetch, so we skipped it" is the fallback the issue names, wearing a different hat.

### 3. Whether the leg ran is asserted in **shell, over a report** — never by the test

The test writes `GV_STATUS_FLOOR_REPORT` naming both binaries and every shape each one read. CI then asserts over that file: the report exists, `floor=` is not `unrun`, the version it names is the documented floor, and the two binaries covered the **same number** of shapes.

This is the same anti-vacuity shape the sandbox job already uses against `GV_ESCAPE_REPORT`, and it is chosen for the same reason. The two alternatives are both worse:

- **The test skips when `GV_GIT_FLOOR` is unset.** A skipped test reports the same green as a passing one. This is the exact failure this repository has now written down six times.
- **The test fails when `GV_GIT_FLOOR` is unset.** Then `cargo test --workspace` is impossible for any contributor with one git installed, and the pressure to add the skip arrives within a week.

Splitting it resolves the tension honestly: the *test* is a test, runnable by anyone, and always checks the current git against named expectations. The *policy* — that a second version is required — is CI's, stated where the test cannot vote on it.

### The expectations are named, and agreement is checked as well as correctness

Both binaries' output is held to a **written-out expected value**, not merely to each other. Two identical wrong answers compare equal, so "the versions agree" is necessary and nowhere near sufficient. The cross-version equality is asserted too, and separately, because it covers the branch headers the entry expectations do not.

A binary that is not the documented floor is refused *before* it is compared against anything. Pointing `GV_GIT_FLOOR` at a second copy of the current git fails on identity rather than comparing a version with itself and reporting green.

## Alternatives weighed

**Trust git's release notes.** Porcelain v2 has not changed shape since 2.11, and nothing between 2.32 and 2.43 documents a change. That reasoning is sound, it is written in `status.rs`, and it is why the expected result is "no difference" — but it is an argument, and #68's criterion asks for fixtures. An argument cannot notice the day it stops being true.

**Test every version between 2.32 and current.** Cost grows with no evidence of benefit; the floor is the boundary the document actually commits to, and it is the one that is never exercised by anybody's daily work. If a difference is ever found at the floor, bisecting to the version that introduced it is the follow-up, not the standing job.

**Put the test in `git-vista-protocol`.** Rejected: that crate's module doc opens by calling `parse_porcelain_v2_z` "a **pure function** over bytes: no git process spawn", and its `Cargo.toml` says it must stay pure and wasm-safe. The test lives in `git-vista-fixtures`, which already spawns git for a living and now takes `git-vista-protocol` as a dev-dependency — a direction that keeps the purity claim exactly true.

## Consequences

**The admission in `status.rs` is retired, because it is no longer true.** Leaving an honest confession in place after closing the gap it confesses would be its own small lie. The comment now says what is measured, and records the finding.

**"No difference" is a result, and it is recorded as one.** The floor parses this workspace's whole status vocabulary identically to the current git — every shape, under all three read modes, at both 2.43.0 and 2.55.0. Every CI run now measures that instead of inferring it.

**The upper end of the comparison is deliberately unpinned.** The second leg is whatever git the machine has, so the span widens by itself as runners move — CI's first run of this gate already spanned 2.32.0 to 2.55.0 — and nothing has to be edited to keep that true. Pinning both ends would have frozen the test at the day it was written.

**Two records exist that production can never see, and that is now written down.** `!` (ignored) requires `--ignored`, and `C` (copy) requires `status.renames=copies` *and* a copy source that is itself part of the change set. `/api/status/v2` passes neither flag. The battery is read three ways so both record shapes are exercised against every supported git, but a reader looking for `StatusEntry::Ignored` in a live response will not find one, and now learns why from the fixture rather than from a debugging session.

**The `core` job got slower by about a minute, once.** After the first run the cache carries the built binary, keyed on the floor number, so the cost recurs only when the floor moves.

**`git::run_configured` exists now.** Some git settings are only honoured on the command line: `protocol.file.allow` is deliberately *not* read from the repository's own config when deciding whether a submodule may be cloned over a `file` transport — a repository could otherwise authorise its own clone. The battery needs two real submodules, so the helper passes such settings as `-c`, where they reach the child through `GIT_CONFIG_PARAMETERS`.

**Every test added here was proved able to fail two different ways** — an identity check that goes red when pointed at the current git, an expectation that goes red when it lies, a fixture mutation that removes a record kind, and a floor leg deliberately given a different argv so the cross-version comparison itself is shown to be live rather than trivially satisfied by a shared expectation.

---

**Signed:** max
