# Browser tests

`cargo test` never compiles code behind `#[cfg(target_arch = "wasm32")]`. Every UI
defect found in August 2026 lived exactly there, and every one of them coexisted
with a fully green seven-check gate and ~1,800 passing Rust tests:

| defect | what was true | what the suite said |
|---|---|---|
| #68d | `StatusSections` had 20+ tests and **zero consumers** | green |
| #69c | `CumulativeHeights` had 9 tests and **zero consumers** | green |
| #210 | keyboard navigation had never worked | green |
| #350 | `scroll_to_reveal` was mutation-proven, then never called | green |

The Rust suite proves the pure core is **correct**. This suite proves it is
**reached**.

## Running

```
ci/browser/run.sh                 # everything
ci/browser/run.sh --headed        # watch it
ci/browser/run.sh hunk-keyboard   # one file
```

First run installs `@playwright/test` and needs a Chromium build present
(`npx playwright install chromium`). Everything after that is offline.

Prerequisites, both checked with a clear message rather than a stack trace:

- `target/debug/git-vista-server` — `cargo build -p git-vista-server`
- `crates/git-vista/dist/` — `trunk build --config crates/git-vista/Trunk.toml`

## Why it runs in a network namespace

`crates/git-vista-server/src/state.rs` compiles the listen address in
(`PORT: u16 = 8080`) and `parse_bind_addr` refuses anything else — binding beyond
loopback is a security decision, not a setting. So a test server cannot pick a
free port, and `dev testbed` pays for its own port with a 10–25 minute rebuild.

`run.sh` uses `unshare --user --map-root-user --net`, giving the tests their own
loopback and therefore their own 8080, invisible to whatever the operator is
running on the host's 8080. Nothing is rebuilt, the bind guard is untouched, and
the binary under test is the real one.

**The cost of that choice, stated plainly:** a namespace with only loopback has
no network, so Chromium reports `navigator.onLine === false` and the app's
offline guard refuses to open a repository. `helpers.mjs` forges
`navigator.onLine` to get past it — which means **this suite can never test the
offline guard**, because it fabricates the exact signal that guard reads. That
coverage has to come from a manual device pass.

## What's here

| file | role |
|---|---|
| `fixture.mjs` | builds the throwaway repo — every shape in it exists for a specific defect |
| `server.mjs` | spawns a server with its own state dir and repository list |
| `global-setup.mjs` | fixture + server + spends the one-time token, saves `storageState` |
| `tests/reachability.spec.mjs` | is the shipped code actually reached? |
| `tests/hunk-keyboard.spec.mjs` | #210, as a matched control/defect pair |
| `tests/harness-selfcheck.spec.mjs` | can these assertions go red at all? |

### The self-check is not optional

A test never observed to fail is a hypothesis. `harness-selfcheck.spec.mjs` runs
each assertion against a DOM broken in the exact way the real test claims to
detect, and requires it to fail. It earned its place on its first run by catching
a **vacuous mutation of its own**: `document.body.focus()` is a silent no-op
(`<body>` has no `tabindex`), so a "focus was lost" mutation had not moved focus
at all.

Add an assertion here whenever you add one to the suite.

### Expected failures are deliberate

`hunk-keyboard.spec.mjs` marks the long-patch cases `test.fail()`. Playwright then
reports an **error if they pass**. So when #210 is fixed, this suite demands
attention rather than going quietly green — the opposite of the failure mode that
let #210 survive this long.

The short-patch cases pass today and are the positive control. The pair is the
point: the same keypress on the same widget works on a short patch and fails on a
long one, which locates the defect in virtualization rather than in the focus
model.

## What this does not cover

- **The offline guard** — forged, see above.
- **Safari/WebKit and iPad** — Chromium only. iPad behaviour (VoiceOver, Split
  View, Magic Keyboard, home-screen install) is still a manual pass.
- **Focus-blocking of the update overlay** — pointer-blocking is measured; whether
  Tab can reach controls behind it is not.
- **Real network conditions** — there is no network in the namespace at all.
