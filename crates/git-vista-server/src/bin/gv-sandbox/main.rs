//! M1.13b (#66): the fused sandbox shim.
//!
//! **Fused on purpose.** Round 4 ran Landlock and seccomp as separate helper
//! binaries and hit `exec …: Permission denied` — a helper's own path must sit
//! inside a granted tree or the `execve` of it is denied, and the failure made
//! a benchmark look *faster* than bare git because nothing actually ran. One
//! binary that applies Landlock, applies seccomp (Task 4), and `execve`s git
//! avoids the entire class.
//!
//! This file must contain `.exec()` and must **not** contain `.spawn()`,
//! `.output()` or `.status()`: the shim replaces its own process image, it
//! never becomes a parent. It also names `git` literally, so the argv tripwire
//! in `argv_boundary.rs` can prove it cannot exec anything else.
//!
//! # Everything here was measured, not reasoned
//!
//! Four rounds of this design died of reasoning about kernel behaviour instead
//! of measuring it. Every non-obvious constant and every ordering decision
//! below carries the measurement that produced it. If you change one, measure
//! it again — the header on this host is *stale* relative to the running
//! kernel, so reading `/usr/include/linux/landlock.h` is not sufficient
//! evidence for anything.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Exit codes. Distinct on purpose: a caller must be able to tell "the host
// cannot provide the sandbox" (91) from "the sandbox broke" (92/93) from "git
// itself failed", because the first is a degradation decision (INV-13) and the
// others are bugs.
// ---------------------------------------------------------------------------
const EXIT_ARGV: i32 = 90;
const EXIT_ABI_FLOOR: i32 = 91;
const EXIT_LANDLOCK: i32 = 92;
const EXIT_SECCOMP: i32 = 93;
const EXIT_EXEC: i32 = 94;

// ---------------------------------------------------------------------------
// Landlock ABI. Declared by hand rather than taken from the system header:
// measured 2026-07-29, this host's `/usr/include/linux/landlock.h` declares a
// TWO-field `landlock_ruleset_attr` and no `LANDLOCK_SCOPE_*` at all, while the
// running kernel reports ABI 8 and accepts the three-field struct. Trusting the
// header would silently drop the scopes that are the entire reason this design
// declares a floor of 6.
// ---------------------------------------------------------------------------
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;
const LANDLOCK_RULE_NET_PORT: libc::c_int = 2;

// Filesystem access bits, verified against the kernel header 2026-07-29.
// Mislabelling these is not cosmetic: `1 << 5` is REMOVE_FILE, not MAKE_REG,
// and a policy built from guessed names grants a different set than it claims.
const A_EXECUTE: u64 = 1 << 0;
const A_WRITE_FILE: u64 = 1 << 1;
const A_READ_FILE: u64 = 1 << 2;
const A_READ_DIR: u64 = 1 << 3;
const A_REMOVE_DIR: u64 = 1 << 4;
const A_REMOVE_FILE: u64 = 1 << 5;
const A_MAKE_CHAR: u64 = 1 << 6;
const A_MAKE_DIR: u64 = 1 << 7;
const A_MAKE_REG: u64 = 1 << 8;
const A_MAKE_SOCK: u64 = 1 << 9;
const A_MAKE_FIFO: u64 = 1 << 10;
const A_MAKE_BLOCK: u64 = 1 << 11;
const A_MAKE_SYM: u64 = 1 << 12;
const A_REFER: u64 = 1 << 13;
const A_TRUNCATE: u64 = 1 << 14;
/// ABI 5. Absent from this host's header; included so a ruleset built here
/// mediates device ioctls rather than leaving them unhandled and therefore
/// allowed.
const A_IOCTL_DEV: u64 = 1 << 15;

const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;

/// ABI 6, and the reason the declared floor is 6 rather than 4.
///
/// Measured A/B on this host: with `scoped = 0`, a child could `connect()` to
/// an abstract unix socket and could signal its parent. With
/// `SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL` both returned `EPERM`, while a
/// no-rule filesystem control stayed denied in *both* runs — so the ruleset was
/// live either way and the scopes are what changed the outcome. This is what
/// withholds D-Bus and abstract-socket deputies, and it is independent of any
/// network rule.
const LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const LANDLOCK_SCOPE_SIGNAL: u64 = 1 << 1;

/// Everything a repository write path legitimately needs.
///
/// `REFER` is deliberately excluded: it authorises cross-directory rename/link,
/// which a sandboxed git does not need and which is the one access right
/// Landlock always implicitly handles.
const RW_ACCESS: u64 = A_EXECUTE
    | A_WRITE_FILE
    | A_READ_FILE
    | A_READ_DIR
    | A_REMOVE_DIR
    | A_REMOVE_FILE
    | A_MAKE_DIR
    | A_MAKE_REG
    | A_MAKE_SOCK
    | A_MAKE_FIFO
    | A_MAKE_SYM
    | A_TRUNCATE;

const RO_DIR_ACCESS: u64 = A_EXECUTE | A_READ_FILE | A_READ_DIR;

/// Landlock's own `ACCESS_FILE` set: the **only** rights `landlock_add_rule`
/// accepts on a `path_beneath` rule whose target is not a directory. Every other
/// bit is directory-only, and offering one for a regular file does not grant a
/// subset — it makes the entire rule fail with `EINVAL`.
///
/// Measured on this host 2026-07-29 (ABI 8, one ruleset declaring `HANDLED_FS`,
/// nine `landlock_add_rule` calls). The first, third and sixth lines are
/// re-measured on every run by `tests::the_kernel_rejects_directory_only_rights_on_a_regular_file`,
/// so this table is a claim the build checks rather than a note someone took:
///
/// ```text
/// FILE + RO_DIR_ACCESS                     -> EINVAL   (READ_DIR is dir-only)
/// FILE + RW_ACCESS                         -> EINVAL
/// FILE + RW_ACCESS & !(READ_DIR|EXECUTE)   -> EINVAL   (MAKE_*/REMOVE_* are dir-only)
/// FILE + RO_DIR_ACCESS & ACCESS_FILE       -> 0
/// FILE + RW_ACCESS   & ACCESS_FILE         -> 0
/// FILE + 0                                 -> ENOMSG   (an empty rule is not a rule)
/// DIR  + RO_DIR_ACCESS                     -> 0
/// DIR  + RW_ACCESS                         -> 0
/// ```
///
/// The third line is why this constant exists rather than the narrower
/// `& !(A_READ_DIR | A_EXECUTE)` mask `enumerate` used to carry: that mask is
/// sufficient for a read-only tree *by luck* — `RO_DIR_ACCESS` happens to have
/// no other directory-only bit — and **insufficient for a read-write one**,
/// which still kept `MAKE_DIR`/`MAKE_REG`/`MAKE_SOCK`/`MAKE_FIFO`/`MAKE_SYM`/
/// `REMOVE_DIR`/`REMOVE_FILE` and was rejected outright. So a regular file
/// inside an enumerated `--rw` tree was silently granted nothing too; only the
/// read-only half of the enumerate path ever worked.
const ACCESS_FILE: u64 = A_EXECUTE | A_WRITE_FILE | A_READ_FILE | A_TRUNCATE | A_IOCTL_DEV;

