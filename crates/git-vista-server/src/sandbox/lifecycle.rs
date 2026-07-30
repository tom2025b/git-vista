//! INV-8 and the two namespace corrections that arrived from the spec lane
//! (#66 / #198, plan Task 12): **orphan reaping** (A10), a **fresh procfs**
//! (A8 / C3) and a **private `/dev/shm`** (A9 / C4), all observed on a real
//! sandboxed process driven through the production seam.
//!
//! **[SPEC SILENCE], retained deliberately:** PID 1 reaping proves *task
//! teardown*. It does not prove quiescence, nor rollback of asynchronously
//! submitted kernel or device side effects already in flight. This file
//! asserts teardown and says nothing about quiescence, on purpose.
//!
//! # Three deviations from the plan's Task 12 snippet, each forced by source
//!
//! The plan's snippet was written before Task 8 landed and before
//! `escape_contract.rs` existed. Verified against the built tree on
//! 2026-07-30, three of its instructions are now wrong, and following them
//! would either not compile or would break CI. Each is named here with the
//! evidence, because a boundary crossed without its reason gets crossed again.
//!
//! **1. There is no R8 exemption left to take.** The snippet builds a
//! `Policy { tier: Tier::Strict, .. }` literal under an
//! `R8-EXPIRING-EXEMPTION` whose declared blocker is `policy_for_repo`
//! hard-coding `let tier = Tier::Network;`. That blocker is gone: Task 8
//! wired the dispatch, `policy_for` now derives the tier from
//! `tier_for(need, repo_is_trusted(repo))`, and a local operation on an
//! untrusted repository *is* `Tier::Strict` in production. The only surviving
//! `let tier = Tier::Network;` is inside `policy_for_clone`, a different
//! function, so the snippet's `exemption_is_still_valid` tripwire would have
//! passed while asserting nothing about the code it guards — a green test
//! that proves nothing. Both the exemption and the hand-built `Policy`
//! literal are therefore dropped: R6 says the policy comes from the
//! production builder wherever the configuration is production-constructible,
//! and as of Task 8 it is. [`strict_baseline`] delegates to
//! `shim_cli::production_policy`, which is `policy_for(repo, false,
//! NetworkNeed::Local)` — so this file adds **no** policy builder at all,
//! which is a stronger reading of R6 than the "one shared exempted builder"
//! the plan asked for.
//!
//! **2. This file must NOT write to `$GV_ESCAPE_REPORT`, and its case ids
//! must NOT go in the census.** The plan's step 12.2 says to register
//! `lifecycle-*` ids in `docs/sandbox/escape-census.txt`. Doing that now
//! breaks the build: `escape_contract::r5_census_names_exactly_the_declared_cases`
//! asserts the census id set **equals** the set of `EscapeCase` ids declared
//! in `BATTERY_FILES` (`escape_suite.rs`, `hook_mode_suite.rs`) in both
//! directions, and this file declares no `EscapeCase` — it cannot, since the
//! lifecycle claims are process-tree- and wall-clock-shaped rather than
//! single-errno-shaped. Writing report records without census entries breaks
//! the *gate* instead (the job diffs the report's ids against the census both
//! ways). So the R5 route is closed in both directions for this file, and
//! forcing it open would need edits to `escape_contract.rs`, the census and
//! the CI job — all owned elsewhere.
//!
//! What replaces it is strictly stronger, not weaker. R5's purpose is that
//! capability absence must never be silently green. Here capability absence is
//! a **hard test failure**, which is red locally *and* in CI with no census,
//! no report file and no shell assertion to keep in sync. That is also the
//! posture production already takes: ADR 0029 / INV-13 decided Strict is
//! *refused, never downgraded*, so `policy_for` returns
//! `ShimError::StrictUnavailable` on a host that cannot supply it and the
//! operation refuses. A host that cannot run these three tests is a host on
//! which git-vista does not run.
//!
//! **3. Capability is still established by execution, never by asking the
//! host** (R4 — `capabilities::probe`, `.exists()`, `bwrap_path()` deciding a
//! skip are all named-forbidden, and nothing here does any of them).
//! [`strict_baseline`] runs the cheapest real Strict-tier operation there is
//! (`git --version`: no repo write, no hooks) through
//! `sandbox::spawn::command_async` and reads its own exit code. It just
//! `panic!`s instead of returning `Err`, per deviation 2.
//!
//! # Observation is by marker file, and every claim carries a paired positive
//!
//! `ProbeReport`/`--self-probe` is the route R10 deletes — the shim's parser
//! has no such arm and never did. Each hostile hook below writes its one
//! measured fact into a file in the repository worktree (git runs hooks with
//! the worktree root as the working directory, and the repository is the one
//! tree the production policy already grants read-write, so no policy
//! mutation is needed and the policy under test stays byte-identical to
//! production). A missing or unparsable marker is a hard failure, never a
//! silent pass.
//!
//! Every denial-shaped claim is paired with a positive observed in the same
//! run, because "nothing happened" satisfies an unpaired absence assertion
//! trivially:
//!
//! - **Reaping** is paired with a `Tier::Network` **control leg** — the same
//!   hook script, the same seam, the same production builder, one tier down.
//!   The network tier has no pid namespace and no `--die-with-parent`, so the
//!   double-forked `setsid` grandchild there *must survive* its supervisor's
//!   SIGKILL. That is what makes "it did not survive under Strict" evidence
//!   about the namespace rather than about `setsid` being unavailable, the
//!   hook never firing, or the loop having already finished.
//! - **Fresh procfs** is paired with the host's own procfs: the test asserts
//!   `/proc/<this test process's pid>` exists *on the host* at the moment of
//!   the check, and that the same path is invisible *inside*. It also asserts
//!   `/proc/1` is visible inside, so an unmounted or empty `/proc` cannot
//!   satisfy the invisibility claim.
//! - **Private `/dev/shm`** is paired twice: with the same `Tier::Network`
//!   control (where the host marker *is* visible, proving the marker exists
//!   and is findable by this exact test), and with a reverse check that the
//!   sandbox's own write never appears on the host.

