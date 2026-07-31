//! #188: "a real git ls-remote over SSH succeeds through the composed
//! launcher" — the one acceptance box that needs a real SSH server, not a
//! unit-level probe. Everything else #188 changed (the known_hosts
//! carve-out itself, the agent-socket grant, Strict-tier absence) is already
//! proven by `escape_suite::ssh_known_hosts_carveout` (a real hostile-hook
//! probe through the production `policy_for_repo`) and by `argv.rs`'s
//! structural tests (production `policy_for`/`policy_for_clone` really
//! populate both grants, in the real `$HOME`). This file is the remaining
//! end-to-end proof: a real `ssh` transport, with a real host-key handshake
//! and a real agent-backed pubkey authentication, actually completing
//! through the real `gv-sandbox` launcher.
//!
//! # Why this file builds its own `Policy` rather than calling `policy_for`
//!
//! `policy_for` reads the **real** `$HOME` environment variable, which every
//! other test in this crate that touches `policy_for` also reads —
//! concurrently, since `cargo test` runs many tests in parallel threads
//! within one process. Two ways to get a *hermetic* `$HOME` for this test
//! were considered and rejected:
//!
//! * **Redirect `$HOME` process-wide** (`std::env::set_var("HOME", ..)` for
//!   the duration of this test). `HOME` is read by a large fraction of this
//!   crate's test suite, not just the handful of tests that mention it by
//!   name, so this would race every concurrently-running test that resolves
//!   `policy_for`'s `$HOME` — a much larger blast radius than the
//!   `SSH_AUTH_SOCK` mutation `argv.rs`'s tests already do (verified nothing
//!   else touches that key; the same is not true of `HOME`).
//! * **Mutate the operator's real `~/.ssh/known_hosts`** (append a throwaway
//!   host-key line, Drop-guarded removal). This is the most production-
//!   faithful option, but it is a genuinely new risk category for this
//!   crate's test suite — the first test whose most faithful implementation
//!   touches a real dotfile outside a tempdir — and it was flagged as a real
//!   design fork rather than decided unilaterally. Rejected here in favour
//!   of the option below, which needs no such trade.
//!
//! So this module builds a `Policy` directly, field-for-field the same
//! *shape* `policy_for` builds for the Network tier (`default_system_trees`,
//! `secret_excludes_for_home`, `ssh_known_hosts_carveout`,
//! `DEFAULT_GIT_PORTS`), but pointed at a throwaway `$HOME`-shaped tempdir
//! instead of the operator's real one, and with the agent-socket path
//! spliced in directly rather than read from the real `SSH_AUTH_SOCK`. What
//! that leaves genuinely unexercised **here** is `policy_for`'s own
//! `std::env::var_os("HOME")`/`("SSH_AUTH_SOCK")` reads — which is exactly
//! what `argv.rs`'s `production_policy_for_carries_the_known_hosts_carveout_in_network_only`
//! and `production_policy_for_wires_the_agent_socket_grant_into_the_network_argv`
//! cover, against the real environment. The two files together are what add
//! up to the full claim; neither alone would.
//!
//! Every leg still spawns through `spawn::command_async` — the crate's one
//! production seam — even though this file sits outside the escape
//! battery's `EscapeCase`/`run_case` harness (it is not a containment claim
//! in that harness's shape: there is no denial/paired-positive pair here,
//! only "does the whole real transport work end to end").
//!
//! `net_ports` here is the fixture's own ephemeral port, not `22`: binding
//! port 22 needs root, which a hermetic `cargo test` run must not require.
//! This is a deliberate, narrow substitution — `add_net_rule`
//! (`bin/gv-sandbox/main.rs`) is parameterised by port number with no
//! special-casing of `22`, so a rule proven here for an arbitrary port
//! exercises the identical mechanism production uses for `22`.

use super::*;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Poll until `f` returns `true` or `deadline` passes. Used instead of a
/// fixed `sleep` for both `sshd` and the agent socket coming up: a fixed
/// sleep is either wastefully long or, on a loaded host (this suite already
/// runs a real bwrap-composed Strict tier in the same binary), flaky-short.
fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A free TCP port, found by binding to port 0 and releasing it immediately.
/// A small TOCTOU race is possible (another process could grab it before
/// `sshd` binds) but is rare enough on a dev/CI box that a collision would
/// fail this test loudly and diagnosably rather than silently — an
/// acceptable trade for not needing a second `PortClaim`-style registry
/// scoped to a set of ports this module does not share with anything else.
fn free_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener.local_addr().expect("listener local_addr").port()
}

/// A child process killed (and reaped) on drop, so a panicking assertion
/// mid-test leaves no `sshd`/`ssh-agent` behind.
struct KillOnDrop(Child, &'static str);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("{what}: could not run: {e}"));
    assert!(status.success(), "{what}: exited with {status}");
}

