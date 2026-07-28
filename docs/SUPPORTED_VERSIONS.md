# Supported Git and Safari versions

Both floors below are derived from something the codebase actually depends
on, not asserted. Where the evidence is thin, that's said explicitly rather
than rounded up to a plausible-sounding number.

## Git: 2.32 or later

**Why 2.32:** the `GIT_CONFIG_GLOBAL` environment variable — which lets
git-vista's write path pin the global-config file it reads without touching
`$HOME`, so it can override a user's `core.hooksPath` or credential helper
deterministically rather than trusting whatever the ambient environment
provides — was added in Git 2.32 (June 2021). Below that version the
variable does not exist and config isolation falls back to weaker
mechanisms.

This floor is tied to the git-process-execution-policy work landing in #66
(M1.13, in progress in a parallel session as of this writing), which is what
actually depends on `GIT_CONFIG_GLOBAL`. That policy is not yet merged to
`main`, so as of this doc nothing on `main` strictly requires 2.32 yet — the
number is documented now because it's the floor the in-flight design commits
to, and because current dev/CI environments already comfortably clear it (see
below), so recording it early costs nothing and avoids the floor drifting
undocumented once #66 lands.

- **Empirically confirmed:** this repo's dev environment and this session's
  test runs are on **git 2.43.0** (`git --version`), well above the 2.32
  floor.
- **CI:** `ubuntu-latest` GitHub-hosted runners currently ship a git version
  in the same 2.4x range, also clear of the floor. **Now checked**: the
  `core` job's "Git version meets the documented floor" step (#67 M1.14)
  parses the `## Git: X.Y or later` heading above out of this file and fails
  the build if the runner's `git --version` is older — one source of truth,
  so the doc and the check cannot drift apart. Added ahead of #66 actually
  landing, on the same reasoning as documenting the number early: it costs
  nothing now and means the floor is already enforced the moment it becomes
  load-bearing rather than being remembered later.

## Safari: 16.4 or later (iOS/iPadOS 16.4, and the matching macOS Safari 16.4)

**Why 16.4:** the frontend's app shell and full-screen overlays size
themselves with the CSS dynamic-viewport-height unit (`dvh`), specifically to
track the real visible box under Safari's collapsing/expanding URL and tab
bars rather than the static `100vh`, which on iOS undercounts or overcounts
depending on chrome state. The code states this directly — see
`crates/git-vista/styles.css`, three call sites (`.app`, the menu inline
`max-height`, and the full-screen viewer), each with the comment `iOS 16.4+:
track the real visible box under Safari's bars`. Safari (all platforms)
shipped `dvh`/`svh`/`lvh` support in 16.4, released March 2023.

Each of those three call sites sets `100vh` first and `100dvh` second (later
declarations win), so a browser that doesn't understand `dvh` falls back to
the static unit rather than breaking — the floor is a *quality* floor, not a
hard cutoff below which the app fails to render, and `docs/IPAD_DESIGN.md`'s
"Browser and PWA Requirements" section separately asks to "test current
Safari/iPadOS... do not depend on a WebKit-only feature for correctness,"
which is consistent with graceful degradation rather than a hard gate.

- **Evidence used:** the in-code comment in `styles.css` and the `dvh`/`vh`
  fallback pattern itself. `docs/IPAD_DESIGN.md` asks for "modern dynamic
  viewport units" and "current Safari/iPadOS" testing but does not itself
  state a version number, so 16.4 is derived from the CSS feature's actual
  ship date, not asserted from that doc.
- **Evidence not available:** no CI job or manual test matrix currently
  pins or verifies a minimum Safari version against a real device/simulator
  — `docs/IPAD_DESIGN.md`'s "Validation Matrix" section lists device/orientation
  combinations to cover but not a version floor to enforce. If a hard floor
  (rather than a graceful-degradation target) is wanted, that needs an
  explicit decision and a device/BrowserStack-class check, neither of which
  exists today.

---

**Signed:** thomas2010 · 2026-07-27T20:51:16-04:00
