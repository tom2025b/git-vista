#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use git_vista_plan_runner::{
    checkpoint_from_yaml, manifest_from_plans, manifest_sha256, manifest_to_yaml,
};
use git_vista_protocol::Plan;

const PLANS: &str = include_str!("../../git-vista-protocol/tests/fixtures/plan_v1.json");
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn plan(op: &str) -> Plan {
    serde_json::from_str::<Vec<Plan>>(PLANS)
        .unwrap()
        .into_iter()
        .find(|plan| serde_json::to_value(&plan.operation).unwrap()["op"].as_str() == Some(op))
        .unwrap()
}

fn temp_dir() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gv-run-test-{}-{nonce}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

/// INVARIANT: the real binary executes exact argv in order, returns the first
/// child's non-zero status, and its next invocation resumes after the durable
/// prefix.
///
/// MUTATION 1 (remove): ignore the child status — the first invocation and
/// log assertions are red.
/// MUTATION 2 (weaken): always create a fresh checkpoint — the second log
/// repeats `add` and is red even though all commands eventually run.
#[test]
fn binary_stops_then_resumes_without_repeating_the_completed_prefix() {
    let directory = temp_dir();
    let fake_git = directory.join("git");
    let manifest_path = directory.join("operations.yaml");
    let state_path = directory.join("state.yaml");
    let log_path = directory.join("git.log");
    fs::write(
        &fake_git,
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$GV_RUN_LOG\"\n\
         if [ \"$1\" = \"$GV_RUN_FAIL\" ]; then exit 17; fi\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();

    let manifest = manifest_from_plans(&[plan("stage_all"), plan("pull_branch")]).unwrap();
    let yaml = manifest_to_yaml(&manifest).unwrap();
    fs::write(&manifest_path, &yaml).unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(directory.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();

    let first = Command::new(env!("CARGO_BIN_EXE_gv-run"))
        .args([
            manifest_path.as_os_str(),
            "--state".as_ref(),
            state_path.as_os_str(),
        ])
        .env("PATH", &path)
        .env("GV_RUN_LOG", &log_path)
        .env("GV_RUN_FAIL", "fetch")
        .status()
        .unwrap();
    assert_eq!(first.code(), Some(17));
    assert_eq!(fs::read_to_string(&log_path).unwrap(), "add\nfetch\n");
    let checkpoint = checkpoint_from_yaml(
        &fs::read_to_string(&state_path).unwrap(),
        &manifest_sha256(yaml.as_bytes()),
        manifest.steps.len() as u32,
    )
    .unwrap();
    assert_eq!(checkpoint.last_completed_step, 1);

    let second = Command::new(env!("CARGO_BIN_EXE_gv-run"))
        .args([
            manifest_path.as_os_str(),
            "--state".as_ref(),
            state_path.as_os_str(),
        ])
        .env("PATH", &path)
        .env("GV_RUN_LOG", &log_path)
        .env("GV_RUN_FAIL", "nothing")
        .status()
        .unwrap();
    assert!(second.success());
    assert_eq!(
        fs::read_to_string(&log_path).unwrap(),
        "add\nfetch\nfetch\nrebase\n"
    );

    let final_checkpoint = checkpoint_from_yaml(
        &fs::read_to_string(&state_path).unwrap(),
        &manifest_sha256(yaml.as_bytes()),
        manifest.steps.len() as u32,
    )
    .unwrap();
    assert_eq!(final_checkpoint.last_completed_step, 3);
    fs::remove_dir_all(directory).unwrap();
}
