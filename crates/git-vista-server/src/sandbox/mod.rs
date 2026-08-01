//! M1.13b (#66): the single argv chokepoint for every git process this server
//! spawns. See `docs/superpowers/specs/2026-07-28-m1.13-round4-verdict.md`.
//!
//! Everything in this file is **pure**. No syscall, no I/O, no async. That is
//! deliberate and it is what dissolved the "do the ~39 synchronous test helpers
//! have to become async?" question (verdict §7.1): because the sandbox is
//! nothing but argv, the sync and async spawn wrappers share one policy
//! function and neither needs `block_on` or a `pre_exec` closure.

// Task 1 lands the pure chokepoint before Task 5 wires the spawn wrappers to
// it. `cargo clippy --workspace --all-targets -- -D warnings` builds the bin
// target *without* `cfg(test)`, where these items have no caller yet.
// REMOVE THIS ALLOW IN TASK 5, once `sandbox::spawn` calls `sandbox_argv`.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use git_vista_protocol::GitOperation;

/// The one impure corner of `sandbox`: it stats the filesystem to find the
/// strict tier's launcher. Task 5's policy builders call
/// `bwrap::bwrap_path()` to fill `Policy::bwrap`.
pub(crate) mod bwrap;
/// Task 9: the factual capability probe — what tiers can this host provide.
pub(crate) mod capabilities;
/// Task 16: INV-15's disclosure seam — the one crossing from the internal
/// [`Tier`] to the wire `HookPolicy`, plus ADR 0029's refusal for a host that
/// cannot supply the tier a repository needs. See its module doc for why the
/// plan's `CapabilityAbsent => Blocked` mapping is not implemented.
pub(crate) mod hook_policy;
/// Task 9, part 2: the boot probe — launches the composed launcher against a
/// throwaway hostile-hook repo and classifies the result into a
/// [`probe::ProbeVerdict`], gating server startup (INV-13 / Global
/// Constraint 15). See its own module doc for the full account.
pub(crate) mod probe;
/// D2 (#66, Task 7): validated repository-metadata resolution — resolves a
/// repository's actual git directory(ies) and refuses when that resolution
/// lands outside the server's managed root. See its module doc for how this
/// composes with (rather than duplicates) `worktree`'s containment rule.
pub(crate) mod repo_paths;
/// The other impure corner: locating the `gv-sandbox` shim. Kept out of this
/// file for the same reason as `bwrap` — `sandbox_argv` stays a total function
/// of its `Policy`.
pub(crate) mod shim;
/// Task 5: the two spawn wrappers. The single chokepoint where the pure argv
/// becomes a real git process. Task 6 migrates the server's spawn sites here.
pub(crate) mod spawn;
/// Task 7: the persisted per-repo trust flag — the only route to `Unsandboxed`.
pub(crate) mod trust;
/// The third impure corner: resolving a linked worktree's real git directory
/// so `policy_for_repo` can grant it — with the containment rule that keeps a
/// repository-writable `.git` pointer file from becoming an arbitrary-grant
/// escalation. Kept out of this file for the same reason as `bwrap`/`shim`.
pub(crate) mod worktree;

#[cfg(test)]
mod argv;
/// A real HTTPS clone through the production `policy_for_clone`. Separate from
/// `documented_gaps` on purpose: that module records what is *not* proven,
/// this one proves the most basic thing the clone path's missing coverage left
/// unasked — that a clone through that policy succeeds at all.
#[cfg(test)]
mod clone_live;
/// #66 / #200 (plan Task 14): the **compatibility** battery — the mirror of the
/// escape battery. Every case is a real `git commit` run twice over one
/// fixture, once at `Tier::Unsandboxed` and once at `Tier::Strict`, so a pass
/// is attributable to the policy rather than to git working regardless. Its
/// census is `docs/sandbox/compat-census.txt`, kept separate from the escape
/// census — see its own module doc for why.
#[cfg(test)]
mod compat;
#[cfg(test)]
mod deps;
#[cfg(test)]
mod dispatch;
/// #66 / #199 (plan Task 13): INV-17, "documented non-coverage is tested as
/// non-coverage". Its tests assert that attacks **succeed** — see its own
/// module doc. Outside the `EscapeCase` harness on purpose: an inverted claim
/// cannot be scored by `run_case`'s contained/escaped verdict. Also carries a
/// second, clearly-delineated section (below the confused-deputy doc test)
/// for ordinary missing-coverage gaps that are not INV-17 shaped — see that
/// section's own header comment for why it lives here anyway.
#[cfg(test)]
mod documented_gaps;
/// #66 Task 25, step 3: the anti-vacuity contract's tripwires and the
/// EscapeCase/run_case harness step 5 rewrites the battery onto. Landed here
/// (rather than left for whichever lane does step 5) so no later lane touches
/// this module list — see
/// `docs/sandbox/escape-battery-anti-vacuity-contract.md`.
#[cfg(test)]
mod escape_contract;
#[cfg(test)]
mod escape_suite;
/// #66 Task 25, step 5: the `class = functional` blocked-hooks case moves
/// here out of `escape_suite.rs`. Landed as an empty stub in step 3 so the
/// module list is fixed before any case is rewritten; step 5 populates it.
#[cfg(test)]
mod hook_mode_suite;
/// D2 (#66, Task 7): the hostile-geometry battery for `repo_paths`. Distinct
/// from `escape_suite` — see this module's own doc comment for the boundary.
#[cfg(test)]
mod hostile;
/// #66 / #198 (plan Task 12): the process-lifecycle battery — INV-8 orphan
/// reaping, A8/C3 fresh procfs, A9/C4 private `/dev/shm`. Outside
/// `escape_contract.rs`'s `EscapeCase` harness on purpose (its claims are
/// process-tree- and wall-clock-shaped, not single-errno-shaped); see its own
/// module doc for what replaces the R5 census gate there.
#[cfg(test)]
pub(crate) mod lifecycle;
#[cfg(test)]
mod shim_cli;
/// #188: the one acceptance box that needs a real SSH server — a throwaway
/// local `sshd` + `ssh-agent` + bare repository, driving a real `git
/// ls-remote` over `ssh://` through the composed Network-tier launcher. See
/// this module's own doc comment for why it builds its own `Policy` rather
/// than routing through `policy_for`'s real-`$HOME`-reading path.
#[cfg(test)]
mod ssh_remote;

/// Absolute paths the strict tier's outer launcher is looked for at, in order.
///
/// # Why this is not the bare name `bwrap`
///
/// It used to be, and that was a hole. Every other program path in a launcher
/// argv is absolute — the shim, the repository, the grants — but a bare `bwrap`
/// is resolved by `execvp` against the **inherited `PATH`** at spawn time.
/// bwrap is the strict tier's entire namespace boundary: it is what creates the
/// pid/net/ipc/uts/cgroup namespaces and mounts the fresh procfs (C3) and the
/// private `/dev/shm` (C4). Anything able to influence this process's `PATH`
/// — a systemd unit edit, an inherited environment, a `.env` loader — could
/// substitute a different binary for it, and since Landlock and seccomp are
/// applied by the *shim* that bwrap then execs, a substitute that simply execs
/// its arguments would leave the strict tier looking identical from the outside
/// while running with no namespaces at all. The failure is silent by
/// construction: the argv is unchanged, the exit code is unchanged, and only an
/// escape-battery probe would notice.
///
/// So the launcher is resolved once, from a fixed list of absolute paths, and
/// `PATH` is never consulted. A host that keeps bwrap somewhere else is a host
/// where the strict tier is unavailable — which is a *reported*, degradable
/// condition (INV-13) rather than a silently weaker sandbox.
pub(crate) const BWRAP_CANDIDATES: &[&str] =
    &["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"];

/// Overrides the shim path for tests and for a packaged install where the
/// shim does not sit beside the server binary.
///
/// Task 5's `shim_path()` must consult this **first**, and its fallback must
/// walk *out* of a `deps/` parent when one is present: under `cargo test` the
/// test binary lives at `target/<profile>/deps/<name>-<hash>`, so a naive
/// `current_exe().parent().join("gv-sandbox")` resolves to
/// `target/<profile>/deps/gv-sandbox`, which does not exist. Every test that
/// reaches the shim through production policy construction rather than through
/// `shim_cli` depends on that detail.
pub(crate) const SHIM_BIN_ENV: &str = "GIT_VISTA_SANDBOX_BIN";

/// C5: the declared minimum Landlock ABI. Six, because ABI 6 is the first
/// with the signal and abstract-unix-socket scopes this design uses; ABI 8
/// adds only `TSYNC`, which a single-threaded launcher does not need.
pub(crate) const LANDLOCK_ABI_FLOOR: u32 = 6;

