# Round 1 · Grok reviews

Reviewer only. Diffs, not PR descriptions. Append as each PR opens.

```yaml
started_at: 2026-09-08T21:57:11-04:00
role: REVIEWER ONLY
tag: CLEAR
sign_as: grok
assigned_issues: [757, 752, 357, 748, 762, 89, 396]
```

## Queue at session start

No open PRs in `tom2025b/Git-Vista`. All seven assigned issues are still open with `closed_by_pull_requests.total_count = 0`. All seven lane worktrees are still on `wip/<issue>-placeholder` at `0944c523` (same as `origin/main`). Nothing to review yet.

Territory note for later: the reviewer handoff said #757/#752 must not touch `crates/git-vista-ui/**`. Claude 2's own #752 handoff explicitly allows `crates/git-vista-ui/src/activity/**` because that is where the unscoped tag/stash reads live. When #752's PR opens, judge it against the lane's `allowed_paths`, not the reviewer summary's sandbox-only blanket.

---

## #702 — spare-capacity bookkeeping (not a coding task)

**Verdict: CLOSE as completed.** Coordinator (max, territory A) can close it; this is not a review of a new PR.

Reason, from the issue graph and what is on `origin/main`, not from anyone's session report:

- #723's own acceptance said "#702 closes only when this lands." #723 is **CLOSED / COMPLETED** by merged PR #729 (`fix: deny pathname AF_UNIX during clone checkout`, merged 2026-09-08T11:59:24Z, `Refs #702, Closes #723`).
- The 16-hour plan also said #702 closes when #735 and #744 are answered. Both are **CLOSED / COMPLETED** (#735 at 20:04Z, #744 at 21:56Z). Sibling PRs: #720 (allowlist, refs #702, merged), #761 (enumeration of Network-tier hook spawns, merged).
- Current `origin/main` carries the checkout-specific seccomp path: `checkout_command_async` / `SeccompProfile::Checkout` / `--seccomp-checkout` in `crates/git-vista-server/src/sandbox/`.
- Leftover branch `origin/fix/702-clone-checkout-credential-allowlist` is an ancestor of `origin/main` (empty diff). No remaining commits.
- #702 itself still has `closed_by_pull_requests.total_count = 0` because no PR used `Closes #702` — they used `Refs #702`. That is stale bookkeeping, not remaining work.

Re-derived from PR #729's merged tree (`b9f08e3c`), not the PR body, after Tom asked the remaining-gap question. Mutation arms were **not** re-run.

**Does #729 actually deny pathname AF_UNIX during clone checkout?** Yes. That is the exact capability #702 said #720 left open (env allowlist removes the locator; a hook can still read a path out of `$HOME` and connect).

The production chain, from the clone handler to the filter:

1. `handlers/clone.rs` post-transfer spawns are only `network_command_without_credential(&CheckoutPolicy, …)` — HEAD check and `checkout -f`. Transfer stays on `network_command_with_credential`.
2. That sealed helper calls `spawn::checkout_command_async` → `full_checkout_argv` → `checkout_sandbox_argv` → `shim_argv(..., SeccompProfile::Checkout)`, which emits `--seccomp-checkout` and `--net-allow`. Ordinary `sandbox_argv` does not emit the flag (pinned in `argv.rs`).
3. Shim `main.rs` maps `(net_allow=true, seccomp_checkout=true)` to `NetScope::Checkout`.
4. `seccomp_filter::rules_for(Checkout)` inserts the same `af_unix_rule` Strict uses on `SYS_socket` and `SYS_socketpair`: first argument `== AF_UNIX` → `EPERM`. `SYS_connect` is not named. For a freshly exec'd hook that is equivalent: you cannot `connect()` a pathname socket without first creating an AF_UNIX fd, and Landlock still does not mediate pathname `AF_UNIX` (that is why this rule exists).
5. The new `checkout_security.rs` test is the composed proof of #702's remaining attack, not an argv scan: parent has no `SSH_AUTH_SOCK`; a fetched `post-checkout` hook sources `$HOME/.keychain/fixture-sh`, exports the path, then `socket(AF_UNIX)` + `connect()`. Expected output is `unix=errno:EPERM` with TCP still connecting.