use std::path::Path;
use std::time::{Duration, Instant};

use super::escape_contract::production_env_profile;
use super::escape_suite::hostile_hook_repo;
use super::spawn::command_async;
use super::*;

// ---------------------------------------------------------------------------
// Policy access — no builder of its own (R6)
// ---------------------------------------------------------------------------

/// The Strict-tier policy plus the R4 capability check, in one call: the
/// single entry point this file (and #199 / plan Task 13) uses to get a
/// `Strict` policy that has been *shown* to launch on this host.
///
/// # Why this returns `Policy` and not `Result<Policy, ()>`
///
/// The plan gives it `-> Result<Policy, ()>` so a caller can `else { return }`
/// on a host without bwrap, with the skip made non-green by R5's census gate.
/// That gate cannot cover this file (see deviation 2 in the module doc), so a
/// `return` here would be a silent green skip — precisely the failure mode
/// this milestone has now found five times. Capability absence is therefore a
/// panic with the host's own error in the message. If #199 was written against
/// the `Result` shape it will fail to compile against this signature, which is
/// the intended way to find out.
///
/// # Why it builds nothing
///
/// `shim_cli::production_policy` is already `policy_for(repo, false,
/// NetworkNeed::Local)` — the production builder, at the arity production
/// calls it. R6 forbids a second Strict-policy constructor, so this is a
/// delegation, not a builder. The two assertions below are not decoration:
/// they pin the *claim* that what came back is the strict tier, so a future
/// re-tiering of the production dispatch fails here, loudly, instead of
/// silently re-pointing all three lifecycle claims at a tier that has no
/// namespaces at all.
pub(crate) async fn strict_baseline(repo: &Path, case: &str) -> Policy {
    let policy = shim_cli::production_policy(repo);
    assert_eq!(
        policy.tier,
        Tier::Strict,
        "{case}: a local operation on an untrusted repository must dispatch to \
         Tier::Strict — every claim in sandbox::lifecycle is about that tier's \
         namespaces, and observing them under any other tier proves nothing"
    );
    assert!(
        policy.bwrap.is_some(),
        "{case}: a Strict policy must carry a resolved bwrap path; the pid \
         namespace, the fresh procfs and the private /dev/shm are all bwrap's"
    );

    // R4-CAPABILITY-BY-EXECUTION: the host is never asked whether it can do
    // this. It is made to do it, through the one production seam, on the
    // cheapest Strict operation that exists.
    let out = command_async(&policy, repo, &["--version"])
        .pinned_env_for_test(&production_env_profile())
        .output()
        .await;
    match out {
        Ok(out) if out.status.success() => policy,
        Ok(out) => panic!(
            "{case}: the composed Strict launcher could not run `git --version` \
             (status {status}). INV-13/ADR 0029: Strict is refused, never \
             downgraded, so a host that cannot launch it is a host git-vista \
             does not run on — this is a real failure, not a skip.\nstderr: {err}",
            status = out.status,
            err = String::from_utf8_lossy(&out.stderr),
        ),
        Err(e) => panic!(
            "{case}: the composed Strict launcher could not be spawned at all: {e}"
        ),
    }
}