/// D5 Option B: `$HOME` is read-allowed and write-denied, and *these* are the
/// paths withheld outright. Relative to `$HOME`. The list is short and
/// auditable on purpose — round 4's blanket-deny-plus-allowlist shape needed
/// three iterations to make one `git commit` work (F1, F-NEW-2) and would have
/// kept growing for every credential helper and hook interpreter.
///
/// # How this list is enforced — read before touching `landlock.rs`
///
/// **Not** by a counter-rule. Landlock has no such thing, and the two obvious
/// ways to write one both fail. Measured on this host (kernel Landlock ABI 8,
/// first-party C probe, both rule orders):
///
/// 1. A `path_beneath` rule with `allowed_access = 0` — the mechanism the
///    original plan specified — is **rejected by the kernel**:
///    `landlock_add_rule(…, allowed_access=0) -> rc=-1, errno=42 (ENOMSG)`.
///    The identical call with `allowed_access = READ_FILE` returns 0. Since a
///    non-zero rc becomes `Err` and `restrict()` uses `?`, that mechanism would
///    have aborted the shim on *every* launch on any host where `~/.ssh`
///    exists — no sandboxed git process would ever have run.
/// 2. A nested *lower-privilege* rule does **not** revoke rights an ancestor
///    rule granted. With `$HOME` granted `EXECUTE|READ_FILE|READ_DIR` and
///    `$HOME/.ssh` granted only `MAKE_BLOCK`, reading `$HOME/.ssh/known_hosts`
///    after `restrict_self` returned **OK**, while the control `/etc/hostname`
///    (no rule) returned `EACCES` — so the ruleset was live and the "deny" was
///    simply inert. Adding the nested rule first changed nothing.
///
/// Landlock is deny-by-default, so denial is expressed by **not granting**.
/// The shim therefore *enumerates* a granted tree's entries and adds one rule
/// per entry, skipping any that appears in the exclude set, recursing one level
/// where an exclude is nested (`.config/gh` means granting `.config`'s children
/// individually, minus `gh`). Measured working: 48 entries granted under
/// `$HOME` minus the exclude set ⇒ `~/.ssh/known_hosts` `EACCES`, `~/.bashrc`
/// and `~/projects` OK.
///
/// The enumeration lives in the **shim**, not in the policy builder, for three
/// reasons: `sandbox_argv` stays pure (Task 1's whole premise), the launcher
/// argv stays short enough to review by eye instead of carrying ~50 `--ro`
/// entries, and INV-16's structural assertion keeps something fixed to compare
/// against. What travels in the argv is the *auditable* list — the secrets —
/// which is exactly the property D5 Option B was chosen for.
pub(crate) const DEFAULT_SECRET_EXCLUDES: &[&str] = &[
    ".ssh",
    ".claude",
    // Exact-name matching means `.claude` does not cover this file. It is a
    // separate entry rather than a prefix rule because the shim matches whole
    // path components; see `secret_excludes_for_home`.
    ".claude.json",
    ".config/gh",
    ".aws",
    ".netrc",
    // The direct analogue of `.netrc` for HTTPS remotes: it holds cleartext
    // credentials and was missing from this list while `.netrc` was present.
    ".git-credentials",
    ".npmrc",
    ".gnupg",
    ".docker",
    ".kube",
    ".config/google-chrome",
    ".config/chromium",
    ".mozilla",
];

/// The TCP ports the network tier may `connect()` to.
///
/// # These constrain ports, never hosts — read before trusting them
///
/// Landlock's `landlock_net_port_attr` has **no address field of any kind**.
/// A rule granting port 443 permits `connect()` to port 443 on *every*
/// destination, which was measured directly: one rule, and connections to two
/// different real hosts both succeeded while the same host's port 80 stayed
/// `EACCES`. Identical over IPv6.
///
/// So this list is not an egress policy and must never be described as one. It
/// buys exactly one thing, and it is worth having: a process that reaches code
/// execution inside the network tier cannot reach a service on an *arbitrary
/// port* of the loopback interface — the local CUPS admin socket, a resolver, a
/// development server, this server's own port. It cannot be used to argue that
/// data is confined to the operator's own remote.
///
/// ADR 0028 records the decision to accept and document that limitation rather
/// than build an egress boundary inside M1.13b.
pub(crate) const DEFAULT_GIT_PORTS: &[u16] = &[
    22,   // ssh://  and scp-style remotes
    443,  // https:// — the only one every remote on this host actually uses
    80,   // http://  — plaintext, but still a real remote scheme
    9418, // git://   — the native protocol
];

/// Absolute paths for `Policy::secret_excludes`, given a home directory.
///
/// `DEFAULT_SECRET_EXCLUDES` is relative to `$HOME` while `Policy` requires
/// absolute paths. Without this helper a policy site can pass the constant
/// verbatim, every entry then fails to match anything the shim sees, and the
/// secret set is silently empty — `~/.ssh` re-exposed with nothing to signal it.
/// Every policy builder must go through here rather than joining by hand.
pub(crate) fn secret_excludes_for_home(home: &std::path::Path) -> Vec<PathBuf> {
    DEFAULT_SECRET_EXCLUDES
        .iter()
        .map(|s| home.join(s))
        .collect()
}

/// #188: the `Policy::ro_carveouts` entry for a home directory — the one
/// named exception to the `.ssh` entry in `DEFAULT_SECRET_EXCLUDES`.
///
/// A single-element `Vec` for the same reason `secret_excludes_for_home`
/// returns one rather than a bare `PathBuf`: it is what `Policy::ro_carveouts`
/// is typed as, and every production caller must go through one function
/// rather than joining `home.join(".ssh/known_hosts")` by hand at each call
/// site — the exact silent-miss shape `secret_excludes_for_home`'s own doc
/// comment already records being bitten by once for the exclude side.
///
/// # Why only `known_hosts`, and why this is sound for an arbitrarily-named
/// private key
///
/// `DEFAULT_SECRET_EXCLUDES` withholds `.ssh` as a **whole directory**, not by
/// enumerating filenames inside it — `~/.ssh` can hold a private key under any
/// name at all (`~/.ssh/my_deploy_key`, no `id_` prefix required), so no fixed
/// allowlist of filenames could withhold every private key an operator might
/// have. This function's return value is granted through `Policy::ro_carveouts`,
/// which bypasses the exclude check for **exactly the paths named here** —
/// never a directory (`Policy::ro_carveouts`'s doc comment, and the shim's own
/// refusal in `add_carveout_rule`) — so the soundness argument does not depend
/// on knowing every private key's name: `.ssh` stays wholly excluded, and this
/// is the one, single, explicitly-reviewed file re-admitted from inside it.
pub(crate) fn ssh_known_hosts_carveout(home: &std::path::Path) -> Vec<PathBuf> {
    vec![home.join(".ssh/known_hosts")]
}

/// #188: the SSH agent socket to grant read-write in the **Network** tier —
/// the one tier `git push`/`fetch`/`clone`/`ls-remote` can reach a remote
/// from, and therefore the only tier an SSH agent has any business being
/// reachable from at all. `None` when `$SSH_AUTH_SOCK` is unset (no agent
/// running — nothing to grant) or outside the Network tier.
///
/// This flows through the ordinary `rw_trees`/`grant_tree` path, not
/// `ro_carveouts`: `/tmp`, where an agent socket almost always lives, is not
/// in `DEFAULT_SECRET_EXCLUDES`, so there is no exclude here to bypass — see
/// `Policy::ro_carveouts` for why that mechanism exists at all and why this
/// grant does not need it.
///
/// # What this grant is, and is not, load-bearing for — measured, not assumed
///
/// `$SSH_AUTH_SOCK` already reaches every sandboxed git process's environment
/// unchanged, in **every** tier, with no code change at all:
/// `spawn::command_async`'s doc comment states plainly that production never
/// touches the environment, so a variable set in the server's own process is
/// inherited verbatim by every child it spawns. What was actually missing was
/// never the environment value — it was whether the *socket path itself* is
/// reachable under the sandbox, which is this function's job.
///
/// Measured directly (a live Landlock ruleset — `HANDLED_FS` declared,
/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL` set,
/// byte-identical to what `apply_landlock` installs — against a real
/// `AF_UNIX` `SOCK_STREAM` listener, `connect()` attempted under
/// `restrict_self`, with a same-run `/etc/hostname` open as a live-ruleset
/// control): `connect()` to a **pathname** `AF_UNIX` socket succeeds
/// identically whether the socket carries no Landlock rule at all, a
/// read-only rule, or a read-write one. This matches `seccomp_filter.rs`'s own
/// note that "Landlock ABI 8 does not mediate **pathname** sockets at all" —
/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` covers only the *abstract* namespace,
/// which `ssh-agent`'s filesystem socket is not.
///
/// So, today, on this kernel, what actually makes the agent socket reachable
/// in the Network tier is the pre-existing **seccomp** exemption
/// (`seccomp_filter::af_unix_rule` is Strict-only, landed already anticipating
/// this issue) plus the automatic env inheritance above — **not** this
/// Landlock grant. This function still adds one, for three reasons that all
/// survive that fact: it costs nothing (the kernel accepts the rule
/// regardless of whether it is presently consulted); it keeps the property
/// this design otherwise holds everywhere else, that what the sandbox permits
/// is auditable from the argv alone (D5 Option B) — an agent socket the
/// process can reach with nothing about it visible in the launcher command
/// line is exactly the silent-widening shape this design avoids elsewhere;
/// and it keeps this working unchanged if a future Landlock ABI starts
/// mediating pathname `AF_UNIX` sockets, rather than depending on kernel
/// behaviour this project does not control. Strict must never receive this
/// regardless of any of the above: it denies `AF_UNIX` at the seccomp layer
/// unconditionally, and a Landlock grant there would only contradict that
/// denial in the one place — the argv — a reviewer is supposed to be able to
/// trust.
fn ssh_agent_socket_grant(tier: Tier) -> Option<PathBuf> {
    if tier != Tier::Network {
        return None;
    }
    std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from)
}

