//! INV-17: "Documented non-coverage is tested as non-coverage."
//!
//! **This file's tests assert that attacks SUCCEED.** That is not a mistake and
//! it is not a TODO. Landlock ABI 8 does not mediate `chmod`, `chown`, `utime`,
//! `setxattr`, `flock`, `chdir`, `stat` or `access`, and the round-4 verdict
//! learned it the hard way: a probe ran `chmod 777 /home/tom/.gitconfig`
//! *inside* a Landlock sandbox expecting a denial, and it succeeded, followed
//! the symlink, and changed the mode of a real file in a tree Landlock held no
//! right over (F-NEW-3, verdict §1).
//!
//! If one of these tests ever starts failing, that is **good news** — a kernel
//! or an ABI bump closed a gap — and the correct response is to promote the
//! check into `escape_suite.rs` and amend the whole-sandbox ADR
//! (`git-process-sandbox` — by name, since its number is assigned only when
//! Task 18 runs), not to delete it.
//!
//! Blast radius is [OPEN]: verdict §8 lists "the blast radius of unmediated
//! chmod/chown/utime/setxattr" as genuinely open, with no design. This file
//! records the gap; it does not close it.
//!
//! # Why an inverted claim needs a *stronger* control than a denial claim
//!
//! Every other battery file asserts containment, and R2 guards those against
//! "nothing happened" scoring as a pass. An inverted claim has the same hole
//! rotated 180°: **"the attack succeeded" passes trivially on a run where the
//! sandbox was not enforcing at all.** A chmod that succeeds because the target
//! sits in a tree the test itself granted read-write, or because Landlock never
//! attached, is not evidence about mediation — it is evidence about the grant.
//!
//! So the test below carries a control that a denial-shaped test does not need:
//! in the same run, against the *same path*, an ordinary **write must be
//! DENIED**. That is what makes chmod's success mean "Landlock is enforcing here
//! and does not mediate this operation" rather than "there was no sandbox."
//! Measured on this host: write → exit 2, `Permission denied`, content
//! unchanged; chmod → exit 0, and the host-side mode really moved 0600 → 0777.
//!
//! The observation is the **host-side mode bit**, not the hook's exit code. An
//! exit code is the hook's opinion; the mode bit is the effect having actually
//! crossed the sandbox boundary.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::escape_suite::hostile_hook_repo;
use super::lifecycle::strict_baseline;
use super::spawn::command_async;

/// Mode the target is seeded with, and the mode the hook tries to set. They
/// must differ, or "the mode is 0777 afterwards" would be satisfied by the
/// chmod never having run at all.
const SEEDED_MODE: u32 = 0o600;
const ATTACKED_MODE: u32 = 0o777;

/// Read a marker the hook wrote into the repository worktree.
///
/// The worktree is the one tree the **production** policy already grants
/// read-write, so observing through it needs no policy mutation — see
/// `lifecycle`'s module doc, which takes the same route for the same reason.
/// A missing marker is a hard failure and never a skip: [`strict_baseline`] has
/// already made the composed launcher run a real git on this host by the time
/// this is called, so "the marker is not there" cannot mean "the host could not
/// try."
fn marker(repo: &Path, name: &str) -> String {
    match std::fs::read_to_string(repo.join(name)) {
        Ok(raw) => raw.trim().to_string(),
        Err(e) => panic!(
            "F-NEW-3: the hook's marker file `{name}` is missing or unreadable ({e}). The \
             composed Strict launcher has already been shown to run on this host, so an absent \
             marker means the hook did not run — a failure, not a reason to pass."
        ),
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("F-NEW-3: chmod target must still exist on the host: {e}"))
        .permissions()
        .mode()
        & 0o7777
}

