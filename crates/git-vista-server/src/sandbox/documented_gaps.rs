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
//! # Ordering dependency on `lifecycle::strict_baseline`
//!
//! `strict_baseline` (Task 12) is being written concurrently this round by
//! another agent and does not exist in this tree yet. This file calls
//! `super::lifecycle::strict_baseline` per the plan's explicit instruction —
//! reuse it rather than add a third, independent Strict-policy builder (R6) —
//! so this module will not compile until Task 12 lands `sandbox::lifecycle`
//! and its `mod.rs` declaration. That is a known, reported ordering
//! dependency, not a mistake in this file.

use super::escape_suite::hostile_hook_repo;
use super::lifecycle::strict_baseline;
use super::spawn::command_async;

/// F-NEW-3, observed via a hook marker file under the granted scratch tree —
/// never via the deleted JSON self-probe (see the module doc above). A
/// missing or unparsable marker is a hard failure, not a quiet pass: this
/// test's claim is that chmod succeeds, and a probe that never ran proves
/// nothing about that claim in either direction (the inverted-expectation
/// form of R2's "missing observation is a failure, not a pass").
#[tokio::test]
async fn landlock_does_not_mediate_chmod() {
    let scratch = tempfile::tempdir().expect("scratch");
    let target = scratch.path().join("chmod-target");
    std::fs::write(&target, b"probe target").expect("create chmod target");
    let result = scratch.path().join("chmod-result");
    let repo = hostile_hook_repo(&format!(
        "chmod 777 {t}; echo $? > {r}\n",
        t = target.display(),
        r = result.display()
    ));
    let mut p = strict_baseline(repo.path(), "documented-gaps-chmod-outside").await;
    p.rw_trees.push(scratch.path().to_path_buf());
    let out = command_async(&p, repo.path(), &["commit", "--allow-empty", "-m", "chmod"])
        .output()
        .await
        .expect("launcher runs");
    assert!(
        out.status.success(),
        "commit must land so the hook's own write is not itself the failure"
    );
    let raw = std::fs::read_to_string(&result)
        .expect("F-NEW-3: chmod-result marker must exist — a missing marker is a failure, not a skip");
    assert_eq!(
        raw.trim(),
        "0",
        "F-NEW-3 appears to be CLOSED (chmod exited {raw:?}, not 0, against a target the fixture \
         guarantees exists). That is good news: promote this check into escape_suite.rs, raise the \
         declared ABI floor if that is what closed it, and amend the git-process-sandbox ADR's \
         non-coverage section by name — its number is assigned only when Task 18 runs. Do not just \
         delete this test."
    );
}

#[tokio::test]
async fn tmp_experiment_chmod_target_outside_every_grant() {
    let scratch = tempfile::tempdir().expect("scratch");
    let target = scratch.path().join("chmod-target");
    std::fs::write(&target, b"probe target").expect("create chmod target");
    std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("seed mode");
    // Marker goes in the REPO worktree (already granted rw by the production
    // policy); the chmod TARGET stays outside every grant. No rw_trees push.
    let repo = hostile_hook_repo(&format!(
        "chmod 777 {t}; echo $? > chmod-result\n\
         printf mutated > {t} 2>/dev/null; echo $? > write-result\n",
        t = target.display(),
    ));
    let p = strict_baseline(repo.path(), "tmp-experiment").await;
    eprintln!("EXPERIMENT rw_trees={:?}", p.rw_trees);
    eprintln!("EXPERIMENT ro_trees={:?}", p.ro_trees);
    eprintln!("EXPERIMENT target={}", target.display());
    let out = command_async(&p, repo.path(), &["commit", "--allow-empty", "-m", "chmod"])
        .output()
        .await
        .expect("launcher runs");
    eprintln!("EXPERIMENT commit status={:?}", out.status);
    eprintln!(
        "EXPERIMENT stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = std::fs::read_to_string(repo.path().join("chmod-result"));
    eprintln!("EXPERIMENT chmod marker={raw:?}");
    let w = std::fs::read_to_string(repo.path().join("write-result"));
    eprintln!("EXPERIMENT write marker={w:?}");
    eprintln!("EXPERIMENT content after = {:?}", std::fs::read_to_string(&target));
    let mode = <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::mode(
        &std::fs::metadata(&target).expect("target still there").permissions(),
    );
    eprintln!("EXPERIMENT host mode after = {:o}", mode & 0o7777);
}

/// The other half of INV-17, stated rather than tested: the confused-deputy
/// ceiling. Codex's phrasing, quoted because there is nothing to add: *"a hook
/// can still act on an outside process through any writable file that process
/// watches and treats as instructions."* Neither AF_UNIX denial nor Landlock
/// touches it, and no test can, which is why this is a doc-comment and not an
/// assertion. It is written into `docs/SECURITY_MODEL.md` in Task 18.
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
