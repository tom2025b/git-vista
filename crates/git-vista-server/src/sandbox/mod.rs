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
use std::path::PathBuf;

/// The one impure corner of `sandbox`: it stats the filesystem to find the
/// strict tier's launcher. Task 5's policy builders call
/// `bwrap::bwrap_path()` to fill `Policy::bwrap`.
pub(crate) mod bwrap;
/// The other impure corner: locating the `gv-sandbox` shim. Kept out of this
/// file for the same reason as `bwrap` — `sandbox_argv` stays a total function
/// of its `Policy`.
pub(crate) mod shim;

#[cfg(test)]
mod argv;
#[cfg(test)]
mod deps;

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
    DEFAULT_SECRET_EXCLUDES.iter().map(|s| home.join(s)).collect()
}

/// System trees granted read+execute in every tier.
pub(crate) const DEFAULT_RO_TREES: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc"];

/// System trees granted read+write in every tier. `/dev` is here because it is
/// in the only configuration the round-4 verdict actually measured git
/// succeeding under, and because a `sh` hook redirecting to `/dev/null` fails
/// without it. C4's private `/dev/shm` is supplied by the bwrap prefix (strict)
/// or withheld by simply not granting it (network).
pub(crate) const DEFAULT_RW_TREES: &[&str] = &["/dev"];

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
/// other process on the box, visible. ADR 0026 records this distinction.
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
    }
    (rw, ro)
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

/// The escape battery's launcher. Identical setup, different terminal mode:
/// the shim runs its built-in checks in-process instead of exec'ing anything.
/// That is what lets the battery satisfy the composition rule — the checks run
/// behind the very same Landlock and seccomp code path production uses —
/// without ever teaching the shim to exec an arbitrary program.
///
/// `None` for `Tier::Unsandboxed`: there is no sandbox there to probe, so there
/// is no argv that could probe it. It returns `None` rather than panicking
/// because the caller reaches this on a real path — probing an operator-trusted
/// repository — and a panic there takes down a server worker thread for a
/// condition that is a legitimate answer, not a bug. The caller reports "no
/// sandbox to probe"; INV-15's permanent banner is what tells the operator.
pub(crate) fn probe_argv(policy: &Policy) -> Option<Vec<OsString>> {
    if policy.tier == Tier::Unsandboxed {
        return None;
    }
    let mut argv = shim_argv(policy);
    argv.push(OsString::from("--self-probe"));
    Some(argv)
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