/// System trees granted read+execute in every tier.
pub(crate) const DEFAULT_RO_TREES: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc"];

/// System trees granted read+write in every tier. `/dev` is here because it is
/// in the only configuration the round-4 verdict actually measured git
/// succeeding under, and because a `sh` hook redirecting to `/dev/null` fails
/// without it. C4's private `/dev/shm` is supplied by the bwrap prefix (strict)
/// or withheld by simply not granting it (network).
pub(crate) const DEFAULT_RW_TREES: &[&str] = &["/dev"];

/// Granted read+execute in **every tier except `Strict`** — the resolver state
/// DNS needs, and nothing else.
///
/// # Why a `/run` grant is needed at all
///
/// On a systemd-resolved host `/etc/resolv.conf` is a **symlink into `/run`**
/// (measured on this host: `/etc/resolv.conf ->
/// ../run/systemd/resolve/stub-resolv.conf`). Landlock checks the resolved
/// object, not the link name, so the `/etc` grant in `DEFAULT_RO_TREES` does
/// **not** cover it: glibc's resolver cannot open its own configuration and
/// every hostname lookup fails. Measured, network tier, before this constant
/// existed:
///
/// ```text
/// git ls-remote https://github.com/git/git HEAD
/// fatal: unable to access 'https://github.com/git/git/':
///        Could not resolve host: github.com          (exit 128)
/// ```
///
/// and with `--ro /run/systemd/resolve` added, the same command printed the
/// real `HEAD` oid and exited 0. So this is not a hypothetical hardening gap:
/// without it `push`/`fetch`/`clone`/`ls-remote` against any *named* remote are
/// broken outright. (The push test in this crate passes regardless because it
/// uses a literal `git://127.0.0.1:9418` remote, which needs no resolver — a
/// green test over a broken feature.)
///
/// # Why these paths and not `/run`
///
/// A recursive read grant on all of `/run` would work on every host layout, but
/// it hands the network tier read access to the whole runtime directory
/// (`/run/user/1000`, service state, anything a package drops there) to fix a
/// single configuration file. These are the standard *resolver* runtime
/// directories instead — the three documented targets an `/etc/resolv.conf`
/// symlink has in practice:
///
/// * `/run/systemd/resolve` — systemd-resolved. **Measured working here**, and
///   it also covers the `io.systemd.Resolve` varlink socket `nss-resolve` uses,
///   which lives in the same directory.
/// * `/run/resolvconf`, `/run/NetworkManager` — the other two standard layouts.
///   Not measured (this host is systemd-resolved); included because a missing
///   grant path is *silently skipped* by the shim (`add_path_rule` fails the
///   `O_PATH` open and `grant_tree` returns 0 granted, no rule, no error), so an
///   entry that does not exist costs nothing, while a host whose resolver lives
///   in one of them would otherwise rediscover this exact bug.
///
/// If a future host resolves DNS from some other `/run` subdirectory, add it
/// here — do **not** widen this to `/run`, and do not delete these entries as an
/// unexplained grant: read the symlink first (`ls -l /etc/resolv.conf`).
///
/// # Do not narrow this to the single file either — measured
///
/// The obvious next narrowing is to grant only the symlink's target,
/// `/run/systemd/resolve/stub-resolv.conf`. It does not work, and it fails
/// *silently*. Measured on this host, same shim binary, only the argv differing:
///
/// ```text
/// --ro /run/systemd/resolve/stub-resolv.conf   Could not resolve host: github.com  (exit 128)
/// --ro /run/systemd/resolve                    13c7afec…  HEAD                      (exit 0)
/// --ro /run                                    13c7afec…  HEAD                      (exit 0)
/// ```
///
/// The reason is in the shim, not in the resolver: `grant_tree`'s non-enumerated
/// fast path calls `add_path_rule(tree, access)` with the full directory access
/// mask, and Landlock rejects a `path_beneath` rule that carries a
/// directory-only right (`LANDLOCK_ACCESS_FS_READ_DIR`) for a **regular file**.
/// `add_path_rule` maps that rejection to `false` and `grant_tree` returns "0
/// granted" with no error, so a policy naming a file gets *no rule and no
/// diagnostic*. Confirmed independently of DNS: with `--ro <dir>/f` a sandboxed
/// `git config -f <dir>/f --list` printed `Permission denied` and exited 128 —
/// byte-identical to granting nothing at all — while `--ro <dir>` printed the
/// value and exited 0. (`enumerate` masks `READ_DIR|EXECUTE` off for non-dirs,
/// which is why grants *inside* an enumerated tree do not hit this.)
///
/// So a directory is the narrowest grant this shim can actually express today,
/// and these three are the narrowest *directories*. If the shim's fast path is
/// ever taught to mask file grants the way `enumerate` already does, this
/// constant can shrink to the one file — but not before, and not on the strength
/// of reading the code alone: measure it the way the table above was measured.
///
/// # Never in the strict tier
///
/// The strict tier's whole posture is *no network* (`--net-deny`, plus bwrap's
/// `--unshare-net`), so it has no business reading resolver state: there is
/// nothing it could legitimately resolve. Granting it there would weaken the
/// one tier the escape battery's containment cases are written against, in
/// exchange for nothing.
///
/// The `--exclude` secret set applies to this grant exactly as it does to every
/// other (`grant_tree` checks the excludes first), so nothing here can
/// re-expose a path that `DEFAULT_SECRET_EXCLUDES` withholds.
pub(crate) const NETWORK_ONLY_RO_TREES: &[&str] = &[
    "/run/systemd/resolve",
    "/run/resolvconf",
    "/run/NetworkManager",
];

/// Granted read+execute in the **strict tier only**. Landlock mediates procfs,
/// so without a `/proc` grant the shim cannot open `/proc/self/ns/user`
/// (INV-6 would report `NO_PROCFS` instead of `EACCES`), `open_fds()` would
/// return empty and pass INV-7 for the wrong reason, and `highest_visible_pid()`
/// would return -1 and fail A8/C3 outright — making the `--proc /proc` that C3
/// mandates invisible to the only test that checks it.
///
/// Strict-tier-only is not an oversight: bwrap creates a mount namespace and
/// mounts a **fresh** procfs for the child pid namespace, so what is granted
/// there is the sandbox's own view. The network tier has no mount namespace at
/// all, so granting `/proc` there would grant the **host's** procfs — every
/// other process on the box, visible. The whole-sandbox ADR (M1.13b Task 18)
/// records this distinction — named rather than numbered, because 0026, 0027
/// and 0028 were each claimed by a different decision while this milestone was
/// still in flight, and a hardcoded number went stale twice.
pub(crate) const STRICT_ONLY_RO_TREES: &[&str] = &["/proc"];