/// F-NEW-3: Landlock ABI 8 does not mediate `chmod`, and a hostile hook can
/// therefore change the mode of a file the sandbox otherwise holds no write
/// right over.
///
/// # What makes this non-vacuous
///
/// 1. The target is outside **every** granted tree, asserted structurally
///    below — so the success cannot be an artifact of a grant. (An earlier
///    draft of this test pushed the target's own directory into
///    `policy.rw_trees` and then observed that chmod succeeded there. That
///    proves nothing: a read-write grant is supposed to allow writes. It also
///    made the policy under test differ from production, against R6.)
/// 2. The policy is byte-identical to production — no `rw_trees` mutation, no
///    second builder. Everything comes from [`strict_baseline`].
/// 3. A plain **write to the same path is required to be denied in the same
///    run**, which is what rules out "the sandbox simply was not on."
/// 4. The claim is checked on the **host's** mode bits, not on the hook's
///    reported exit status.
#[tokio::test]
async fn landlock_does_not_mediate_chmod() {
    let scratch = tempfile::tempdir().expect("scratch");
    let target = scratch.path().join("chmod-target");
    std::fs::write(&target, b"probe target").expect("create chmod target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(SEEDED_MODE))
        .expect("seed the target's mode");
    assert_eq!(
        mode_of(&target),
        SEEDED_MODE,
        "the fixture must start from a mode that differs from the attacked one, or the final \
         assertion is satisfied by the chmod never having happened"
    );

    // Markers land in the repository worktree, which production already grants
    // read-write. The target does not, and must not.
    let hook = format!(
        "chmod {mode:o} {t}; echo $? > chmod-status\n\
         printf mutated > {t} 2>/dev/null; echo $? > write-status\n",
        mode = ATTACKED_MODE,
        t = target.display(),
    );
    let repo = hostile_hook_repo(&hook);
    let policy = strict_baseline(repo.path(), "documented-gaps-chmod-outside").await;

    // (1) Structural guard: the target is under no grant of any kind. Without
    // this the test could silently drift back into proving that a granted tree
    // is writable, which is how it was first written.
    for granted in policy.rw_trees.iter().chain(policy.ro_trees.iter()) {
        assert!(
            !target.starts_with(granted),
            "F-NEW-3 would be vacuous: the chmod target {t} lies under the granted tree {g}. The \
             whole claim is that chmod reaches a path the sandbox holds no right over — pick a \
             target outside every entry of rw_trees and ro_trees.",
            t = target.display(),
            g = granted.display(),
        );
    }

    let out = command_async(
        &policy,
        repo.path(),
        &["commit", "--allow-empty", "-m", "chmod"],
    )
    .output()
    .await
    .expect("launcher runs");
    assert!(
        out.status.success(),
        "the commit must land, so that a failure below is the hook's observation and not the \
         operation having never reached the hook.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // (2) The control, and the load-bearing half of this test: an ordinary
    // write to the very same path must be REFUSED. If this ever goes green,
    // the run below proves nothing about mediation.
    let write_status = marker(repo.path(), "write-status");
    assert_ne!(
        write_status,
        "0",
        "the sandbox let a plain write reach {t}, a path outside every grant. That makes the \
         chmod result below meaningless — it would show only that nothing was being enforced. \
         Fix the sandbox before reading anything into F-NEW-3.",
        t = target.display(),
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("read target"),
        "probe target",
        "the denied write must also have left the file's contents alone"
    );

    // (3) The documented gap itself, observed on the host's own inode.
    let chmod_status = marker(repo.path(), "chmod-status");
    assert_eq!(
        chmod_status, "0",
        "F-NEW-3 appears to be CLOSED (chmod exited {chmod_status:?} against a target the fixture \
         guarantees exists, on a path where a write was correctly denied). That is good news: \
         promote this check into escape_suite.rs, raise the declared ABI floor if that is what \
         closed it, and amend the git-process-sandbox ADR's non-coverage section by name — its \
         number is assigned only when Task 18 runs. Do not just delete this test."
    );
    assert_eq!(
        mode_of(&target),
        ATTACKED_MODE,
        "F-NEW-3: chmod reported success but the host-side mode did not move from {SEEDED_MODE:o}. \
         Either the effect did not cross the sandbox boundary (in which case this test's claim is \
         wrong as stated) or the hook chmod'd something else."
    );
}

/// The other half of INV-17, stated rather than tested: the confused-deputy
/// ceiling. Codex's phrasing, quoted because there is nothing to add: *"a hook
/// can still act on an outside process through any writable file that process
/// watches and treats as instructions."* Neither AF_UNIX denial nor Landlock
/// touches it, and no test can, which is why this is a doc-comment and not an
/// assertion. It is written into `docs/SECURITY_MODEL.md` in Task 18.
///
/// This one is a documentation-presence check and nothing more. It is worth
/// having — a security model that silently loses this paragraph starts reading
/// as a containment claim it cannot support — but it asserts the *doc* says the
/// words, not that the system behaves any particular way.
#[test]
fn the_confused_deputy_ceiling_is_documented_not_engineered_against() {
    let model = include_str!("../../../../docs/SECURITY_MODEL.md");
    assert!(
        model.contains("confused deputy"),
        "SECURITY_MODEL.md must name the confused-deputy ceiling explicitly — it is the reason \
         'a hostile repository cannot harm you' is not purchasable, and a model that omits it \
         will be read as claiming containment"
    );
}

// =========================================================================
// A second, different kind of gap — read this before adding to it
// =========================================================================
//
// Everything above is INV-17: a *proven* kernel non-mediation gap, evidenced
// by a test that runs the attack and watches it succeed. What follows is not
// that. `policy_for_clone` (#66/D4, `sandbox/mod.rs`) is an ordinary,
// production-reachable, attacker-facing spawn site that nobody has written an
// `EscapeCase` for yet — ordinary missing coverage, not a kernel limitation.
// It is filed in this module anyway, for now, because this is the project's
// one place for "here is what is NOT proven and why" and no better one exists
// — see the module doc's INV-17 label on `mod documented_gaps;` in
// `sandbox/mod.rs`, which this entry deliberately does not fit. Do not follow
// this shape for a future gap of the same (missing-coverage) kind without
// first asking whether it deserves its own module.
//
// # What `policy_for_clone` is, and why it is separate from `policy_for`
//
// `handlers/clone.rs:140`, inside `clone_repo` (`POST /api/clone`), is the
// only production caller. It builds via `policy_for_clone(&root)` and spawns
// through `sandbox::spawn::command_async` directly — the same chokepoint
// every other sandboxed git spawn goes through, but reached without
// `git_cmd.rs`'s `sandboxed()`/`policy_for()` wrapper, because the clone
// destination does not exist yet at policy time and `policy_for`'s
// `repo_paths::resolve` requires an existing `.git`. Its own doc comment
// (`sandbox/mod.rs`, directly above the function) names the reason it exists
// as a separate constructor rather than a `policy_for` variant: clone is "the
// one operation that fetches attacker-chosen content by design" and must
// never be reachable at `Tier::Unsandboxed` — so `tier` is a hard
// `Tier::Network` constant and the function neither takes nor derives a trust
// flag, structurally rather than by a check that could be edited away.
//
// # What IS verified today
//
// `policy_for_clone` composes a `Policy` from the exact same building blocks
// as `policy_for(.., NetworkNeed::Remote)` — `default_system_trees(Tier::
// Network)`, `secret_excludes_for_home` plus `sandbox_trust_dir()`,
// `DEFAULT_GIT_PORTS`, `bwrap: None`, `HookMode::Run` — and both hand off to
// the identical `sandbox::spawn::command_async` → `sandbox_argv` → shim
// composition. That launcher (Landlock at the declared floor, the seccomp
// filter, `NoNewPrivs`) is exercised, with kernel-level provenance checks, by
// the nine `Tier::Network` cases already in `escape_suite.rs` (e.g.
// `secret_read_denied`, `io_uring_denied`, `high_bit_prctl_denied`). Those
// cases are evidence that the *launcher* contains a hostile process at
// `Tier::Network` — a property of the composition, not of which production
// call site built the policy that fed it.
//
// # What is NOT verified
//
// Nothing exercises `policy_for_clone` itself through a spawned, sandboxed
// process — no `EscapeCase` names it, and it has zero references in
// `escape_suite.rs`, `hook_mode_suite.rs`, or `docs/sandbox/escape-census.txt`
// (grepped directly this session; confirmed empty in all three). Two
// deltas from the covered `policy_for(.., Remote)` cases are consequently
// untested by anything, not just by inference:
//
// 1. **Grant shape.** `policy_for_clone`'s read-write grant is the whole
//    clones root, not one resolved repository path, and its read-only grant
//    is bare `$HOME` with no `repo_paths` commondir push (there is nothing to
//    resolve yet). Whether a hostile clone source can reach outside the
//    clones root through this different — not smaller, not obviously larger
//    — grant shape has not been tried.
// 2. **The vehicle.** Every existing case's harness (`escape_contract.rs`'s
//    `execute`) commits into a repository that already exists, with a
//    pre-commit hook already installed by `install_hook` before the
//    sandboxed process ever runs. A clone's destination does not exist until
//    the sandboxed `git clone` itself creates it, so that vehicle cannot
//    reach a clone at all — hostile content would have to arrive through the
//    clone source (a `git clone --template=<dir>` populates
//    `.git/hooks/post-checkout` before clone's own automatic post-clone
//    checkout, which does fire it — confirmed by direct local reproduction
//    this session, not merely reasoned) or through content fetched from a
//    served remote. Building that is real, new harness plumbing: a
//    clone-shaped arm in `escape_contract.rs`'s `policy_for_case` (which
//    today dispatches only `Tier::Network → policy_for_repo(repo)` and
//    `Tier::Strict → policy_for(repo, false, NetworkNeed::Local)`, both of
//    which assume an existing repository path — `policy_for_clone` takes a
//    clones root, not a repository, and fits neither arm), a
//    `clone_inside`/`hostile_clone_source` pair analogous to `commit_inside`/
//    `hostile_hook_repo`, and a new registered id in `escape-census.txt`
//    (R5). None of that exists yet; inventing it under this session's time
//    and risk budget, against the shared `execute()` chokepoint every other
//    case depends on, was judged disproportionate to do without the review
//    the rest of this battery's harness changes have had.
//
// # Residual risk, in plain terms
//
// If a hostile clone source (a malicious remote, or a local template an
// attacker controls) can plant something that runs during `git clone`'s
// implicit checkout — `post-checkout` is the demonstrated vehicle, and
// nothing rules out others — the launcher composition shared with the nine
// covered `Network`-tier cases is expected, by structural similarity, to
// contain it the same way it contains a hostile pre-commit hook today. That
// expectation is inference from a shared mechanism, not direct evidence: no
// test has run a hostile clone through `policy_for_clone` and watched the
// launcher hold. Should containment *not* hold here specifically — because
// of the grant-shape delta above, or for a reason structural similarity does
// not predict — the consequence is arbitrary code execution triggered by an
// attacker-chosen clone URL, against the exact spawn site the function's own
// doc comment names as the highest-risk one in the crate.
//
// # What would close this
//
// A real `EscapeCase` exercising `policy_for_clone` through
// `sandbox::spawn::command_async` with a hostile clone source, registered in
// `escape-census.txt`, per the plan sketched above. When that lands, delete
// this section (the tripwire below will fail and say so) rather than leaving
// it to describe a gap that no longer exists.

/// Fails (loudly, on purpose) the day someone adds real coverage for clone,
/// so this doc cannot silently outlive the gap it describes — the same
/// "if this ever starts failing, that is good news" posture the INV-17
/// tests above use, applied to a missing-coverage gap instead of a kernel
/// one. Scans *source*, not the crate's public surface, to match this file's
/// existing tripwire style (`escape_contract.rs`'s R-series does the same).
#[test]
fn clone_has_no_escape_battery_coverage_yet() {
    let escape_suite = include_str!("escape_suite.rs");
    let hook_mode_suite = include_str!("hook_mode_suite.rs");
    let census = include_str!("../../../../docs/sandbox/escape-census.txt");
    assert!(
        !escape_suite.contains("policy_for_clone") && !escape_suite.contains("clone_inside"),
        "escape_suite.rs now references clone — this doc's premise (no case exists) is stale. \
         If a real EscapeCase for policy_for_clone was just added, delete the 'known missing \
         coverage: clone' section above and this test; that is the intended outcome, not a bug."
    );
    assert!(
        !hook_mode_suite.contains("policy_for_clone"),
        "hook_mode_suite.rs now references policy_for_clone — same as above, update/delete \
         this section rather than this assertion."
    );
    assert!(
        !census.contains("clone"),
        "docs/sandbox/escape-census.txt now has a clone-related entry — the coverage gap this \
         section documents appears to be closed; delete the section and this test rather than \
         widening the assertion."
    );
}

/// A cheap regression guard on `policy_for_clone`'s shape, **not** containment
/// evidence: it inspects the returned `Policy` struct's fields rather than
/// observing an effect through a spawned, sandboxed process, which is exactly
/// the flavor of check the anti-vacuity contract (R2/R3/R9) rules out as
/// "real coverage." It exists only to catch an accidental field change (a
/// dropped grant, a flipped tier) between now and whenever the real
/// `EscapeCase` above gets built — never cite this test as evidence the gap
/// above is closed.
///
/// Deliberately does not recompute the expected trees via
/// `default_system_trees`/`secret_excludes_for_home` and compare — that would
/// assert the mapping by calling the function that defines it, which proves
/// nothing (a standing caution in this project). Every assertion below checks
/// an independently-stated, minimal fact instead.
#[test]
fn policy_for_clone_shape_regression_guard() {
    let clones_root = tempfile::tempdir().expect("clones root");
    let policy = super::policy_for_clone(clones_root.path())
        .expect("policy_for_clone must build on a host with HOME and a shim");

    assert_eq!(
        policy.tier,
        super::Tier::Network,
        "clone must never be built at any tier other than Network — that is the whole point of \
         giving it a separate, trust-blind constructor"
    );
    assert_eq!(
        policy.hook_mode,
        super::HookMode::Run,
        "policy_for_clone must not silently start blocking hooks — ADR 0029 rejects that \
         posture, and this constructor never selects HookMode::Blocked"
    );
    assert!(
        policy.bwrap.is_none(),
        "Network tier launches no bwrap; a Some here would mean policy_for_clone drifted onto \
         Strict's launcher shape"
    );
    assert!(
        policy.rw_trees.iter().any(|p| p == clones_root.path()),
        "the clones root itself must be a read-write grant, or `git clone` cannot write its \
         destination"
    );
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    assert!(
        policy.ro_trees.iter().any(|p| p == &home),
        "$HOME must be a read-only grant (needed for e.g. credential helpers/config \
         resolution), per policy_for_clone's own doc comment"
    );
    assert!(
        policy
            .secret_excludes
            .iter()
            .any(|p| p == &crate::state::sandbox_trust_dir()),
        "the trust-store directory must stay excluded — clone is the operation that fetches \
         attacker-chosen content, so it must never be able to leave behind a marker that \
         promotes the resulting repository later"
    );
    assert!(
        !policy.net_ports.is_empty(),
        "Network tier must carry a non-empty net_ports set or clone cannot resolve/connect to \
         any remote at all"
    );
}
