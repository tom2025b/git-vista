# ADR 0148 — Clone checkout uses one server-authored LFS filter

- **Status:** Accepted — implemented; mutation proof recorded below
- **Date:** 2026-09-10; amended 2026-09-11
- **Issues:** #831 and #836; retains phase measurements needed by #782
- **Supersedes in part:** [ADR 0137](0137-an-untrusted-checkout-inherits-an-allowlist.md) and [ADR 0146](0146-clone-checkout-does-not-read-operator-git-config.md)
- **Extends:** [ADR 0144](0144-network-spawns-use-server-authored-transport-programs.md)

## Context

ADR 0146 correctly removed system and global Git configuration from fresh-clone
checkout. On this host, however, git-lfs 3.7.1 installs all four
`filter.lfs.*` values in `/etc/gitconfig` and none globally. With both config
scopes hidden, Git 2.53.0 checks out an LFS pointer with status 0 and writes the
pointer text verbatim. The hidden scope also contains
`filter.lfs.required=true`, so the missing driver produces no error.

Restoring `/etc/gitconfig`, copying its effective values, or trusting a
`filter.lfs.*` prefix would reopen the same arbitrary executable surface #827
closed. Git LFS adds two further executable selectors:

- `lfs.customtransfer.<name>.path` starts a transfer-agent process; its `args`
  value is shell-expanded.
- `lfs.extension.<name>.clean` and `.smudge` run programs while streams enter
  and leave LFS storage.

The Git LFS configuration reference documents both surfaces and the basic-only
transfer switch: [git-lfs-config](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-config.adoc).

## Measured phase census

The retained server-bin tests use git-lfs 3.7.1 and real marker programs. They
establish this matrix rather than inferring it from command names:

| Selector | `git clone --no-checkout` transfer | checkout | clean/add |
|---|---|---|---|
| `lfs.customtransfer.<name>.path` selected by `lfs.standalonetransferagent` | not run | **runs** when an object is absent | not run; clean stores the object locally |
| `lfs.extension.<name>.smudge` / `.clean` | neither runs | **smudge runs** for a pointer carrying that extension | **clean runs** and records the extension in the pointer |

`lfs_custom_transfer_executes_on_checkout_but_not_clone_transfer_or_clean_add`
and `lfs_extensions_execute_on_clean_and_checkout_but_not_clone_transfer`
carry the evidence. This deliberately does not claim #782 is otherwise
complete; it preserves the remaining transfer and clean/add facts that issue
needs.

One attempted control changed the design. `lfs.basictransfersonly=true` prevents
server-selected custom adapters, but git-lfs 3.7.1 still ran an explicitly
configured `lfs.standalonetransferagent`. The production config therefore
resets both the generic and exact-endpoint standalone selectors; the compiled
test also plants both forms in repository-local config.

## Decision

### Supply, do not recover, the LFS filter

Only `network_exec::lfs_checkout_command` can build clone materialisation. Its
Git subcommand is fixed to `checkout -f`; callers cannot reuse the LFS config
for add, clean, push, or an arbitrary argument list. It prepends:

```text
-c filter.lfs.process=<absolute reviewed git-lfs> filter-process
-c filter.lfs.smudge=<absolute reviewed git-lfs> smudge
-c filter.lfs.required=true
-c lfs.skipdownloaderrors=false
-c lfs.fetchinclude=
-c lfs.fetchexclude=
```

The fallback smudge command deliberately omits Git's `%f` placeholder. Git LFS
does not require the pathname to read the pointer from stdin, so no
remote-controlled filename needs to enter a shell-interpreted command string.

The executable is resolved only from `/usr/bin/git-lfs`, `/bin/git-lfs`, and
`/usr/local/bin/git-lfs`, in that order, and must be an executable regular
file. `PATH` never selects this driver. If no candidate exists, the required
driver names `/dev/null/git-vista-lfs-unavailable`, a path that cannot become a
file beneath the character device. A non-LFS checkout never invokes it and
succeeds; an LFS checkout fails, identifies the unavailable driver in stderr,
and leaves no apparently materialised pointer file. The arbitrary-program
builder used by that fault-injection test is compiled only under `cfg(test)`;
production exposes no caller-selected LFS executable constructor.

Both process and smudge are pinned because Git may use either filter protocol.
The command-line values outrank repository config. The three additional pins
close Git LFS 3.7.1's tracked-`.lfsconfig` fake-success paths: error skipping or
a matching include/exclude rule can otherwise emit pointer text and report
success without triggering Git's required-filter failure. System and global
config remain disabled exactly as in ADR 0146.