/// The reviewed bwrap **arguments**, pinned as a constant so INV-16's
/// structural assertion has something to compare against and a drift shows up
/// as a test failure rather than as a quietly weaker sandbox.
///
/// The launcher's own path is deliberately *not* in here: it is resolved per
/// host into `Policy::bwrap` (see `BWRAP_CANDIDATES`), so this constant stays a
/// fixed, reviewable value that cannot vary with where bwrap is installed.
///
/// `--proc /proc` is C3 (a pid namespace does not update an inherited procfs).
/// `--tmpfs /dev/shm` is C4 (an ipc namespace does not cover pathname-based
/// POSIX shared memory). `--die-with-parent` plus `--unshare-pid` is INV-8.
pub(crate) const STRICT_BWRAP_ARGS: &[&str] = &[
    "--bind",
    "/",
    "/",
    "--dev-bind",
    "/dev",
    "/dev",
    "--proc",
    "/proc",
    "--tmpfs",
    "/dev/shm",
    "--unshare-pid",
    "--unshare-net",
    "--unshare-ipc",
    "--unshare-uts",
    "--unshare-cgroup",
    "--die-with-parent",
    "--new-session",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Landlock + seccomp inside a bwrap pid/net/ipc/uts/cgroup namespace.
    /// Everything except network operations (D4 Option A).
    Strict,
    /// Landlock + seccomp, no namespaces, `AF_INET`/`AF_INET6` permitted.
    /// The only tier in which `git push`/`fetch`/`clone` can work (F3).
    Network,
    /// No sandbox at all. Reachable only through explicit, persisted, per-repo
    /// operator trust, and it flies a permanent banner (INV-15).
    Unsandboxed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookMode {
    /// Repository hooks run. They gate the operation normally (INV-11).
    Run,
    /// `core.hooksPath` is pointed at a server-owned empty directory, so no
    /// repository hook can run at all. The state the probe drops to when the
    /// host cannot provide the declared minimum (INV-13).
    Blocked { empty_dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Policy {
    pub tier: Tier,
    /// Absolute path of the fused `gv-sandbox` shim.
    pub shim: PathBuf,
    /// Absolute path of the strict tier's `bwrap` launcher, resolved once per
    /// host from `BWRAP_CANDIDATES` — never from `PATH`. `None` in the tiers
    /// that do not launch it; a `Strict` policy cannot be built without it,
    /// because a strict tier that cannot find bwrap must degrade loudly
    /// (INV-13) rather than run with no namespaces.
    pub bwrap: Option<PathBuf>,
    pub rw_trees: Vec<PathBuf>,
    pub ro_trees: Vec<PathBuf>,
    /// Absolute paths withheld from the grants above by enumerate-and-skip.
    /// See `DEFAULT_SECRET_EXCLUDES` for why this is not a deny rule.
    pub secret_excludes: Vec<PathBuf>,
    /// #188: named, single-file exceptions to an entry in `secret_excludes` —
    /// read-only, and checked against nothing but the file's own identity.
    ///
    /// This is **not** a general-purpose escape hatch and must stay narrow:
    /// `grant_tree`'s exclude check (`is_or_inside_exclude`) is what makes
    /// `secret_excludes` outrank a `--ro`/`--rw` grant everywhere else in this
    /// design, and every entry here is a deliberate, reviewed exception to
    /// that rule for one literal path — never a directory (the shim refuses
    /// to grant one, `bin/gv-sandbox/main.rs`'s `add_carveout_rule`). As of
    /// this writing the only populated case is `~/.ssh/known_hosts` in the
    /// `Network` tier, via `ssh_known_hosts_carveout` — a git client needs it
    /// to verify a remote's host key, while the rest of `~/.ssh` (private
    /// keys above all) stays withheld by the ordinary exclude.
    pub ro_carveouts: Vec<PathBuf>,
    /// TCP ports the network tier may `connect()` to. Empty in every other
    /// tier — the strict tier has no network at all (F3), and the unsandboxed
    /// tier has no ruleset to put them in.
    ///
    /// This travels in the argv rather than living inside the shim on purpose.
    /// The shim would otherwise hold a hardcoded egress list that no reviewer
    /// reading a launcher command line could see, which is precisely the
    /// property D5 Option B and INV-16 were chosen to preserve: what the
    /// sandbox permits is auditable from the argv alone.
    pub net_ports: Vec<u16>,
    pub hook_mode: HookMode,
}

/// The system trees a policy for `tier` starts from, before the repository's
/// own paths and `$HOME` are added. One function rather than four hand-rolled
/// lists, because the four policy-building sites (`shim_cli::workable`,
/// `policy_for_repo_unvalidated`, `bootstrap_policy`, `probe::verdict`) each
/// omitting `/dev` and `/proc` independently is exactly how the measured
/// working configuration got lost.
///
/// Returns `(rw_trees, ro_trees)`.
pub(crate) fn default_system_trees(tier: Tier) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let rw = DEFAULT_RW_TREES.iter().map(PathBuf::from).collect();
    let mut ro: Vec<PathBuf> = DEFAULT_RO_TREES.iter().map(PathBuf::from).collect();
    if tier == Tier::Strict {
        ro.extend(STRICT_ONLY_RO_TREES.iter().map(PathBuf::from));
    } else {
        // Everything except Strict may resolve hostnames, and on a
        // systemd-resolved host that means reading `/etc/resolv.conf`'s *target*
        // under `/run` — see `NETWORK_ONLY_RO_TREES` for the measurement and for
        // why the strict tier is excluded rather than merely not needing it.
        ro.extend(NETWORK_ONLY_RO_TREES.iter().map(PathBuf::from));
    }
    (rw, ro)
}

/// Whether a git invocation needs to reach the network. This is the axis
/// `policy_for` dispatches on (Task 8 / D3): the tier is a property of *what
/// the operation does*, not of the repository — with the single exception of
/// operator trust, which is a property of the repository and overrides both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetworkNeed {
    /// The subcommand talks to a remote (`push`/`fetch`/`clone`/`ls-remote`).
    /// Only the `Network` tier can serve it (a namespace breaks push, F3).
    Remote,
    /// Everything else — reads, commits, branch and ref manipulation, merges.
    /// These get the fuller-isolation `Strict` tier once it is available.
    Local,
}

/// Git subcommands known to reach the network. **This list is not
/// authoritative and is not the primary classifier** — git's command surface is
/// open-ended (plumbing like `fetch-pack`/`send-pack`, transport helpers like
/// `remote-https`, `remote update`, `submodule --remote`, and partial-clone
/// *lazy* fetches from otherwise-local commands like `checkout` all reach out),
/// so no argv-name list can be complete. The C10 audit made this point: the
/// authoritative signal is the **typed operation** the server chose, threaded
/// explicitly from the call site (see `network_need`'s note), not a string
/// match on argv.
///
/// This set exists for two narrower jobs: it is the fail-closed *fallback* when
/// only an argv is available, and it documents the common cases. **Fail-closed
/// classification:** a subcommand not in this set is treated as `Local` → the
/// stricter `Strict` tier, so a network command missing from the list *breaks
/// loudly* rather than silently gaining network. `remote` (the config verbs
/// `get-url`/`add`/`remove`) is deliberately absent — those touch only
/// `.git/config`, never a socket.
const REMOTE_SUBCOMMANDS: &[&str] = &[
    "push",
    "fetch",
    "clone",
    "ls-remote",
    "pull",
    // plumbing / transport helpers the C10 audit flagged as network-capable
    "fetch-pack",
    "send-pack",
    "http-fetch",
    "http-push",
];

/// Classify a git argv's first non-flag token — a **fail-closed fallback**, not
/// the authoritative dispatch.
///
/// The C10 audit is right that argv-name classification cannot be complete
/// (aliases expand to other subcommands, plumbing and helpers reach the network
/// under many names, and a partial clone lazily fetches from `checkout`/`diff`).
/// That is now how it works (Task 8 / D3): the primary signal is the **typed
/// operation the server chose**, classified by `network_need_for_operation` and
/// threaded as an explicit `NetworkNeed` from each call site, which knows its
/// own intent (the planner's `push` step is a push; the read helpers are
/// reads). This function survives as the **cross-check** on that declaration —
/// see `reconcile_need`, which may only ever tighten. Both directions of its
/// error remain safe: an unrecognised network command falls to `Local`, which
/// `reconcile_need` cannot use to widen anything, and which routes to `Strict`
/// and breaks loudly rather than gaining access.
///
/// Pure and total: every input maps to exactly one `NetworkNeed`.
pub(crate) fn network_need(args: &[&str]) -> NetworkNeed {
    // The subcommand is the first token that is not a `-C <path>` / `-c k=v`
    // global flag. In practice the server always passes the subcommand first,
    // but skipping leading globals makes the classifier robust to `-c` config
    // injection on the argv (which a hostile *repo* cannot do, but defence in
    // depth is free here).
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        match *tok {
            "-C" | "-c" => {
                it.next(); // consume the flag's value
            }
            t if t.starts_with('-') => {} // a bare flag, skip it
            t => {
                return if REMOTE_SUBCOMMANDS.contains(&t) {
                    NetworkNeed::Remote
                } else {
                    NetworkNeed::Local
                };
            }
        }
    }
    // No subcommand at all (e.g. `git --version`) touches no network.
    NetworkNeed::Local
}