Ordinary Network transfer still allows AF_UNIX on purpose (#188 / authenticated SSH). That is not a leftover of #702.

**#702 is stale bookkeeping.** Close it.

---

## Reviews

### PR #766 — issue #748 — DO NOT LAND

https://github.com/tom2025b/git-vista/pull/766 · `fix/748-dev-browser-honors-cargo-target-dir` @ `36523be1`

**Territory held.** Touched `dev`, `ci/browser/run.sh`, `ci/browser/fixture.mjs`, `ci/browser/server.mjs`, `ci/browser/README.md`, `ci/browser/target_dir_test.sh`. No `crates/gv-sandbox/**`, no `docs/SECURITY_MODEL.md`, no `crates/**`.

**The fix itself is real.** Re-derived from the diff, not the PR body:

- Root cause is lookup after `cargo build`, not cargo ignoring the env. `cmd_browser` already runs `cargo build -p git-vista-server -p git-vista-fixtures`; cargo honors `CARGO_TARGET_DIR`. `run.sh` / `server.mjs` / `fixture.mjs` all hardcoded `$repo/target/debug/…`.
- `run.sh` now resolves `${CARGO_TARGET_DIR:-$repo/target}` to an absolute path **before** `unshare` `cd`s into `ci/browser`, then exports it so the Node launchers see the same tree. That ordering is load-bearing: Node `path.resolve` is cwd-relative, and the inner namespace cwd is `ci/browser`.
- `server.mjs` and `fixture.mjs` join `CARGO_TARGET_DIR` when set, else the worktree `target/`.
- Default fallback is preserved (`env -u CARGO_TARGET_DIR` case).

**The test proves the mechanism when it is run, and CI never runs it.** I ran `ci/browser/target_dir_test.sh` against the PR commit: green. Two cheap mutations in a throwaway tree, both red, both differently:

1. Hardcode `server.mjs` `SERVER_BIN` back to `…/target/debug/git-vista-server` → `AssertionError` assigned-target vs worktree-target.
2. Point `run.sh` `bin=` / `fixture_bin=` back at `$repo/target` → `no server binary at …/repo/target/debug/git-vista-server`.

That is the right proof. Nothing in `dev gate` or GitHub CI invokes the script. `crates/git-vista-server/tests/dev_script_guards.rs` exists specifically because “a guard nobody runs is not a guard,” and it only wraps `ci/*_test.sh` at the `ci/` root. This script sits at `ci/browser/target_dir_test.sh` and is not listed there. The Core CI job is `cargo test`; the browser CI job uses the default `target/` (no `CARGO_TARGET_DIR`), so a revert of the lookup stays green on GitHub.

**What would make this LAND:** wire the script into something `cargo test --workspace` actually runs. Cheapest match to the existing pattern: one `run_guard` line in `dev_script_guards.rs` (may need the script moved to `ci/target_dir_test.sh`, or `run_guard` accepting `browser/target_dir_test.sh`). That file is `crates/**`, which this lane was forbidden to touch — coordinator (codex-coord, territory B) either authorizes that one-file exception on this PR, or lands a one-line follow-up before merge. Calling it only from `gate_body` in `dev` is not enough: GitHub CI does not run `dev gate`.

**Named, not blocking the issue:** `gv` still hardcodes `SERVER_BIN="$REPO/target/debug/git-vista-server"` (line 36). The PR body already said `dev serve` still has this class of defect; confirmed. Out of #748's `allowed_paths`.

Coordinator: **codex-coord** (territory B). Do not merge until the guard is on the `cargo test` path.

### PR #767 — issue #757 — LAND

https://github.com/tom2025b/git-vista/pull/767 · `fix/757-network-tier-parent-death` @ `77c72e1baeec`

**Territory held.** Touched `crates/git-vista-server/src/sandbox/{spawn,lifecycle}.rs`, new ADR 0143, ADR index, and a one-line forward pointer on ADR 0141. No `.github/**`, no `crates/git-vista-ui/**`, no `docs/SECURITY_MODEL.md`. The 0141 pointer is slightly outside “a NEW adr only”; it does not rewrite 0141’s record.

**The claim holds, re-derived from the diff.**

On `origin/main`, `wrap_with_reaper` returns unwrapped argv unless `argv[0]` is the resolved `bwrap` path. Network’s `sandbox_argv` starts with `policy.shim` (`gv-sandbox`), so it never wrapped. Production spawn is one chokepoint: `command_async` and `checkout_command_async` both go through `command_from_argv` → `wrap_with_reaper`. `policy_for` / `policy_for_repo` set `policy.shim` from `shim::shim_path()`, the same `OnceLock` the new comparison reads, so the unit test’s `shim_path()` argv is not a circular fake — the lifecycle test goes through `full_argv(policy)` and would miss the wrap if those paths disagreed.

This PR adds a second structural match: `argv[0] == shim_path()`. Unsandboxed `["git"]` matches neither. The reaper itself is unchanged: fork, `setpgid`, poll `getppid()` against the caller pid passed in, `killpg` on mismatch. For Network that `killpg` is the whole mechanism (no pid namespace). Ordinary descendants stay in the group; `setsid` leaves it. That residual is a run test (`a_network_tier_reaper_does_not_reach_a_double_forked_setsid_grandchild` asserts ticks *keep growing*), which is what #757 asked for when a full Strict-equivalent guarantee is unreachable without breaking F3.

**Mutation arms, from the tests, not from the PR body.** I did not re-run failure-atlas.

1. Remove: `is_network_shim_launch = false`. Unit test network leg fails (`argv[0]` stays the shim). Lifecycle Network-orphan test fails (ticks continue after helper SIGKILL). Native `#[cfg(test)]` in `spawn.rs` / `lifecycle.rs` — `cargo test` compiles both.
2. Weaken: `argv.first().is_some()`. Unit test unsandboxed leg fails (bare `git` gets a reaper prefix). They recorded honestly that this arm *survived* `sandbox::lifecycle` alone, then added this unit test so the second arm has somewhere to go red. Different failure from arm 1.

**#728 overlap:** none. Same reaper, second argv shape. #728 already on main.

**Named, not blocking:** `gv-sandbox-reaper/main.rs` still comments that only Strict is wrapped. That file is outside this lane’s `allowed_paths`; comment drift, not a mechanism miss. Three `wip(#757)` commits were not squashed (permission gate); squash-on-merge is fine.

Coordinator: **max** (territory A). Land when the seven checks are green.

**Signed:** grok · 2026-09-08T22:32:00-04:00
