//! M1.13b (#66) Task 9, part 1: the **factual** capability probe.
//!
//! This module answers one question and refuses the next one: *what can this
//! host actually provide?* — the Landlock ABI it reports, whether `bwrap` is
//! present, whether unprivileged user namespaces are usable. It does **not**
//! decide what to do when a capability is missing (which tier to fall back to,
//! whether to block hooks, whether to refuse). That degradation policy is a
//! security judgement (INV-13), it is deliberately not encoded here, and mixing
//! the measured fact with the policy is exactly how a "best-effort downgrade"
//! silently weakens a sandbox — the failure C5 forbids.
//!
//! Everything here is a direct measurement. No value is inferred: the Landlock
//! ABI comes from the kernel, not from `/usr/include` (which is stale on the
//! development host — it declares a two-field ruleset attr while the kernel is
//! ABI 8).

use std::path::Path;

use super::{bwrap, LANDLOCK_ABI_FLOOR};

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;

/// What the host can provide, measured at startup. Every field is a fact, not a
/// decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Capabilities {
    /// The Landlock ABI the running kernel reports, or `-1` if the syscall is
    /// unavailable (no Landlock at all). This is the number the shim's
    /// `--abi-floor` check compares against.
    pub landlock_abi: i32,
    /// Whether a `bwrap` launcher was found at an absolute reviewed path.
    /// Required for the strict tier's namespaces; irrelevant to the network
    /// tier, which launches the shim directly.
    pub bwrap_present: bool,
    /// Whether unprivileged user namespaces appear usable. Required for bwrap to
    /// create the strict tier's namespaces without privilege.
    pub userns: bool,
    /// Whether the kernel exposes seccomp filtering at all — read from the
    /// `/proc/sys/kernel/seccomp/actions_avail` knob, matching this struct's
    /// "read a kernel fact, never attempt the primitive" discipline rather than
    /// installing a filter just to see if it succeeds. Added for Task 9's boot
    /// probe (`sandbox::probe`), which needs to name this specific capability
    /// when the composed launcher's baseline leg fails to run at all.
    pub seccomp_available: bool,
}

impl Capabilities {
    /// Does the host clear the declared Landlock floor at all? Below this, no
    /// tier that relies on Landlock can run — the shim itself would exit 91.
    pub fn landlock_meets_floor(&self) -> bool {
        self.landlock_abi >= LANDLOCK_ABI_FLOOR as i32
    }

    /// Can the host provide the **strict** tier? It needs Landlock at the floor,
    /// a bwrap launcher, and usable user namespaces — all three, because the
    /// strict tier's isolation is the *composition* of Landlock, seccomp and
    /// bwrap's namespaces, and a missing piece is not a weaker strict tier, it
    /// is a different (and undeclared) one.
    pub fn strict_available(&self) -> bool {
        self.landlock_meets_floor() && self.bwrap_present && self.userns
    }

    /// Can the host provide the **network** tier? Landlock at the floor is
    /// enough — the network tier has no namespaces by design (F3).
    pub fn network_available(&self) -> bool {
        self.landlock_meets_floor()
    }
}

/// Measure the host's capabilities. Pure of side effects beyond reading kernel
/// state; safe to call at startup and cheap enough to call more than once.
pub(crate) fn probe() -> Capabilities {
    Capabilities {
        landlock_abi: landlock_abi(),
        bwrap_present: bwrap::bwrap_path().is_some(),
        userns: userns_usable(),
        seccomp_available: Path::new("/proc/sys/kernel/seccomp/actions_avail").exists(),
    }
}

/// Ask the kernel directly for the Landlock ABI. `landlock_create_ruleset(NULL,
/// 0, VERSION)` returns the supported ABI version, or `-1`/`ENOSYS` where
/// Landlock is absent.
fn landlock_abi() -> i32 {
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    // A negative return means unavailable; clamp to -1 so callers have one
    // "no Landlock" value to test rather than an assortment of errnos.
    if rc < 0 {
        -1
    } else {
        rc as i32
    }
}