/// A throwaway local SSH server plus agent, real ed25519 host and client
/// keys, a bare repository with one seeded commit, and a `$HOME`-shaped
/// tempdir carrying exactly the `known_hosts` line #188's carve-out is meant
/// to expose. Everything here is unsandboxed setup — the fixture, not the
/// claim under test.
struct SshFixture {
    // Declared first so `Drop` kills these processes before the tempdirs
    // beneath them (`_home`, `_work`) are removed — struct fields drop in
    // declaration order.
    _sshd: KillOnDrop,
    _agent: KillOnDrop,
    _home: tempfile::TempDir,
    _work: tempfile::TempDir,
    /// The `$HOME`-shaped tempdir's path — what the sandboxed policy grants
    /// as `ro_trees`/`ro_carveouts`/`secret_excludes` are all relative to.
    home: PathBuf,
    /// A plain, non-repository directory to run `git -C <dir> ls-remote`
    /// from. `ls-remote` with an explicit URL needs no local repository at
    /// all — only a shorthand remote *name* would.
    cwd: PathBuf,
    port: u16,
    agent_sock: PathBuf,
    repo_url: String,
    /// The exact ref line seeded into the bare repository, asserted present
    /// in `ls-remote`'s real stdout — evidence the transport actually
    /// completed a real exchange, not merely that the process exited 0.
    seeded_ref: String,
}