/// Every filesystem right this ruleset mediates. Anything omitted here is
/// **allowed unconditionally** — Landlock only forbids what a ruleset declares
/// it handles, so an under-declared mask is a silently weaker sandbox.
const HANDLED_FS: u64 = A_EXECUTE
    | A_WRITE_FILE
    | A_READ_FILE
    | A_READ_DIR
    | A_REMOVE_DIR
    | A_REMOVE_FILE
    | A_MAKE_CHAR
    | A_MAKE_DIR
    | A_MAKE_REG
    | A_MAKE_SOCK
    | A_MAKE_FIFO
    | A_MAKE_BLOCK
    | A_MAKE_SYM
    | A_REFER
    | A_TRUNCATE
    | A_IOCTL_DEV;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// `.port` is a `__u64` in **plain host byte order**. Measured: an `htons()`'d
/// value grants a different port than intended, and declaring the field as
/// `__u16` makes `landlock_add_rule` fail outright with `EINVAL` — which,
/// layered on a ruleset that declares TCP handled, denies *all* TCP while
/// looking like it configured something.
#[repr(C, packed)]
struct NetPortAttr {
    allowed_access: u64,
    port: u64,
}

// ---------------------------------------------------------------------------
// Parsed command line
// ---------------------------------------------------------------------------
#[derive(Debug, Default)]
struct Args {
    abi_floor: Option<u32>,
    rw: Vec<PathBuf>,
    ro: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    /// #188: named, single-file exceptions to an `--exclude` above. See
    /// `add_carveout_rule` for the mechanism and why it is a separate flag
    /// rather than a `--ro` entry.
    ro_carveouts: Vec<PathBuf>,
    net_ports: Vec<u16>,
    net_allow: Option<bool>,
    hooks_blocked_dir: Option<PathBuf>,
    hooks_seen: bool,
    /// Everything after `--`. Must begin with exactly `git`.
    program_args: Vec<String>,
}

fn die(code: i32, msg: &str) -> ! {
    eprintln!("gv-sandbox: {msg}");
    std::process::exit(code);
}

fn parse() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = |what: &str| -> String {
            it.next()
                .unwrap_or_else(|| die(EXIT_ARGV, &format!("{what} requires a value")))
        };
        match flag.as_str() {
            "--abi-floor" => {
                let v = value("--abi-floor");
                a.abi_floor = Some(
                    v.parse()
                        .unwrap_or_else(|_| die(EXIT_ARGV, "--abi-floor must be a number")),
                );
            }
            "--rw" => a.rw.push(absolute(&value("--rw"), "--rw")),
            "--ro" => a.ro.push(absolute(&value("--ro"), "--ro")),
            // A relative exclude silently matches nothing, and a secret set
            // that matches nothing is an empty secret set. Reject it loudly.
            "--exclude" => a.excludes.push(absolute(&value("--exclude"), "--exclude")),
            // #188: distinct from `--ro` on purpose, so a reviewer scanning an
            // argv can see immediately which grants are the sanctioned
            // exception to an `--exclude` rather than an ordinary tree grant.
            "--ro-carveout" => a
                .ro_carveouts
                .push(absolute(&value("--ro-carveout"), "--ro-carveout")),
            "--net-port" => {
                let v = value("--net-port");
                a.net_ports.push(
                    v.parse()
                        .unwrap_or_else(|_| die(EXIT_ARGV, "--net-port must be 1..=65535")),
                );
            }
            "--net-allow" => a.net_allow = Some(true),
            "--net-deny" => a.net_allow = Some(false),
            "--hooks-run" => a.hooks_seen = true,
            "--hooks-blocked" => {
                a.hooks_seen = true;
                a.hooks_blocked_dir = Some(absolute(&value("--hooks-blocked"), "--hooks-blocked"));
            }
            "--" => {
                a.program_args = it.collect();
                break;
            }
            other => die(EXIT_ARGV, &format!("unknown flag `{other}`")),
        }
    }
    a
}

fn absolute(s: &str, flag: &str) -> PathBuf {
    let p = PathBuf::from(s);
    if !p.is_absolute() {
        die(
            EXIT_ARGV,
            &format!("{flag} requires an absolute path, got `{s}`"),
        );
    }
    p
}

/// Required flags are required. A defaulted `--abi-floor` would contradict the
/// design's own rule that the floor travels in the argv rather than living as a
/// default nobody can see from a command line.
fn validate(a: &Args) {
    if a.abi_floor.is_none() {
        die(
            EXIT_ARGV,
            "--abi-floor is required (it must travel in the argv, never default)",
        );
    }
    if !a.hooks_seen {
        die(
            EXIT_ARGV,
            "one of --hooks-run or --hooks-blocked is required",
        );
    }
    let Some(net_allow) = a.net_allow else {
        die(EXIT_ARGV, "one of --net-allow or --net-deny is required");
    };
    if !net_allow && !a.net_ports.is_empty() {
        die(EXIT_ARGV, "--net-port is meaningless with --net-deny");
    }
    if a.program_args.first().map(String::as_str) != Some("git") {
        die(EXIT_ARGV, "this launcher execs only `git`");
    }
}

// ---------------------------------------------------------------------------
// fd hygiene
// ---------------------------------------------------------------------------