### Permit only the built-in HTTP adapter, with layered TLS enforcement

The same command pins `lfs.url` to the standard `/info/lfs` endpoint derived
from the already-validated clone URL after removing query, fragment, and URL
userinfo. A pasted URL credential therefore does not cross into checkout argv.
The command also supplies generic plus exact-URL controls:

```text
-c lfs.basictransfersonly=true
-c lfs.<endpoint>.basictransfersonly=true
-c lfs.standalonetransferagent=
-c lfs.<endpoint>.standalonetransferagent=
```

This prevents a fetched `.lfsconfig` or repository-local value from switching
endpoint discovery to SSH or selecting a standalone/custom transfer program.
The checkout policy still grants only TCP port 443. Landlock's network rule has
no scheme or address field, so it is not the TLS mechanism by itself. The clone
handler now rejects an original `http://` URL before `git clone` runs; `https://`
and the existing native `git://` transfer remain accepted. Consequently an HTTP
clone URL cannot evade the application boundary by spelling port 443, and an
HTTPS clone supplies an HTTPS-derived LFS batch endpoint.

The batch response is a separate source of URLs. It can advertise a direct
object-action href, which is not a redirect and never reaches Git LFS's
HTTPS-to-HTTP redirect guard. The checkout command therefore also pins:

```text
-c lfs.transfer.enablehrefrewrite=true
-c url.https://127.0.0.1:1/git-vista-refused-plaintext-lfs-action/.insteadOf=http://
```

Git LFS applies this rewrite before its built-in HTTP adapter constructs the
object request. Every direct `http://` action is mapped to the fixed HTTPS
refusal sink on port 1; port 1 remains outside `CLONE_CHECKOUT_PORTS`, so the
unchanged Landlock boundary rejects it. The advertised plaintext host is never
contacted, even if it names port 443 or also offers HTTPS. HTTPS action hrefs
remain unchanged and can connect only on the existing port-443 grant. HTTP on
port 80, native Git, and SSH remain available to the transfer phase but have no
checkout grant. LFS endpoints on nonstandard ports remain unsupported.

### Block hooks and preserve the no-secret boundary

`policy_for_clone_checkout` now selects `HookMode::Blocked` with
`core.hooksPath=/dev/null`. `/dev/null` is immutable host state, not an empty
directory inside the checkout's read-write tree. Git appends a hook name and
observes no executable path. This is a clone-phase least-authority decision,
not ADR 0029's rejected fallback for an unavailable Strict tier; every ordinary
production policy continues to run hooks or refuse the operation.

The LFS process remains inside `UntrustedCheckoutCommand`:

- the environment is built from the seven-name allowlist, so the Git-Vista
  token, ambient token sources, `SSH_AUTH_SOCK`, and unenumerated secrets are
  absent;
- the checkout policy has no agent-socket grant or `known_hosts` carve-out;
- the checkout seccomp profile returns `EPERM` for `AF_UNIX` socket and
  socketpair creation;
- captured output has URL userinfo and complete URL queries removed before the
  clone handler receives it. This covers time-limited credentials in LFS
  object-action URLs before stderr reaches either the response or release log.

The credentialed `clone --no-checkout` process exits before this command is
built. This change does not give private-LFS checkout the transfer token; it
restores credentialless LFS behavior on the policy's permitted TCP port.

## Executable-selection audit

Every value that could name a process during the new path is accounted for:

| Source | Potential executable | Disposition |
|---|---|---|
| System/global `filter.<name>.*`, `lfs.customtransfer.*`, `lfs.extension.*`, hooks path | arbitrary operator program | scopes remain unreadable through `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null` |
| Clone init template (`init.templateDir`, `GIT_TEMPLATE_DIR`) | repository-local filter, hook, LFS extension or custom transfer | neutralized during transfer by ADR 0146 before destination config/hooks exist |
| Fetched `.gitattributes` | filter name | only `lfs` resolves, and its process/smudge values are command-line pins; every other absent filter is a no-op |
| Fetched `.lfsconfig` | endpoint or LFS selectors | endpoint, basic-only mode, generic/exact standalone selectors, `skipdownloaderrors=false`, and empty fetch include/exclude selectors are command-line pins; Git LFS does not accept extension commands from `.lfsconfig` |
| Destination `.git/config` created by clone | LFS/filter/hook selector | fresh clone writes transport metadata, not source-local executable config; a same-user process that later writes another generic filter already controls the managed tree and is outside this remote/operator-config threat |
| Operator `PATH` / `GIT_EXEC_PATH` | top-level `git` and programs it ordinarily resolves | unchanged trusted host boundary for the existing shim; the new `git-lfs` driver itself uses an absolute reviewed path |
| Validated clone URL | clone transport and LFS endpoint data | the handler rejects `http://`; accepted text supplies one LFS config value after query/fragment/userinfo removal and cannot alter argv shape or an executable field |
| LFS server response | advertised transfer adapter and direct object-action href | basic-only mode selects the built-in HTTP adapter; custom adapter paths are ineligible; direct `http://` hrefs are rewritten to the fixed denied HTTPS sink before the adapter requests them |
| Git hooks | hook executable | `core.hooksPath=/dev/null` from the sealed checkout policy |

