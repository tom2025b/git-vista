# #782 — Measuring the repository-supplied `.lfsconfig` vector

Measured at `9e54e6fe0896e6af34bdcb3b17a124f006c87e4d` on 2026-09-10.
This is a measurement report only. It changes no production code and uses only
inert controls; it provides no extraction recipe.

## Verdict

**OBSERVED:** git-lfs 3.7.1 applies its safe-key restriction to a tracked
`.lfsconfig` during both a plain `git checkout -f` and the complete Git-Vista
checkout process stack (`gv-sandbox-reaper` -> `gv-sandbox` -> `git`). The
restriction is implemented by git-lfs, so the sandbox neither supplies nor
removes it.

**OBSERVED:** a repository-supplied `.lfsconfig` did not supply any of these
executable-selection values:

- `lfs.customtransfer.<name>.path`
- `lfs.customtransfer.<name>.args`
- `lfs.customtransfer.<name>.concurrent`
- `lfs.customtransfer.<name>.direction`
- `lfs.standalonetransferagent`
- `lfs.extension.<name>.clean`
- `lfs.extension.<name>.smudge`
- `filter.lfs.process`

git-lfs named each tested value as unsafe and ignored it. Inert local-config
controls reached the same custom-transfer and extension consumers through the
same Git-Vista sandbox, so the negative result was not caused by a fixture that
never reached those consumers.

**OBSERVED, and absent from the installed manual's purportedly exhaustive
list:** `lfs.extension.<name>.priority` is accepted from `.lfsconfig`. It is
integer metadata, not a command. It cannot name an executable, but it can
register an extension with its executable fields empty. A prepared pointer
requiring that extension then made checkout fail. This is a repository-driven
availability effect, not executable reachability; this lane leaves it live and
unchanged.

**REASONED from the observed matrix and the exact v3.7.1 parser:** the effective
allowed `lfs.*` shapes are:

| Accepted shape | Kind |
|---|---|
| `lfs.allowincompletepush` | exact key |
| `lfs.fetchexclude` | exact key |
| `lfs.fetchinclude` | exact key |
| `lfs.gitprotocol` | exact key |
| `lfs.locksverify` | exact key |
| `lfs.pushurl` | exact key |
| `lfs.skipdownloaderrors` | exact key |
| `lfs.url` | exact key |
| `lfs.<one-or-more-middle-components>.access` | suffix shape |
| `lfs.extension.<name>.priority` | special case; non-negative integer metadata |

`remote.<name>.lfsurl` is also accepted, but is not an `lfs.*` key. The source
also accepts some odd multi-component `remote.*` shapes; they are outside this
lane's question and were not characterized here.

## 1. Inputs and unchanged operator configuration

```
$ git --version
git version 2.53.0
$ git lfs version
git-lfs/3.7.1 (GitHub; linux amd64; go 1.25.0)
$ git config --system --show-origin --get-regexp '^(filter\.lfs|lfs\.)'
file:/etc/gitconfig filter.lfs.clean git-lfs clean -- %f
file:/etc/gitconfig filter.lfs.smudge git-lfs smudge -- %f
file:/etc/gitconfig filter.lfs.process git-lfs filter-process
file:/etc/gitconfig filter.lfs.required true
$ git config --global --show-origin --get-regexp '^(filter\.lfs|lfs\.)'
(empty, exit 1)
```

**OBSERVED:** the two config queries printed the same result again after all
experiments. No system or global config command was run with a mutating option.

## 2. Deriving the accepted set from git-lfs itself

The installed command's own help says `.lfsconfig` is limited to eight exact
`lfs.*` keys, the URL-scoped `access` shape, and
`remote.<name>.lfsurl`:

```
$ git lfs help config
...
Lfsconfig
---------
...
* lfs.allowincompletepush
* lfs.fetchexclude
* lfs.fetchinclude
* lfs.gitprotocol
* lfs.locksverify
* lfs.pushurl
* lfs.skipdownloaderrors
* lfs.url
* lfs.\{*}.access
* remote.\{name}.lfsurl

The set of keys allowed in this file is restricted for security reasons.
```

The help text was treated as a hypothesis. This matrix put all documented
`lfs.*` keys next to safe-looking and executable-bearing counterexamples in one
real `.lfsconfig`:

```
$ git -C $FIX/matrix config -f .lfsconfig --list
lfs.allowincompletepush=true
lfs.fetchexclude=excluded/**
lfs.fetchinclude=included/**
lfs.gitprotocol=v2
lfs.locksverify=true
lfs.pushurl=https://push.invalid/repo/info/lfs
lfs.skipdownloaderrors=true
lfs.url=https://fetch.invalid/repo/info/lfs
lfs.concurrenttransfers=99
lfs.standalonetransferagent=probe
lfs.https://fetch.invalid/.access=basic
remote.origin.lfsurl=https://remote.invalid/repo/info/lfs
lfs.extension.probe.priority=7
lfs.extension.probe.clean=/bin/false
lfs.extension.probe.smudge=/bin/false
lfs.customtransfer.probe.path=/bin/false
lfs.customtransfer.probe.args=sentinel-args
lfs.customtransfer.probe.concurrent=false
lfs.customtransfer.probe.direction=download

$ env -i HOME=$FIX/home PATH=/usr/bin:/bin git -C $FIX/matrix lfs env 2>&1 |
    rg "unsafe|^  lfs\\.|^Endpoint|^ConcurrentTransfers|^SkipDownloadErrors|^Access|^Fetch|^Extension"
warning: These unsafe '.lfsconfig' keys were ignored:
  lfs.concurrenttransfers
  lfs.standalonetransferagent
  lfs.extension.probe.clean
  lfs.extension.probe.smudge
  lfs.customtransfer.probe.path
  lfs.customtransfer.probe.args
  lfs.customtransfer.probe.concurrent
  lfs.customtransfer.probe.direction
Endpoint=https://fetch.invalid/repo/info/lfs (auth=basic)
ConcurrentTransfers=8
SkipDownloadErrors=true
FetchRecentAlways=false
FetchRecentRefsDays=7
FetchRecentCommitsDays=0
FetchRecentRefsIncludeRemotes=true
AccessDownload=basic
AccessUpload=none
FetchExclude=excluded/**
FetchInclude=included/**
Extension[7]=probe
```

**OBSERVED:** `lfs.url`, the URL-scoped `access` key,
`lfs.skipdownloaderrors`, both fetch path keys, and extension priority all
changed values printed by `git lfs env`. The rejected
`lfs.concurrenttransfers=99` did not: the effective value remained 8.

**OBSERVED through git-lfs's rejection diagnostic:** none of the eight exact
documented keys, the `access` shape, `remote.origin.lfsurl`, or extension
priority was classified unsafe; every displayed executable-bearing candidate
was classified unsafe.