/// Close every inherited descriptor above stderr before applying policy.
///
/// A descriptor opened by the parent is authority the sandbox cannot revoke:
/// Landlock mediates *opening* a path, never a file that is already open. So an
/// inherited fd to anything outside the grants would survive `restrict_self`
/// and survive the `execve`, and no filesystem rule would ever see it.
fn close_inherited_fds() {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        // Without procfs we cannot enumerate. Fall back to a bounded sweep
        // rather than silently doing nothing.
        for fd in 3..1024 {
            unsafe { libc::close(fd) };
        }
        return;
    };
    let mut fds: Vec<i32> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .collect();
    // Skip the descriptor the enumeration itself is holding; closing it
    // mid-iteration would be a use-after-close, and it is CLOEXEC anyway.
    fds.sort_unstable();
    for fd in fds {
        if fd > 2 {
            unsafe { libc::close(fd) };
        }
    }
}

// ---------------------------------------------------------------------------
// Landlock
// ---------------------------------------------------------------------------

fn abi_version() -> i32 {
    unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        ) as i32
    }
}

/// The rights that can actually be granted on one object.
///
/// `EXECUTE` is deliberately **kept** for a non-directory: the kernel accepts it
/// on a file (measured above), and a whole-tree grant of the same `--ro` path
/// already confers it on every file beneath. Stripping it only where the tree
/// happens to be enumerated would make one policy mean two different things
/// depending on whether a secret exclude lives underneath — the kind of
/// invisible divergence this file's header exists to forbid.
fn rights_for_target(declared: u64, is_dir: bool) -> u64 {
    if is_dir {
        declared
    } else {
        declared & ACCESS_FILE
    }
}

/// Why one `path_beneath` rule could not be added.
///
/// A named error rather than the `bool` this used to return, because exactly one
/// of these is benign and the old signature could not say which: every caller
/// discarded the `false` and carried on with a *weaker* ruleset that reported
/// success.
#[derive(Debug)]
enum AddRuleError {
    /// The path could not be opened, so there is nothing to grant. The one
    /// tolerated case: `DEFAULT_RO_TREES`/`NETWORK_ONLY_RO_TREES` name system
    /// paths that legitimately do not exist on every host (`/run/resolvconf` is
    /// absent on this one), and an enumerated tree can lose an entry to a race
    /// between `read_dir` and `open`.
    Unopenable(std::io::Error),
    /// The descriptor could not be stat'ed, so the mask cannot be computed.
    /// Never tolerated: guessing "not a directory" here would silently narrow a
    /// directory grant to file rights.
    Unstattable(std::io::Error),
    /// Every declared right was directory-only and the target is not a
    /// directory, so masking left nothing to add (the kernel answers `ENOMSG`
    /// for an empty rule). Never tolerated — the policy asked for something this
    /// object cannot carry.
    NoApplicableRight { declared: u64 },
    /// The kernel refused the rule. Never tolerated: this is the silent no-op
    /// that motivated all of the above.
    Refused {
        effective: u64,
        is_dir: bool,
        error: std::io::Error,
    },
}

impl AddRuleError {
    /// Is this "the path is not here", as opposed to "the grant failed"?
    fn is_absent(&self) -> bool {
        matches!(self, AddRuleError::Unopenable(_))
    }
}

impl std::fmt::Display for AddRuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddRuleError::Unopenable(e) => write!(f, "cannot open the path ({e})"),
            AddRuleError::Unstattable(e) => write!(f, "cannot fstat the opened path ({e})"),
            AddRuleError::NoApplicableRight { declared } => write!(
                f,
                "every declared right ({declared:#x}) is directory-only and this is not a \
                 directory, so the grant would be empty"
            ),
            AddRuleError::Refused {
                effective,
                is_dir,
                error,
            } => write!(
                f,
                "landlock_add_rule rejected access {effective:#x} (is_dir={is_dir}): {error}"
            ),
        }
    }
}

/// Add one `path_beneath` rule, masked to the rights the target can carry.
///
/// Returns the access actually granted, so a caller (and the tests below) can
/// assert what the kernel was handed rather than only that *something* was.
fn add_path_rule(ruleset: i32, path: &Path, declared: u64) -> Result<u64, AddRuleError> {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return Err(AddRuleError::Unopenable(std::io::Error::from(
            std::io::ErrorKind::InvalidInput,
        )));
    };
    // `O_PATH` so this never blocks and never needs read permission: a plain
    // open on a FIFO in an enumerated directory would hang the shim forever,
    // and with it every git operation the server runs.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(AddRuleError::Unopenable(std::io::Error::last_os_error()));
    }
    // Directory-ness is read from the very descriptor that will carry the rule,
    // not from a second `metadata()` call on the path: the mask has to describe
    // the object the kernel is about to be handed, and a path can be replaced
    // between the two lookups.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(AddRuleError::Unstattable(error));
    }
    let is_dir = (st.st_mode & libc::S_IFMT) == libc::S_IFDIR;
    let effective = rights_for_target(declared, is_dir);
    if effective == 0 {
        unsafe { libc::close(fd) };
        return Err(AddRuleError::NoApplicableRight { declared });
    }
    let attr = PathBeneathAttr {
        allowed_access: effective,
        parent_fd: fd,
    };
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const _,
            0usize,
        )
    };
    let error = std::io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(AddRuleError::Refused {
            effective,
            is_dir,
            error,
        });
    }
    Ok(effective)
}

/// Add one rule, or **refuse the launch**.
///
/// A rule the kernel rejected is the disease this function exists to prevent.
/// `add_path_rule` used to return a bare `bool` that every caller discarded, so
/// a `--ro`/`--rw` entry naming a regular file granted nothing and reported
/// success. That does not fail closed: it produces a *weaker* sandbox wearing
/// the costume of a configured one, which is strictly worse than refusing to
/// start, because the operator's policy file still reads as if the grant landed.
///
/// Measured before the fix, on this host: `--ro <dir>/f` followed by
/// `git config -f <dir>/f --list` returned `Permission denied` and exit 128 —
/// byte-identical to passing no grant at all, with no diagnostic anywhere.
///
/// An unopenable path is the one tolerated outcome, and it is tolerated because
/// it is not a grant failure at all (see `AddRuleError::Unopenable`).
fn grant_one(ruleset: i32, path: &Path, declared: u64) -> usize {
    match add_path_rule(ruleset, path, declared) {
        Ok(_) => 1,
        Err(e) if e.is_absent() => 0,
        Err(e) => die(
            EXIT_LANDLOCK,
            &format!(
                "cannot grant `{}`: {e}. Refusing to run: a grant that silently grants \
                 nothing makes a weaker sandbox look like a configured one",
                path.display()
            ),
        ),
    }
}

