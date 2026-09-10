//! #723/#831's composed checkout proof.
//!
//! Clone checkout now blocks hooks, so the executable child that still matters
//! is a content filter. This test crosses the production boundaries: a
//! no-checkout clone fetches attributes and a hook, a repository-local test
//! fixture selects a filter, the #831 LFS checkout builder applies the real
//! `CheckoutPolicy`, and the compiled shim denies the filter's AF_UNIX socket.

use std::os::unix::fs::PermissionsExt;

/// A filter recovers an ssh-agent pathname from readable `$HOME` and attempts
/// the connection without inheriting `SSH_AUTH_SOCK`. The tracked hook beside
/// it is selected in local config but must not run at all.
#[tokio::test]
async fn a_checkout_filter_cannot_reach_an_agent_and_a_fetched_hook_does_not_run() {
    let clones = tempfile::tempdir().expect("clones root");
    let home = tempfile::tempdir().expect("synthetic HOME");
    let socket_dir = tempfile::tempdir().expect("agent socket directory");
    let socket_path = socket_dir.path().join("agent.831");
    let _agent = std::os::unix::net::UnixListener::bind(&socket_path)
        .expect("bind the pathname AF_UNIX control listener");

    let keychain = home.path().join(".keychain");
    std::fs::create_dir(&keychain).expect("create keychain directory");
    std::fs::write(
        keychain.join("fixture-sh"),
        format!(
            "SSH_AUTH_SOCK={}; export SSH_AUTH_SOCK;\n",
            socket_path.display()
        ),
    )
    .expect("write the socket locator a real keychain file carries");

    let source = clones.path().join("source");
    let dest = clones.path().join("checkout");
    std::fs::create_dir(&source).expect("create source repository");
    super::network_exec::run_fixture_git(&source, ["init", "-q"]);
    super::network_exec::run_fixture_git(&source, ["config", "user.name", "git-vista-test"]);
    super::network_exec::run_fixture_git(&source, ["config", "user.email", "test@example.invalid"]);

    std::fs::write(source.join(".gitattributes"), "payload filter=gv831\n")
        .expect("write tracked filter selection");
    std::fs::write(source.join("payload"), "attacker-chosen content\n")
        .expect("write filtered payload");
    let hooks = source.join("hooks");
    std::fs::create_dir(&hooks).expect("create tracked hooks directory");
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, "#!/bin/sh\nprintf RAN > hook-observed\n")
        .expect("write fetched post-checkout hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("hook metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).expect("make hook executable");
    super::network_exec::run_fixture_git(&source, ["add", "."]);
    super::network_exec::run_fixture_git(&source, ["commit", "-qm", "fixture"]);

    super::network_exec::run_fixture_git(
        clones.path(),
        [
            std::ffi::OsString::from("clone"),
            std::ffi::OsString::from("-q"),
            std::ffi::OsString::from("--no-checkout"),
            std::ffi::OsString::from("--"),
            source.as_os_str().to_owned(),
            dest.as_os_str().to_owned(),
        ],
    );
    assert!(!dest.join("hooks/post-checkout").exists(), "premise");

    let observed = dest.join("filter-observed");
    let probe = home.path().join("filter.py");
    std::fs::write(
        &probe,
        format!(
            r#"#!/usr/bin/python3
import os
import socket
import sys

socket_path = None
with open(os.path.join(os.environ["HOME"], ".keychain", "fixture-sh"), encoding="utf-8") as stream:
    socket_path = stream.read().split(";", 1)[0].split("=", 1)[1]
try:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(socket_path)
    result = "connected"
except OSError as error:
    result = f"errno:{{error.errno}}"
with open("{}", "w", encoding="utf-8") as stream:
    stream.write(result)
sys.stdout.buffer.write(sys.stdin.buffer.read())
"#,
            observed.display()
        ),
    )
    .expect("write filter socket probe");
    let mut permissions = std::fs::metadata(&probe)
        .expect("probe metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&probe, permissions).expect("make probe executable");

    super::network_exec::run_fixture_git(&dest, ["config", "core.hooksPath", "hooks"]);
    super::network_exec::run_fixture_git(
        &dest,
        [
            std::ffi::OsString::from("config"),
            std::ffi::OsString::from("filter.gv831.smudge"),
            probe.as_os_str().to_owned(),
        ],
    );
    super::network_exec::run_fixture_git(&dest, ["config", "filter.gv831.required", "true"]);

    let checkout = super::test_env::with_env(
        &[
            ("HOME", Some(home.path().as_os_str())),
            ("SSH_AUTH_SOCK", None),
        ],
        || {
            assert!(std::env::var_os("SSH_AUTH_SOCK").is_none(), "premise");
            let policy = super::policy_for_clone_checkout(clones.path())
                .expect("checkout policy must build");
            super::network_exec::lfs_checkout_command(
                &policy,
                &dest,
                "https://example.invalid/repo.git",
            )
        },
    );
    let output = checkout.output().await.expect("checkout starts");
    assert!(
        output.status.success(),
        "checkout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&observed).expect("the selected filter must run"),
        format!("errno:{}", libc::EPERM),
        "the filter must recover the agent path itself and still receive EPERM"
    );
    assert!(
        !dest.join("hook-observed").exists(),
        "the fetched post-checkout hook must be blocked"
    );
}