/// **The authoritative classifier (D3).** The network need of a typed
/// [`GitOperation`] — what the server *decided to do*, not what its argv looks
/// like.
///
/// # Why this outranks `network_need`
///
/// `network_need`'s own doc comment already says a string match on argv cannot
/// be complete: aliases expand, plumbing reaches the network under names no
/// list will hold (`fetch-pack`, `send-pack`, transport helpers), and a partial
/// clone lazily fetches from otherwise-local commands. The C10 audit's
/// conclusion was that the dispatch must key on the *typed operation the server
/// chose*, because that value is known before any argv exists and is the only
/// thing that carries intent.
///
/// # Why a match with no wildcard arm
///
/// [`GitOperation`] is a closed enum, so this match is checked by the compiler.
/// Adding a variant **fails the build here** until somebody states what network
/// that operation needs — which is the whole reason this is a match and not a
/// lookup table or a `_ => Local` default. A default arm would silently
/// classify tomorrow's `FetchRemote` as `Local`, route it to the strict tier,
/// and break it at runtime instead of at compile time; worse, a default of
/// `Remote` would silently *widen* every new operation's sandbox. Neither
/// failure is acceptable, and the fix for both is to refuse to have a default.
///
/// If a variant's answer is ever unobvious, the tie-break is fail-closed:
/// `Local` routes to the stricter tier, so a misclassified network operation
/// breaks loudly rather than quietly gaining a socket.
pub(crate) fn network_need_for_operation(op: &GitOperation) -> NetworkNeed {
    match op {
        // The one operation in the enum that talks to a remote. `remote` is
        // part of its argv, but that is not why it is classified here — it is
        // classified here because pushing is what the server decided to do.
        GitOperation::PushBranch { .. } => NetworkNeed::Remote,

        // Everything below manipulates refs, the index, the working tree or
        // the object database, all of it local. None of them opens a socket in
        // any configuration this server constructs: no `--recurse-submodules`,
        // no partial-clone promisor (a promisor fetch would make `CheckoutBranch`
        // and `RevertCommit` reach the network — see the note below), no
        // `git merge` of a remote-tracking ref this server does not create.
        GitOperation::CreateBranch { .. } => NetworkNeed::Local,
        GitOperation::CommitOnHead { .. } => NetworkNeed::Local,
        GitOperation::EmptyCommitOnBranch { .. } => NetworkNeed::Local,
        GitOperation::StageAll => NetworkNeed::Local,
        GitOperation::UnstageAll => NetworkNeed::Local,
        GitOperation::CheckoutBranch { .. } => NetworkNeed::Local,
        GitOperation::MergeBranch { .. } => NetworkNeed::Local,
        GitOperation::DeleteBranch { .. } => NetworkNeed::Local,
        GitOperation::ForceDeleteBranch { .. } => NetworkNeed::Local,
        GitOperation::RebaseOntoBase { .. } => NetworkNeed::Local,
        GitOperation::RestoreBranch { .. } => NetworkNeed::Local,
        GitOperation::ResetBranch { .. } => NetworkNeed::Local,
        GitOperation::RevertCommit { .. } => NetworkNeed::Local,
        // `git apply --cached` + pathspec add/reset: index-only, local.
        GitOperation::StageSelection { .. } => NetworkNeed::Local,
        GitOperation::ResetTestRepo => NetworkNeed::Local,
    }
}

/// D3's cross-check: the declared need is authoritative, the argv classifier is
/// a **tripwire on it**, and the tripwire may only ever tighten.
///
/// # The one disagreement that matters, and why only one
///
/// There are two ways `declared` and `network_need(args)` can disagree, and
/// they are not symmetric.
///
/// * **Declared `Local`, argv looks `Remote`.** A caller said "this operation
///   needs no network" and then built an argv whose first subcommand is in
///   `REMOTE_SUBCOMMANDS`. That is a *bug in the server*, not a hostile input —
///   nothing outside this process picks the subcommand. In a debug build it
///   panics, because a developer should meet it the first time they write it.
///   In release it is logged and the **declared** value is kept.
///
///   Keeping `Local` *is* the escalation: `tier_for(Local, false)` is
///   `Tier::Strict`, the tier with no network at all, which is strictly
///   stricter than the `Tier::Network` the argv would have argued for. So the
///   release behaviour and the "escalate to the stricter tier" instruction are
///   the same action, and the operation fails closed — a genuinely-remote
///   command mislabelled `Local` gets `EACCES` on `connect()` and reports it,
///   which is loud, rather than silently gaining a socket it was not declared
///   to need.
///
/// * **Declared `Remote`, argv looks `Local`.** This is *expected* and is not
///   reported. `REMOTE_SUBCOMMANDS` is documented as incomplete by
///   construction, so this direction is what an argv the list has never heard
///   of looks like. Acting on it would mean narrowing `Remote` to `Local`,
///   i.e. taking the network away from an operation that declared it needs the
///   network, on the word of a list that admits it is not authoritative. That
///   breaks working pushes to fix nothing: the declaration is the tighter
///   signal about intent, and a wrongly-`Remote` declaration costs a wider
///   sandbox for one spawn, not an escape.
///
/// So this function is a **checked identity**: it returns `declared`, always.
/// That is the point — after D3 the argv can only ever *complain*, never
/// decide. Anything else would reintroduce exactly the "argv is the dispatch"
/// posture C10 rejected, just with an extra step.
pub(crate) fn reconcile_need(declared: NetworkNeed, args: &[&str]) -> NetworkNeed {
    if declared == NetworkNeed::Local && network_need(args) == NetworkNeed::Remote {
        debug_assert!(
            false,
            "sandbox tier cross-check (D3): an operation declared \
             NetworkNeed::Local but its argv starts with a known remote \
             subcommand — argv = {args:?}. Either the declaration in \
             `network_need_for_operation` is wrong for this operation, or this \
             call site is running a remote command under a local declaration. \
             Fix the declaration; do not widen the tier."
        );
        eprintln!(
            "git-vista: sandbox tier cross-check (D3): declared NetworkNeed::Local \
             but argv looks remote ({args:?}); keeping the stricter tier (Strict). \
             This is a server bug — the operation will fail if it really needs a socket."
        );
    }
    declared
}

/// INV-13 / ADR 0029, at policy-construction time: the strict tier's launcher,
/// or a named refusal.
///
/// Pure in its inputs so the refusal is testable without a broken host — the
/// caller passes the measured [`capabilities::Capabilities`] and the resolved
/// bwrap path, and a unit test can pass a synthetic pair. That matters: the
/// only alternative way to test "Strict was selected and the host cannot supply
/// it" is to uninstall bwrap on the development machine.
///
/// # Why it refuses instead of returning a different tier
///
/// The boot probe (`sandbox::probe`) already gates startup on this host being
/// able to compose the strict tier, so on a healthy host this never fires. It
/// exists for the case the boot gate cannot cover: a capability that goes away
/// *after* boot (bwrap uninstalled by a package upgrade, `max_user_namespaces`
/// set to 0 by a sysctl push, a live-patched kernel), and for the process that
/// reaches policy construction without having run the boot probe at all — every
/// `cargo test` binary in this crate. In both cases the honest answer is the
/// same one ADR 0029 mandates for the boot case: refuse, and name what is
/// missing. Returning `Tier::Network` here would mean a repository the operator
/// believes runs with namespaces, a fresh procfs and no network is quietly
/// running with none of the three; returning `HookMode::Blocked` would be the
/// posture ADR 0029 rejects by name.
fn strict_launcher(
    caps: &capabilities::Capabilities,
    bwrap_path: Option<PathBuf>,
) -> Result<PathBuf, shim::ShimError> {
    match bwrap_path {
        // `strict_missing` is consulted even when a bwrap path was found,
        // because bwrap alone is not the strict tier: without usable user
        // namespaces it cannot create the pid/net/ipc/uts/cgroup namespaces
        // that make the tier what it claims to be, and without Landlock at the
        // floor the shim it launches exits 91 before git ever runs.
        Some(path) if caps.strict_available() => Ok(path),
        _ => {
            let mut missing = caps.strict_missing();
            // A host that clears every capability knob but has no launcher at
            // the reviewed absolute paths still cannot run the tier. Naming it
            // separately keeps the message truthful rather than empty.
            if missing.is_empty() {
                missing.push("bwrap");
            }
            Err(shim::ShimError::StrictUnavailable { missing })
        }
    }
}

/// Whether the operator has explicitly trusted `repo` enough to run it with no
/// sandbox at all — `tier_for`'s `trusted` argument, and the **only** route to
/// [`Tier::Unsandboxed`].
///
/// # Canonicalisation is part of the check, not a convenience
///
/// `trust::is_trusted` requires an already-canonical path and compares it
/// byte-for-byte against what `trust::grant` stored. Handing it the raw `repo`
/// would mean `/home/tom/projects/x`, `/home/tom/projects/./x` and a path
/// reached through a symlink are three different repositories as far as trust
/// is concerned — which is not merely untidy: a request that reaches this
/// function by a spelling the operator did not grant would be told "untrusted"
/// (harmless, fail-closed) while a request that reaches it by a spelling that
/// *aliases* a granted path would be told "trusted" (not harmless). Resolving
/// to the real path first collapses both.
///
/// A canonicalisation failure — the path does not exist, a component is not
/// searchable, a symlink loop — returns `false`. Every uncertainty in this
/// whole chain means untrusted, which is what makes `Unsandboxed` unreachable
/// by accident.
fn repo_is_trusted(repo: &Path) -> bool {
    match repo.canonicalize() {
        Ok(canonical) => trust::is_trusted(&canonical),
        Err(_) => false,
    }
}

