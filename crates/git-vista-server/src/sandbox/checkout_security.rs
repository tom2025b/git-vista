//! #723's composed checkout proof.
//!
//! The unit test in `bin/gv-sandbox/seccomp_filter.rs` can prove the rule map,
//! but it cannot prove clone checkout selects that map. This test crosses every
//! production boundary that matters: a tracked hook is fetched by a no-checkout
//! clone, the checkout command is built from `CheckoutPolicy`, the real shim
//! applies Landlock and seccomp, and Git executes the fetched hook.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn git(command: &mut Command) {
    let output = command.output().expect("git starts");
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A fetched hook reads an ssh-agent pathname from `$HOME`, exports
/// `SSH_AUTH_SOCK` itself, and makes the actual connection attempt. The parent
/// deliberately has no `SSH_AUTH_SOCK`, so env scrubbing cannot make this pass.
///
/// The TCP leg is in the same Python process, under the same compiled filter.
/// Port 9418 is used because it is the only unprivileged port in the production
/// checkout allowlist; `sandbox::argv::only_the_clone_checkout_phase_gives_up_the_188_grants`
/// separately pins 443, the HTTPS Git LFS port, in that exact policy and argv.
/// Together they prove the checkout profile retained real TCP while denying
/// only AF_UNIX.
#[tokio::test]
async fn a_fetched_hook_that_self_sets_ssh_auth_sock_cannot_connect_but_tcp_survives() {
    let _port = crate::test_ports::PortClaim::acquire();
    let tcp_listener =
        std::net::TcpListener::bind(("127.0.0.1", crate::test_ports::PortClaim::PORT))
            .expect("bind the checkout policy's unprivileged TCP port");

    let clones = tempfile::tempdir().expect("clones root");
    let home = tempfile::tempdir().expect("synthetic HOME");
    let socket_dir = tempfile::tempdir().expect("agent socket directory");
    let socket_path = socket_dir.path().join("agent.723");
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
    git(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&source));
    git(Command::new("git")
        .args(["config", "user.name", "git-vista-test"])
        .current_dir(&source));
    git(Command::new("git")
        .args(["config", "user.email", "test@example.invalid"])
        .current_dir(&source));

    let hooks = source.join("hooks");
    std::fs::create_dir(&hooks).expect("create tracked hooks directory");
    let probe = hooks.join("probe.py");
    std::fs::write(
        &probe,
        format!(
            r#"#!/usr/bin/python3
import os
import socket

try:
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp.connect(("127.0.0.1", {port}))
    tcp.sendall(b"checkout-tcp")
    tcp_result = "connected"
except OSError as error:
    tcp_result = f"errno:{{error.errno}}"

try:
    unix = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    unix.connect(os.environ["SSH_AUTH_SOCK"])
    unix_result = "connected"
except OSError as error:
    unix_result = f"errno:{{error.errno}}"

print(f"tcp={{tcp_result}}|unix={{unix_result}}|sock={{os.environ['SSH_AUTH_SOCK']}}")
"#,
            port = crate::test_ports::PortClaim::PORT,
        ),
    )
    .expect("write the hook's socket probe");
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        "#!/bin/sh\n. \"$HOME/.keychain/fixture-sh\"\nexec /usr/bin/python3 hooks/probe.py > hook-observed\n",
    )
    .expect("write fetched post-checkout hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("hook metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).expect("make hook executable");
    std::fs::write(source.join("tracked"), "attacker-chosen content\n")
        .expect("write tracked content");
    git(Command::new("git").args(["add", "."]).current_dir(&source));
    git(Command::new("git")
        .args(["commit", "-qm", "fixture"])
        .current_dir(&source));

    // Materialise no remote-controlled files yet: this mirrors clone's first
    // phase closely enough that the hook itself arrives only at checkout.
    git(Command::new("git")
        .args(["clone", "-q", "--no-checkout", "--"])
        .arg(&source)
        .arg(&dest)
        .current_dir(clones.path()));
    assert!(!dest.join("hooks/post-checkout").exists(), "premise");
    git(Command::new("git")
        .args(["config", "core.hooksPath", "hooks"])
        .current_dir(&dest));

    let home_path: &Path = home.path();
    let checkout = super::test_env::with_env(
        &[
            ("HOME", Some(home_path.as_os_str())),
            ("SSH_AUTH_SOCK", None),
        ],
        || {
            assert!(
                std::env::var_os("SSH_AUTH_SOCK").is_none(),
                "premise: the hook must recover and set the variable itself"
            );
            let policy = super::policy_for_clone_checkout(clones.path())
                .expect("checkout policy must build");
            super::network_exec::network_command_without_credential(
                &policy,
                &dest,
                &["checkout", "-f"],
            )
        },
    );
    let output = checkout.output().await.expect("checkout starts");
    assert!(
        output.status.success(),
        "checkout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let observed =
        std::fs::read_to_string(dest.join("hook-observed")).expect("the fetched hook must run");
    assert_eq!(
        observed.trim(),
        format!(
            "tcp=connected|unix=errno:{}|sock={}",
            libc::EPERM,
            socket_path.display()
        ),
        "the hook must read the pathname from HOME and set SSH_AUTH_SOCK itself; \
         the AF_UNIX connect must then fail with EPERM while TCP still connects"
    );

    let (mut accepted, _) = tcp_listener
        .accept()
        .expect("the hook's TCP connection must reach the listener");
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut accepted, &mut bytes).expect("read TCP marker");
    assert_eq!(bytes, b"checkout-tcp");
}