A same-user process that races the server and replaces `/usr/bin/git-lfs` or
writes directly into the destination already controls the host account or
managed tree; this sandbox does not claim to defend the operator from itself.

## Alternatives considered

- **Restore system config or replay `filter.lfs.*`: rejected.** Both trust
  operator-authored executable strings and reopen arbitrary named filters.
- **Run `git lfs pull` after checkout: rejected.** Checkout would first report
  success with pointer files, recreating the silent interval, and the pull has
  the same custom-transfer/config audit plus a second materialisation path.
- **Prefetch with the clone token: rejected for this change.** It would keep a
  credential alive while Git LFS reads fetched repository metadata and would
  require a separate credential mediation design. The existing no-credential
  boundary stays intact.
- **Keep hooks or checkout ports 80/9418 for compatibility: rejected.** #827
  leaves no fresh-clone hook source, and the explicit consumer needs only TCP
  443. TLS is enforced by the URL gates above in addition to, not inferred
  from, that port grant.
- **Fail every clone when git-lfs is absent: rejected.** Required-filter
  failure is conditional on an LFS-attributed path, so ordinary repositories
  remain usable without the optional executable.

## Verification

The server-bin tests cover real materialisation, explicit refusal, phase
measurements, selector pinning, hook suppression, and the no-agent/no-secret
boundary. Failure-atlas cloned committed HEAD and ran each compiled test through
the buildlocked driver in `sandbox/test_failure_atlas_831.py`:

| Record | Mutation | Verdict and distinct failure |
|---|---|---|
| `mutation_history` **571** | Remove `with_untrusted_checkout_env()` from the credentialless launcher | **caught** — baseline 1 passed; the mutant filter observed both `/tmp/gv831-atlas-agent.sock` and the ambient canary instead of `unset` |
| `mutation_history` **572** | Route `CheckoutPolicy` through ordinary `full_argv`, omitting the checkout seccomp selection | **caught** — baseline 1 passed; the mutant filter's real pathname-AF_UNIX result changed from `errno:1` to `connected` |
| `mutation_history` **576** | Remove `lfs.skipdownloaderrors=false` | **caught** — baseline 2 passed; the tracked `skipdownloaderrors=true` fixture then returned successful pointer text instead of failing checkout |
| `mutation_history` **577** | Remove both empty fetch include/exclude pins | **caught** — baseline 2 passed; the tracked fetch-filter fixture then returned successful pointer text instead of failing checkout |
| `mutation_history` **578** | Remove query stripping while retaining URL-userinfo stripping | **caught** — baseline 1 passed; the sealed checkout output returned `token=checkout-secret` verbatim |
| `mutation_history` **579** | Bypass `redact_output` in `UntrustedCheckoutCommand::output` | **caught** — baseline 1 passed; the raw LFS action URL crossed the sealed output boundary |
| `mutation_history` **580** | Restore the stale M12 exact test name | **caught** — baseline 2 passed; the matrix contract found that the row no longer named a live checkout-security test |
| `mutation_history` **581** | Accept a Cargo summary reporting zero passed tests | **caught** — baseline 2 passed; the anti-vacuity contract rejected the weakened exact-row criterion |
| `mutation_history` **582** | Restore an HTTPS-enforcement claim in `SECURITY_MODEL.md` | **caught** — baseline 2 passed; the documentation boundary contract rejected the port/scheme contradiction |
| `mutation_history` **583** | Restore the same HTTPS-enforcement claim in this ADR | **caught** — baseline 2 passed; the contract rejected the false claim beside the two plaintext paths |

Each repaired invariant has two distinct caught mutations. Records 574 and 575
were inconclusive baseline timeouts caused by nesting the driver's buildlock on
the lock already held by failure-atlas; they are not counted. The driver now
keeps every Cargo command buildlocked on its own one-slot inner file while the
atlas parent retains the host-wide outer exclusion.

Signed: **codex** · 2026-09-10