/// Why one `--ro-carveout` rule could not be added.
///
/// Deliberately not [`AddRuleError`]: two of these variants (`Unresolvable`,
/// `NotAFile`) have no analogue there, because a plain `--ro`/`--rw` entry is
/// never asked to reach inside an excluded directory and never needs to prove
/// it named a *file* rather than a directory. Conflating the two would either
/// weaken `--ro`/`--rw`'s existing tolerance or narrow it in a way nothing
/// asks for.
#[derive(Debug)]
enum CarveoutError {
    /// The path does not exist. Tolerated — the same posture `--ro`/`--rw`
    /// already have for a path that legitimately varies by host
    /// (`AddRuleError::Unopenable`): a fresh `$HOME` with no SSH connections
    /// yet has no `known_hosts`.
    Absent,
    /// The path exists but could not be fully canonicalised (a symlink loop,
    /// a component that is not searchable). **Never** tolerated, for the same
    /// reason `resolve_excludes` never tolerates it for an `--exclude`
    /// entry: this path lives inside a directory `--exclude` withholds, so a
    /// resolution this process cannot complete must not silently become "0
    /// granted, carry on" — that reads identically to an operator who simply
    /// forgot to grant it.
    Unresolvable(std::io::Error),
    /// The resolved path is not a regular file. **Never** tolerated: this
    /// flag's one safety property is "one named file, never a directory"
    /// (#188) — accepting anything else would make `--ro-carveout` a second,
    /// unreviewed way to grant whatever `--exclude` was just told to
    /// withhold.
    NotAFile { real: PathBuf },
    /// Resolved and confirmed a regular file, but it could not be stat'ed
    /// through the descriptor the rule would carry.
    Unstattable(std::io::Error),
    /// The kernel refused the rule.
    Refused(AddRuleError),
}

impl CarveoutError {
    fn is_absent(&self) -> bool {
        matches!(self, CarveoutError::Absent)
    }
}

impl std::fmt::Display for CarveoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CarveoutError::Absent => write!(f, "the path does not exist"),
            CarveoutError::Unresolvable(e) => write!(f, "cannot resolve the path ({e})"),
            CarveoutError::NotAFile { real } => write!(
                f,
                "`{}` is not a regular file — --ro-carveout may only name a single file, \
                 never a directory",
                real.display()
            ),
            CarveoutError::Unstattable(e) => write!(f, "cannot stat the resolved path ({e})"),
            CarveoutError::Refused(e) => write!(f, "{e}"),
        }
    }
}

/// Resolve, validate and grant one `--ro-carveout` target — the pure-ish half
/// (still does real syscalls, but returns rather than dying), so tests can
/// exercise every branch including the refusals without risking a `die()`
/// inside the test process.
///
/// # Why this bypasses `is_or_inside_exclude`/`is_ancestor_of_exclude` entirely
///
/// It does not call `grant_tree`, and it takes no `excludes` parameter at
/// all — bypassing the exclude check is this function's whole reason to
/// exist (#188: `~/.ssh` stays wholly excluded, but `~/.ssh/known_hosts`
/// must still be readable in the Network tier). The safety property that
/// makes this acceptable is enforced here instead, structurally: the target
/// must resolve to an existing regular file or the grant is refused
/// (`NotAFile`) — never a directory, which is what would turn this into a
/// second, unreviewed way to grant an entire excluded tree.
fn add_carveout_rule(ruleset: i32, path: &Path) -> Result<u64, CarveoutError> {
    let real = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(CarveoutError::Absent),
        Err(e) => return Err(CarveoutError::Unresolvable(e)),
    };
    match std::fs::metadata(&real) {
        Ok(md) if md.is_file() => {}
        Ok(_) => return Err(CarveoutError::NotAFile { real }),
        Err(e) => return Err(CarveoutError::Unstattable(e)),
    }
    add_path_rule(ruleset, &real, RO_DIR_ACCESS).map_err(CarveoutError::Refused)
}

/// Add one `--ro-carveout` rule, or **refuse the launch** — same posture as
/// [`grant_one`], for the same reason: a carve-out that silently grants
/// nothing while claiming to have granted something is a weaker sandbox
/// wearing the costume of a configured one. The one tolerated outcome is an
/// absent path, exactly as it is for `--ro`/`--rw`.
fn grant_carveout(ruleset: i32, path: &Path) -> usize {
    match add_carveout_rule(ruleset, path) {
        Ok(_) => 1,
        Err(e) if e.is_absent() => 0,
        Err(e) => die(
            EXIT_LANDLOCK,
            &format!("cannot grant --ro-carveout `{}`: {e}", path.display()),
        ),
    }
}

fn add_net_rule(ruleset: i32, port: u16) -> bool {
    let attr = NetPortAttr {
        allowed_access: NET_CONNECT_TCP,
        port: u64::from(port),
    };
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset,
            LANDLOCK_RULE_NET_PORT,
            &attr as *const _,
            0usize,
        )
    };
    rc == 0
}

/// Is `p` the exclude itself, or inside one? Those are withheld outright.
fn is_or_inside_exclude(p: &Path, excludes: &[PathBuf]) -> bool {
    excludes.iter().any(|e| p == e || p.starts_with(e))
}

/// Is `p` a proper *ancestor* of an exclude? Such a directory must be
/// **recursed into**, never granted whole — a Landlock grant is recursive, so
/// granting the ancestor hands over the excluded child.
///
/// Collapsing this case into `is_or_inside_exclude` is the regression this
/// project already shipped once: `.config` is an ancestor of the exclude
/// `.config/gh`, and treating that as "skip" drops `.config` entirely, making
/// `~/.config/git/ignore` unreadable and failing every `git commit` outright.
fn is_ancestor_of_exclude(p: &Path, excludes: &[PathBuf]) -> bool {
    excludes.iter().any(|e| e != p && e.starts_with(p))
}