/// The tier an operation runs in, given its network need and whether the
/// repository is operator-trusted.
///
/// # Unsandboxed is reachable ONLY through `trusted`
///
/// This is the single most important property of the dispatch. `Unsandboxed` is
/// returned by exactly one arm, `(true, _)`, so an **untrusted** repository can
/// never reach it — and that half is compile-enforced: the match has no
/// wildcard, so a new `NetworkNeed` variant forces a new `(false, NewVariant)`
/// arm to be written, which cannot be `Unsandboxed` without a deliberate edit a
/// reviewer would see. (The `(true, _)` arm *does* use a wildcard, so a new
/// variant inherits `Unsandboxed` for an already-trusted repo — which is
/// correct: trust is a property of the repository, not the operation.)
///
/// `trusted` must come only from a persisted per-repo trust flag set by an
/// explicit operator action (Task 7), stored **outside repository-writable
/// paths** — the C10 audit noted that deriving it from `.git/config` or any file
/// a hostile hook can write would turn the flag into an escalation path. Its
/// absence, a read failure, or a parse failure must all mean `false`.
///
/// As of Task 8 that argument has a real production source: `policy_for` fills
/// it from `repo_is_trusted` → `sandbox::trust::is_trusted`, keyed on the
/// canonicalised repository path and backed by a marker file under the
/// server's own state directory that a sandboxed repository can read but never
/// write. Until an operator grants one, no marker exists and every repository
/// is `false` — still the safe state, now by rule rather than by omission.
pub(crate) fn tier_for(need: NetworkNeed, trusted: bool) -> Tier {
    match (trusted, need) {
        // An operator-trusted repository runs with no sandbox at all, and flies
        // a permanent banner (INV-15). This is the ONLY route to Unsandboxed.
        (true, _) => Tier::Unsandboxed,
        // A remote operation needs the network, which only the Network tier
        // provides (the strict tier's namespace breaks push — F3).
        (false, NetworkNeed::Remote) => Tier::Network,
        // Everything else gets the fuller-isolation strict tier.
        (false, NetworkNeed::Local) => Tier::Strict,
    }
}