/// Whether unprivileged user namespaces look usable, read from the kernel knobs
/// that gate them. This is a *necessary* signal, not a guarantee that a specific
/// `bwrap` invocation will succeed — the definitive check is the startup escape
/// probe (INV-14), which actually launches the strict tier. This cheaper read
/// lets the probe report "strict unavailable" without a launch on the common
/// hosts that disable userns outright.
fn userns_usable() -> bool {
    // Debian/Ubuntu gate: `kernel.unprivileged_userns_clone` = 0 disables it.
    if let Some(v) = read_sysctl_int("/proc/sys/kernel/unprivileged_userns_clone") {
        if v == 0 {
            return false;
        }
    }
    // The upstream limit: 0 max namespaces means none can be created.
    if let Some(v) = read_sysctl_int("/proc/sys/user/max_user_namespaces") {
        if v == 0 {
            return false;
        }
    }
    // AppArmor's newer restriction (Ubuntu 24.04+): non-zero disables
    // unprivileged userns for unconfined profiles. Task 17's CI preflight is
    // what sets this to 0 on runners; here we only *observe* it.
    if let Some(v) = read_sysctl_int("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") {
        if v != 0 {
            return false;
        }
    }
    // None of the disabling knobs were set (or none exist): treat as usable.
    // A host where it is nonetheless blocked is caught by the launch probe.
    true
}

fn read_sysctl_int(path: &str) -> Option<i64> {
    std::fs::read_to_string(Path::new(path))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe runs and reports a coherent picture on the development host,
    /// which is known to have Landlock ABI 8, bwrap, and userns. This is a
    /// smoke test of the measurement, paired below with logic tests that do not
    /// depend on the host.
    #[test]
    fn the_probe_reports_this_hosts_real_capabilities() {
        let caps = probe();
        assert!(
            caps.landlock_abi >= 1,
            "this host is known to support Landlock; got abi={}",
            caps.landlock_abi
        );
        // seccomp has shipped in every kernel this project targets (since
        // Linux 3.17); unlike bwrap/userns it is not something a container or
        // CI runner plausibly lacks, so this one is asserted absolutely.
        assert!(
            caps.seccomp_available,
            "this host is known to expose /proc/sys/kernel/seccomp/actions_avail"
        );
        // Don't assert bwrap/userns absolutely — CI or a container may lack
        // them. Assert only the relationship the logic depends on.
        assert_eq!(
            caps.strict_available(),
            caps.landlock_meets_floor() && caps.bwrap_present && caps.userns
        );
    }

    /// The availability logic is host-independent and is where a mistake would
    /// silently permit a tier the host cannot actually provide, so it is tested
    /// against constructed capability sets, not the live host.
    #[test]
    fn strict_needs_all_three_and_network_needs_only_landlock() {
        let full = Capabilities {
            landlock_abi: 8,
            bwrap_present: true,
            userns: true,
            seccomp_available: true,
        };
        assert!(full.strict_available());
        assert!(full.network_available());

        let no_bwrap = Capabilities {
            bwrap_present: false,
            ..full
        };
        assert!(!no_bwrap.strict_available(), "strict needs bwrap");
        assert!(no_bwrap.network_available(), "network does not need bwrap");

        let no_userns = Capabilities {
            userns: false,
            ..full
        };
        assert!(!no_userns.strict_available(), "strict needs userns");
        assert!(
            no_userns.network_available(),
            "network does not need userns"
        );

        let below_floor = Capabilities {
            landlock_abi: LANDLOCK_ABI_FLOOR as i32 - 1,
            ..full
        };
        assert!(
            !below_floor.strict_available(),
            "strict needs the ABI floor"
        );
        assert!(
            !below_floor.network_available(),
            "network also needs the ABI floor"
        );

        let no_landlock = Capabilities {
            landlock_abi: -1,
            ..full
        };
        assert!(
            !no_landlock.network_available(),
            "no Landlock, no sandboxed tier"
        );
    }

    /// The floor is the declared minimum, not the host's actual ABI: a host at
    /// exactly the floor is sufficient, one below is not. Pins the boundary so a
    /// `>` vs `>=` slip is caught.
    #[test]
    fn exactly_the_floor_is_sufficient() {
        let at = Capabilities {
            landlock_abi: LANDLOCK_ABI_FLOOR as i32,
            bwrap_present: true,
            userns: true,
            seccomp_available: true,
        };
        assert!(at.landlock_meets_floor());
        assert!(at.strict_available());
    }
}
