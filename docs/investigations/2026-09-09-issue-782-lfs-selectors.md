# #782 — Measuring the Git LFS selector surface in checkout

Measured at `76c17c80fd45adf17cbcedde1dee743062d97ad7`. `git-vista` itself
tracks no LFS files, so nothing in this measurement touched the live repo or
another lane's worktree.

**Scope of this document.** Measurement only, per the lane's mandate. It
proposes a candidate mitigation direction at the end but implements nothing in
`crates/**`.

## 0. Correcting the handoff before using it

The handoff this lane started from (`design-docs/handoffs/2026-09-09-r4-h-claude-782-lfs-measure.md`)
had already been corrected once by a pre-flight audit (the "checkout is a
different tier" claim). Verifying it further, section 1c's specific claim
needed a second correction, made here because it changes what is worth
measuring:

> `network_exec.rs:1742 sets GIT_CONFIG_NOSYSTEM=1`

**This is true only inside a `#[cfg(test)]` fixture, not in production.**
Opened the file: line 1742 sits inside
`installed_remote_helper_selectors_are_blocked` (`#[tokio::test]`, line 1698),
and the value is applied via `.pinned_env_for_test(&env)`
(`network_exec.rs:1753`), calling `SandboxedCommand::pinned_env_for_test`
(`spawn.rs:477`), which is itself gated `#[cfg(test)]` (`spawn.rs:477`, doc
comment at `spawn.rs:464-476` says plainly: *"Gated to `#[cfg(test)]` so the
production surface stays free of environment control entirely"*). A search of
`crates/**/*.rs` (not the whole repo — narrower than "project-wide") confirms
this is the only production-adjacent mention:

```
$ grep -rn "GIT_CONFIG_NOSYSTEM" crates/ --include="*.rs"
crates/git-vista-server/src/sandbox/network_exec.rs:1742:                ("GIT_CONFIG_NOSYSTEM", "1".into()),
crates/git-vista-server/src/sandbox/spawn.rs:114:/// * `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` / `GIT_CONFIG_NOSYSTEM` and
```

The second hit is a doc comment describing a *different* mechanism
(`SCRUBBED_GIT_GEOMETRY_ENV`, which deliberately does **not** scrub these three
names for ordinary spawns — see §3). So: no production code path sets
`GIT_CONFIG_NOSYSTEM`. That made the question genuinely open rather than
already answered, and is why the rest of this document measures it directly
instead of trusting the handoff's premise.

## 1. Environment, recorded before and after

```
$ git --version
git version 2.53.0
$ git lfs version
git-lfs/3.7.1 (GitHub; linux amd64; go 1.25.0)
$ git config --system --get-regexp '^filter\.lfs'
filter.lfs.clean git-lfs clean -- %f
filter.lfs.smudge git-lfs smudge -- %f
filter.lfs.process git-lfs filter-process
filter.lfs.required true
$ git config --global --get-regexp '^filter\.lfs'
(empty, exit 1)
```

Confirmed unchanged at the end of the run (same two commands, same output).
Neither was written to by anything in this investigation — no
`git lfs install` of any kind was run against system or global scope; the box
was already in this state (apt's postinst) before this lane started.

## 2. Which selectors exist, and where each config scope can set them

| Selector | Set by | Reaches `.lfsconfig`? | Reaches clone's remote `.git/config`? |
|---|---|---|---|
| `filter.lfs.process` / `.clean` / `.smudge` / `.required` | system (`/etc/gitconfig`), global (`~/.gitconfig`), repo `.git/config` | **No** — `.lfsconfig`'s accepted key set is a restricted safe subset and does not include `filter.*` (git-lfs's own docs; not independently re-derived here, see gap list) | **No** — `git clone` never copies the source repository's `.git/config`; only refs and objects transfer |
| `lfs.customtransfer.<name>.path` / `.args` | same three scopes as above | No, same reason | No, same reason |
| `lfs.extension.<name>.clean` / `.smudge` | same three scopes as above | No, same reason | No, same reason |

So for every selector in scope, the two config sources that can actually name
an executable are **operator-owned local/global config** and **the operator's
own system package config** (`/etc/gitconfig`, which is what `apt install
git-lfs` wrote). A cloned repository's own tracked files cannot supply any of
these directly — that half of the question was already structurally closed
before any experiment, and this document does not claim otherwise.

## 3. Where the boundary actually is in code

**Checkout is Network tier, and it does chain the transport policy** — this
was the pre-flight audit's correction and it still holds:

- `policy_for_clone_checkout` (`sandbox/mod.rs:1374`) copies `tier:
  transfer.tier` at `sandbox/mod.rs:1380`, and `policy_for_clone`
  (`sandbox/mod.rs:1283`) fixes `tier = Tier::Network` at `sandbox/mod.rs:1286`.
- `network_command_without_credential` (`network_exec.rs:359`) chains
  `.with_network_transport_policy()` and `.with_untrusted_checkout_env()` at
  `network_exec.rs:367-368`.

Checkout's argv also carries `network_command_without_credential`'s own fixed
config pins — `compose_network_args(args, None)` (`network_exec.rs:364`)
prepends `FORCED_NETWORK_ARGS` (`network_exec.rs:129-140`): `-c
core.askpass=`, `-c credential.helper=`, `-c core.sshCommand=ssh`, `-c
core.fsmonitor=false`, `-c protocol.ext.allow=never`. None of these five name
a `filter.*` key or touch `HOME`/`XDG_CONFIG_HOME`/the ro-tree grants, so
they do not bear on the filter-selector question this document measures —
but the first draft of §4's reconstruction omitted them, which a reviewer
correctly flagged (see §7); §4.4's command now includes them.

**What actually differs for checkout vs. an ordinary spawn, as far as
`filter.*` config is concerned, is the environment-construction *strategy*,
not a config-key filter:**

- An ordinary spawn (`command_async`, `spawn.rs:504`) removes a fixed,
  named list of repository-geometry variables
  (`SCRUBBED_GIT_GEOMETRY_ENV`, `spawn.rs:141-151`) and otherwise **inherits**
  the server process's environment unchanged. `GIT_CONFIG_GLOBAL` /
  `GIT_CONFIG_SYSTEM` / `GIT_CONFIG_NOSYSTEM` are explicitly **not** in that
  removal list — `spawn.rs:112-121` documents this as the "user-git parity"
  decision, and the test `the_launcher_scrubs_gits_repository_geometry_environment`
  (`spawn.rs:960-1007`) asserts both directions: the geometry family is
  removed, and `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` are asserted **not**
  removed (`spawn.rs:994-1006`).
- Checkout (`with_untrusted_checkout_env`, `spawn.rs:391-397`) does something
  structurally different: `env_clear()` first, then copies in **only** the
  seven names in `UNTRUSTED_CHECKOUT_ENV_ALLOWLIST` (`spawn.rs:215-223`:
  `PATH`, `HOME`, `XDG_CONFIG_HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`,
  `LC_MESSAGES`). This is a genuine allowlist, not a denylist, and the doc
  comment at `spawn.rs:196-206` explains explicitly *why*
  `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`/`GIT_CONFIG_NOSYSTEM` and the
  `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n` family are **left out**: they "would
  buy the same config parity `HOME` and `XDG_CONFIG_HOME` buy, and they can
  *carry a credential in the value itself*."

The load-bearing fact the code states plainly and this measurement confirms
by experiment: **an environment allowlist is not a Git-configuration-key
allowlist.** Leaving `GIT_CONFIG_NOSYSTEM` off the allowlist does not stop
system config from being read — it just means nothing sets the variable that
would. Git's default behavior (read `/etc/gitconfig` unless told not to)
applies unchanged.

Filesystem side: `DEFAULT_RO_TREES` (`sandbox/mod.rs:428`) grants `/etc`
read-execute in **every tier**, and `policy_for_clone_checkout` does not
remove it (`ro_trees: transfer.ro_trees,` unchanged, `sandbox/mod.rs`
constructor body). `$HOME` is also ro-granted for the same "operator filter
configuration... deliberate product behaviour" reason (`spawn.rs:182-188`,
quoting ADR 0128). Network tier applies no mount namespace at all — no
`bwrap` prefix is added unless `policy.tier == Tier::Strict`
(`sandbox/mod.rs:1522` in `shim_argv`) — so these are Landlock grants over the
**real host filesystem**, not a virtualized view. Both `/etc/gitconfig` and
`~/.gitconfig` are therefore visible to the checkout child exactly as they
are on disk.

## 4. The experiment

Rust's `pub(crate)` visibility means `policy_for_clone_checkout`,
`network_command_without_credential`, and the rest are not callable from
outside `git-vista-server`'s own compiled test binary, and adding a new test
to exercise them would mean writing into `crates/**`, which this lane may not
do. The production HTTP path is also not reachable safely: `bind_addr()`
(`state.rs:37-60`) hardcodes `127.0.0.1:8080` (`state.rs:31-32`, `PORT: u16 =
8080`) with **no configurable port at all** — `GIT_VISTA_BIND_ADDR` is only
ever accepted if it parses to that exact address. There is no "different
port" to run a second instance on; the handoff's instruction to do so
describes a knob that does not exist in this codebase. Running the actual
server binary at all would therefore risk exactly the collision with Tom's
live session the standing rule forbids, so this measurement does not start
the `git-vista-server` binary.

**What it does instead: invoke the real, unmodified `gv-sandbox` binary
directly**, with an argv and environment hand-derived from the source cited
above, line by line, rather than through the HTTP/axum layer. This is not
"raw git" — `gv-sandbox` is the actual security-enforcing binary (it is what
applies Landlock and seccomp; `main.rs:1047-1076`), unmodified, built from
this exact tree. The only thing not exercised is the axum routing glue and
`wrap_with_reaper`'s process-group cleanup wrapper (`spawn.rs:592-611`) —
noted as a gap in §6, and immaterial to the filesystem/config question this
issue asks about, since the reaper only forwards a pid and re-execs the same
argv (`reaper.rs`, not separately re-derived here beyond its call site).

Cross-check: this exact call shape — `policy_for_clone_checkout(...)` then
`network_command_without_credential(&policy, &dest, &["checkout","-f"])` then
`.output()` — is not invented for this measurement; it is the identical
pattern `handlers/clone.rs:226-231` uses in production (`.output()` itself at
`clone.rs:234`) and the identical pattern an *existing* test already
exercises end-to-end at `sandbox/checkout_security.rs:113-134`
(`.output()` at `checkout_security.rs:137`) inside
`a_fetched_hook_that_self_sets_ssh_auth_sock_cannot_connect_but_tcp_survives`
(`checkout_security.rs:23`). This document could not run that test unmodified
to add an LFS fixture (would require editing `crates/**`), but its existence
confirms the call shape reconstructed below is the one production and the
crate's own tests already use.

### 4.1 Building the real binaries

```
$ CARGO_TARGET_DIR=/home/tom/.cargo-targets/gv-782 buildlock cargo build --bin git-vista-server --bin gv-sandbox
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 32.83s
```

(`gv-sandbox-reaper` was queued behind other lanes' builds on this box's
single build slot for the duration of this measurement and never completed —
see gap list.)

### 4.2 Fixture

A throwaway fixture outside the repo (`/tmp/.../scratchpad/gv782-fixture/`,
not part of this PR):

- `src/` — a real git repo with `.gitattributes` marking `payload.bin`
  `filter=lfs`, and a hand-written LFS pointer file (LFS pointers are plain
  text; no `git lfs` needed to author one).
- `home/bin/git-lfs` — a stub that logs its own invocation to stderr and
  exits, standing in for the real binary in the primary experiment so the
  result doesn't depend on network access to a real LFS store:
  ```sh
  #!/bin/sh
  echo "GV782-LFS-WRAPPER-INVOKED argv=[$*] pwd=$(pwd) whoami-path=$0" >&2
  exit 17
  ```
- `home/.gitconfig` — **empty of LFS config** for the primary experiment (so
  only `/etc/gitconfig`'s already-installed filter is in play — this
  reproduces a real developer machine after `apt install git-lfs`, not a
  contrived one), and used separately in §4.5 to test the operator-global-config
  case with its own filter name.

Transfer-phase fixture setup (`git clone --no-checkout`) was done with plain
git, unsandboxed — this step is not part of what checkout's policy governs
and mirrors `clone_transfer_args` (`clone.rs:148-150`) only to produce a
realistic "already-transferred, not-yet-checked-out" destination directory.

### 4.3 Positive control — unsandboxed, proves the fixture actually triggers LFS

```
$ env -i HOME=$FIX/home PATH="$FIX/home/bin:/usr/bin:/bin" \
    git -C $FIX/clones_root/repo-control checkout -f
GV782-LFS-WRAPPER-INVOKED argv=[filter-process] pwd=.../repo-control whoami-path=.../home/bin/git-lfs
fatal: the remote end hung up unexpectedly
```

**Observed.** `filter.lfs.process` (system config) fired, invoking our
wrapper as `git-lfs filter-process`. The subsequent failure is expected: the
wrapper doesn't speak git's filter pkt-line protocol, it just proves it was
reached.

### 4.4 The real question — same fixture, through the reconstructed checkout sandbox

Argv assembled from `shim_argv` (`sandbox/mod.rs:1520-1586`) applied to
`policy_for_clone_checkout(clones_root)`, plus `compose_network_args`'s
`FORCED_NETWORK_ARGS` (§3), with the fixture's `HOME` standing in for the
operator's. **This is the corrected, complete reconstruction** — the version
first run here omitted `FORCED_NETWORK_ARGS` and 4 of 14
`DEFAULT_SECRET_EXCLUDES` entries (`.kube`, `.config/google-chrome`,
`.config/chromium`, `.mozilla`, `sandbox/mod.rs:227-247`); a cross-family
review caught both (§7), and this is the re-run with them included — same
result, now on a fully faithful argv:

```
$ env -i HOME=$FIX/home PATH="$FIX/home/bin:/usr/bin:/bin" \
    GIT_ALLOW_PROTOCOL="http:https:ssh:git:file" GIT_PROXY_COMMAND="" \
    /home/tom/.cargo-targets/gv-782/debug/gv-sandbox \
    --abi-floor 6 \
    --rw /dev --rw "$FIX/clones_root" \
    --ro /usr --ro /bin --ro /lib --ro /lib64 --ro /etc \
    --ro /run/systemd/resolve --ro /run/resolvconf --ro /run/NetworkManager \
    --ro "$FIX/home" \
    --exclude "$FIX/home/.ssh" --exclude "$FIX/home/.claude" \
    --exclude "$FIX/home/.claude.json" --exclude "$FIX/home/.config/gh" \
    --exclude "$FIX/home/.aws" --exclude "$FIX/home/.netrc" \
    --exclude "$FIX/home/.git-credentials" --exclude "$FIX/home/.npmrc" \
    --exclude "$FIX/home/.gnupg" --exclude "$FIX/home/.docker" \
    --exclude "$FIX/home/.kube" --exclude "$FIX/home/.config/google-chrome" \
    --exclude "$FIX/home/.config/chromium" --exclude "$FIX/home/.mozilla" \
    --exclude "$FIX/home/.local/state/git-vista/trusted-repos" \
    --seccomp-checkout --hooks-run --net-allow \
    --net-port 443 --net-port 80 --net-port 9418 \
    -- git \
    -c core.askpass= -c credential.helper= -c core.sshCommand=ssh \
    -c core.fsmonitor=false -c protocol.ext.allow=never \
    -C "$FIX/clones_root/repo-sandboxed-v2" checkout -f

GV782-LFS-WRAPPER-INVOKED argv=[filter-process] pwd=.../repo-sandboxed-v2 whoami-path=.../home/bin/git-lfs
fatal: the remote end hung up unexpectedly
exit=128
```

**Observed, not reasoned: REACHABLE.** Identical invocation, identical argv
(`filter-process`), under the real Landlock-enforcing `gv-sandbox` binary,
Network tier, `--seccomp-checkout` profile, with every flag
`policy_for_clone`/`policy_for_clone_checkout`/`compose_network_args`
construct for a real clone's checkout phase now included. What is still not
reproduced here is the axum HTTP layer and `wrap_with_reaper`'s pid-forwarding
wrapper (`spawn.rs:592-611`) — see §6.

### 4.5 Corroboration with the real, unmodified `git-lfs` binary

Same argv, `PATH` pointing at the system `/usr/bin` (no stub), no LFS
remote configured:

```
$ env -i HOME=$FIX/home PATH="/usr/bin:/bin" ... gv-sandbox ... -- git -C $FIX/clones_root/repo-reallfs checkout -f
Downloading payload.bin (12 KB)
Error downloading object: payload.bin (df8a9eb): Smudge error: Error downloading
  payload.bin (...): error initializing custom adapter "lfs-standalone-file"
  worker 0: [0] error creating handler: failed to call `git rev-parse --git-dir`:
  fatal: not a git repository (or any parent up to mount point /)
error: external filter 'git-lfs filter-process' failed
fatal: payload.bin: smudge filter lfs failed
exit=128
```

**Observed.** The real 3.7.1 binary was reached and read the pointer; its
local-file transfer adapter began initializing to fetch the object
(spawning its own nested `git rev-parse --git-dir`) before failing on an
unrelated discovery error in that nested call — it did not complete a
transfer. This corroborates §4.4 with unmodified production tooling rather
than a stand-in (the binary was reached and started real work), but the
*cause* of the nested-call failure was not further diagnosed — flagged in
§6, not needed to answer the reachability question, which §4.4's cleaner
stub result already answers unambiguously. This run used a reduced exclude
list (the first-draft 10 of 14) since it predates the §4.4 correction; the
gap is immaterial here for the same reason given in §4.4 (`.kube` and the
three browser-profile excludes are unrelated to `/etc`, `$HOME/.gitconfig`,
or the filter binary's own path).

### 4.6 Counterfactual — does the fix the handoff assumed already existed actually work?

Same argv as §4.4, one variable added:

```
$ env -i HOME=$FIX/home PATH="$FIX/home/bin:/usr/bin:/bin" \
    GIT_ALLOW_PROTOCOL=... GIT_PROXY_COMMAND="" GIT_CONFIG_NOSYSTEM=1 \
    gv-sandbox ... -- git -C $FIX/clones_root/repo-sandboxed-nosys checkout -f

Your branch is up to date with 'origin/master'.
exit=0
$ cat repo-sandboxed-nosys/payload.bin
version https://git-lfs.github.com/spec/v1
oid sha256:df8a9eb3086c8de25294f2acfae489e12e12f644abbdd078dccc1356fbd7b9bd
size 12345
```

**Observed.** Wrapper never invoked, checkout succeeds cleanly, the file is
left as the raw LFS pointer text (proof the smudge filter genuinely did not
run, not just that no error printed). `GIT_CONFIG_NOSYSTEM=1` is a real,
working mitigation for the *system*-config vector specifically — see §5 for
why it does not address the global-config vector.

### 4.7 Generalizing beyond LFS — operator global config, isolated

To confirm this is git's general `filter.<name>.*` executable-selector
mechanism (LFS is one consumer of it, not the boundary itself), and to
directly test the **operator-owned global config** case the issue names
separately from system config: a fresh fixture repo, no LFS file at all,
one file under a filter name defined **only** in `$HOME/.gitconfig`:

```
$ cat $FIX/home/.gitconfig
[filter "gvtest"]
	smudge = gv782-exec-marker smudge
	clean = cat
	required = true

$ env -i HOME=$FIX/home PATH="$FIX/home/bin:/usr/bin:/bin" ... \
    gv-sandbox ... -- git -C $FIX/clones_root/repo3 checkout -f

GV782-GLOBAL-CONFIG-FILTER-INVOKED argv=[smudge]
Your branch is up to date with 'origin/master'.
exit=0
$ cat repo3/payload.glob
smudged-by-global-config
```

**Observed.** Not merely "reached and failed" this time — the operator's
global-config-selected executable ran **to completion**, and git wrote its
output into the working tree. This is the clearest possible demonstration
that a config-named executable selector, sourced from operator config
preserved by design (`spawn.rs:182-188`'s "deliberate product behaviour"),
executes fully during git-vista's real checkout sandbox.

## 5. Verdicts

| Selector | Verdict | Evidence |
|---|---|---|
| `filter.lfs.process`, sourced from **system** config (`/etc/gitconfig`) | **REACHABLE — directly measured, exact selector name** | §4.4 (stub, corrected full argv), §4.5 (real binary, reduced-exclude argv) |
| `filter.lfs.smudge`, sourced from **system** config | **NOT directly measured under the literal `lfs` name** — system config sets `filter.lfs.process`, and git prefers the process protocol over `smudge`/`clean` whenever `process` is configured, so §4.4/§4.5 never isolated a standalone smudge invocation for this name. §4.7 measures `filter.<name>.smudge` directly (to completion, output written to the working tree) under a different filter name with no `process` key. Reasoned, not measured, that `lfs` would behave identically — nothing in the allowlist or ro-tree code is name-specific (§5 discussion below) | §4.7 (direct, different name), reasoning only for the `lfs` name itself |
| `filter.lfs.clean` | **OUT OF SCOPE for checkout, not measured here** — `clean` runs on `git add`/staging, not on `git checkout -f`; this document's experiments only exercise the checkout phase (§4.2's transfer step used plain git, not this project's clean path) | Not measured; scope, not a finding |
| `filter.<name>.smudge`/`.process`, sourced from **operator global** config (`~/.gitconfig`) | **REACHABLE — directly measured**, under a generic filter name (`gvtest`), run to completion (checkout succeeded, filter output written to disk) | §4.7 |
| `filter.lfs.*` specifically, sourced from operator **global** config | **REACHABLE by generalization, not separately re-run for the literal `lfs` name** — same reasoning gap as the smudge row above | §4.7 (different name), plus source: nothing in `UNTRUSTED_CHECKOUT_ENV_ALLOWLIST` or the ro/exclude trees distinguishes filter names |
| `filter.lfs.*`, sourced from tracked **`.lfsconfig`** | **BLOCKED, structurally** (not exercised by sandbox policy at all — closed by git-lfs's own restricted safe-key parsing before it ever becomes a `filter.*` git-config value) | §2 (not independently re-derived from git-lfs source in this pass — see gap list) |
| `filter.lfs.*`, sourced from a **remote's own `.git/config`** | **BLOCKED, structurally** (clone never copies it) | §2, general git-clone behavior, not separately measured here |
| `lfs.customtransfer.<name>.path`/`.args` | **NOT INDEPENDENTLY MEASURED** — reasoned only | See §6 |
| `lfs.extension.<name>.clean`/`.smudge` | **NOT INDEPENDENTLY MEASURED** — reasoned only | See §6 |

**The genuinely open, measured finding is narrower than "LFS is unconstrained":**
it is that **`UNTRUSTED_CHECKOUT_ENV_ALLOWLIST` (`spawn.rs:215-223`) does not
gate which Git *configuration keys* apply, only which environment variable
*names* survive** — and the comment at `spawn.rs:196-206` shows this was a
deliberate, reasoned trade (avoiding a credential-carrying config channel),
not an oversight. The **operator-global-config** exposure (§4.7) is the
direct, intended consequence of that same trade, stated as intentional in
`spawn.rs:182-188` and ADR 0128. The **system-config** exposure (§4.4) is the
*same mechanism*, but nothing in the source comments discusses the
apt-installed system default specifically — it appears to be an unconsidered
edge of an otherwise-deliberate decision, not a separately-argued-for one.

## 6. Honest gap list

- **`gv-sandbox-reaper` was never built** — queued behind other Round-4
  lanes' builds on this box's single build slot for the whole measurement
  window. `wrap_with_reaper` (`spawn.rs:592-611`) only prepends the reaper
  binary and the parent pid ahead of the same argv; it does not touch
  filesystem/network/env policy, so its absence should not change this
  question's answer, but it was not verified experimentally.
- **`lfs.customtransfer.<name>.path` and `lfs.extension.<name>.{clean,smudge}`
  were not independently measured.** Both are read by git-lfs's *own*
  internal config lookups after `filter.lfs.process` has already been
  invoked and handed control — which §4.4/§4.5/§4.7 establish is reachable.
  Reasoning that they inherit the same reachability (same `HOME`/`/etc`
  visibility, same environment) is stated here as reasoning, not as a
  separate measurement. Confirming it directly would need either the real
  `git-lfs` binary progressing far enough to reach its extension/custom-transfer
  code (blocked here by the same unrelated `git rev-parse --git-dir` failure
  seen in §4.5) or a stub sophisticated enough to complete the filter
  pkt-line handshake and impersonate a custom-transfer negotiation, which
  this lane did not build.
- **`.lfsconfig`'s safe-key restriction was not re-derived from git-lfs's own
  source** in this pass — the "cannot directly supply these executable
  selectors" claim (§2) is carried from the issue text and general knowledge
  of git-lfs, not independently confirmed against `git-lfs`'s source in this
  measurement. If that claim is wrong, the `.lfsconfig` row in §5 is wrong
  too, and `.lfsconfig` deserves its own experiment before being relied on.
- **`git-vista-server`'s HTTP layer itself was never exercised.** The server
  hardcodes `127.0.0.1:8080` (`state.rs:31-32, 37-60`) with no configurable
  alternative port, so this measurement could not safely start it (see §4).
  Everything measured here is the real `gv-sandbox` binary under a
  hand-reconstructed argv, cross-checked against source and against an
  existing crate test's use of the identical call shape
  (`checkout_security.rs:82-134`) — not the compiled `execute_clone`
  function itself running end to end. A reviewer who wants that exact
  function exercised would need to add a test inside `crates/**`, which this
  lane's boundary forbids.
- **The §4.5 nested `git rev-parse --git-dir` failure inside git-lfs's own
  worker process was not diagnosed.** It might be an ordinary git-lfs
  quirk unrelated to sandboxing, or it might itself be a Landlock boundary
  effect (a denied `stat`/`open` on some ancestor directory during the
  upward directory walk, which git conflates with "hit a different
  filesystem"). Left unresolved because it doesn't change §4.4's answer,
  which used a stub specifically to avoid depending on this behavior.
- **Only one host was measured** (this box, kernel/Landlock ABI reported ≥ 6,
  bwrap 0.11.1 present but unused for Network tier). No cross-host
  variation was checked.

## 7. Codex review (verbatim)

Run against the diff adding this document, per the lane's mandate:

```
$ cd /home/tom/projects/gv-782 && timeout 900 codex exec --model gpt-5.6-sol \
    --config model_reasoning_effort=high --sandbox read-only \
    "Read the investigation document added by this diff (git diff origin/main
     in the current repo). Which of its claims are backed by a command whose
     output is shown, and which are asserted? Open every file:line it cites
     and say whether the cited line actually supports the claim. Does it
     confuse the checkout policy with the transport policy? Name anything it
     presents as measured that was actually reasoned. Be terse; disagreement
     is more useful than agreement."
```

> Bottom line: the narrow finding is credible—`filter.lfs.process` executed inside the real `gv-sandbox`. The document overstates that as an exact production-path measurement.
>
> ### Command-backed
>
> - Installed versions and current system/global LFS config.
> - The two Rust mentions of `GIT_CONFIG_NOSYSTEM` within `crates/**/*.rs`.
> - Successful build of `git-vista-server` and `gv-sandbox`.
> - Stub `git-lfs filter-process` invocation:
>   - unsandboxed;
>   - through directly invoked `gv-sandbox`.
> - Real `git-lfs` started smudging and reached adapter initialization.
> - Adding `GIT_CONFIG_NOSYSTEM=1` made that fixture check out as a raw pointer.
> - A generic global-configured `smudge` executable ran and changed worktree content.
>
> ### Asserted or inferred
>
> - "Confirmed unchanged at the end"; only one copy of the config output is shown.
> - apt/postinst provenance, no system/global writes, no live-repo contact, and "git-vista tracks no LFS files."
> - Fixture construction and isolation from repo/global filter config.
> - `.lfsconfig`'s safe-key restriction and the entire selector-source table.
> - Clone not transferring source `.git/config`—true-looking, but unmeasured here.
> - Reaper build queue/history.
> - Kernel ABI and bwrap version/presence.
> - That the real-LFS nested failure was "unrelated"; it was explicitly undiagnosed.
> - The direct shim invocation's applicability to the production HTTP path.
>
> ### Presented too strongly as measured
>
> - **"Exact policy" / "identical call shape": false.** The shown command omits production `compose_network_args` settings:
>   `core.askpass=`, `credential.helper=`, `core.sshCommand=ssh`, `core.fsmonitor=false`, and `protocol.ext.allow=never`.
> - It also omits four current checkout exclusions: `.kube`, `.config/google-chrome`, `.config/chromium`, and `.mozilla`.
> - Only `filter.lfs.process` was directly observed. `filter.lfs.smudge` was reasoned from a differently named generic smudge test. `filter.lfs.clean` was not measured and is not a checkout-time filter.
> - "Operator-global `filter.lfs.*` reachable" is a generalization from `filter.gvtest.smudge`, which the verdict table admits.
> - "Genuine object fetch" overstates §4.5: adapter initialization began, but failed before successful transfer.
> - Reaper irrelevance is reasoned, not experimentally verified.
>
> ### Citation audit
>
> | Citation | Verdict |
> |---|---|
> | `network_exec.rs:1742,1753`; `spawn.rs:464-477` | Yes: NOSYSTEM is in a Tokio test and applied through a `#[cfg(test)]` method. |
> | grep transcript | Partial: searches `crates/`, not "project-wide." |
> | `sandbox/mod.rs:1283,1286,1374,1380` | Yes: checkout derives from clone and remains Network tier. |
> | `network_exec.rs:359,367-368` | Yes: checkout separately chains transport hardening and environment allowlisting. |
> | `spawn.rs:465` | **No:** wrong line; `command_async` is at 504. |
> | `spawn.rs:141-151` | Yes: geometry scrub list. |
> | `spawn.rs:112-121`; `960-1007` | Mostly: supports ordinary-spawn intent/test source, but the test does not cover `GIT_CONFIG_NOSYSTEM`. |
> | `spawn.rs:391-397`; `215-223` | Yes: `env_clear()` plus seven-name allowlist. |
> | `spawn.rs:196-206` | **Partial:** explains GLOBAL/SYSTEM and `GIT_CONFIG_COUNT`; it does not mention NOSYSTEM, contrary to the document. |
> | `sandbox/mod.rs:428` | Yes: `/etc` is read-granted. |
> | `spawn.rs:182-188` | Yes, as a source comment: global filters are deliberate behavior. |
> | `sandbox/mod.rs:1522` | Yes: bwrap is added only for Strict. |
> | `state.rs:31-32,37-60` | Yes: loopback port is fixed at 8080. |
> | `gv-sandbox/main.rs:1047-1076` | Yes: calls Landlock, seccomp, then execs Git. |
> | `spawn.rs:592-611` | Partial: prepends reaper and PID; actual unchanged `execvp` behavior is elsewhere. |
> | `handlers/clone.rs:226-231` | Partial: constructs the production command; `.output()` is at 234. |
> | `checkout_security.rs:113-134` | Partial: constructs the test command; `.output()` is at 137. |
> | `checkout_security.rs:82` | **No:** unrelated formatting line; the named test starts at 23. |
> | `handlers/clone.rs:148-150` | Yes: `clone --no-checkout` arguments. |
> | `sandbox/mod.rs:1520-1586` | Yes about argv construction, but it exposes that the displayed reconstruction is incomplete. |
>
> It does not fundamentally confuse checkout policy with transport policy in §3: they are correctly shown as separate layers. It does conflate them later when calling the hand-built invocation "the exact policy" constructed by `policy_for_clone[_checkout]`; the transport environment is applied by the command wrapper, while other wrapper-added arguments were omitted.

### Corrections made in response, before this document was finalized

Every citation and claim the review flagged as wrong or overstated was
checked against source again and fixed, not merely noted:

- Added the missing `FORCED_NETWORK_ARGS` (`-c` pins) and the 4 missing
  `DEFAULT_SECRET_EXCLUDES` entries to §4.4's reconstruction, then **re-ran
  the sandboxed experiment** with the corrected argv (§4.4) — same verdict,
  now on a faithful reconstruction rather than an approximate one.
- Fixed `spawn.rs:465` → `spawn.rs:504` (`command_async`).
- Fixed `checkout_security.rs:82` → `checkout_security.rs:23` (the named
  test's actual start line; the earlier number came from counting a `sed`
  excerpt's relative line numbers as if they were absolute).
- Added the precise `.output()` line numbers the review asked for
  (`clone.rs:234`, `checkout_security.rs:137`).
- Split the verdict table's `filter.lfs.smudge`/`.clean` rows out from
  `.process`: `.process` is the only one directly measured under the literal
  `lfs` name; `.clean` doesn't run at checkout at all (scope correction);
  `.smudge`/global-config-for-`lfs`-specifically are marked as reasoned by
  generalization from §4.7's differently-named filter, not separately
  measured — the review was right that the original table blurred this.
- Softened "genuine object fetch" (§4.5) to reflect that the transfer
  attempt began but did not complete.
- Corrected the `spawn.rs:196-206` claim: that comment names
  `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`/the `GIT_CONFIG_COUNT` family as
  deliberately excluded, but does **not** mention `GIT_CONFIG_NOSYSTEM` —
  the earlier draft implied it did. `GIT_CONFIG_NOSYSTEM`'s absence from the
  allowlist is simply unexplained in that comment, which if anything
  strengthens the "unconsidered edge" framing in §5 rather than weakening it.
- Fixed "project-wide search" → "search of `crates/**/*.rs`" (§0).

Not fixed, because the review's own hedges show it isn't a clean miss: the
`.lfsconfig` safe-key claim and the "clone doesn't copy `.git/config`" claim
remain reasoned, not re-derived from git-lfs's own source — already flagged
as a gap in §6 before this review ran, and still one after it.

---

**Signed:** max · 2026-09-10T01:15:00-04:00