/// Build the production policy for running git in `repo`.
///
/// This is the single production policy-construction site for repository
/// operations (Task 6; D2, #66 Task 7 gave it its current signature). It
/// mirrors what the `shim_cli::workable` test helper does, but resolves the
/// shim through `sandbox::shim` — so a missing or moved shim is a named
/// `ShimError` here, at construction time, rather than an ENOENT surfacing
/// from inside a spawn. `policy_for_clone` (D4) is the sibling for the one
/// operation this function does not cover — cloning into a destination that
/// does not exist yet.
///
/// # Repository geometry is resolved (not merely trusted) before any grant is
/// built (D2) — but the managed-root containment check lives one layer up
///
/// `repo_paths::resolve` finds `repo`'s actual git directory(ies), composing
/// `worktree`'s linked-worktree containment rule the way that module's own
/// doc comment describes: a resolution that cannot be proven internally
/// consistent (a dangling pointer, a symlinked `.git`, a gitdir not
/// registered under the commondir it claims) refuses the whole policy
/// (fail-closed), never falls back to granting only `repo` itself.
///
/// What this function deliberately does **not** do is check that resolution
/// against the catalog's allowed roots (`repo_paths::resolve_and_validate`'s
/// full contract, or `state::path_is_allowed`). That containment check needs
/// a *request-scoped* notion of "what this server instance is configured to
/// serve" — the catalog's `AllowedRoots`, a single process-wide `RwLock` — and
/// this function is the one every git spawn in the crate funnels through,
/// including the ~40 unit tests across `git_cmd.rs`, `planner`'s contract/
/// lifecycle/coordination suites, `history.rs` and others that deliberately
/// spawn git against a throwaway `tempfile::tempdir()` with **no** catalog
/// registration at all — by design, so they stay independent of the shared
/// global `CATALOG`/`CURRENT` statics the way `state.rs`'s own test module
/// documents doing on purpose. Making *this* function catalog-aware would
/// silently couple every one of those tests to whatever some other,
/// concurrently-running test happened to have registered in the same
/// process — exactly the cross-test global-state fragility that pattern
/// exists to avoid, not a hardening this crate is short of.
///
/// The managed-root check the D2 brief describes for `policy_for` is real and
/// still lands — at `state::resolve_target`, the resolution point actual HTTP
/// mutation requests funnel through, which both has a legitimate reason to
/// consult the catalog and runs *before* any repository path reaches this
/// function at all. See that function's doc comment, and `deviations` in the
/// implementation report, for the full reasoning. Reads do not yet route
/// through an equivalent check — see the same report for that named gap.
///
/// # `read_only` withholds the write grant (D2's actual behavioural change)
///
/// Before D2 this function granted `repo` (and any resolved worktree
/// commondir) read-write **unconditionally**. A `read_only == true` path now
/// gets **no RW grant at all**: `repo` and any resolved worktree commondir go
/// into `ro_trees` instead of `rw_trees`.
///
/// **`read_only` and `reject_if_read_only` must agree — they are the same
/// fact, checked twice, not two independent signals.** An earlier version of
/// this comment argued the opposite: that `state::read_only_for_path` should
/// key off the catalog's own static record rather than the current
/// selection's live mode, as "defense in depth... it holds even if
/// [`reject_if_read_only`] has a bug." That reasoning silently reintroduced
/// the always-read-only-clone posture ADR 0007 already considered and
/// rejected (*"a clone opened in active mode accepts local writes...
/// `RepoEntry.read_only` is superseded"*), and it shipped a real bug:
/// reselecting a clone into Active mode passed the app-level gate and then
/// failed writes here anyway, with a raw sandbox error instead of a clean
/// refusal or an actual success. Decided 2026-07-30, Option A of
/// `design-docs/2026-07-30-read-only-vs-mode-conflict.md`: mode is the single
/// source of truth. `state::read_only_for_path` now derives its answer from
/// live mode for the current selection, read at call time — the same one
/// `reject_if_read_only` reads, not a second, independently-drifting record.
///
/// **Not a closed race, though — see `read_only_for_path`'s own doc comment.**
/// "At call time" is doing real work in that sentence: a write's target is
/// captured once, early, but this function (and the `read_only` it's handed)
/// runs later, after real `.await` points, so a *concurrent* reselection to a
/// different repo in between can make this fall back to the catalog's stale
/// flag for the original repo. Fail-closed only (a legitimate write can be
/// wrongly refused; never the reverse) and narrow — same-path mode flips,
/// what this fix targets, are unaffected — but real, and not fixed tonight.
///
/// # `need` selects the tier (Task 8 / D3) — this is the production dispatch
///
/// `need` is the **declared** network need of the operation: for a planner
/// mutation it comes from `network_need_for_operation`, an exhaustive match on
/// the typed [`git_vista_protocol::GitOperation`] the server chose; for the
/// read and ref-maintenance helpers that have no typed operation it is declared
/// at the helper (all `Local` — see `git_cmd`'s own doc comments). Whatever its
/// origin, it reaches `tier_for` unchanged; the argv classifier `network_need`
/// only cross-checks it (`reconcile_need`), and only in the tightening
/// direction.
///
/// The second dispatch input is `trusted`, from `repo_is_trusted` →
/// `sandbox::trust::is_trusted`. This is that module's **first production
/// caller**: before Task 8 the persisted trust flag existed, was tested, and
/// was consulted by nothing, so `Unsandboxed` was unreachable in production by
/// accident rather than by rule. It is now unreachable by rule — `tier_for`
/// returns it from exactly one arm, and that arm needs a marker file only an
/// explicit operator action writes, outside every repository-writable path.
///
/// So the three outcomes are: local operation on an untrusted repository →
/// `Strict`; remote operation on an untrusted repository → `Network`; any
/// operation on an operator-trusted repository → `Unsandboxed`.
///
/// # Strict is refused, never downgraded (INV-13 / ADR 0029)
///
/// A `Strict` policy needs a `bwrap` launcher at a reviewed absolute path plus
/// usable user namespaces and Landlock at the floor. When the dispatch selects
/// `Strict` and the host cannot supply it, this function returns
/// [`shim::ShimError::StrictUnavailable`] and the operation refuses. It does
/// **not** fall back to `Network` (a weaker sandbox than the one selected, with
/// outbound TCP the operation has no use for) and it does not degrade-and-block
/// hooks — ADR 0029 rejects that posture by name. See `strict_launcher`.
///
/// # Tier-dependent fields
///
/// `net_ports` is `DEFAULT_GIT_PORTS` in `Network` and **empty** everywhere
/// else: `Strict` has no network at all (F3, `--net-deny`), and `Unsandboxed`
/// has no ruleset to put ports in. `bwrap` is `Some` only in `Strict`.
/// `secret_excludes` is populated regardless of tier, so the secret set is
/// never silently empty in any of them.
pub(crate) fn policy_for(
    repo: &Path,
    read_only: bool,
    need: NetworkNeed,
) -> Result<Policy, shim::ShimError> {
    // D3's dispatch, in two lines. `need` is the caller's declaration; trust is
    // a persisted property of the repository. Nothing else feeds the tier —
    // notably not the argv, which by this point has already had its say through
    // `reconcile_need` at the call site.
    let tier = tier_for(need, repo_is_trusted(repo));

    let home = PathBuf::from(std::env::var_os("HOME").ok_or(shim::ShimError::NoHome)?);
    let shim = shim::shim_path().map_err(Clone::clone)?.to_path_buf();
    // INV-13: refuse *before* building anything else, so a host that cannot
    // supply the selected tier produces one named error rather than a policy
    // that dies later inside `shim_argv`'s `expect`.
    let bwrap = match tier {
        Tier::Strict => Some(strict_launcher(
            &capabilities::current(),
            bwrap::bwrap_path().map(Path::to_path_buf),
        )?),
        Tier::Network | Tier::Unsandboxed => None,
    };
    let (mut rw, mut ro) = default_system_trees(tier);

    // An absent `.git` is not a hostile or malformed geometry to refuse —
    // it is "not a repository (yet)", which is not this function's problem
    // to diagnose. Two real, non-hostile callers depend on that leniency
    // surviving D2: `git init` itself needs a policy for the very command
    // that is about to create `.git` (see `shim_cli::fixture`, exercised
    // transitively by most of this module's own test suite), and a
    // degraded-mode selection's *reads* are documented (`resolve_repo`) to
    // run anyway and surface git's own "not a repository" error rather than
    // refuse up front. So a missing `.git` falls back to the pre-D2 shape —
    // grant `repo` itself, no extra worktree-style grant — while a `.git`
    // that exists but cannot be proven safe (`repo_paths::resolve`'s other
    // error variants) still refuses the whole policy, exactly as before.
    let paths = match repo_paths::resolve(repo) {
        Ok(paths) => Some(paths),
        Err(repo_paths::RepoPathsError::MissingGitFile { .. }) => None,
        Err(other) => return Err(shim::ShimError::RepoPaths(other)),
    };

    if read_only {
        ro.push(repo.to_path_buf());
        if let Some(paths) = paths {
            if paths.commondir != repo {
                ro.push(paths.commondir);
            }
        }
    } else {
        rw.push(repo.to_path_buf());
        if let Some(paths) = paths {
            if paths.commondir != repo {
                rw.push(paths.commondir);
            }
        }
    }
    ro.push(home.clone());
    // #188: Network tier only. See `ssh_agent_socket_grant`'s doc comment for
    // why this is added despite not being what currently makes the socket
    // reachable at the Landlock layer.
    rw.extend(ssh_agent_socket_grant(tier));
    Ok(Policy {
        tier,
        shim,
        // `Some` only in Strict; the Network tier launches the shim directly
        // (F3) and Unsandboxed launches nothing at all.
        bwrap,
        rw_trees: rw,
        ro_trees: ro,
        // The `$HOME`-relative secret set, plus the one path that must be
        // withheld from *every* grant regardless of what is being served: the
        // trust store.
        //
        // # Why the trust store is an exclude and not merely un-granted
        //
        // `trust.rs`'s security argument is that a sandboxed repository cannot
        // forge its own trust marker, because markers live outside every
        // repository and `$HOME` is granted read-only. That argument had a
        // hole, and it is the `rw.push(repo)` above: whatever path the operator
        // is serving gets **read-write**, and nothing stopped that path from
        // containing the state directory. Serve a repository at or above
        // `~/.local/state` — or point `XDG_STATE_HOME` inside a served tree —
        // and a hostile hook could write its own marker, so the *next*
        // operation on that repository would resolve `trusted = true` and
        // `tier_for` would hand it `Tier::Unsandboxed`. A total sandbox bypass,
        // reached entirely through sanctioned paths, and the precise
        // escalation `trust.rs`'s module doc claims is impossible.
        //
        // An exclude closes it because the shim withholds excluded paths from
        // every tree it grants, read-write ones included
        // (`is_or_inside_exclude` / `is_ancestor_of_exclude` in
        // `bin/gv-sandbox/main.rs`) — the one mechanism here that outranks a
        // grant rather than competing with it.
        //
        // Computed from `state::sandbox_trust_dir()` rather than added to
        // `DEFAULT_SECRET_EXCLUDES` as a `$HOME`-relative string, because the
        // real location follows `XDG_STATE_HOME` when that is set. A
        // `$HOME/.local/state/…` literal would silently protect nothing on any
        // host that sets it — the same silent-miss class `secret_excludes_for_home`'s
        // own doc comment already records being bitten by once.
        secret_excludes: {
            let mut excludes = secret_excludes_for_home(&home);
            excludes.push(crate::state::sandbox_trust_dir());
            excludes
        },
        // #188: the one named exception to the exclude above. Network tier
        // only — Strict must never see `known_hosts`, and it denies the
        // agent socket's own `connect()` at the seccomp layer regardless of
        // any filesystem grant, so there is nothing this tier legitimately
        // reads under `~/.ssh` at all.
        ro_carveouts: match tier {
            Tier::Network => ssh_known_hosts_carveout(&home),
            Tier::Strict | Tier::Unsandboxed => Vec::new(),
        },
        // Ports are a Network-tier ruleset entry. Strict denies the network
        // outright (`--net-deny`, plus bwrap's `--unshare-net`) and
        // Unsandboxed installs no ruleset, so a non-empty list in either would
        // be an argv that contradicts itself — see `shim_argv`.
        net_ports: match tier {
            Tier::Network => DEFAULT_GIT_PORTS.to_vec(),
            Tier::Strict | Tier::Unsandboxed => Vec::new(),
        },
        hook_mode: HookMode::Run,
    })
}

/// The **Network-tier** production policy for `repo`, at the one-argument arity
/// `escape_contract::policy_for_case` is pinned to.
///
/// `read_only = false` (D2's behavioural change was making that conditional;
/// this caller always wants the RW grant) and `need = NetworkNeed::Remote`.
///
/// # Why `Remote`, and why this function did not become the Strict one (Task 8)
///
/// Before Task 8 this wrapper was `need = NetworkNeed::Local` and the note here
/// said the argument was irrelevant, because `policy_for` hard-coded
/// `Tier::Network` regardless. That is no longer true: `need` now *chooses* the
/// tier, so this wrapper has to declare what it actually wants — and what its
/// remaining caller wants is the **Network** tier.
///
/// `escape_contract::policy_for_case` routes the **nine** `Exemption::None`
/// battery cases that declare `tier: Tier::Network` through this function
/// (`secret_read_denied`, `io_uring_denied`, `high_bit_prctl_denied`,
/// `high_bit_io_uring_denied`, `write_home_denied`, `cgroup_tree_denied`,
/// `no_new_privs_irrevocable`, `second_landlock_ruleset_denied`,
/// `unshare_userns_denied`). Since #206 that dispatch matches on `case.tier`:
/// the seven `Tier::Strict` cases go to `policy_for(repo, false,
/// NetworkNeed::Local)` directly, and `hook_mode_suite`'s `blocked_hooks` is
/// the one remaining exemption and reaches neither. (This paragraph used to
/// say "all ten such cases declare `Tier::Network`" and list `blocked_hooks`
/// among them; that was wrong before #206 as well as after — an exempt case
/// has never routed through here.)
/// Each is a containment claim written
/// *against that tier* — `unshare_userns_denied` in particular is only a
/// meaningful claim where nothing has already unshared a user namespace, which
/// bwrap does for us in Strict. Declaring `Local` here to make the wrapper
/// "more production-like" would silently re-tier all ten and quietly change
/// what they prove, which is the exact anti-vacuity failure the R-rules exist
/// to catch. `NetworkNeed::Remote` is therefore not a fudge: a remote operation
/// on an untrusted repository is genuinely how production reaches this tier,
/// and it is what these cases are written about.
///
/// The Strict half of the production dispatch is exercised instead by
/// `shim_cli::production_policy` (which declares `Local`, drives real
/// `git init`/`commit`/`status`/`config` through the composed bwrap launcher,
/// and is what `spawn.rs`'s end-to-end tests use) and by `dispatch.rs`'s
/// tier-selection tests.
///
/// # Why this delegates through a real policy build, not a one-line forward
///
/// R8 **no longer reads this function at all.** It used to: the tripwire
/// grepped this exact body for the literal tokens `Tier::Network` and
/// `HookMode::Run`, because before #197 `policy_for` hard-coded both and the
/// hard-codes *were* the declared blockers. #197 removed them, but the tokens
/// survived inside the `debug_assert!`s below — so the grep went on passing
/// while the condition it stood for no longer existed, and eight exemptions
/// were reworded instead of retired. #206 re-anchored R8 onto a property of
/// **production** source (no production `Policy` constructor may yield
/// `HookMode::Blocked`) plus set-equality on the declared blocker strings. A
/// tripwire pointed at a token in `#[cfg(test)]` code cannot expire, which is
/// the one thing R8 exists to do.
///
/// The `debug_assert_eq!`/`debug_assert!` below therefore have no scanner
/// reading them, and they stay anyway: they are a real self-check that this
/// wrapper's behaviour has not
/// drifted from what it claims — after Task 8 they are a genuinely load-bearing
/// pin, because the tier is now *derived* rather than constant, and a future
/// edit to `tier_for` or to `network_need_for_operation` that moved a remote
/// operation off the Network tier would land here as a failing assertion
/// instead of as ten battery cases silently changing tier.
#[cfg(test)]
pub(crate) fn policy_for_repo(repo: &Path) -> Result<Policy, shim::ShimError> {
    let policy = policy_for(repo, false, NetworkNeed::Remote)?;
    debug_assert_eq!(policy.tier, Tier::Network);
    debug_assert!(matches!(policy.hook_mode, HookMode::Run));
    Ok(policy)
}

