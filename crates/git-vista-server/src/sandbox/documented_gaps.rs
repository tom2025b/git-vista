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