impl SshFixture {
    fn build() -> Self {
        let work = tempfile::tempdir().expect("work tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let home_path = home.path().to_path_buf();
        std::fs::create_dir_all(home_path.join(".ssh")).expect("mkdir $HOME/.ssh");

        // --- keys -----------------------------------------------------
        let host_key = work.path().join("host_key");
        run(
            Command::new("ssh-keygen").args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                host_key.to_str().expect("utf8 path"),
            ]),
            "ssh-keygen (host key)",
        );
        let client_key = work.path().join("client_key");
        run(
            Command::new("ssh-keygen").args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                client_key.to_str().expect("utf8 path"),
            ]),
            "ssh-keygen (client key)",
        );
        let client_pub =
            std::fs::read_to_string(work.path().join("client_key.pub")).expect("read client pub");
        let host_pub =
            std::fs::read_to_string(work.path().join("host_key.pub")).expect("read host pub");

        // --- bare repository with one seeded commit --------------------
        let scratch = work.path().join("scratch");
        std::fs::create_dir_all(&scratch).expect("mkdir scratch");
        let git_env = |cmd: &mut Command| {
            cmd.env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &home_path)
                .env("GIT_AUTHOR_NAME", "gv-ssh-fixture")
                .env("GIT_AUTHOR_EMAIL", "gv-ssh-fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "gv-ssh-fixture")
                .env("GIT_COMMITTER_EMAIL", "gv-ssh-fixture@example.invalid");
        };
        let mut c = Command::new("git");
        git_env(&mut c);
        run(
            c.args(["init", "-q", "-b", "main", scratch.to_str().unwrap()]),
            "git init",
        );
        std::fs::write(scratch.join("f.txt"), "gv-vista #188 ssh fixture\n").expect("write f.txt");
        let mut c = Command::new("git");
        git_env(&mut c);
        run(
            c.args(["-C", scratch.to_str().unwrap(), "add", "-A"]),
            "git add",
        );
        let mut c = Command::new("git");
        git_env(&mut c);
        run(
            c.args([
                "-C",
                scratch.to_str().unwrap(),
                "commit",
                "-q",
                "-m",
                "seed",
            ]),
            "git commit",
        );
        let mut c = Command::new("git");
        git_env(&mut c);
        let out = c
            .args(["-C", scratch.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse runs");
        assert!(out.status.success(), "git rev-parse HEAD failed");
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let seeded_ref = format!("{head}\trefs/heads/main");

        let repo_git = work.path().join("repo.git");
        let mut c = Command::new("git");
        git_env(&mut c);
        run(
            c.args([
                "clone",
                "-q",
                "--bare",
                scratch.to_str().unwrap(),
                repo_git.to_str().unwrap(),
            ]),
            "git clone --bare",
        );

        // --- authorized_keys: force git-upload-pack, nothing else -------
        let authorized_keys = work.path().join("authorized_keys");
        std::fs::write(
            &authorized_keys,
            format!(
                "command=\"git-upload-pack '{}'\",no-port-forwarding,no-X11-forwarding,\
                 no-agent-forwarding,no-pty {}",
                repo_git.display(),
                client_pub.trim()
            ),
        )
        .expect("write authorized_keys");

        // --- sshd, foreground, loopback, ephemeral port -----------------
        let port = free_tcp_port();
        let sshd_config = work.path().join("sshd_config");
        std::fs::write(
            &sshd_config,
            format!(
                "Port {port}\n\
                 ListenAddress 127.0.0.1\n\
                 HostKey {host_key}\n\
                 AuthorizedKeysFile {authorized_keys}\n\
                 PubkeyAuthentication yes\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 ChallengeResponseAuthentication no\n\
                 UsePAM no\n\
                 StrictModes no\n\
                 PrintMotd no\n\
                 LogLevel ERROR\n",
                host_key = host_key.display(),
                authorized_keys = authorized_keys.display(),
            ),
        )
        .expect("write sshd_config");

        let sshd_bin = ["/usr/sbin/sshd", "/usr/bin/sshd"]
            .into_iter()
            .find(|p| std::path::Path::new(p).is_file())
            .unwrap_or_else(|| {
                panic!("no sshd binary found at any of the reviewed absolute paths")
            });
        let sshd_child = Command::new(sshd_bin)
            .args(["-D", "-e", "-f", sshd_config.to_str().unwrap()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn sshd: {e}"));
        let sshd = KillOnDrop(sshd_child, "sshd");
        let up = wait_until(Duration::from_secs(5), || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(
            up,
            "sshd on 127.0.0.1:{port} never accepted a TCP connection"
        );

        // --- ssh-agent, foreground, at a chosen socket path -------------
        let agent_sock = work.path().join("agent.sock");
        let agent_child = Command::new("/usr/bin/ssh-agent")
            .args(["-D", "-a", agent_sock.to_str().unwrap()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn ssh-agent: {e}"));
        let agent = KillOnDrop(agent_child, "ssh-agent");
        let sock_up = wait_until(Duration::from_secs(5), || agent_sock.exists());
        assert!(
            sock_up,
            "ssh-agent never created its socket at {agent_sock:?}"
        );

        let mut add = Command::new("/usr/bin/ssh-add");
        add.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("SSH_AUTH_SOCK", &agent_sock)
            .arg(&client_key);
        run(&mut add, "ssh-add");

        // --- known_hosts: the exact carve-out target, pre-populated -----
        // Bracketed host:port form (RFC-less but OpenSSH-standard) because
        // the port is non-default; a bare `127.0.0.1` line would not match.
        let known_hosts_line = format!("[127.0.0.1]:{port} {}", host_pub.trim());
        std::fs::write(
            home_path.join(".ssh/known_hosts"),
            format!("{known_hosts_line}\n"),
        )
        .expect("write known_hosts");

        let repo_url = format!("ssh://127.0.0.1:{port}{}", repo_git.display());

        Self {
            _sshd: sshd,
            _agent: agent,
            _home: home,
            _work: work,
            home: home_path,
            cwd: scratch, // any existing directory works as the `-C` target
            port,
            agent_sock,
            repo_url,
            seeded_ref,
        }
    }

    /// The Network-tier policy this fixture's launcher runs under —
    /// identical in shape to what `policy_for` builds for `Tier::Network`,
    /// see the module doc for why it is hand-built rather than routed
    /// through that function.
    ///
    /// `with_known_hosts_carveout` and `with_agent_socket` let the negative-
    /// control tests below drop exactly one #188 grant at a time while
    /// keeping everything else — including the other grant — identical, so
    /// a failure is attributable to the one grant that changed.
    fn policy(&self, with_known_hosts_carveout: bool, with_agent_socket: bool) -> Policy {
        let (mut rw, mut ro) = default_system_trees(Tier::Network);
        ro.push(self.home.clone());
        if with_agent_socket {
            rw.push(self.agent_sock.clone());
        }
        Policy {
            tier: Tier::Network,
            shim: shim::shim_path()
                .expect("gv-sandbox must be built")
                .to_path_buf(),
            bwrap: None,
            rw_trees: rw,
            ro_trees: ro,
            secret_excludes: secret_excludes_for_home(&self.home),
            ro_carveouts: if with_known_hosts_carveout {
                ssh_known_hosts_carveout(&self.home)
            } else {
                Vec::new()
            },
            net_ports: vec![self.port],
            hook_mode: HookMode::Run,
        }
    }

    fn ls_remote_env(&self) -> Vec<(&'static str, String)> {
        // OpenSSH resolves `~` in `UserKnownHostsFile`'s default
        // (`~/.ssh/known_hosts`) through `getpwuid(getuid())`'s passwd-database
        // home directory, **not** through the `$HOME` environment variable —
        // a deliberate OpenSSH hardening against `$HOME` spoofing. Measured
        // directly: with only `HOME` pinned to this fixture's tempdir, `ssh`
        // still tried `/home/<real-user>/.ssh/known_hosts` and got a sandbox
        // `Permission denied` there (that path is outside every grant this
        // policy builds), never touching the fixture's own known_hosts at
        // all. So `UserKnownHostsFile` is named explicitly here, pointed at
        // the exact path `Policy::ro_carveouts` grants — the same
        // `#[cfg(test)]` discipline as `command_async`'s other pinned-env
        // tests, just with one more `-o` than usual because `ssh`, unlike
        // `git`, does not trust `$HOME` for this.
        let known_hosts = self.home.join(".ssh/known_hosts");
        vec![
            ("PATH", "/usr/bin:/bin".to_string()),
            ("HOME", self.home.to_string_lossy().into_owned()),
            (
                "SSH_AUTH_SOCK",
                self.agent_sock.to_string_lossy().into_owned(),
            ),
            ("GIT_TERMINAL_PROMPT", "0".to_string()),
            (
                "GIT_SSH_COMMAND",
                format!(
                    "ssh -o BatchMode=yes -o UserKnownHostsFile={} -o GlobalKnownHostsFile=/dev/null",
                    known_hosts.display()
                ),
            ),
        ]
    }
}

async fn ls_remote(fixture: &SshFixture, policy: &Policy) -> std::process::Output {
    spawn::command_async(policy, &fixture.cwd, &["ls-remote", &fixture.repo_url])
        .pinned_env_for_test(&fixture.ls_remote_env())
        .output()
        .await
        .expect("the composed launcher spawns")
}

/// The acceptance box itself: a real `git ls-remote` over a real `ssh://`
/// transport — real host-key verification against `known_hosts`, real
/// agent-backed pubkey authentication — succeeding through the composed
/// Network-tier launcher.
#[tokio::test]
async fn a_real_ssh_ls_remote_succeeds_through_the_composed_launcher() {
    let fixture = SshFixture::build();
    let policy = fixture.policy(true, true);
    let out = ls_remote(&fixture, &policy).await;
    assert!(
        out.status.success(),
        "ls-remote failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&fixture.seeded_ref),
        "ls-remote succeeded but did not report the seeded ref — real evidence of a \
         completed exchange, not just an exit code. wanted a line containing {:?}, got:\n{stdout}",
        fixture.seeded_ref
    );
}

/// Negative control 1/2: drop the known_hosts carve-out, keep the agent
/// socket grant. Must fail — proves the carve-out is load-bearing, not a
/// grant riding along on something else that happens to work.
#[tokio::test]
async fn ls_remote_fails_without_the_known_hosts_carveout() {
    let fixture = SshFixture::build();
    let policy = fixture.policy(false, true);
    let out = ls_remote(&fixture, &policy).await;
    assert!(
        !out.status.success(),
        "ls-remote succeeded with no known_hosts carve-out granted — this control is \
         supposed to fail, or it proves nothing about the carve-out being necessary"
    );
}

/// Not a negative control, deliberately — see why below. Same shape as
/// `ls_remote_fails_without_the_known_hosts_carveout`, but the **opposite**
/// expectation, and the difference between the two is the actual finding.
///
/// #188's agent-socket `rw` grant is not what gates connectivity on *this*
/// kernel — measured directly (`ssh_agent_socket_grant`'s doc comment in
/// `mod.rs`): a raw Landlock probe against a real `AF_UNIX` `SOCK_STREAM`
/// listener, under a live ruleset proven live by a same-run control read
/// that correctly returned `EACCES`, showed `connect()` to a **pathname**
/// socket succeeding identically whether the socket carried no rule at all,
/// a read-only rule, or a read-write one. This test is that same
/// measurement one layer up, through the real launcher instead of a raw
/// probe: dropping the socket grant from an otherwise-identical policy must
/// **not** break `ls-remote`, because nothing in this stack is actually
/// enforcing it today — the seccomp `AF_UNIX` denial that genuinely does
/// gate the socket is Strict-tier-only, and Strict has no network access at
/// all, so it cannot be exercised in this same end-to-end shape.
///
/// What this guards is the measurement itself, not a security property: if
/// a future Landlock ABI starts mediating pathname `AF_UNIX` sockets, this
/// assertion flips from pass to fail — the signal that the socket grant has
/// become load-bearing and this comment (and `ssh_agent_socket_grant`'s)
/// needs updating, not silencing.
#[tokio::test]
async fn ls_remote_still_succeeds_without_the_agent_socket_grant_on_this_kernel() {
    let fixture = SshFixture::build();
    let policy = fixture.policy(true, false);
    let out = ls_remote(&fixture, &policy).await;
    assert!(
        out.status.success(),
        "ls-remote failed with the agent-socket grant withheld: stdout={} stderr={}. If \
         this is a genuine regression rather than a kernel/Landlock behaviour change, \
         #188's SSH_AUTH_SOCK rw grant has become load-bearing and this test's premise \
         (and ssh_agent_socket_grant's doc comment) needs revisiting, not silencing.",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