/// Grant one tree, enumerating it only where an exclusion forces us to.
///
/// A tree with no exclusion beneath it is granted whole in a single rule; only
/// a tree that actually contains a secret pays the cost of enumeration. This is
/// what keeps the rule count proportional to the exclusions rather than to
/// `$HOME`.
///
/// **`tree` need not be a directory.** A policy entry naming a regular file is a
/// legitimate grant, and `grant_one` masks the rights accordingly rather than
/// handing the kernel a directory-only right it will reject — which is what this
/// function used to do, silently, for every `--ro`/`--rw` file entry. See
/// `ACCESS_FILE` for the measurement and `grant_one` for why the failure is now
/// terminal.
///
/// # `tree` is resolved here too, not only inside `enumerate`
///
/// `excludes` arrives already canonicalised (`apply_landlock` does it once, up
/// front). `is_or_inside_exclude`/`is_ancestor_of_exclude` are pure lexical
/// `Path` comparisons — no stat, no symlink following — so comparing them
/// against a still-unresolved `tree` would just move the same bug one call
/// frame up: `$HOME` (the production caller of this function, via
/// `policy_for`) is passed through verbatim from `std::env::var_os("HOME")`,
/// never canonicalised, and a symlinked `$HOME` would make this function's own
/// membership test fail exactly the way `enumerate`'s per-entry test used to.
/// Worse than a re-run of the original bug, in fact: a mismatch *here* skips
/// `enumerate` entirely and takes the non-enumerated fast path instead,
/// granting the excluded secret's ancestor **whole** with a single rule —
/// turning a symlink race into an unconditional bypass. So `tree` is resolved
/// with the identical `std::fs::canonicalize` before either comparison runs,
/// and the resolved path is what every downstream call (`grant_one`,
/// `enumerate`'s `dir`/`root`) receives, keeping tree and excludes in the same
/// namespace end to end.
///
/// An unresolvable `tree` is **not** fatal, unlike an unresolvable exclude:
/// `DEFAULT_RO_TREES`/`NETWORK_ONLY_RO_TREES` name system paths that
/// legitimately do not exist on every host, and a tree that is not there has
/// nothing to grant or to leak either way — `grant_one` would reach the same
/// "0 granted" outcome via `AddRuleError::Unopenable` a few lines later, this
/// just gets there without a spurious lexical comparison in between.
fn grant_tree(ruleset: i32, tree: &Path, access: u64, excludes: &[PathBuf]) -> usize {
    let Ok(real_tree) = std::fs::canonicalize(tree) else {
        return 0;
    };
    if is_or_inside_exclude(&real_tree, excludes) {
        return 0;
    }
    if !is_ancestor_of_exclude(&real_tree, excludes) {
        return grant_one(ruleset, &real_tree, access);
    }
    enumerate(ruleset, &real_tree, &real_tree, access, excludes)
}

/// The measured enumerate-and-skip walk. See `docs/adr/0027`.
fn enumerate(ruleset: i32, dir: &Path, root: &Path, access: u64, excludes: &[PathBuf]) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut granted = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        // Resolve first: every later test is about the object, not the name.
        let Ok(real) = std::fs::canonicalize(&path) else {
            continue; // dangling symlink, or a race — skip and carry on
        };
        if is_or_inside_exclude(&real, excludes) {
            continue;
        }
        // `metadata` follows symlinks, which is what we want for the access
        // mode; `symlink_metadata` is the one that tells us it *was* a link.
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        if md.is_dir() && is_ancestor_of_exclude(&real, excludes) {
            granted += enumerate(ruleset, &path, root, access, excludes);
            continue;
        }
        // Hard links: an inode with more than one name may be an alias of a
        // secret, and a hard link has no target to canonicalise. `st_nlink > 1`
        // is a *necessary* condition for such an alias, so skipping those has
        // no false negatives. Measured alternative — collecting every inode
        // under each exclude — is correct but costs a walk of ~11,000 entries
        // on this host, on a path whose whole budget is ~10ms.
        if md.is_file() && std::os::unix::fs::MetadataExt::nlink(&md) > 1 {
            continue;
        }
        if let Ok(link_md) = std::fs::symlink_metadata(&path) {
            if link_md.file_type().is_symlink() {
                // For a symlink, an ancestor-of-exclude target is disqualifying
                // rather than a reason to recurse: the alias would *grant* the
                // ancestor, not descend into it.
                if !real.starts_with(root)
                    || real == Path::new("/")
                    || real == Path::new("/home")
                    || real == root
                    || is_ancestor_of_exclude(&real, excludes)
                {
                    continue;
                }
            }
        }
        // The right-masking that used to live here (`access & !(A_READ_DIR |
        // A_EXECUTE)`) is now `add_path_rule`'s job, computed from an `fstat` of
        // the descriptor the rule will carry. Two reasons it moved: the old mask
        // was *wrong for a read-write tree* (it left `MAKE_*`/`REMOVE_*` set, so
        // the kernel rejected every regular file in an enumerated `--rw` tree —
        // see `ACCESS_FILE`), and having one mask here and none on the
        // non-enumerated path is exactly how the two paths came to disagree.
        granted += grant_one(ruleset, &path, access);
    }
    granted
}