/// Build the production policy for `git clone` (D4): RW on the clones root
/// (the clone's destination does not exist yet at policy time, so
/// `repo_paths` — which resolves an *existing* repository's `.git` — has
/// nothing to validate here), and `trusted` structurally absent from this
/// signature rather than merely defaulted to `false`.
///
/// # Why clone gets its own constructor instead of reusing `policy_for`
///
/// Clone is the one operation that fetches attacker-chosen content by design
/// — the URL is the request. Task 8 wired the per-repo trust lookup into
/// `policy_for`, which is what makes this separation load-bearing rather than
/// anticipatory: `policy_for` can now return `Tier::Unsandboxed`, and clone
/// must never be able to. It cannot, structurally — clone has no repository to
/// look a trust flag up *for*, so this function neither takes nor derives one
/// and the tier below is a constant. That is a property of the signature, not a
/// runtime check some future edit could remove.
pub(crate) fn policy_for_clone(clones_root: &Path) -> Result<Policy, shim::ShimError> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or(shim::ShimError::NoHome)?);
    let shim = shim::shim_path().map_err(Clone::clone)?.to_path_buf();
    let tier = Tier::Network; // clone is always NetworkNeed::Remote.
    let (mut rw, mut ro) = default_system_trees(tier);
    rw.push(clones_root.to_path_buf());
    ro.push(home.clone());
    // #188: identical carve-out and agent-socket grant to `policy_for`'s
    // Network branch. This function is an **independent** `Policy`
    // constructor, not a call into `policy_for` — skipping either grant here
    // would leave `git clone git@host:…` broken while push/fetch on an
    // already-cloned repository worked, since clone is the one operation
    // that never reaches `policy_for` at all (see the doc comment above).
    rw.extend(ssh_agent_socket_grant(tier));
    Ok(Policy {
        tier,
        shim,
        bwrap: None,
        rw_trees: rw,
        ro_trees: ro,
        // Same trust-store exclude as `policy_for`, and clone needs it at least
        // as much: it is the one operation that fetches attacker-chosen
        // content, so it must never be able to leave behind a marker that
        // promotes the resulting repository on a later operation.
        secret_excludes: {
            let mut excludes = secret_excludes_for_home(&home);
            excludes.push(crate::state::sandbox_trust_dir());
            excludes
        },
        ro_carveouts: ssh_known_hosts_carveout(&home),
        net_ports: DEFAULT_GIT_PORTS.to_vec(),
        hook_mode: HookMode::Run,
    })
}

/// The chokepoint. Returns the complete launcher argv **up to and including
/// the program name `git`**; the caller appends `-C <repo> <args…>`.
///
/// INV-16: the result is one of exactly three shapes —
/// 1. `["git"]` — the `Unsandboxed` tier with hooks running;
/// 2. `["git", "-c", "core.hooksPath=<dir>"]` — the `Unsandboxed` tier with
///    hooks blocked (see below);
/// 3. a fixed reviewed prefix ending in `["--", "git"]` — every sandboxed tier.
///
/// # Why shape 2 exists
///
/// `Unsandboxed` used to return a bare `["git"]` unconditionally, which threw
/// away `HookMode::Blocked`. That combination is reachable and it is the worst
/// one: `Blocked` is the state the probe drops to when the host cannot supply
/// the declared minimum (INV-13), and `Unsandboxed` is a repository the
/// operator has explicitly trusted. A trusted repository on a degraded host
/// would therefore have run its hooks — arbitrary code, with no sandbox and no
/// hook suppression — while the policy in memory said hooks were blocked.
/// `-c core.hooksPath=<empty dir>` is the same suppression the shim applies,
/// expressed in the only mechanism available when there is no shim in the argv.
pub(crate) fn sandbox_argv(policy: &Policy) -> Vec<OsString> {
    if policy.tier == Tier::Unsandboxed {
        let mut argv = vec![OsString::from("git")];
        if let HookMode::Blocked { empty_dir } = &policy.hook_mode {
            argv.push(OsString::from("-c"));
            let mut setting = OsString::from("core.hooksPath=");
            setting.push(empty_dir);
            argv.push(setting);
        }
        return argv;
    }
    let mut argv = shim_argv(policy);
    argv.push(OsString::from("--"));
    argv.push(OsString::from("git"));
    argv
}

/// # Panics
///
/// Never for `Tier::Unsandboxed` — both callers return before reaching here.
/// Panics if a `Strict` policy carries no `bwrap` path; `Policy` construction
/// is responsible for degrading to `Network` or reporting INV-13 instead of
/// building a strict policy that cannot launch its own namespace boundary.
fn shim_argv(policy: &Policy) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    if policy.tier == Tier::Strict {
        let bwrap = policy.bwrap.as_ref().expect(
            "a Strict policy must carry a resolved bwrap path; without namespaces it is \
             not the strict tier and must degrade loudly (INV-13), never silently",
        );
        argv.push(bwrap.clone().into_os_string());
        argv.extend(STRICT_BWRAP_ARGS.iter().map(OsString::from));
        argv.push(OsString::from("--"));
    }
    argv.push(policy.shim.clone().into_os_string());
    argv.push(OsString::from("--abi-floor"));
    argv.push(OsString::from(LANDLOCK_ABI_FLOOR.to_string()));
    for p in &policy.rw_trees {
        argv.push(OsString::from("--rw"));
        argv.push(p.clone().into_os_string());
    }
    for p in &policy.ro_trees {
        argv.push(OsString::from("--ro"));
        argv.push(p.clone().into_os_string());
    }
    for p in &policy.secret_excludes {
        argv.push(OsString::from("--exclude"));
        argv.push(p.clone().into_os_string());
    }
    // #188: emitted after the excludes above so the argv reads left to
    // right as a narrative — grants, then excludes, then the explicit,
    // reviewed exception to an exclude. A distinct flag from `--ro` on
    // purpose (`Policy::ro_carveouts`'s doc comment): a reviewer scanning
    // this argv should see immediately which grants are the sanctioned
    // exception rather than an ordinary tree grant.
    for p in &policy.ro_carveouts {
        argv.push(OsString::from("--ro-carveout"));
        argv.push(p.clone().into_os_string());
    }
    match &policy.hook_mode {
        HookMode::Run => argv.push(OsString::from("--hooks-run")),
        HookMode::Blocked { empty_dir } => {
            argv.push(OsString::from("--hooks-blocked"));
            argv.push(empty_dir.clone().into_os_string());
        }
    }
    argv.push(OsString::from(match policy.tier {
        Tier::Strict => "--net-deny",
        Tier::Network => "--net-allow",
        Tier::Unsandboxed => unreachable!("handled by the caller"),
    }));
    // Ports only follow `--net-allow`. Emitting them after `--net-deny` would
    // be a contradiction the shim would have to arbitrate, and an argv that
    // contradicts itself is one a reviewer cannot check by eye.
    if policy.tier == Tier::Network {
        for port in &policy.net_ports {
            argv.push(OsString::from("--net-port"));
            argv.push(OsString::from(port.to_string()));
        }
    }
    argv
}
