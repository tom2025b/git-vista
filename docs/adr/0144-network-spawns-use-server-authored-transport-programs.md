# ADR 0144 — Network spawns use server-authored transport programs

- **Status:** Accepted — extended by #779 to constrain native Git and custom helpers
- **Date:** 2026-09-08
- **Issue:** #755, #779; LFS checkout/filter follow-up #782
- **Extends:** [ADR 0036](0036-network-tier-exec-harness-askpass-and-redaction.md)
- **Related:** [ADR 0122](0122-the-token-is-a-credential-not-a-header.md), [ADR 0128](0128-a-credential-exists-only-before-untrusted-checkout.md), [ADR 0137](0137-an-untrusted-checkout-inherits-an-allowlist.md)

## #779 amendment — fixed transport policy (2026-09-09)

The original decision below left two transport selectors open. The shared
Network launcher now seals `GIT_ALLOW_PROTOCOL=http:https:ssh:git:file` and
`GIT_PROXY_COMMAND=` into every command, including credentialed transfer and
credentialless checkout. These restrictions are applied immediately before
both `output()` and `spawn()`, after all environment construction, so inherited
values or checkout's `env_clear` cannot remove them. The sealed API accepts no
caller-supplied protocol names or proxy commands.

Git [documents the protocol allowlist](https://git-scm.com/docs/git#Documentation/git.txt-GITALLOWPROTOCOL)
as overriding existing protocol configuration. Unlike `-c protocol.allow=never`,
it beats a repository's specific `protocol.<name>.allow=always`, including
selection through `remote.<name>.vcs`, custom URLs, push URLs and URL rewrites.
Git also [documents the proxy environment override](https://git-scm.com/docs/git-config#Documentation/git-config.txt-coregitProxy).
An empty `GIT_PROXY_COMMAND` bypasses all `core.gitProxy` entries: the marker
experiment verifies that Git attempts a direct connection instead of executing
the first matching configured proxy. This works where a later
`-c core.gitProxy=none` does not, so disabling native Git is unnecessary.

Supported transports are HTTP, HTTPS, SSH (including scp-style syntax), direct
native `git://`, and local paths / `file://`. HTTP(S) still uses Git's standard
installed helpers; executable lookup through the operator's `PATH` /
`GIT_EXEC_PATH` remains trusted, as do existing SSH environment overrides.
This is not binary provenance verification or general hook containment.

Compatibility costs are explicit:

- Proxy commands configured by `core.gitProxy` or inherited through
  `GIT_PROXY_COMMAND` no longer run. Native Git must connect directly; an
  operator requiring such a proxy must use another supported transport.
- Custom remote helpers such as `git-remote-hg`, and `ext`, are denied even
  when installed and enabled in global or repository config.
- The fixed protocol policy replaces an inherited `GIT_ALLOW_PROTOCOL`,
  including a more restrictive parent value; protocol policy is server-owned.
- Checkout receives the same fixed transport policy in addition to its
  inherited-environment allowlist. Hooks and filters still run. Git LFS's own
  custom-transfer and extension selectors are not governed by these settings;
  [#782](https://github.com/tom2025b/git-vista/issues/782) owns runtime measurement
  and compatibility decisions for that separate scope.

Behavioral regressions in `network_exec::https_suite` install real marker
programs with ordinary hooks disabled. Their positive controls demonstrate a
proxy surviving the naive reset and an installed helper surviving the blanket
protocol default. Hardened runs require an absent marker and either direct
connection failure (proxy) or Git's protocol denial (custom helper), through
ordinary output, streamed spawn, credentialed and tokenless wrappers, and the
checkout wrapper. Proxy tests cover config-only and hostile inherited values;
helper cases include a non-origin named remote's `vcs`, URL, `insteadOf`,
`pushurl`, and `pushInsteadOf`. Existing real native-Git and SSH transfer suites
verify that supported transports still work.

Verification on Git 2.53.0: all 135 affected Network, spawn, SSH, clone,
fetch/push/pull, and sandbox-contract tests pass; server all-target Clippy and
workspace formatting checks pass. Two compiled-code mutations were caught by
both marker tests: removing the transport policy, and replacing it with the
naive `protocol.allow=never` / `core.gitProxy=none` config pins. In each mutation
both forbidden programs actually ran, causing the marker-absence assertions to
fail. Production source was restored after each experiment.

The remainder of this ADR records PR #775's original decision and evidence;
its proxy and installed-helper residuals are superseded by this amendment.
LFS is tracked separately; #755's closure still requires its broader reassessment.

Signed: **codex** · 2026-09-09

## Context

The Network sandbox grants the served repository read-write and gives its Git
process the transport capabilities that Strict withholds. The repository grant
includes `.git/config`: `policy_for` pushes the worktree and, when distinct,
its commondir into `rw_trees`. The secret exclusions are paths under the
operator's home plus the server trust store; they do not exclude repository
configuration.

ADR 0036 already forced `-c core.askpass=` at the Network command chokepoint.
That precedent matters because command-line config outranks `.git/config`.
It did not establish that askpass was the only program Git could select from
repository config.

## Measurement: markers, not a config-source inference

On Git 2.53.0, a throwaway worktree and bare remote were created under a temp
directory. Every candidate was configured to name an executable marker that
appended its name and argv to a file, and every Git invocation also used
`-c core.hooksPath=<empty-directory>`. The result therefore says which marker
processes actually ran; it is not a list copied from `git-config(1)`.

The measured transport shapes were fetch and push against a path remote,
fetch/push against an SSH URL, fetch against a `git://` URL, and fetch against
a loopback HTTP server returning `401`. Fetch was also run with a custom
`ext::` URL and an arbitrary remote-helper protocol. Candidate keys whose
normal consumers are diff, merge, signing, archive or interactive commands
were installed one at a time and tested against the same fetch shape.

### Reached

| Selector | Measured route | Observation |
|---|---|---|
| `remote.origin.uploadpack` | path fetch | marker ran with the bare remote path |
| `remote.origin.receivepack` | path push | marker ran with the bare remote path |
| `core.sshCommand` | SSH fetch and push | marker ran first for SSH option detection and then for `git-upload-pack` / `git-receive-pack` |
| `core.gitProxy` | `git://` fetch | marker ran with host and port |
| `credential.helper` | HTTP `401` fetch | marker ran with `get` |
| `core.askpass` | HTTP `401` fetch | marker ran with the username prompt |
| `core.fsmonitor` | path fetch | marker ran with protocol v2, then v1; this was not in the issue's starting list |
| `remote.origin.vcs` | path fetch | Git ran `git-remote-<value>`; this was not in the issue's starting list |
| `protocol.ext.allow=always` plus an `ext::` remote URL | fetch | the command embedded in the URL ran |
| `protocol.<custom>.allow=always` plus a custom URL protocol | fetch | the installed `git-remote-<custom>` helper ran |

The URL rows are included because a repository-local `remote.<name>.url` and
protocol policy jointly select a process even though the executable name is
not stored in a key called `program` or `command`.

### Expected candidates not reached on the measured transport shapes

Markers configured as `core.pager`, `core.editor`, `sequence.editor`,
`gpg.program`, `gpg.ssh.defaultKeyCommand`, `diff.external`,
`diff.<driver>.textconv`, `merge.<driver>.driver`,
`filter.<driver>.{clean,smudge,process}`, `difftool.<tool>.cmd`,
`mergetool.<tool>.cmd`, `interactive.diffFilter`, `tar.<format>.command`,
`browser.<tool>.cmd`, `man.<tool>.cmd`, and `gc.recentObjectsHook` did not run
during the fetch. `core.pager` also did not run for the Network-tier
`for-each-ref` observation shape.

These negatives are route-scoped, not claims that the settings are inert.
Merge, filter, diff, signing and archive commands have consumers elsewhere.
Pull's integration half deliberately runs as Local/Strict, and clone's
untrusted checkout is governed by ADR 0137's separate no-credential,
no-agent-socket policy. This ADR concerns the transport-capable spawn that
holds the operator's ambient transport authority.

## Decision

Choose option 4: pin each directly arbitrary command selector for which Git
has a safe higher-precedence value, and retain two operator-owned executable
channels whose safety depends on provenance rather than an empty spelling.

The Network harness now composes:

| Repository selector | Server-authored result |
|---|---|
| `core.askpass` | empty; no repository askpass |
| `credential.helper` and URL-scoped helper chains | empty reset; when Git-Vista supplies a token, exactly its own non-empty helper follows the reset |
| `core.sshCommand` | `ssh` |
| `core.fsmonitor` | `false` |
| `protocol.ext.allow` | `never` |
| `remote.<name>.uploadpack` | `--upload-pack=git-upload-pack` after fetch, pull, ls-remote or clone |
| `remote.<name>.receivepack` | `--receive-pack=git-receive-pack` after push |

The explicit transport options are used instead of a config key because the
remote name is typed request data. A fixed `remote.origin.uploadpack` pin would
leave every other valid `RemoteName` exposed. The option applies regardless of
the selected name and has higher precedence than its config default.

### What remains outside the pins

`GIT_SSH_COMMAND`, `GIT_SSH`, `GIT_PROXY_COMMAND`, `GIT_ASKPASS` and
`SSH_ASKPASS` remain inherited. A repository cannot mutate its server parent's
environment. These values therefore express the operator who launched the
server, like `PATH`, rather than the repository inside the grant. They can
override the corresponding config pins; that is deliberate operator authority,
not an assertion that the pins beat environment variables.

`core.gitProxy` remains a repository-controlled arbitrary-command selector.
It is a first-match multi-value key: the experiment was repeated with the
repository marker configured first and `-c core.gitProxy=none` on the command
line, and the marker still ran. The apparent pin was removed rather than
shipping a value that could not enforce its claim. Disabling
`protocol.git.allow` would close the execution path by disabling all native
`git://` remotes; changing the child's environment to override
`GIT_PROXY_COMMAND` would require reopening the sealed spawn API outside this
lane. Neither compatibility decision is smuggled into this fix. This is the
repository-controlled residual I am least comfortable with.

`remote.<name>.vcs` and custom URL protocols remain able to select an installed
`git-remote-<vcs>` helper. Git has no command-line “unset” value for
`remote.<name>.vcs`: an empty value tries to execute `git-remote-`. Setting
`protocol.allow=never` is not a closure because a repository's more specific
`protocol.<name>.allow=always` still wins; this was measured. Unlike `ext::`,
these selectors cannot name an arbitrary path or shell fragment. They select a
helper the operator installed into Git's executable environment. A future
server-side transport allowlist may narrow that installed-helper authority,
but a blanket pin that breaks every custom transport while remaining
bypassable would be worse than recording the residual honestly.

## Rejected alternatives

### 1. Pin every value empty

Rejected because several empty values do not mean “use Git's default.” An
empty `core.sshCommand` or `remote.<name>.vcs` makes the transport fail, and an
empty remote pack command is not a usable default. It would maximize breakage
without producing a coherent transport.

### 2. Preserve every operator value by reading and replaying it

Chosen only where the server can author a safe semantic value (`ssh`, `false`,
standard pack programs). Globally replaying operator config loses on
provenance: Git combines system, global, conditional include, repository and
command scopes. Asking Git for the “effective” value reintroduces the
repository value; hand-parsing a subset invents a second Git config engine.
Credential helper values may themselves be shell fragments. This option needs
an explicit server settings surface, not an implicit read from the same merged
configuration under attack.

### 3. Allowlist operator-supplied values

Rejected for this change. It is the likely shape if Git-Vista later exposes
custom SSH commands, proxies or remote helpers, but no such server-owned
settings surface or executable grammar exists today. Adding a validator here
would require deciding whether arguments, shell metacharacters, absolute paths,
wrapper scripts and per-host variation are permitted. That is a product and
security design of its own, not a small extension of ADR 0036's fixed argv.

### 4. Pin some, leave others

Chosen with the per-selector reasons above. The dividing line is not
popularity. Direct paths and shell commands from `.git/config` are replaced by
safe server values. Parent-environment commands and installed Git remote
helpers retain operator provenance, and the installed-helper residual is
stated rather than hidden behind an ineffective default-protocol pin.

## Compatibility cost

This change breaks the following configurations for Network-tier spawns:

- HTTPS authentication supplied only by an operator/global or repository
  `credential.helper`; tokenless fetch, push and clone now fail authentication.
  A credentialed clone using Git-Vista's own token helper continues to work.
- `core.sshCommand` wrappers used to select a key, `ProxyJump`, a host alias or
  another SSH binary. Ordinary `~/.ssh/config`, including `IdentityFile` and
  `ProxyJump`, remains effective through `ssh`; the inherited SSH command
  environment variables also remain an explicit operator override.
- hook-path `core.fsmonitor` acceleration during Network spawns.
- custom `remote.<name>.uploadpack` / `receivepack` commands, including remote
  namespace wrappers and nonstandard server-side program names.
- `ext::` remotes enabled through repository config.

The cost is deliberate. Each broken setting was also a repository-writable
arbitrary-program selector in a process with transport authority. Preserving
it requires moving the value to a server-owned, validated configuration source.

## Proof

`planner::fetch_suite::a_repository_named_upload_pack_never_executes_on_the_production_fetch_path`
first runs plain Git with hooks disabled and proves both a repository-named
upload-pack and the experimentally discovered fsmonitor program really create
markers. It then runs the named-remote operation through the full planner
pipeline and asserts the upload-pack marker is absent while the fetch succeeds.
Finally it drives `git_streamed_for -> sandboxed -> network_command` directly,
clearing evidence from the planner's Local/Strict preflight commands first, and
asserts the fsmonitor marker is absent while that Network fetch succeeds.

The credential-helper HTTP test likewise proves the unforced repository helper
runs, then proves it does not run through `network_command`. The real SSH suite
proves that server-authored `ssh`, `git-upload-pack` and `git-receive-pack`
still complete ls-remote, fetch and push.

The two required mutation arms use the same production-path test run key:

1. remove the upload-pack option entirely;
2. replace it with a plausible but incomplete fixed
   `remote.origin.uploadpack=git-upload-pack` pin, allowing the test's
   `remote.named.uploadpack` value to win.

Both mutations must be reported as `caught`; their run identifiers and the
final gate count are recorded in the lane report rather than predicted here.