**REASONED, after opening the tagged source rather than trusting the manual:**
the eight exact keys are `safeKeys` in git-lfs v3.7.1
[`config/git_fetcher.go:186-195`](https://github.com/git-lfs/git-lfs/blob/v3.7.1/config/git_fetcher.go#L186-L195).
The same parser separately accepts any key with at least three dot components
whose last component is `access`, accepts `remote.<name>.lfsurl`, rejects
extension `clean` and `smudge`, and accepts extension `priority`
([`git_fetcher.go:51-100`](https://github.com/git-lfs/git-lfs/blob/v3.7.1/config/git_fetcher.go#L51-L100)).
That special `priority` branch is why actual 3.7.1 behavior is one key wider
than its installed manual says.

## 3. Checkout fixture

A throwaway repository under `/tmp` contained:

- a tracked `.lfsconfig` with an accepted loopback HTTP `lfs.url`, accepted
  `lfs.skipdownloaderrors=true`, and the five rejected custom-transfer and
  extension command fields shown below;
- `.gitattributes` selecting `filter=lfs` for `payload.bin`;
- a canonical LFS pointer for `payload.bin` whose object was deliberately
  unavailable.

```
$ git -C $FIX/sandboxed-proof show HEAD:.lfsconfig
[lfs]
        url = http://127.0.0.1:80/repo/info/lfs
        skipdownloaderrors = true
        standalonetransferagent = probe
[lfs "customtransfer.probe"]
        path = /bin/false
        args = sentinel-args
[lfs "extension.probe"]
        clean = /bin/false
        smudge = /bin/false
        priority = 0
```

Both checkouts started from `git clone --no-checkout`. The worktree copy of
`.lfsconfig` was therefore absent; git-lfs found the tracked version in the
index/HEAD fallback before checkout materialized it:

```
$ test ! -e $FIX/sandboxed-proof/.lfsconfig &&
    printf 'before_checkout_worktree_lfsconfig=absent\n'
before_checkout_worktree_lfsconfig=absent
```

## 4. Plain Git control

```
$ env -i HOME=$FIX/home PATH=/usr/bin:/bin LANG=C \
    git -C $FIX/plain-proof checkout -f 2>&1; printf 'exit=%s\n' "$?"
warning: These unsafe '.lfsconfig' keys were ignored:

  lfs.standalonetransferagent
  lfs.customtransfer.probe.path
  lfs.customtransfer.probe.args
  lfs.extension.probe.clean
  lfs.extension.probe.smudge
Downloading payload.bin (12 KB)
Error downloading object: payload.bin (df8a9eb): Smudge error: Error downloading
  payload.bin (...): batch response: Post "http://127.0.0.1:80/...":
  dial tcp 127.0.0.1:80: socket: operation not permitted
Your branch is up to date with 'origin/base-only'.
exit=0
$ sed -n '1,10p' $FIX/plain-proof/payload.bin
version https://git-lfs.github.com/spec/v1
oid sha256:df8a9eb3086c8de25294f2acfae489e12e12f644abbdd078dccc1356fbd7b9bd
size 12345
```

**OBSERVED:** the five selector fields were ignored. The two accepted fields
were active: git-lfs attempted the configured loopback endpoint, and
`skipdownloaderrors=true` converted that failure into checkout success with the
pointer retained. The outer test environment denied the loopback connection;
the network error is not evidence about Git-Vista and is not used in the
selector verdict.

## 5. The same checkout through Git-Vista's real process stack

Current source was opened before reconstructing the launcher:

- `handlers/clone.rs:148-154,175-238` supplies `clone --no-checkout`, then
  `checkout -f` through `network_command_without_credential`.
- `sandbox/network_exec.rs:129-168,359-369` supplies the five fixed `-c` pins,
  the network transport environment, and the untrusted-checkout environment.
- `sandbox/spawn.rs:176-253,340-397,500-527,592-608` defines the seven-name
  environment allowlist and prepends the reaper.
- `sandbox/mod.rs:227-247,321-325,428-435,527-531,1296-1330,1387-1413,1533-1598`
  supplies the exact filesystem exclusions, grants, checkout ports, seccomp
  mode, and hook mode.

The two production binaries were built from this tree under the required lock:

```
$ GV_BUILD_SLOTS=4 CARGO_TARGET_DIR=/home/tom/.cargo-targets/gv-782b \
    buildlock cargo build --bin gv-sandbox --bin gv-sandbox-reaper
(exit 0)
$ stat -c '%n %s bytes %y' /home/tom/.cargo-targets/gv-782b/debug/gv-sandbox \
    /home/tom/.cargo-targets/gv-782b/debug/gv-sandbox-reaper
.../gv-sandbox 6570032 bytes 2026-09-10 04:31:39.149982802 -0400
.../gv-sandbox-reaper 4797240 bytes 2026-09-10 04:31:38.452982765 -0400
```

The command below spells every current production flag. `$FIX` abbreviates
`/tmp/gv782-r5c.KSuSwQ`; the actual command used that absolute path in every
position.

```
$ env -i HOME=$FIX/home PATH=/usr/bin:/bin LANG=C \
    GIT_ALLOW_PROTOCOL=http:https:ssh:git:file GIT_PROXY_COMMAND= \
    /home/tom/.cargo-targets/gv-782b/debug/gv-sandbox-reaper "$$" \
    /home/tom/.cargo-targets/gv-782b/debug/gv-sandbox \
    --abi-floor 6 \
    --rw /dev --rw $FIX \
    --ro /usr --ro /bin --ro /lib --ro /lib64 --ro /etc \
    --ro /run/systemd/resolve --ro /run/resolvconf --ro /run/NetworkManager \
    --ro $FIX/home \
    --exclude $FIX/home/.ssh --exclude $FIX/home/.claude \
    --exclude $FIX/home/.claude.json --exclude $FIX/home/.config/gh \
    --exclude $FIX/home/.aws --exclude $FIX/home/.netrc \
    --exclude $FIX/home/.git-credentials --exclude $FIX/home/.npmrc \
    --exclude $FIX/home/.gnupg --exclude $FIX/home/.docker \
    --exclude $FIX/home/.kube \
    --exclude $FIX/home/.config/google-chrome \
    --exclude $FIX/home/.config/chromium --exclude $FIX/home/.mozilla \
    --exclude $FIX/home/.local/state/git-vista/trusted-repos \
    --seccomp-checkout --hooks-run --net-allow \
    --net-port 443 --net-port 80 --net-port 9418 \
    -- git -c core.askpass= -c credential.helper= -c core.sshCommand=ssh \
    -c core.fsmonitor=false -c protocol.ext.allow=never \
    -C $FIX/reaper-proof checkout -f 2>&1; printf 'exit=%s\n' "$?"
warning: These unsafe '.lfsconfig' keys were ignored:

  lfs.standalonetransferagent
  lfs.customtransfer.probe.path
  lfs.customtransfer.probe.args
  lfs.extension.probe.clean
  lfs.extension.probe.smudge
Downloading payload.bin (12 KB)
Error downloading object: payload.bin (df8a9eb): Smudge error: Error downloading
  payload.bin (...): batch response: Post "http://127.0.0.1:80/...":
  dial tcp 127.0.0.1:80: socket: operation not permitted
Your branch is up to date with 'origin/base-only'.
exit=0
$ sed -n '1,10p' $FIX/reaper-proof/payload.bin
version https://git-lfs.github.com/spec/v1
oid sha256:df8a9eb3086c8de25294f2acfae489e12e12f644abbdd078dccc1356fbd7b9bd
size 12345
```

**OBSERVED:** the complete Git-Vista process stack produced the same unsafe-key
list and the same effective safe settings as plain Git. The sandbox did not
weaken the safe-key parser.

**REASONED:** this equivalence is expected because Git-Vista intentionally
preserves `HOME`, runs the installed `git-lfs`, and filters environment names,
not git-lfs configuration keys. The measured result, not that reasoning, is the
verdict.

## 6. Non-vacuous executable-selector controls

### Custom transfer

The same reaper-and-sandbox checkout was repeated in a fresh clone after local
`.git/config` selected a custom transfer named `probe`. Its inert command was
`/bin/echo GV782-CUSTOM-CONTROL`, which cannot speak the transfer protocol.

```
$ git -C $FIX/custom-echo-r config --get-regexp '^lfs\.(standalone|customtransfer)'
lfs.standalonetransferagent probe
lfs.customtransfer.probe.path /bin/echo
lfs.customtransfer.probe.args GV782-CUSTOM-CONTROL
```

Command: section 5's complete command, changing only its final repository
operand to `$FIX/custom-echo-r`. Its output was:

```
warning: These unsafe '.lfsconfig' keys were ignored:
  ... the same five repository-supplied keys ...
Downloading payload.bin (12 KB)
Error downloading object: payload.bin (...): Smudge error: ...:
  invalid character 'G' looking for beginning of value
exit=0
```

**OBSERVED:** the `G` from the local command's output reached git-lfs's JSON
parser under the sandbox. Thus the consumer was live, while the identically
named `.lfsconfig` path and args were ignored. Checkout still exited zero only
because the repository's accepted `lfs.skipdownloaderrors=true` remained in
effect.

### Extension smudge

An LFS pointer with `ext-0-probe` metadata and its 69-byte object preloaded in
the clone made the `.lfsconfig`-only checkout fail:

```
$ # Section 5's complete command, with -C $FIX/extension-negative-r
warning: These unsafe '.lfsconfig' keys were ignored:
  lfs.standalonetransferagent
  lfs.customtransfer.probe.path
  lfs.customtransfer.probe.args
  lfs.extension.probe.clean
  lfs.extension.probe.smudge
fatal: ext2.bin: smudge filter lfs failed
exit=128
```

The same prepared pointer and object, with inert case-conversion commands
supplied by local `.git/config`, completed under the same sandbox:

```
$ git -C $FIX/extension-positive-r config --get-regexp '^lfs\.extension\.probe'
lfs.extension.probe.clean tr a-z A-Z
lfs.extension.probe.smudge tr A-Z a-z
lfs.extension.probe.priority 0
```

Command: section 5's complete command with its final repository operand changed
to `$FIX/extension-positive-r`. Its output, followed by the comparison, was:

```
warning: These unsafe '.lfsconfig' keys were ignored:
  ... the same five repository-supplied keys ...
Your branch is up to date with 'origin/master'.
exit=0
$ cmp -s $FIX/src/.gitattributes $FIX/extension-positive-r/ext2.bin
extension_output_matches_input=0
```

**OBSERVED:** local extension commands were usable under the checkout sandbox;
the repository-supplied `clean` and `smudge` values were not. The accepted
repository-supplied priority metadata alone registered `probe`, but without a
usable command it could only make the prepared extension pointer fail.

## 7. What remains reasoned or unmeasured

- No Git-Vista server was started. Its live session and fixed port were left
  untouched as required. The measurement starts at the exact binaries and argv
  that the server constructs, including the reaper; it does not exercise axum,
  request routing, or clone cleanup.
- The eight exact safe keys were all placed in the matrix and none was reported
  unsafe. Direct visible effects were separately observed for URL, URL access,
  skip-on-error, fetch include/exclude, and extension priority. The downstream
  effects of `allowincompletepush`, `gitprotocol`, `locksverify`, and `pushurl`
  were not independently exercised; their acceptance is corroborated by the
  exact v3.7.1 parser, not promoted to a separate runtime effect measurement.
- The loopback request failed with `operation not permitted` in both plain and
  Git-Vista runs because the surrounding test environment denied it. No claim
  about Git-Vista's TCP-port enforcement is drawn from that message.
- No pinning test was written, so there are no mutation arms. The lane forbids
  changes under `crates/**`; this document is evidence, not an executable test.
- git-lfs v3.7.1's accepted extension-priority behavior is wider than its own
  manual and can produce checkout/add failures when command fields are absent.
  That behavior was deliberately left alone: changing git-lfs or production
  policy is outside this measurement lane.

**Signed:** codex · 2026-09-10