/// Resolve every `--exclude` entry to the object it actually names, before any
/// grant is built.
///
/// # Why this has to live here, in the shim, and not in the server
///
/// `secret_excludes_for_home` (server side, `sandbox/mod.rs`) builds the
/// exclude list as plain string concatenation — `home.join(".ssh")` and
/// friends — over whatever `$HOME` the server process inherited, and never
/// canonicalises it. `enumerate`, below, resolves every entry it walks with
/// `std::fs::canonicalize` before testing it against that list. If any
/// component of `$HOME` (or of `XDG_STATE_HOME`, for the trust-store exclude)
/// is a symlink, those two paths live in different string namespaces:
/// `enumerate`'s `real` is the resolved object, the exclude is still the
/// symlinked name, and `is_or_inside_exclude`'s `==`/`starts_with` — pure
/// lexical comparison, no stat, no symlink awareness of its own — silently
/// never matches. `$HOME` is granted read-only, so the practical effect of
/// that silent miss is `~/.ssh`, `~/.gnupg`, `~/.git-credentials`, the trust
/// store, and everything else in `DEFAULT_SECRET_EXCLUDES` becoming readable
/// through the symlinked name — the one thing this whole file exists to
/// prevent. Measured directly against this binary, 2026-07-30 (see the test
/// module): a granted tree with a symlinked component and an unresolved
/// exclude beneath it leaked a canary file in full.
///
/// The tempting fix is to canonicalise the excludes on the **server** side,
/// before they ever reach this process's argv. That was considered and
/// rejected: it still leaves a cross-process window — the server resolves the
/// symlink at policy-build time, this shim resolves the *walked entry* at
/// enumeration time, and nothing pins those two resolutions to the same
/// instant. A symlink component swapped in between (even a static
/// misconfiguration that just happens to differ from what the server saw, let
/// alone an adversarial retarget) reopens the identical mismatch one process
/// hop later. Resolving **here**, inside `apply_landlock`, immediately before
/// the first `grant_tree` call and before any attacker-influenced code has had
/// a chance to run, means the exclude list and every walked path are resolved
/// in the *same* process, by the *same* `std::fs::canonicalize` call, close
/// enough in time that there is no cross-process gap left to race. It does not
/// close every theoretical window — a concurrent process with write access to
/// a symlink component could still retarget it in the microseconds between
/// this resolution and a later `open()` — but that residual is a TOCTOU no
/// single-process design can close without atomic path resolution the kernel
/// does not offer, and it is a different, far narrower claim than "any static
/// symlink under `$HOME` defeats every exclude," which is what this closes
/// unconditionally.
///
/// # Fatal, not a skip — with exactly one tolerated exception
///
/// An exclude that cannot be resolved must never quietly become "does not
/// match anything" — that is the exact fail-open shape `enumerate`'s own
/// `std::fs::read_dir` failure already has to guard against elsewhere in this
/// file (an unreadable directory returns 0 granted rather than refusing), and
/// letting it happen to the *secret list itself* would be worse: the operator
/// asked for `~/.ssh` withheld, and a policy that cannot prove it withheld it
/// must not proceed as though it had.
///
/// The one exception is `NotFound`, and it is the same exception `grant_one`
/// already carries for `--ro`/`--rw` entries (`AddRuleError::Unopenable`,
/// "the one tolerated case"). `DEFAULT_SECRET_EXCLUDES` names fifteen
/// candidate paths — `.aws`, `.docker`, `.kube`, `.gnupg`, `.mozilla` and more
/// — and no real host has all of them: measured on this one, eight of fifteen
/// are simply absent. An absent exclude is not a resolution *failure*, it is a
/// fact ("this secret does not exist here"), and it is safe to drop: nothing
/// `enumerate` walks can ever canonicalise to a target that is not on disk, so
/// a dropped absent exclude matches exactly as much — nothing — resolved as
/// it would unresolved. Treating `NotFound` as fatal was tried first and
/// measured wrong immediately: it turned "this host doesn't happen to have a
/// `~/.kube`" into "refuse to run git at all," failing closed against a
/// condition that was never a security question to begin with. Every *other*
/// error — permission denied resolving an ancestor component, a symlink loop,
/// a component that is not a directory where one was expected — means this
/// process cannot prove what the exclude names, which is the real fail-open
/// risk, and stays fatal.
fn resolve_excludes(raw: &[PathBuf]) -> Vec<PathBuf> {
    raw.iter()
        .filter_map(|e| match std::fs::canonicalize(e) {
            Ok(p) => Some(p),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => die(
                EXIT_LANDLOCK,
                &format!(
                    "cannot resolve --exclude `{}`: {err}. An exclude that cannot be \
                     canonicalised for a reason other than not existing must not silently \
                     match nothing — refusing to build a ruleset that would grant it by \
                     accident.",
                    e.display()
                ),
            ),
        })
        .collect()
}

fn apply_landlock(a: &Args) {
    let floor = a.abi_floor.expect("validated");
    let abi = abi_version();
    if abi < 0 {
        die(EXIT_ABI_FLOOR, "Landlock ABI unavailable on this kernel");
    }
    if (abi as u32) < floor {
        die(
            EXIT_ABI_FLOOR,
            &format!(
                "Landlock ABI {abi} is below the declared floor {floor}; refusing to run a weaker policy"
            ),
        );
    }

    let net_allow = a.net_allow.expect("validated");
    let attr = RulesetAttr {
        handled_access_fs: HANDLED_FS,
        // TCP is declared handled in both tiers. With `--net-deny` no port rule
        // is ever added, so every connect is denied; the strict tier layers
        // that on top of its own network namespace, which is measured harmless.
        handled_access_net: NET_CONNECT_TCP | NET_BIND_TCP,
        scoped: LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL,
    };
    let ruleset = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const _,
            std::mem::size_of::<RulesetAttr>(),
            0usize,
        )
    } as i32;
    if ruleset < 0 {
        die(
            EXIT_LANDLOCK,
            &format!(
                "landlock_create_ruleset failed: {}",
                std::io::Error::last_os_error()
            ),
        );
    }

    // Resolved once, here, and used for every membership test from this point
    // on — never `a.excludes` again. See `resolve_excludes`'s doc comment for
    // why this has to happen in this process, at this point, rather than on
    // the server side that built the raw list.
    let excludes = resolve_excludes(&a.excludes);

    for tree in &a.ro {
        grant_tree(ruleset, tree, RO_DIR_ACCESS, &excludes);
    }
    // Read-write trees are granted after the read-only ones so a repository
    // nested under a read-only ancestor still ends up writable. Rule *order*
    // does not matter to the kernel — measured — but reading them in this
    // order matches how the policy is described.
    for tree in &a.rw {
        grant_tree(ruleset, tree, RW_ACCESS, &excludes);
    }
    // #188: named, single-file exceptions to an `--exclude` above. Granted
    // last among the filesystem rules and never consulted against
    // `excludes` — see `add_carveout_rule` for why that bypass is the
    // point, not an oversight.
    for path in &a.ro_carveouts {
        grant_carveout(ruleset, path);
    }
    if net_allow {
        for port in &a.net_ports {
            if !add_net_rule(ruleset, *port) {
                die(
                    EXIT_LANDLOCK,
                    &format!(
                        "landlock_add_rule for port {port} failed: {}",
                        std::io::Error::last_os_error()
                    ),
                );
            }
        }
    }

    // `no_new_privs` is a precondition for restricting unprivileged, and it is
    // also what makes the domain irrevocable across the exec that follows.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        die(EXIT_LANDLOCK, "prctl(PR_SET_NO_NEW_PRIVS) failed");
    }
    if unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset, 0usize) } != 0 {
        die(
            EXIT_LANDLOCK,
            &format!(
                "landlock_restrict_self failed: {}",
                std::io::Error::last_os_error()
            ),
        );
    }
    unsafe { libc::close(ruleset) };
}

