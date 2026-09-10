# #782 acceptance audit — LFS checkout/filter selectors

Audited on 2026-09-10 against `6f8daad7c401c4cf007c1f2ab5d631f00ca2b603`
with Git 2.53.0 and git-lfs 3.7.1 installed. The issue body and reopening
comment were read directly before this audit. Every source and test citation
below was then opened in this worktree; neither PR #835's body nor an ADR claim
was used as evidence for current behavior.

## Decision

**Satisfied: close #782.** All five acceptance items are met for the production
surface this issue owns. The supported fresh-clone shape is deliberately narrow:
credentialless HTTPS LFS on TCP port 443, using an executable regular file from
`/usr/bin/git-lfs`, `/bin/git-lfs`, or `/usr/local/bin/git-lfs`. System/global
executable configuration, arbitrary git-lfs locations, custom transfer agents,
LFS extensions, nonstandard HTTPS ports, and carrying the clone token into LFS
checkout are not supported by this boundary.

This does not claim TLS enforcement. The sandbox grants a port, not a URL
scheme; the remaining scheme gap is separately owned by #836.

## Acceptance ledger

### 1. Measure custom transfer and extensions by phase

Met. The retained tests use the installed git-lfs and distinct executable
markers, and I ran them from the current tree:

- [`lfs_extensions_execute_on_clean_and_checkout_but_not_clone_transfer`](../../crates/git-vista-server/src/sandbox/lfs.rs#L451-L568)
  observes the extension clean command during `git add`, neither extension
  command during `git clone --no-checkout`, and the smudge command during
  checkout of the extension-bearing pointer.
- [`lfs_custom_transfer_executes_on_checkout_but_not_clone_transfer_or_clean_add`](../../crates/git-vista-server/src/sandbox/lfs.rs#L570-L654)
  observes no custom-transfer marker during clean/add or no-checkout clone, then
  observes the selected standalone agent when checkout needs an absent object.

The marker paths can only be written by the configured programs. Those fixtures
contain no hook, so a hook override is not needed to identify their consumer;
the separate production checkout proof plants a real post-checkout hook and
requires it not to run ([`checkout_security.rs`](../../crates/git-vista-server/src/sandbox/checkout_security.rs#L15-L145)).

### 2. Decide operator support and compatibility before selecting a policy

Met. The decision is policy separation, not a claim that every Git operation
has stopped honoring configuration:

- Ordinary existing-repository commands still go through `policy_for` and the
  general launcher ([`git_cmd.rs`](../../crates/git-vista-server/src/git_cmd.rs#L268-L280)).
  That launcher removes repository-geometry variables, but does not clear the
  environment ([`spawn.rs`](../../crates/git-vista-server/src/sandbox/spawn.rs#L559-L571));
  the ordinary policy grants the repository and read-only home
  ([`mod.rs`](../../crates/git-vista-server/src/sandbox/mod.rs#L1023-L1093)).
  Operator/repository filters and extensions therefore retain their established
  sandboxed behavior for ordinary filter-clean/add and checkout consumers.
- Fresh-clone materialisation is different. Its launcher always sets
  `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null` immediately before
  execution ([`spawn.rs`](../../crates/git-vista-server/src/sandbox/spawn.rs#L377-L398)).
  Clone transfer also forces an empty `init.templateDir` and removes inherited
  `GIT_TEMPLATE_DIR` ([`network_exec.rs`](../../crates/git-vista-server/src/sandbox/network_exec.rs#L134-L161),
  [`spawn.rs`](../../crates/git-vista-server/src/sandbox/spawn.rs#L360-L374)), so
  an operator template cannot manufacture repository-local executable state for
  the next phase.
- The one restored filter is server-authored: the executable comes from a fixed
  absolute-path list, process/smudge/required are pinned, custom transfers are
  made ineligible, and the generic and exact-endpoint standalone selectors are
  reset ([`lfs.rs`](../../crates/git-vista-server/src/sandbox/lfs.rs#L12-L107)).

The persistence precondition is therefore explicit. On a pre-existing
repository, an executable selector can come from already-present local/operator
configuration or from code that ran earlier and persisted such configuration.
A remote source repository's `.git/config` is not copied by clone, and the fresh
transfer cannot import an operator template. The installed git-lfs help was also
checked directly: tracked `.lfsconfig` accepts a restricted key subset and does
not accept `filter.*`, custom-transfer path/args, standalone-agent, or extension
clean/smudge executable fields. The production pins additionally override the
safe `.lfsconfig` keys that can redirect the endpoint or turn a missing object
into successful pointer text.

The compatibility costs follow from that boundary: fresh-clone checkout gives
up operator-defined filters/extensions/adapters and config-only git-lfs
installations; default-port credentialless HTTPS LFS is retained. Ordinary
non-LFS clones still succeed without git-lfs, while an LFS clone with no reviewed
driver fails loudly ([`lfs.rs`](../../crates/git-vista-server/src/sandbox/lfs.rs#L231-L377)).

### 3. Preserve HTTPS LFS and the no-credential/no-agent boundary

Met for the supported shape. Production clone transfer exits before the HEAD
probe and materialising checkout; the handler constructs the latter through
`lfs_checkout_command` and the sealed `UntrustedCheckoutCommand`
([`clone.rs`](../../crates/git-vista-server/src/handlers/clone.rs#L148-L232)).
The LFS builder derives `/info/lfs` from the validated clone URL after removing
userinfo, query, and fragment, and pins the result as data
([`lfs.rs`](../../crates/git-vista-server/src/sandbox/lfs.rs#L38-L103)). The
current positive executes the real reviewed git-lfs and materialises the cached
object bytes through the production checkout builder
([`lfs.rs`](../../crates/git-vista-server/src/sandbox/lfs.rs#L241-L295)).

The checkout policy has only TCP 443, no agent-socket grant, no known-hosts
carveout, and blocked hooks ([`mod.rs`](../../crates/git-vista-server/src/sandbox/mod.rs#L1367-L1396)).
The launcher builds the child environment from its seven-name allowlist and
then applies the fixed config policy ([`spawn.rs`](../../crates/git-vista-server/src/sandbox/spawn.rs#L217-L255),
[`spawn.rs`](../../crates/git-vista-server/src/sandbox/spawn.rs#L430-L436)). The
executing environment control observes all three credential names, an ambient
canary, and `SSH_AUTH_SOCK` absent while `PATH` and `HOME` remain present
([`clone.rs`](../../crates/git-vista-server/src/handlers/clone.rs#L1201-L1352)).
The independent socket control recovers an agent pathname from readable home
state and receives `EPERM` from the checkout seccomp profile
([`checkout_security.rs`](../../crates/git-vista-server/src/sandbox/checkout_security.rs#L71-L145)).
Finally, checkout output is redacted inside the sealed return type before the
handler can receive an LFS action URL ([`network_exec.rs`](../../crates/git-vista-server/src/sandbox/network_exec.rs#L350-L368)).

### 4. Prove the restriction through production launchers and mutations

Met. `execute_clone` can obtain its materialising command only from
`lfs_checkout_command`; that builder fixes `checkout -f` and delegates to the
credentialless production launcher ([`network_exec.rs`](../../crates/git-vista-server/src/sandbox/network_exec.rs#L384-L426)).
Executing controls are not absence-only: the phase tests above execute each
selector at its real consumer, the operator-config test first runs all three
system/home/XDG filters through the compiled checkout stack before the hardened
leg suppresses them ([`network_exec.rs`](../../crates/git-vista-server/src/sandbox/network_exec.rs#L1434-L1608)),
and the production LFS test materialises exact non-pointer bytes.

I queried failure-atlas directly and checked its recorded diffs against the
current source. The following compiled-code arms had green baselines and red
mutants:

- records 546 and 547 removed the checkout config completion call, then removed
  only `GIT_CONFIG_GLOBAL=/dev/null`; both were caught;
- records 571 and 572 removed the checkout environment construction, then routed
  checkout around its AF_UNIX seccomp profile; both were caught by different
  executing observations;
- records 576 and 577 removed `skipdownloaderrors=false`, then removed the
  include/exclude pins; each made its corresponding tracked `.lfsconfig` case
  report false success and was caught;
- records 578 and 579 independently removed query stripping and then all sealed
  checkout redaction; each leaked the planted LFS action query and was caught.

The first pair alone meets the requested two-arm proof for the executable-config
restriction; the later pairs prove the credential/agent-adjacent and
fake-success repairs that are also part of the accepted boundary.

### 5. Route any pure Git transport result to ADR 0144 and #755

Met with no amendment required. Both direct measurements found the selectors
dormant during `git clone --no-checkout`; the reached consumers were checkout
and clean/add. No custom-transfer or extension executable ran in a pure Git
transport phase, so there is no measured transport result to add to ADR 0144 or
the #755 closeout record.

## Verification performed

- Built `gv-sandbox` and `gv-sandbox-reaper`, then ran the complete
  `sandbox::lfs::tests::` group: **11 passed**.
- Ran the exact current tests for operator config suppression, the checkout
  environment allowlist, AF_UNIX/agent denial plus hook suppression, and sealed
  LFS-query redaction: **1 passed in each command**.
- Checked Git/git-lfs versions, the active system/global LFS config, current
  launcher wiring, policy construction, test bodies, and the failure-atlas
  mutation records themselves.

The first LFS-group attempt used a fresh target directory before building the
two sibling sandbox executables: five non-launcher tests passed and six launcher
tests failed at policy construction with `NotFound`. That setup-only run is not
counted. After the required binaries were built, the unchanged group passed 11
of 11. One initially mistyped exact module path selected zero tests; it was also
not counted, and the corrected fully-qualified test then passed one of one.

## Not checked

- No live remote LFS object was downloaded in this audit. The current positive
  uses a real git-lfs process and production launcher with a cached object; URL
  derivation and the TCP-443 grant were checked separately in source/tests.
- I did not rerun the historical mutations, the full mutation matrix, the full
  workspace suite, or the ignored public-network clone suite. I inspected the
  direct mutation records and reran the current unmutated acceptance tests.
- This is evidence for Git 2.53.0 and git-lfs 3.7.1 on this host, not a claim
  about every Git LFS version or installation layout.
- #836's TLS/scheme enforcement is explicitly outside this decision.

Signed: **codex** · 2026-09-10
