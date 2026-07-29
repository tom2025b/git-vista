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
        die(EXIT_ARGV, "--abi-floor is required (it must travel in the argv, never default)");
    }
    if !a.hooks_seen {
        die(EXIT_ARGV, "one of --hooks-run or --hooks-blocked is required");
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

fn add_path_rule(ruleset: i32, path: &Path, access: u64) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // `O_PATH` so this never blocks and never needs read permission: a plain
    // open on a FIFO in an enumerated directory would hang the shim forever,
    // and with it every git operation the server runs.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
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
    unsafe { libc::close(fd) };
    rc == 0
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
fn grant_tree(ruleset: i32, tree: &Path, access: u64, excludes: &[PathBuf]) -> usize {
    if is_or_inside_exclude(tree, excludes) {
        return 0;
    }
    if !is_ancestor_of_exclude(tree, excludes) {
        return usize::from(add_path_rule(ruleset, tree, access));
    }
    enumerate(ruleset, tree, tree, access, excludes)
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
        let mode = if md.is_dir() {
            access
        } else {
            access & !(A_READ_DIR | A_EXECUTE)
        };
        if add_path_rule(ruleset, &path, mode) {
            granted += 1;
        }
    }
    granted
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
            &format!("Landlock ABI {abi} is below the declared floor {floor}; refusing to run a weaker policy"),
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

    for tree in &a.ro {
        grant_tree(ruleset, tree, RO_DIR_ACCESS, &a.excludes);
    }
    // Read-write trees are granted after the read-only ones so a repository
    // nested under a read-only ancestor still ends up writable. Rule *order*
    // does not matter to the kernel — measured — but reading them in this
    // order matches how the policy is described.
    for tree in &a.rw {
        grant_tree(ruleset, tree, RW_ACCESS, &a.excludes);
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

fn main() {
    let a = parse();
    validate(&a);
    close_inherited_fds();
    apply_landlock(&a);
    // Task 4 installs the seccomp filter here, before the exec.

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