// A directory binary (`src/bin/gv-sandbox/main.rs`) rather than a single file,
// so this module can be a plain sibling. A `.rs` file directly under `src/bin/`
// is auto-discovered by Cargo as its *own* binary target and would be required
// to have a `main` of its own.
mod seccomp_filter;

/// Install the terminal denylist. Applied **after** Landlock and immediately
/// before the exec, so the ordering matches the layering: the filesystem
/// boundary is established first, then the syscall boundary, then the process
/// image is replaced — and both survive the `execve` because
/// `PR_SET_NO_NEW_PRIVS` is already set.
///
/// `net` is the tier, derived in `main` from the `--net-deny`/`--net-allow` flag
/// the launcher already emits. One rule varies with it (AF_UNIX socket creation,
/// denied in Strict only); everything else is identical in both tiers. See
/// `seccomp_filter::af_unix_rule`.
fn apply_seccomp(net: seccomp_filter::NetScope) {
    let program = match seccomp_filter::build(net) {
        Ok(p) => p,
        Err(e) => die(EXIT_SECCOMP, &format!("seccomp filter build failed: {e}")),
    };
    if let Err(e) = seccompiler::apply_filter(&program) {
        die(EXIT_SECCOMP, &format!("seccomp apply failed: {e}"));
    }
}

fn main() {
    let a = parse();
    validate(&a);
    close_inherited_fds();
    apply_landlock(&a);
    // `validate` has already refused an argv carrying neither net flag, so the
    // catch-all arm is unreachable — and it resolves to the *stronger* filter, so
    // if that ever stops being true the failure is a compatibility complaint and
    // not a silently weaker sandbox.
    apply_seccomp(match a.net_allow {
        Some(true) => seccomp_filter::NetScope::Allowed,
        _ => seccomp_filter::NetScope::Denied,
    });

    // `git` is named literally so the argv tripwire can prove this process
    // cannot exec anything else, and `validate` has already refused any
    // program_args that did not begin with exactly `git`.
    let mut cmd = Command::new("git");
    if let Some(dir) = &a.hooks_blocked_dir {
        // The same suppression the unsandboxed tier expresses, applied here so
        // a blocked-hooks policy means blocked hooks in every tier.
        let mut setting = std::ffi::OsString::from("core.hooksPath=");
        setting.push(dir);
        cmd.arg("-c").arg(setting);
    }
    cmd.args(&a.program_args[1..]);
    let err = cmd.exec();
    die(EXIT_EXEC, &format!("exec git failed: {err}"));
}

/// Landlock grant tests. They build a **real** ruleset and call the real
/// `landlock_add_rule`, then throw the ruleset away without
/// `landlock_restrict_self` — so nothing here restricts the test process, and
/// nothing here is a re-implementation of the primitive under test. A test that
/// asserted against a hand-rolled model of what the kernel accepts is precisely
/// what let the file-grant no-op live: the header on this host declares a
/// two-field `landlock_ruleset_attr` and no scopes, and reasoning from it is how
/// four rounds of this design died.
#[cfg(test)]
mod tests {
    use super::*;

