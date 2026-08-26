# 0087 — The floor build pins a commit, not a tag; and the cache key says what it is caching

**Status:** Accepted — implemented; verified by dry-run in the PR, and by the PR's own CI run
**Date:** 2026-08-26
**Issue:** [#520](https://github.com/tom2025b/Git-Vista/issues/520)
**Follows:** [0082](0082-a-version-floor-is-exercised-not-asserted.md), which built this job

---

## Context

ADR 0082 made the documented Git floor something CI **runs**, not merely something
it asserts a lower bound against. The mechanism is in `core`, one of the seven
required checks: parse `## Git: X.Y or later` out of `docs/SUPPORTED_VERSIONS.md`,
`git clone --depth 1 -b "v${floor}.0" https://github.com/git/git`, `make … install`,
then run the whole status vocabulary through both that binary and the runner's own.

Codex's cloud review (§M3) raised this as a reasoned finding. The local review of the
same job found nothing. #520 says, in as many words, **reconcile rather than assume** —
so this ADR opens with the reconciliation, and the fix is sized by what it found.

### The reconciliation, link by link

The range the issue cites, `.github/workflows/ci.yml:186-226`, was re-checked on this
branch's base (`405a764`) before anything was edited and had **not** drifted: line 186
was `- name: Cache the git floor build`, line 226 was the provisioning step's closing
`"$HOME/.cache/gv-git-floor/bin/git" --version`.

**1. Tag → tree. Open.** `-b "v${floor}.0"` names a ref in somebody else's repository.
A ref is a name, and a name can be repointed. HTTPS authenticates `github.com`; it says
nothing about which object that host hands back. There was no pin, no checksum, and no
signature check.

**2. Tree → binary. Open, and this is the sharpest point — it is not where the review
was looking.** The next thing to touch the cloned tree is `make … install`, which runs
that tree's own makefiles, configuration probes and generators. Whatever is wrong with
the tree gets to execute, as the runner user, *at that moment* — before any version or
behaviour assertion in this pipeline exists. So "it verifies the printed version" is not
just a weak check; it is a check that happens strictly downstream of the execution it
would need to have prevented.

**3. Binary → verification. Open, exactly as the issue describes.** The only identity
check on the resulting binary is the string it prints. `version_of()`
(`crates/git-vista-fixtures/tests/status_floor.rs:120-130`) runs `<binary> --version`
and strips the literal prefix `git version `; the test asserts that string against the
documented floor, and CI asserts the same string a second time out of the test's report.
A binary that prints `git version 2.32.0` satisfies every one of them.

There is a real partial mitigation here, and it deserves saying rather than being quietly
dropped to make the finding look worse: the battery holds **both** binaries to
written-out expected values for the entire status vocabulary, so a substitute must also
parse every shape identically. That is a genuine bar on *behaviour*. It is not a claim
about *provenance*, and it is evaluated after `make` has already run.

**4. Cache → required job. Open — with the threat model corrected.** The key was
`gv-git-floor-${{ runner.os }}-${{ steps.git_floor.outputs.version }}`, which on every
run of this workflow evaluates to the literal string `gv-git-floor-Linux-2.32`. On a hit
the provisioning step is skipped entirely (`if: … cache-hit != 'true'`), so the cached
tree becomes the floor binary with no check whatsoever beyond (3).

What the issue's framing implies but is **not** live here is third-party cache poisoning.
Writing an Actions cache entry requires a workflow run holding a write token in this
repository; `on: pull_request` from a fork gets a read-only token and cannot write to the
base repository's cache, and this is a single-owner repository. Nobody outside is
reaching that key.

What *is* live is **persistence**. The key was a function of two facts that essentially
never change, so exactly one bad provisioning — a moved tag, a hijacked retrieval path,
or a merely corrupt fetch — is written under a key that stays trusted for as long as the
entry survives, and every later run reuses it and touches the network not at all. The
cache is not the attack. It is what makes one bad minute durable, and it is why the fix
has to reach the key and not only the clone.

Separately, and with no attacker at all: `runner.os` is `Linux` for ubuntu-22.04 and
ubuntu-24.04, and for x86_64 and arm64 alike. `ubuntu-latest` has migrated before and
will again. The key claimed more identity than it carried, so a C binary built against
one glibc could be handed to a job running on another.

**5. "Required merge job." Confirmed.** `core` is one of the seven jobs the workflow's
own header enumerates, and `on: pull_request: branches: [main]` puts it on every PR.

### Verdict

**The reasoned path is real, end to end, on the YAML as it stood. No link was already
closed, so the fix does not shrink.** Two corrections to the issue's framing, and both
change the *shape* of the fix rather than the conclusion:

- The compromise lands at `make`, not at `git status`. **The check must therefore precede
  the build**, not merely exist.
- The cache's role is persistence under a stable trusted key, not third-party poisoning.
  That is precisely the argument for putting the pin **into the key**: it is what makes
  every pre-pin entry unreachable and forces the new check to run on any newly-pinned
  source, instead of waiting for an eviction that may be a week away.

### Why the local review saw nothing, which is worth writing down

Everything the local review would naturally have examined is real and correct. The floor
is parsed from exactly one place. The tag is derived from it rather than duplicated.
Retries are bounded. Failures are loud, named, and distinguished from parser regressions.
The printed version is checked twice, and the parse is held to written-out expectations
rather than to the other binary. Every individual step is well built.

The gap is a missing **category** of check — provenance — not a defective one. A review
that asks "is each step correct?" gets *yes* at every step and reports nothing. Finding
this needs the different question: *what does this job take on faith, and from whom?*

## Decision

### 1. A reviewed version → commit table, checked after the clone and before `make`

`docs/SUPPORTED_VERSIONS.md` remains the single source of the **version**. It is not a
source of **source**. A small table in `ci.yml`'s existing `git_floor` step maps the
documented floor to the commit that was reviewed for it, and publishes it as a step
output alongside `version`:

```
2.32) pin=ebf3c04b262aa27fbb97f8a0156c2347fecafafb ;;
```

`v2.32.0` is an annotated tag; it peels to that commit (subject `Git 2.32`, 2021-06-06),
resolved from upstream on 2026-08-26. After the clone, and **before** `apt-get` or
`make`, the provisioning step compares `git rev-parse 'HEAD^{}'` against it and refuses
on mismatch, with an `::error::` naming **expected and got on the same line** — the
annotation is what appears in the Actions summary, so both hashes have to be in it.

Ordering is the load-bearing part, per finding (2). A refusal builds nothing and caches
nothing: the step exits non-zero, the job fails, and `actions/cache` writes its entry
only when the job succeeds.

A floor with no row **fails the job**. It does not fall back to trusting the tag — that
fallback is the defect wearing a hat, and it is the same shape ADR 0082 refused when it
declined to make the floor leg advisory on fetch failure.

### 2. The update procedure is part of the boundary, so it is written next to the table

A pin nobody knows how to move correctly gets moved incorrectly. The five steps live in a
comment above the table in `ci.yml`; the one that matters is step 3 — **corroborate the
commit against a second, independent source before writing it down** (a clone you already
had, git's release announcement, or `git verify-tag` with Junio C Hamano's key in your
keyring). That human review, done once, is what the pinned hash is a durable record of.

The `::error::` for a mismatch says, in the message itself, *do not resolve this by
copying the got hash over the want hash*. That is the obvious wrong fix, it silently
reverts this entire ADR, and the moment somebody hits a red build is exactly the moment
they will reach for it.

### 3. The cache key names everything that decides what may be served from it

```
gv-git-floor-<os>-<arch>-tc<toolchain digest>-<version>-<pinned commit>
```

The toolchain digest is a 12-hex-character SHA-256 prefix over `uname -m`,
`cc -dumpmachine` and the glibc release from `ldd --version`, with the three facts echoed
in full beside it so a key that changes is diagnosable from the log rather than
mysterious. If any of the three cannot be read, the step **fails** rather than proceeding
— a key that quietly loses its toolchain identity is the exact failure this clause exists
to prevent, and it would fail silently and permanently.

`ImageOS` was rejected for this: it exists on GitHub-hosted runners and not on
self-hosted ones. Measuring the host says what actually determines whether a compiled C
binary is the right artifact.

**There are deliberately no `restore-keys`.** A prefix fallback would let an entry keyed
on some other commit answer a lookup for this one, handing the whole boundary straight
back. The workflow says so at the step, because the absence of a line is not self-
explanatory and "add restore-keys, we're missing the cache too often" is a reasonable-
sounding future edit.

## What would make this pass while the mechanism was broken?

- **The check never runs, because the cache always hits.** Closed by construction: the
  pin is *in* the key, so landing this ADR is itself a cache miss, and so is every future
  change to the pin. The check is exercised on the first run after any change to the
  thing it checks.
- **The pin is compared against something derived from the clone.** It is not: the
  expected value comes from the table in the workflow, the observed value from
  `rev-parse` in the cloned tree. Deriving one from the other is the
  "never assert a mapping by calling the function that defines it" trap, and it would
  make the comparison a tautology.
- **The check passes vacuously on an empty value.** `got` is captured with `|| true` so a
  broken clone yields the empty string, which cannot equal a 40-character hash; the
  message prints `<unreadable>` for it.
- **A floor with no row silently skips the check.** The `case`'s default arm fails the
  job.
- **The YAML says one thing and the dry run tests another.** The dry-run harness reads the
  step bodies **out of `ci.yml` itself** and substitutes only the `${{ }}` expressions
  that Actions resolves before bash ever sees them, so what ran locally is the thing that
  will run on the runner.

## Alternatives weighed

**Checksum the release tarball instead of pinning the commit.** The bytes of
`archive/v2.32.0.tar.gz` are *generated by GitHub* and are not contractually stable across
their own tooling changes — an assumption that has broken other ecosystems' pinning. A
commit hash is a content hash of the tree by construction, and it is the same number from
any host or mirror. The commit is the more durable checksum, not merely the more
convenient one.

**`git verify-tag` in CI.** Rejected; see the non-goals. It trades a pinned hash for a
keyring in CI — a key-distribution problem with its own trust root — in order to
re-establish, on every run, a fact a human can establish once.

**Pin the tag object hash as well as the commit.** Two things to update, no additional
guarantee: the tag object's security-relevant content is the commit it names, plus a
signature nothing here checks.

**Pin a container image containing git 2.32.** Refused for the reason ADR 0082 already
gives about distro packages: it moves the floor number into a second place — an image tag
— where it can disagree with the document.

**Vendor git's source in-tree.** Roughly forty megabytes of someone else's C in a
repository about drawing git history. Refused on proportion.

**Put the table in its own tracked file beside the workflow.** Considered. One row does
not need a file, and a second file is a second place to forget when the floor moves. If
the table ever grows past a handful of rows, that is the moment to reconsider.

## Non-goals — what this deliberately does not defend

Named explicitly, because a boundary whose limits are unstated gets credited with
strength it does not have.

- **No reproducible builds.** Two `make` runs over the same commit may differ byte for
  byte and that is fine. The pin fixes the **source**, not the artifact.
- **No signature verification of upstream, in CI.** The signature is checked once, by a
  person, at step 3 of the update procedure. The pin is the record of that.
- **No defence against the reviewed commit itself being bad.** If `ebf3c04…` was always
  malicious, this reproduces it faithfully. The pin makes the source *fixed and
  reviewable*; it does not make it *proven good*.
- **The rest of the provisioning chain stays unpinned:** `zlib1g-dev` and the apt archive
  it comes from, the runner image, and the floating major tags on third-party actions
  (`actions/cache@v4`, `actions/checkout@v4`, `dtolnay/rust-toolchain@stable`). That last
  one is a real and separate gap — `secrets` already SHA-pins gitleaks, so the repository
  knows the pattern — but it is a different boundary from #520's and pretending otherwise
  would make this ADR's claim broader than its diff.
- **Proportion is the point.** Single-user repository, not urgent. A pinned commit hash
  and an honest cache key are the right-sized boundary for a required job that builds a
  five-year-old C program off a name it does not control.

## Consequences

**Every artifact under the new key was provably built from the reviewed commit**, because
the only path to that key runs through a check that refuses anything else.

**Every pre-existing cache entry is unreachable**, immediately and without a purge. With
no `restore-keys` only an exact key match restores at all, and `gv-git-floor-Linux-2.32`
is in any case not a prefix of anything the new key can produce. The PR that
lands this is a guaranteed cache miss, so the check runs live on its own CI rather than
being merged untested — which is the whole answer to "how do you verify a workflow change
you cannot run locally".

**Moving the floor now costs a human step.** Resolve, corroborate, add a row: about two
minutes. That is not friction to be optimised away later; it *is* the review, and the
pinned hash is only worth what that step put into it.

**A runner-image glibc move, or a change of arch, re-provisions.** ADR 0082 measured that
at about a minute on four cores. Accepted, and cheap for a key that now means what it
says.

**The failure mode is legible without leaving the Actions log**: one `::error::` line
carrying both hashes and the reason, and a sentence heading off the wrong fix.