/// The `Tier::Network` control policy — the production builder at the *other*
/// arity (`policy_for_repo` is `policy_for(repo, false, NetworkNeed::Remote)`).
///
/// This is not a weaker copy of the subject policy; it is the differential.
/// Network is a real production tier that applies Landlock and seccomp and
/// has **no bwrap and no namespaces at all**, which is exactly the "same
/// everything, minus the mechanism under test" leg a containment claim needs.
fn network_control(repo: &Path, case: &str) -> Policy {
    let policy = policy_for_repo(repo).unwrap_or_else(|e| {
        panic!("{case}: the Network-tier control policy must build: {e}")
    });
    assert_eq!(
        policy.tier,
        Tier::Network,
        "{case}: the control leg must be the Network tier — the whole point is \
         that it lacks the namespace the subject leg is claiming credit for"
    );
    assert!(
        policy.bwrap.is_none(),
        "{case}: the Network-tier control must launch no bwrap, or it is not a \
         control for a bwrap-provided property"
    );
    policy
}

// ---------------------------------------------------------------------------
// Marker-file plumbing
// ---------------------------------------------------------------------------

/// Read a marker the hook wrote into the repository worktree.
///
/// A missing marker is a hard failure and never a skip: by the time this is
/// called, [`strict_baseline`] has already made the composed launcher run a
/// real git on this host, so "the marker is not there" cannot mean "the host
/// could not try."
fn marker(repo: &Path, name: &str, leg: &str) -> String {
    let path = repo.join(name);
    match std::fs::read_to_string(&path) {
        Ok(raw) => raw.trim().to_string(),
        Err(e) => panic!(
            "{leg}: the hook's marker file `{name}` is missing or unreadable ({e}). \
             The composed launcher has already been shown to run on this host, so an \
             absent marker means the hook did not run as expected — a failure, not a \
             reason to pass."
        ),
    }
}

/// Parse a marker that must be a non-negative integer.
fn numeric_marker(repo: &Path, name: &str, leg: &str) -> i64 {
    let raw = marker(repo, name, leg);
    raw.parse().unwrap_or_else(|_| {
        panic!("{leg}: marker `{name}` must be an integer, got {raw:?}")
    })
}

/// A suffix unique to this process, so two concurrently running test binaries
/// (or a stale one) can never read each other's `/dev/shm` markers and call it
/// isolation.
fn run_tag() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// A10 / INV-8 — orphan reaping
// ---------------------------------------------------------------------------

const TICKS: &str = "lifecycle-ticks";
const ORPHAN_MARKER: &str = "lifecycle-orphan-alive";