    /// A ruleset that handles everything this shim handles in production. Never
    /// restricted onto the test process — it is a bag of rules the kernel
    /// validates, which is the only thing these tests need from it.
    ///
    /// Failure here is a hard failure, never a skip: the CI preflight
    /// (`escape_contract::ci_preflight_host_meets_the_declared_minimum`) already
    /// asserts this host meets the Landlock floor, so a ruleset that will not
    /// create means the measurement below cannot be made — and a green test that
    /// proved nothing is worse than a red one.
    fn handled_ruleset() -> i32 {
        let abi = abi_version();
        assert!(
            abi >= 6,
            "Landlock ABI {abi} cannot demonstrate this test's premise; the CI preflight \
             asserts the floor, so this is a host defect, not a reason to skip"
        );
        let attr = RulesetAttr {
            handled_access_fs: HANDLED_FS,
            handled_access_net: NET_CONNECT_TCP | NET_BIND_TCP,
            scoped: LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL,
        };
        let rs = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const _,
                std::mem::size_of::<RulesetAttr>(),
                0usize,
            )
        } as i32;
        assert!(
            rs >= 0,
            "landlock_create_ruleset failed: {}",
            std::io::Error::last_os_error()
        );
        rs
    }

    /// `landlock_add_rule` with no masking at all — the pre-fix code path,
    /// preserved here as the *premise* of every assertion below. Returns the
    /// raw errno.
    fn add_rule_unmasked(ruleset: i32, path: &Path, access: u64) -> i32 {
        let c = CString::new(path.as_os_str().as_bytes()).expect("no interior NUL");
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        assert!(fd >= 0, "O_PATH open of {} failed", path.display());
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: fd,
        };
        let rc = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset,
                LANDLOCK_RULE_PATH_BENEATH,
                &attr as *const _,
                0usize,
            )
        };
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
        unsafe { libc::close(fd) };
        if rc == 0 {
            0
        } else {
            errno
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().expect("tempdir");
        let f = d.path().join("granted.txt");
        std::fs::write(&f, "granted\n").expect("write fixture file");
        (d, f)
    }

    /// The premise, measured rather than assumed: the kernel rejects a
    /// directory-only right on a regular file, and it rejects it for the whole
    /// rule — there is no partial grant. If this ever starts passing, the mask
    /// in `rights_for_target` has stopped being load-bearing and every
    /// assertion below is vacuous, so this test is what keeps them honest.
    #[test]
    fn the_kernel_rejects_directory_only_rights_on_a_regular_file() {
        let rs = handled_ruleset();
        let (_d, file) = fixture();

        assert_eq!(
            add_rule_unmasked(rs, &file, RO_DIR_ACCESS),
            libc::EINVAL,
            "READ_DIR on a regular file must be EINVAL — this is the rejection the \
             pre-fix fast path mapped to `false` and then discarded"
        );
        assert_eq!(
            add_rule_unmasked(rs, &file, RW_ACCESS),
            libc::EINVAL,
            "a read-write file grant must be EINVAL unmasked"
        );
        // The mask `enumerate` used to carry, on a read-write tree. Still
        // rejected: MAKE_*/REMOVE_* are directory-only too, so the enumerated
        // path was silently granting nothing for regular files under `--rw`.
        assert_eq!(
            add_rule_unmasked(rs, &file, RW_ACCESS & !(A_READ_DIR | A_EXECUTE)),
            libc::EINVAL,
            "the old `& !(READ_DIR|EXECUTE)` mask must still be rejected for a \
             read-write tree; if not, this test's reason to exist has changed"
        );
        assert_eq!(
            add_rule_unmasked(rs, &file, 0),
            libc::ENOMSG,
            "an empty access mask is not a rule"
        );
        unsafe { libc::close(rs) };
    }

    /// The fix: a policy entry naming a regular file is granted, at exactly the
    /// rights a file can carry. This is the test that would have caught the
    /// no-op — pre-fix, `add_path_rule` returned `false` here and `grant_tree`
    /// answered `0 granted` with no error.
    #[test]
    fn a_grant_naming_a_regular_file_is_honoured_at_file_rights() {
        let rs = handled_ruleset();
        let (_d, file) = fixture();

        assert_eq!(
            add_path_rule(rs, &file, RO_DIR_ACCESS).expect("a read-only file grant must land"),
            A_EXECUTE | A_READ_FILE,
            "a read-only file grant keeps exactly the file-applicable rights"
        );
        assert_eq!(
            add_path_rule(rs, &file, RW_ACCESS).expect("a read-write file grant must land"),
            A_EXECUTE | A_WRITE_FILE | A_READ_FILE | A_TRUNCATE,
            "a read-write file grant keeps exactly the file-applicable rights"
        );
        unsafe { libc::close(rs) };
    }

    /// A directory grant is untouched by the mask — the masking must not have
    /// quietly narrowed the case that already worked.
    #[test]
    fn a_directory_grant_is_still_granted_whole() {
        let rs = handled_ruleset();
        let (dir, _file) = fixture();

        assert_eq!(
            add_path_rule(rs, dir.path(), RO_DIR_ACCESS).expect("a read-only tree grant lands"),
            RO_DIR_ACCESS,
        );
        assert_eq!(
            add_path_rule(rs, dir.path(), RW_ACCESS).expect("a read-write tree grant lands"),
            RW_ACCESS,
        );
        assert_eq!(rights_for_target(RW_ACCESS, true), RW_ACCESS);
        unsafe { libc::close(rs) };
    }

    /// #188's load-bearing property: a carve-out must grant its file even
    /// though **nothing** granted its parent directory — not merely "the
    /// parent was granted but had an exclude nested under it" (that is
    /// `grant_tree`'s enumerate-and-skip case, unrelated), but "the parent
    /// carries no rule of any kind." This is what a real `--exclude .ssh`
    /// looks like from `add_carveout_rule`'s point of view: it never
    /// consults `excludes` at all, so an excluded parent and an ungranted
    /// parent are indistinguishable to it, which is exactly the bypass #188
    /// needs.
    #[test]
    fn a_carveout_grants_a_file_with_no_grant_on_its_parent_at_all() {
        let rs = handled_ruleset();
        let (_d, file) = fixture();

        assert_eq!(
            add_carveout_rule(rs, &file).expect("a carve-out on a bare file must be granted"),
            A_EXECUTE | A_READ_FILE,
            "a carve-out is read-only and file-masked, identical to an ordinary \
             --ro grant on a regular file"
        );
        unsafe { libc::close(rs) };
    }

    /// The enforced safety property behind "read-only, that single file, not
    /// the directory" (#188's own words): pointing `--ro-carveout` at a
    /// directory must be refused, never silently granted. Without this check
    /// the flag would be a second, unreviewed way to re-grant an entire
    /// excluded tree — `--ro-carveout ~/.ssh` would defeat `--exclude ~/.ssh`
    /// outright.
    #[test]
    fn a_carveout_refuses_a_directory() {
        let rs = handled_ruleset();
        let (dir, _file) = fixture();

        let err = add_carveout_rule(rs, dir.path())
            .expect_err("a carve-out naming a directory must be refused");
        assert!(
            matches!(err, CarveoutError::NotAFile { .. }),
            "wrong refusal reason for a directory target: {err:?}"
        );
        assert!(
            !err.is_absent(),
            "a directory that exists must never be reported as merely absent: {err}"
        );
        unsafe { libc::close(rs) };
    }

    /// The one tolerated outcome, matching `--ro`/`--rw`'s existing posture
    /// for a path that legitimately varies by host: a fresh `$HOME` with no
    /// SSH connections yet has no `known_hosts`, and that must not refuse the
    /// launch.
    #[test]
    fn a_carveout_tolerates_an_absent_path() {
        let rs = handled_ruleset();
        let (d, _file) = fixture();

        let err = add_carveout_rule(rs, &d.path().join("no-such-file"))
            .expect_err("an absent carve-out target cannot be granted");
        assert!(
            err.is_absent(),
            "an absent path must report as absent, not as some other refusal: {err}"
        );
        unsafe { libc::close(rs) };
    }

    /// The two failure shapes must stay distinguishable, because exactly one of
    /// them is survivable: an absent path is a host that does not have
    /// `/run/resolvconf`, while a rejected rule is a policy the shim cannot
    /// honour and must refuse (`grant_one`).
    #[test]
    fn an_absent_path_is_distinguishable_from_a_refused_rule() {
        let rs = handled_ruleset();
        let (d, file) = fixture();

        let absent = add_path_rule(rs, &d.path().join("no-such-entry"), RO_DIR_ACCESS)
            .expect_err("an absent path cannot be granted");
        assert!(
            absent.is_absent(),
            "an absent path must report as absent, not as a refused grant: {absent}"
        );

        // A dir-only right set on a regular file masks to nothing. Not merely
        // "no rule added" — the policy asked for something the object cannot
        // carry, and `grant_one` must be able to tell that apart from absence.
        let empty = add_path_rule(rs, &file, A_READ_DIR | A_MAKE_DIR)
            .expect_err("a directory-only grant on a file has nothing to add");
        assert!(
            !empty.is_absent(),
            "a grant masked to nothing must NOT read as absence: {empty}"
        );
        assert_eq!(rights_for_target(A_READ_DIR | A_MAKE_DIR, false), 0);
        unsafe { libc::close(rs) };
    }
}