/// The hostile hook, identical in both legs.
///
/// Two `setsid` children, both double-detached from the git process that
/// spawned them (`setsid` gives each its own session and process group, so
/// neither dies from a signal aimed at the supervisor's group, and neither is
/// reachable by killing the process tree by pid):
///
/// - a **ticker** that appends one line every 100 ms for 8 s, so "did it keep
///   running after the supervisor died" is a countable fact rather than a
///   guess;
/// - a **delayed marker** that writes a file 2 s in, so an orphan that
///   outlives the kill leaves a positive trace and not merely a missing one.
///
/// The trailing `sleep` keeps the hook — and therefore the commit, and
/// therefore the supervisor — alive well past the observation window, so the
/// supervisor is always killed mid-operation rather than found already exited.
fn orphan_hook() -> String {
    format!(
        "setsid sh -c 'i=0; while [ $i -lt 80 ]; do echo t >> {TICKS}; \
         i=$((i+1)); sleep 0.1; done' >/dev/null 2>&1 &\n\
         setsid sh -c 'sleep 2; echo alive > {ORPHAN_MARKER}' >/dev/null 2>&1 &\n\
         sleep 10\n"
    )
}

fn tick_count(repo: &Path) -> usize {
    std::fs::read_to_string(repo.join(TICKS))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

struct OrphanObservation {
    ticks_at_kill: usize,
    ticks_after: usize,
    delayed_marker: bool,
    teardown: Duration,
}

/// Run one leg: spawn the composed launcher on a hostile-hook commit, wait
/// until the detached grandchild is demonstrably running, SIGKILL the
/// supervisor, and then measure what survived.
async fn observe_orphan(repo: &Path, policy: &Policy, leg: &str) -> OrphanObservation {
    /// How long the detached ticker is given to prove it is running before the
    /// supervisor is killed. Generous, because it is polled, not slept: the
    /// kill happens as soon as the evidence appears.
    const TICK_DEADLINE: Duration = Duration::from_secs(30);
    /// Let the kernel finish tearing the namespace down before the "frozen"
    /// baseline is read, so a tick already in flight is counted on the right
    /// side of the kill.
    const SETTLE: Duration = Duration::from_millis(400);
    /// The window in which a surviving orphan would produce ~35 more ticks and
    /// would have written its 2 s marker.
    const OBSERVE: Duration = Duration::from_millis(3500);

    let mut child = command_async(policy, repo, &["commit", "--allow-empty", "-m", "orphan"])
        .pinned_env_for_test(&production_env_profile())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("{leg}: the composed launcher must spawn: {e}"));

    // Wait for evidence the detached grandchild is actually running. Polling
    // rather than sleeping a fixed interval is what makes `ticks_at_kill > 0`
    // reliably meaningful: bwrap's startup and the shim's Landlock
    // enumeration are not constant-time, and a fixed delay that lands before
    // the hook fires turns this whole leg into a vacuous zero-versus-zero.
    let deadline = Instant::now() + TICK_DEADLINE;
    loop {
        if tick_count(repo) >= 2 {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "{leg}: the launcher exited ({status}) before the hook's detached \
                 grandchild wrote two ticks — nothing about reaping can be observed \
                 from a leg whose hook never ran"
            );
        }
        assert!(
            Instant::now() < deadline,
            "{leg}: the hook's detached `setsid` grandchild never wrote two ticks \
             within {TICK_DEADLINE:?}. That is a failure, not a skip: the paired \
             control leg's whole job is to show this host does produce a surviving \
             orphan, and it cannot show it if the orphan never starts."
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // SIGKILL, not SIGTERM: the question is whether the boundary tears the
    // tree down, not whether the supervisor cooperates in doing so.
    child
        .start_kill()
        .unwrap_or_else(|e| panic!("{leg}: SIGKILL must be deliverable: {e}"));
    let started = Instant::now();
    let _ = child.wait().await;
    let teardown = started.elapsed();

    tokio::time::sleep(SETTLE).await;
    let ticks_at_kill = tick_count(repo);
    tokio::time::sleep(OBSERVE).await;

    OrphanObservation {
        ticks_at_kill,
        ticks_after: tick_count(repo),
        delayed_marker: repo.join(ORPHAN_MARKER).exists(),
        teardown,
    }
}

/// INV-8 / A10: SIGKILL of the supervisor reaps a double-forked `setsid`
/// grandchild, and `waitid` confirms teardown.
///
/// The control leg is not a courtesy — it is what makes the subject leg mean
/// anything. Under `Tier::Network` (no pid namespace, no `--die-with-parent`)
/// the very same hook script, run through the very same seam, must leave a
/// grandchild that keeps ticking and still writes its delayed marker after the
/// supervisor is dead. Only against that measured baseline does the subject
/// leg's silence say "the namespace reaped it" rather than "the hook never
/// ran", "`setsid` is missing", or "the loop had already finished".
#[tokio::test]
async fn strict_reaps_a_double_forked_setsid_orphan_that_the_network_tier_does_not() {
    let case = "lifecycle-orphan-reaped";

    // ---- control: the mechanism is absent, so the orphan must survive ----
    let control_repo = hostile_hook_repo(&orphan_hook());
    let control_policy = network_control(control_repo.path(), case);
    let control = observe_orphan(control_repo.path(), &control_policy, "control(Network)").await;

    assert!(
        control.ticks_at_kill >= 2,
        "control(Network): the detached ticker must be running at kill time, got \
         {} ticks — without that the comparison below is zero versus zero",
        control.ticks_at_kill
    );
    assert!(
        control.ticks_after > control.ticks_at_kill,
        "control(Network): a double-forked `setsid` grandchild must SURVIVE its \
         supervisor's SIGKILL when there is no pid namespace ({} ticks at kill, {} \
         after). If it does not, this host reaps orphans for some reason other than \
         the strict tier's namespace, and the subject leg below proves nothing about \
         the sandbox.",
        control.ticks_at_kill,
        control.ticks_after
    );
    assert!(
        control.delayed_marker,
        "control(Network): the surviving orphan must have written its delayed marker \
         — the subject leg's `!delayed_marker` is only evidence if this leg's is true"
    );

    // ---- subject: the mechanism is present, so the orphan must be reaped ----
    let repo = hostile_hook_repo(&orphan_hook());
    let policy = strict_baseline(repo.path(), case).await;
    let subject = observe_orphan(repo.path(), &policy, "subject(Strict)").await;

    assert!(
        subject.teardown < Duration::from_secs(5),
        "subject(Strict): waitid did not confirm teardown promptly ({:?})",
        subject.teardown
    );
    assert!(
        subject.ticks_at_kill >= 2,
        "subject(Strict): the detached ticker must be running at kill time, got {} \
         ticks — a frozen tick log means nothing if the log was never moving",
        subject.ticks_at_kill
    );
    assert_eq!(
        subject.ticks_at_kill, subject.ticks_after,
        "INV-8: a descendant kept running after the supervisor died — the tick log \
         grew from {} to {} in the {:?} after SIGKILL",
        subject.ticks_at_kill,
        subject.ticks_after,
        Duration::from_millis(3500)
    );
    assert!(
        !subject.delayed_marker,
        "INV-8: a double-forked `setsid` orphan survived the supervisor's SIGKILL \
         and wrote its delayed marker"
    );
}

// ---------------------------------------------------------------------------
// A8 / C3 — a fresh procfs
// ---------------------------------------------------------------------------

/// A8 / C3: "a pid namespace does not update an inherited procfs mount —
/// `/proc` keeps its mounter's PID view." `STRICT_BWRAP_ARGS` answers that
/// with `--proc /proc`; this is the test that the answer is really there.
///
/// The discriminator is exact rather than a threshold: **this test process's
/// own pid**, which is asserted to exist on the host in the same breath, must
/// not exist inside. A fresh procfs for a fresh pid namespace allocates from 1
/// upward and cannot contain it; an inherited host procfs necessarily does,
/// because the process holding it is the one doing the asserting.
///
/// `/proc/1` visible inside is the paired positive. Without it, "the host pid
/// is absent" would be equally satisfied by `/proc` being unmounted, empty, or
/// unreadable — three ways to learn nothing, all of which would otherwise
/// score as containment.
#[tokio::test]
async fn strict_mounts_a_fresh_procfs_that_cannot_see_the_host_process_table() {
    const INIT: &str = "lifecycle-proc-init";
    const HOST: &str = "lifecycle-proc-host";
    const MAX: &str = "lifecycle-proc-max";
    const SELF: &str = "lifecycle-proc-self";
    let case = "lifecycle-fresh-procfs";
    let leg = "subject(Strict)";

    let host_pid = std::process::id();
    assert!(
        host_pid > 1000,
        "this test's discriminator is that a fresh pid namespace cannot have \
         allocated pid {host_pid}; at that value it might legitimately have done so, \
         so the observation below would not be attributable. Re-run on a host whose \
         pid counter has advanced."
    );

    let repo = hostile_hook_repo(&format!(
        "if [ -d /proc/1 ]; then echo PRESENT > {INIT}; else echo ABSENT > {INIT}; fi\n\
         if [ -d /proc/{host_pid} ]; then echo PRESENT > {HOST}; else echo ABSENT > {HOST}; fi\n\
         ls /proc 2>/dev/null | grep -E '^[0-9]+$' | sort -n | tail -1 > {MAX}\n\
         echo $$ > {SELF}\n\
         exit 0\n"
    ));

    let policy = strict_baseline(repo.path(), case).await;
    let out = command_async(&policy, repo.path(), &["commit", "--allow-empty", "-m", "procfs"])
        .pinned_env_for_test(&production_env_profile())
        .output()
        .await
        .expect("the composed launcher runs");
    assert!(
        out.status.success(),
        "{leg}: the commit must land, so the hook's own inability to write is never \
         mistaken for the property under test.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The paired positive, on the host side of the boundary: the pid the hook
    // looked for is a pid that genuinely exists right now.
    assert!(
        Path::new(&format!("/proc/{host_pid}")).is_dir(),
        "the host's own procfs must show this test process (pid {host_pid}); if it \
         does not, `ABSENT` from inside the sandbox is not evidence of anything"
    );

    assert_eq!(
        marker(repo.path(), INIT, leg),
        "PRESENT",
        "C3: /proc/1 must be visible inside the sandbox. An unmounted, empty or \
         unreadable /proc would satisfy the host-pid check below for entirely the \
         wrong reason, so this is asserted first."
    );
    assert_eq!(
        marker(repo.path(), HOST, leg),
        "ABSENT",
        "C3: the host's process table was visible from inside the strict sandbox — \
         /proc/{host_pid} is this very test process, and a fresh procfs for a fresh \
         pid namespace cannot contain it. `--proc /proc` is missing or ineffective."
    );

    let highest = numeric_marker(repo.path(), MAX, leg);
    assert!(
        (1..100).contains(&highest),
        "C3: the highest pid visible inside was {highest}. A fresh procfs over a \
         fresh pid namespace shows a handful of small pids; anything else is a \
         host-inherited /proc."
    );
    let hook_pid = numeric_marker(repo.path(), SELF, leg);
    assert!(
        (1..100).contains(&hook_pid),
        "C3: the hook's own pid inside the sandbox was {hook_pid} — under \
         `--unshare-pid` it must be namespace-local and small"
    );
}

// ---------------------------------------------------------------------------
// A9 / C4 — a private /dev/shm
// ---------------------------------------------------------------------------

/// Removes whatever this test put in the host's `/dev/shm`, including on the
/// panic paths — an assertion failure must not leave a marker behind that a
/// later run could read and mistake for its own.
struct ShmPaths {
    host: std::path::PathBuf,
    inside_control: std::path::PathBuf,
    inside_subject: std::path::PathBuf,
}

impl Drop for ShmPaths {
    fn drop(&mut self) {
        for p in [&self.host, &self.inside_control, &self.inside_subject] {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn shm_hook(host: &Path, inside: &Path) -> String {
    format!(
        "if echo ok > {inside} 2>/dev/null; then echo 0 > lifecycle-shm-write; \
         else echo 1 > lifecycle-shm-write; fi\n\
         if [ -f {host} ]; then echo PRESENT > lifecycle-shm-host; \
         else echo ABSENT > lifecycle-shm-host; fi\n\
         exit 0\n",
        inside = inside.display(),
        host = host.display(),
    )
}

async fn run_shm_leg(repo: &Path, policy: &Policy, leg: &str) {
    let out = command_async(policy, repo, &["commit", "--allow-empty", "-m", "devshm"])
        .pinned_env_for_test(&production_env_profile())
        .output()
        .await
        .expect("the composed launcher runs");
    assert!(
        out.status.success(),
        "{leg}: the commit must land, so a failed hook is never mistaken for the \
         property under test.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// A9 / C4: an ipc namespace does not cover pathname-based POSIX shared
/// memory, so `STRICT_BWRAP_ARGS` carries `--tmpfs /dev/shm`. Global
/// Constraint 8 already settles what that means: a **private, writable**
/// tmpfs, always — not a deny rule.
///
/// So the declared outcome is single and unconditional (the write succeeds),
/// asserted alongside its security property (the host's marker is invisible)
/// and its converse (the sandbox's write never reaches the host). There is no
/// `EACCES || OK` branch here: an acceptance condition satisfied by two
/// different security postures is not an acceptance condition.
///
/// The `Tier::Network` control leg is what makes `ABSENT` attributable. The
/// network tier has no mount namespace, so it sees the real `/dev/shm`; if the
/// host marker is not `PRESENT` there, then this test cannot see its own
/// marker for some unrelated reason and `ABSENT` under Strict would be
/// meaningless.
#[tokio::test]
async fn strict_gets_a_private_dev_shm_tmpfs_that_the_network_tier_does_not() {
    let case = "lifecycle-dev-shm-private";
    let tag = run_tag();
    let shm = ShmPaths {
        host: std::path::PathBuf::from(format!("/dev/shm/gv-lifecycle-host-{tag}")),
        inside_control: std::path::PathBuf::from(format!("/dev/shm/gv-lifecycle-in-c-{tag}")),
        inside_subject: std::path::PathBuf::from(format!("/dev/shm/gv-lifecycle-in-s-{tag}")),
    };
    std::fs::write(&shm.host, b"host").unwrap_or_else(|e| {
        panic!("the host marker in /dev/shm must be creatable, or nothing here is testable: {e}")
    });

    // ---- control: no mount namespace, so the host's /dev/shm must show through ----
    let control_repo = hostile_hook_repo(&shm_hook(&shm.host, &shm.inside_control));
    let control_policy = network_control(control_repo.path(), case);
    run_shm_leg(control_repo.path(), &control_policy, "control(Network)").await;

    assert_eq!(
        marker(control_repo.path(), "lifecycle-shm-write", "control(Network)"),
        "0",
        "control(Network): the write into the host's /dev/shm must succeed"
    );
    assert_eq!(
        marker(control_repo.path(), "lifecycle-shm-host", "control(Network)"),
        "PRESENT",
        "control(Network): the host's /dev/shm marker must be visible to a tier with \
         no mount namespace. If it is not, this test cannot see its own marker at all \
         and `ABSENT` under Strict would be evidence of nothing."
    );
    assert!(
        shm.inside_control.is_file(),
        "control(Network): a write to /dev/shm from a tier with no mount namespace \
         must land on the host — that is what makes the subject leg's isolation claim \
         a difference rather than a definition"
    );

    // ---- subject: a private tmpfs, in both directions ----
    let repo = hostile_hook_repo(&shm_hook(&shm.host, &shm.inside_subject));
    let policy = strict_baseline(repo.path(), case).await;
    run_shm_leg(repo.path(), &policy, "subject(Strict)").await;

    assert_eq!(
        marker(repo.path(), "lifecycle-shm-write", "subject(Strict)"),
        "0",
        "C4: the write into the sandbox's private /dev/shm tmpfs must succeed — \
         Global Constraint 8 makes `--tmpfs /dev/shm` a private writable tmpfs, \
         always, never a deny rule"
    );
    assert_eq!(
        marker(repo.path(), "lifecycle-shm-host", "subject(Strict)"),
        "ABSENT",
        "C4: the host's /dev/shm marker was visible from inside the strict sandbox — \
         /dev/shm is not a private tmpfs there"
    );
    assert!(
        !shm.inside_subject.exists(),
        "C4: the sandbox's own /dev/shm write reached the host at {} — a private \
         tmpfs must be private in both directions",
        shm.inside_subject.display()
    );
    assert!(
        shm.host.is_file(),
        "C4: the host marker must still exist after the run; if the sandbox could \
         delete it, `ABSENT` would be self-inflicted rather than observed"
    );
}
