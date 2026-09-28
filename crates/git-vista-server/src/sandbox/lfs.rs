//! #831's explicit Git LFS checkout configuration.
//!
//! Fresh-clone checkout cannot read system or global Git config: a fetched
//! `.gitattributes` can choose any filter name, so restoring either scope would
//! also restore every operator-selected executable filter. On this host that
//! hides Git LFS's system-only `filter.lfs.*` settings and makes Git silently
//! write pointer text. This module restores only the values the server authors.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;

/// Reviewed installation locations, never `PATH`. The first executable file
/// wins. These are the same system prefixes already read-granted by the sandbox.
/// A package installed elsewhere is unavailable to this boundary until its
/// absolute location is reviewed and added here.
#[cfg(unix)]
const GIT_LFS_CANDIDATES: &[&str] = &["/usr/bin/git-lfs", "/bin/git-lfs", "/usr/local/bin/git-lfs"];

/// Deliberately absent. When no candidate exists, keeping a required filter
/// with this program makes an LFS-attributed checkout fail at the exact file
/// instead of silently leaving its pointer. Non-LFS repositories never invoke
/// it and remain cloneable.
#[cfg(any(unix, test))]
const GIT_LFS_UNAVAILABLE: &str = "/dev/null/git-vista-lfs-unavailable";

/// Fail-closed target for plaintext object-action URLs advertised by an LFS
/// batch response. Git LFS's built-in HTTP adapter applies `url.*.insteadOf`
/// to action hrefs only when `lfs.transfer.enablehrefrewrite` is enabled. The
/// checkout config below forces both settings, replacing every leading
/// `http://` with this HTTPS URL. Port 1 is deliberately outside
/// `CLONE_CHECKOUT_PORTS`, so the unchanged Landlock port boundary rejects the
/// rewritten request before it can reach any service.
pub(crate) const PLAINTEXT_ACTION_REFUSAL_URL: &str =
    "https://127.0.0.1:1/git-vista-refused-plaintext-lfs-action/";

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(unix)]
fn program() -> &'static str {
    GIT_LFS_CANDIDATES
        .iter()
        .copied()
        .find(|candidate| is_executable_file(Path::new(candidate)))
        .unwrap_or(GIT_LFS_UNAVAILABLE)
}

/// ADR 0152 refuses server startup on this platform. If a future caller
/// bypasses that gate, refuse here too: no Windows LFS executable or sandbox
/// has been reviewed, so neither PATH lookup nor a Unix sentinel is honest.
#[cfg(not(unix))]
fn program() -> &'static str {
    panic!("Git LFS checkout requires the supported Unix sandbox; unavailable on this platform")
}

/// Git LFS's ordinary HTTP endpoint for the validated clone URL.
///
/// Fragment and query text are request data for the Git transfer, not part of
/// an LFS endpoint path. URL userinfo may itself be a credential, so it must not
/// cross from the transfer into checkout argv. The result is still data passed
/// as one argv element; it is never parsed as a command.
fn endpoint(clone_url: &str) -> String {
    let without_fragment = clone_url.split_once('#').map_or(clone_url, |(url, _)| url);
    let without_query = without_fragment
        .split_once('?')
        .map_or(without_fragment, |(url, _)| url);
    let without_userinfo = without_query
        .split_once("://")
        .map(|(scheme, rest)| {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at(authority_end);
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            format!("{scheme}://{host}{path}")
        })
        .unwrap_or_else(|| without_query.to_string());
    format!("{}/info/lfs", without_userinfo.trim_end_matches('/'))
}

/// Command-line config for the one checkout allowed to invoke Git LFS.
///
/// `process`, `smudge`, and `required` are all pinned because Git can fall back
/// between filter protocols. A repository-local value must not redirect either
/// route. `basictransfersonly` prevents a server-selected custom transfer from
/// being advertised. That setting alone is insufficient: measured with 3.7.1,
/// an explicit `lfs.standalonetransferagent` still executes. Both the generic
/// and exact-URL standalone selectors are therefore reset. The generic reset
/// is independently necessary (#885): fresh clone admits `git://` URLs, whose
/// LFS endpoint Git LFS 3.7.1 changes to `https://` before selector lookup, so
/// the `lfs.git://...` reset does not match. The executable-marker test below
/// pins this case. Git LFS's tracked-config denylist remains a separate layer:
/// a selector scoped to the converted URL could outrank the generic reset if
/// those currently unsafe keys were admitted. See upstream v3.7.1
/// `lfsapi/endpoint_finder.go` (`endpointFromGitUrl`) and `tq/manifest.go`
/// (`findStandaloneTransfer`). The exact URL's basic-only value is pinned as
/// well so URL-match specificity cannot outrank the generic setting. Git LFS
/// also admits `lfs.gitprotocol` from tracked config and uses it to replace a
/// `git://` endpoint's scheme before that lookup, so pinning it to HTTPS keeps
/// repository content from selecting the built-in file adapter or a plaintext
/// batch endpoint. `skipdownloaderrors`, `fetchinclude`, and `fetchexclude` are
/// also pinned: Git LFS otherwise accepts each from a tracked `.lfsconfig`, can
/// deliberately leave pointer text for a selected path, and still report
/// filter success. Pinning `lfs.url` prevents a fetched `.lfsconfig` from
/// switching checkout to SSH and thereby selecting `ssh` or
/// `git-lfs-authenticate` as another executable path. Finally, href rewriting
/// maps every direct `http://` object action to a fixed HTTPS URL on a denied
/// port. This covers batch-selected URLs, which are not redirects and never
/// pass through Git LFS's HTTPS-to-HTTP redirect guard.
fn checkout_config_for_program(clone_url: &str, program: &str) -> Vec<String> {
    let endpoint = endpoint(clone_url);
    vec![
        "-c".into(),
        format!("filter.lfs.process={program} filter-process"),
        "-c".into(),
        format!("filter.lfs.smudge={program} smudge"),
        "-c".into(),
        "filter.lfs.required=true".into(),
        "-c".into(),
        "lfs.skipdownloaderrors=false".into(),
        "-c".into(),
        "lfs.fetchinclude=".into(),
        "-c".into(),
        "lfs.fetchexclude=".into(),
        "-c".into(),
        "lfs.gitprotocol=https".into(),
        "-c".into(),
        "lfs.basictransfersonly=true".into(),
        "-c".into(),
        format!("lfs.{endpoint}.basictransfersonly=true"),
        "-c".into(),
        "lfs.standalonetransferagent=".into(),
        "-c".into(),
        format!("lfs.{endpoint}.standalonetransferagent="),
        "-c".into(),
        "lfs.transfer.enablehrefrewrite=true".into(),
        "-c".into(),
        format!("url.{PLAINTEXT_ACTION_REFUSAL_URL}.insteadOf=http://"),
        "-c".into(),
        format!("lfs.url={endpoint}"),
    ]
}

pub(super) fn checkout_config(clone_url: &str) -> Vec<String> {
    checkout_config_for_program(clone_url, program())
}

/// Test-only fault injection for the loud-unavailability behavior. Production
/// exposes no builder that accepts an executable selected by its caller.
#[cfg(test)]
pub(super) fn checkout_config_with_program(clone_url: &str, program: &str) -> Vec<String> {
    checkout_config_for_program(clone_url, program)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::process::{Child, Command, Output, Stdio};
    #[cfg(unix)]
    use std::sync::mpsc;

    #[cfg(unix)]
    struct PlainObjectServer {
        port: u16,
        request: mpsc::Receiver<Vec<u8>>,
    }

    #[cfg(unix)]
    impl PlainObjectServer {
        fn start(contents: &'static [u8]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind plaintext object server");
            let port = listener.local_addr().expect("object server address").port();
            let (request_tx, request) = mpsc::channel();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else {
                        return;
                    };
                    let mut bytes = vec![0; 8192];
                    let count = stream.read(&mut bytes).unwrap_or(0);
                    bytes.truncate(count);
                    let _ = request_tx.send(bytes.clone());
                    if bytes.starts_with(b"GET ") {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            contents.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(contents);
                    }
                }
            });
            Self { port, request }
        }
    }

    #[cfg(unix)]
    struct HttpsBatchServer {
        child: Child,
        port: u16,
        request_path: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl HttpsBatchServer {
        fn start(root: &Path, name: &str, response_body: String) -> Self {
            let key = root.join(format!("{name}-key.pem"));
            let certificate = root.join(format!("{name}-cert.pem"));
            let request_path = root.join(format!("{name}-request"));
            let response_path = root.join(format!("{name}-response"));
            let responder = root.join(format!("{name}-tls-server.py"));
            let generated = Command::new("openssl")
                .args([
                    "req",
                    "-x509",
                    "-newkey",
                    "rsa:2048",
                    "-nodes",
                    "-subj",
                    "/CN=127.0.0.1",
                    "-days",
                    "1",
                    "-keyout",
                ])
                .arg(&key)
                .arg("-out")
                .arg(&certificate)
                .output()
                .expect("openssl certificate generation starts");
            assert!(
                generated.status.success(),
                "openssl certificate generation failed: {}",
                String::from_utf8_lossy(&generated.stderr)
            );

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(), response_body
            );
            std::fs::write(&response_path, response).expect("write TLS response");
            std::fs::write(
                &responder,
                r#"import socket
import ssl
import sys

port, certificate, key, response_path, request_path = sys.argv[1:]
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(certificate, key)
with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", int(port)))
    listener.listen()
    while True:
        connection, _ = listener.accept()
        try:
            with context.wrap_socket(connection, server_side=True) as stream:
                request = b""
                while b"\r\n\r\n" not in request:
                    request += stream.recv(4096)
                headers, body = request.split(b"\r\n\r\n", 1)
                length = 0
                for line in headers.split(b"\r\n"):
                    if line.lower().startswith(b"content-length:"):
                        length = int(line.split(b":", 1)[1].strip())
                while len(body) < length:
                    body += stream.recv(4096)
                with open(request_path, "wb") as recorded:
                    recorded.write(headers + b"\r\n\r\n" + body)
                with open(response_path, "rb") as prepared:
                    stream.sendall(prepared.read())
        except (ConnectionError, ssl.SSLError):
            connection.close()
"#,
            )
            .expect("write TLS server fixture");

            let reservation =
                TcpListener::bind("127.0.0.1:0").expect("reserve TLS batch server port");
            let port = reservation
                .local_addr()
                .expect("TLS reservation address")
                .port();
            drop(reservation);
            let mut child = Command::new("python3")
                .arg(&responder)
                .arg(port.to_string())
                .arg(&certificate)
                .arg(&key)
                .arg(&response_path)
                .arg(&request_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("Python TLS batch server starts");

            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(
                child.try_wait().expect("inspect TLS server").is_none(),
                "Python TLS batch server exited before accepting connections"
            );

            Self {
                child,
                port,
                request_path,
            }
        }

        fn request(&self) -> Vec<u8> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Ok(bytes) = std::fs::read(&self.request_path) {
                    if !bytes.is_empty() {
                        return bytes;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "TLS batch request was not recorded"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    #[cfg(unix)]
    impl Drop for HttpsBatchServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(unix)]
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("create copied directory");
        for entry in std::fs::read_dir(from).expect("read copied directory") {
            let entry = entry.expect("read copied entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("copied entry type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).expect("copy file");
            }
        }
    }

    #[cfg(unix)]
    fn git_output(repo: &Path, args: &[std::ffi::OsString]) -> Output {
        super::super::network_exec::fixture_git_output(repo, args)
    }

    #[cfg(unix)]
    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write executable fixture");
        let mut permissions = std::fs::metadata(path)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("make fixture executable");
    }

    #[cfg(unix)]
    fn lfs_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let clones = tempfile::tempdir().expect("clones root");
        let source = clones.path().join("source");
        std::fs::create_dir(&source).expect("create source repository");
        super::super::network_exec::run_fixture_git(&source, ["init", "-q"]);
        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "user.name", "git-vista-test"],
        );
        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(
            source.join(".gitattributes"),
            "asset.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .expect("write LFS attributes");
        std::fs::write(source.join("asset.bin"), b"materialised LFS bytes\0\xff\n")
            .expect("write LFS asset");
        super::super::network_exec::run_fixture_git(&source, ["add", "."]);
        super::super::network_exec::run_fixture_git(&source, ["commit", "-qm", "LFS fixture"]);
        (clones, source)
    }

    #[cfg(unix)]
    fn clone_with_cached_lfs_object(
        clones: &Path,
        source: &Path,
        name: &str,
    ) -> std::path::PathBuf {
        let dest = clones.join(name);
        super::super::network_exec::run_fixture_git(
            clones,
            [
                std::ffi::OsString::from("clone"),
                std::ffi::OsString::from("-q"),
                std::ffi::OsString::from("--no-checkout"),
                std::ffi::OsString::from("--"),
                source.as_os_str().to_owned(),
                dest.as_os_str().to_owned(),
            ],
        );
        copy_tree(
            &source.join(".git/lfs/objects"),
            &dest.join(".git/lfs/objects"),
        );
        dest
    }

    #[cfg(unix)]
    fn clone_without_lfs_object(clones: &Path, source: &Path, name: &str) -> std::path::PathBuf {
        let dest = clones.join(name);
        super::super::network_exec::run_fixture_git(
            clones,
            [
                std::ffi::OsString::from("clone"),
                std::ffi::OsString::from("-q"),
                std::ffi::OsString::from("--no-checkout"),
                std::ffi::OsString::from("--"),
                source.as_os_str().to_owned(),
                dest.as_os_str().to_owned(),
            ],
        );
        super::super::network_exec::run_fixture_git(&dest, ["config", "http.sslVerify", "false"]);
        super::super::network_exec::run_fixture_git(
            &dest,
            ["config", "lfs.transfer.maxretries", "1"],
        );
        dest
    }

    #[test]
    fn endpoint_discards_query_fragment_and_appends_the_standard_path() {
        assert_eq!(
            endpoint("https://user:secret@example.invalid/org/repo.git/?token=data#fragment"),
            "https://example.invalid/org/repo.git/info/lfs"
        );
    }

    #[test]
    fn checkout_config_pins_every_lfs_executable_and_transfer_selector() {
        #[cfg(unix)]
        let selected = program();
        #[cfg(not(unix))]
        let selected = "reviewed-test-program";
        #[cfg(unix)]
        let config = checkout_config("https://example.invalid/repo.git");
        #[cfg(not(unix))]
        let config = checkout_config_for_program("https://example.invalid/repo.git", selected);
        let joined = config.join("\n");
        assert!(joined.contains(&format!("filter.lfs.process={selected} filter-process")));
        assert!(joined.contains(&format!("filter.lfs.smudge={selected} smudge")));
        assert!(
            !joined.contains("%f"),
            "a remote-controlled pathname is unnecessary in the server-authored command"
        );
        assert!(joined.contains("filter.lfs.required=true"));
        assert!(joined.contains("lfs.skipdownloaderrors=false"));
        assert!(joined.contains("lfs.fetchinclude="));
        assert!(joined.contains("lfs.fetchexclude="));
        assert!(joined.contains("lfs.gitprotocol=https"));
        assert!(joined.contains("lfs.basictransfersonly=true"));
        assert!(joined.contains("lfs.standalonetransferagent="));
        assert!(joined
            .contains("lfs.https://example.invalid/repo.git/info/lfs.standalonetransferagent="));
        assert!(joined.contains("lfs.transfer.enablehrefrewrite=true"));
        assert!(joined.contains(&format!(
            "url.{PLAINTEXT_ACTION_REFUSAL_URL}.insteadOf=http://"
        )));
        assert!(joined.contains("lfs.url=https://example.invalid/repo.git/info/lfs"));
        assert!(
            !selected.contains(' '),
            "reviewed candidates need no shell quoting"
        );
    }

    /// An HTTPS batch response can select a direct object URL; that URL is not
    /// a redirect and therefore never reaches Git LFS's downgrade guard. Drive
    /// the real `git checkout -f` -> `git-lfs filter-process` -> batch -> basic
    /// adapter path twice. The unprotected control reaches the plaintext object
    /// server (and may then meet the checkout sandbox's existing cross-directory
    /// rename restriction). The production configuration receives the same
    /// response but must fail before that server observes any request.
    ///
    /// MUTATION 1 (remove the mechanism): remove both command-line href rewrite
    /// pins. The guarded leg issues the control's plaintext GET.
    /// MUTATION 2 (invert the condition): make the refusal rewrite match
    /// `https://` instead of `http://`. The direct plaintext href again reaches
    /// the object server.
    #[tokio::test]
    #[cfg(unix)]
    async fn an_https_lfs_batch_cannot_select_a_direct_plaintext_object_action() {
        use sha2::{Digest, Sha256};

        const OBJECT: &[u8] = b"materialised LFS bytes\0\xff\n";
        assert_ne!(
            program(),
            GIT_LFS_UNAVAILABLE,
            "this integration test requires one reviewed git-lfs installation"
        );
        let (clones, source) = lfs_fixture();
        let oid = format!("{:x}", Sha256::digest(OBJECT));

        let run = |name: &str, guarded: bool| {
            let dest = clone_without_lfs_object(clones.path(), &source, name);
            let object_server = PlainObjectServer::start(OBJECT);
            let batch_body = format!(
                "{{\"transfer\":\"basic\",\"objects\":[{{\"oid\":\"{oid}\",\"size\":{},\"actions\":{{\"download\":{{\"href\":\"http://127.0.0.1:{}/object\"}}}}}}]}}",
                OBJECT.len(), object_server.port
            );
            let batch_server = HttpsBatchServer::start(clones.path(), name, batch_body);
            let clone_url = format!("https://127.0.0.1:{}/repo.git", batch_server.port);
            let mut policy = super::super::policy_for_clone_checkout(clones.path())
                .expect("checkout policy must build");
            policy.0.net_ports = vec![batch_server.port, object_server.port];

            let command = if guarded {
                super::super::network_exec::lfs_checkout_command(&policy, &dest, &clone_url)
            } else {
                let mut unguarded = Vec::new();
                let guarded_config = checkout_config(&clone_url);
                let (pairs, remainder) = guarded_config.as_chunks::<2>();
                assert!(
                    remainder.is_empty(),
                    "Git config args must be -c/value pairs"
                );
                for pair in pairs {
                    if pair[1] != "lfs.transfer.enablehrefrewrite=true"
                        && !pair[1].starts_with("url.https://127.0.0.1:1/")
                    {
                        unguarded.extend_from_slice(pair);
                    }
                }
                unguarded.extend(["checkout".to_string(), "-f".to_string()]);
                let refs: Vec<&str> = unguarded.iter().map(String::as_str).collect();
                super::super::network_exec::network_command_without_credential(
                    &policy, &dest, &refs,
                )
            };
            (dest, object_server, batch_server, command)
        };

        let (control_dest, control_object, control_batch, control_command) =
            run("plaintext-action-control", false);
        let control = control_command
            .output()
            .await
            .expect("unprotected control checkout starts");
        let control_batch_request = control_batch.request();
        let control_object_request = control_object
            .request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("control plaintext object request observed");
        let _ = (control_dest, control);
        assert!(
            control_batch_request.starts_with(b"POST ")
                && control_batch_request
                    .windows(b"/objects/batch".len())
                    .any(|window| window == b"/objects/batch"),
            "control did not make the expected HTTPS LFS batch request: {}",
            String::from_utf8_lossy(&control_batch_request)
        );
        assert!(
            control_object_request.starts_with(b"GET /object "),
            "control did not make the advertised direct plaintext GET: {}",
            String::from_utf8_lossy(&control_object_request)
        );

        let (guarded_dest, guarded_object, guarded_batch, guarded_command) =
            run("plaintext-action-guarded", true);
        let guarded = guarded_command
            .output()
            .await
            .expect("production checkout starts");
        assert!(
            !guarded.status.success(),
            "a direct plaintext object action must fail the production checkout"
        );
        assert!(
            String::from_utf8_lossy(&guarded.stderr)
                .contains(PLAINTEXT_ACTION_REFUSAL_URL.trim_end_matches('/')),
            "the checkout failed without applying the fixed HTTPS refusal rewrite: {}",
            String::from_utf8_lossy(&guarded.stderr)
        );
        assert!(
            !guarded_dest.join("asset.bin").exists(),
            "a refused plaintext action must not leave an apparent object"
        );
        let guarded_batch_request = guarded_batch.request();
        assert!(
            guarded_batch_request.starts_with(b"POST ")
                && guarded_batch_request
                    .windows(b"/objects/batch".len())
                    .any(|window| window == b"/objects/batch"),
            "guarded leg did not reach the direct-action batch response: {}",
            String::from_utf8_lossy(&guarded_batch_request)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            guarded_object.request.try_recv().is_err(),
            "the built-in adapter contacted the plaintext object server"
        );
    }

    #[test]
    fn an_unavailable_program_stays_required_instead_of_becoming_a_noop_filter() {
        let config =
            checkout_config_with_program("https://example.invalid/repo.git", GIT_LFS_UNAVAILABLE)
                .join("\n");
        assert!(config.contains(&format!("filter.lfs.process={GIT_LFS_UNAVAILABLE}")));
        assert!(config.contains(&format!("filter.lfs.smudge={GIT_LFS_UNAVAILABLE}")));
        assert!(config.contains("filter.lfs.required=true"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn the_production_checkout_materialises_a_cached_lfs_object() {
        assert_ne!(
            program(),
            GIT_LFS_UNAVAILABLE,
            "this integration test requires one reviewed git-lfs installation"
        );
        let (clones, source) = lfs_fixture();
        let dest = clone_with_cached_lfs_object(clones.path(), &source, "materialised");
        let marker = clones.path().join("repo-local-lfs-filter-ran");
        let hostile = clones.path().join("repo-local-lfs-filter.sh");
        executable(
            &hostile,
            &format!("#!/bin/sh\nprintf RAN > '{}'\nexit 74\n", marker.display()),
        );
        super::super::network_exec::run_fixture_git(
            &dest,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("filter.lfs.process"),
                hostile.as_os_str().to_owned(),
            ],
        );
        super::super::network_exec::run_fixture_git(
            &dest,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("filter.lfs.smudge"),
                hostile.as_os_str().to_owned(),
            ],
        );
        let policy = super::super::policy_for_clone_checkout(clones.path())
            .expect("checkout policy must build");
        let output = super::super::network_exec::lfs_checkout_command(
            &policy,
            &dest,
            "https://example.invalid/repo.git",
        )
        .output()
        .await
        .expect("checkout starts");
        assert!(
            output.status.success(),
            "LFS checkout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(dest.join("asset.bin")).expect("materialised asset"),
            b"materialised LFS bytes\0\xff\n"
        );
        assert!(
            !marker.exists(),
            "command-line process/smudge pins must outrank repository-local executables"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn an_unavailable_required_lfs_driver_fails_instead_of_leaving_a_pointer() {
        let (clones, source) = lfs_fixture();
        let dest = clone_with_cached_lfs_object(clones.path(), &source, "unavailable");
        let policy = super::super::policy_for_clone_checkout(clones.path())
            .expect("checkout policy must build");
        let output = super::super::network_exec::lfs_checkout_command_with_program(
            &policy,
            &dest,
            "https://example.invalid/repo.git",
            GIT_LFS_UNAVAILABLE,
        )
        .output()
        .await
        .expect("git checkout itself starts");
        assert!(
            !output.status.success(),
            "missing required LFS must be loud"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("git-vista-lfs-unavailable"),
            "the diagnostic must identify the unavailable server-selected driver: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !dest.join("asset.bin").exists(),
            "a failed required filter must not leave an apparently checked-out pointer"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn an_unavailable_lfs_driver_does_not_reject_a_non_lfs_clone() {
        let clones = tempfile::tempdir().expect("clones root");
        let source = clones.path().join("plain-source");
        let dest = clones.path().join("plain-dest");
        std::fs::create_dir(&source).expect("create plain source");
        super::super::network_exec::run_fixture_git(&source, ["init", "-q"]);
        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "user.name", "git-vista-test"],
        );
        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(source.join("plain"), "ordinary tracked content\n")
            .expect("write plain file");
        super::super::network_exec::run_fixture_git(&source, ["add", "."]);
        super::super::network_exec::run_fixture_git(&source, ["commit", "-qm", "plain"]);
        super::super::network_exec::run_fixture_git(
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
        let policy = super::super::policy_for_clone_checkout(clones.path())
            .expect("checkout policy must build");
        let output = super::super::network_exec::lfs_checkout_command_with_program(
            &policy,
            &dest,
            "https://example.invalid/repo.git",
            GIT_LFS_UNAVAILABLE,
        )
        .output()
        .await
        .expect("checkout starts");
        assert!(
            output.status.success(),
            "an unused unavailable filter must not reject plain content: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("plain")).expect("plain file materialised"),
            "ordinary tracked content\n"
        );
    }

    #[cfg(unix)]
    async fn assert_tracked_lfsconfig_cannot_skip_missing_object(
        clone_name: &str,
        lfsconfig: &str,
    ) {
        assert_ne!(
            program(),
            GIT_LFS_UNAVAILABLE,
            "this integration test requires one reviewed git-lfs installation"
        );
        let (clones, source) = lfs_fixture();
        std::fs::write(source.join(".lfsconfig"), lfsconfig)
            .expect("write tracked LFS configuration");
        super::super::network_exec::run_fixture_git(&source, ["add", ".lfsconfig"]);
        super::super::network_exec::run_fixture_git(
            &source,
            ["commit", "-qm", "hostile LFS configuration"],
        );

        let dest = clones.path().join(clone_name);
        super::super::network_exec::run_fixture_git(
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
        let policy = super::super::policy_for_clone_checkout(clones.path())
            .expect("checkout policy must build");
        let output = super::super::network_exec::lfs_checkout_command(
            &policy,
            &dest,
            "https://127.0.0.1/repo.git",
        )
        .output()
        .await
        .expect("checkout starts");
        assert!(
            !output.status.success(),
            "tracked .lfsconfig must not convert a missing object into successful pointer text"
        );
        assert!(
            !dest.join("asset.bin").exists(),
            "failed checkout must not leave an LFS pointer as apparent success"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn tracked_lfsconfig_skipdownloaderrors_cannot_make_checkout_succeed() {
        assert_tracked_lfsconfig_cannot_skip_missing_object(
            "skip-errors-dest",
            "[lfs]\n\tskipdownloaderrors = true\n",
        )
        .await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn tracked_lfsconfig_fetch_filters_cannot_make_checkout_succeed() {
        for (name, config) in [
            (
                "fetch-include-dest",
                "[lfs]\n\tfetchinclude = another-file.bin\n",
            ),
            ("fetch-exclude-dest", "[lfs]\n\tfetchexclude = asset.bin\n"),
        ] {
            assert_tracked_lfsconfig_cannot_skip_missing_object(name, config).await;
        }
    }

    // last_edited_by: codex
    // **Signed:** codex · 2026-09-26T22:05:01-04:00
    /// #840: observe execution, not git-lfs's unsafe-key warning. Register the
    /// test on every target: an unsupported runner must fail explicitly rather
    /// than quietly compile the security evidence out of the suite.
    #[tokio::test]
    async fn tracked_lfsconfig_cannot_select_an_executable() {
        #[cfg(unix)]
        for scoped in [false, true] {
            assert_tracked_lfsconfig_cannot_select_an_executable(
                scoped,
                "https://127.0.0.1:1/repo.git",
            )
            .await;
        }
        #[cfg(not(unix))]
        panic!("#840 needs the supported Unix production checkout sandbox and real git-lfs");
    }

    // last_edited_by: codex
    // **Signed:** codex · 2026-09-28T11:28:05-04:00
    /// #885 / atlas 744: Git LFS converts a native Git endpoint to HTTPS.
    /// The scoped reset then misses; the generic reset must prevent the
    /// admitted-key marker from executing under the real checkout sandbox.
    /// Retain the ordinary tracked-file arms too: current git-lfs rejects
    /// these keys, independently of Git-Vista's command-line defence.
    #[tokio::test]
    async fn git_protocol_endpoint_requires_generic_standalone_reset() {
        #[cfg(unix)]
        {
            const URL: &str = "git://127.0.0.1:9418/repo.git";
            assert!(git_vista_protocol::validate_clone_url(URL).is_ok());
            assert!(super::super::DEFAULT_GIT_PORTS.contains(&9418));
            assert_tracked_lfsconfig_cannot_select_an_executable(false, URL).await;
        }
        #[cfg(not(unix))]
        panic!("#885 needs the supported Unix production checkout sandbox and real git-lfs");
    }

    // last_edited_by: codex
    // **Signed:** codex · 2026-09-28T17:00:36-04:00
    /// #889: Git LFS 3.7.1 admits `lfs.gitprotocol` from `.lfsconfig` and uses
    /// it to replace a native Git endpoint's scheme. Run the exact checkout
    /// config through real Git LFS: each unpinned control must expose the
    /// tracked scheme, while the production config must retain HTTPS.
    ///
    /// MUTATION 1: remove the `lfs.gitprotocol=https` pair. The guarded result
    /// becomes the tracked `file://` or `http://` endpoint.
    /// MUTATION 2: change the pin to another value. The guarded result exposes
    /// that different scheme instead of HTTPS.
    #[test]
    #[cfg(unix)]
    fn tracked_lfsconfig_cannot_choose_git_protocol_endpoint_scheme() {
        assert_ne!(program(), GIT_LFS_UNAVAILABLE, "#889 requires real git-lfs");
        const URL: &str = "git://127.0.0.1:9418/repo.git";
        const HTTPS_ENDPOINT: &str =
            "Endpoint=https://127.0.0.1:9418/repo.git/info/lfs (auth=none)";

        for protocol in ["file", "http"] {
            let (clones, source) = lfs_fixture();
            let tracked = format!("[lfs]\n\tgitprotocol = {protocol}\n");
            std::fs::write(source.join(".lfsconfig"), &tracked)
                .expect("write tracked LFS protocol configuration");
            super::super::network_exec::run_fixture_git(&source, ["add", ".lfsconfig"]);
            super::super::network_exec::run_fixture_git(
                &source,
                ["commit", "-qm", "tracked LFS protocol configuration"],
            );

            let dest = clone_with_cached_lfs_object(clones.path(), &source, protocol);
            super::super::network_exec::run_fixture_git(
                &dest,
                ["checkout", "-q", "HEAD", "--", ".lfsconfig"],
            );
            assert_eq!(
                std::fs::read_to_string(dest.join(".lfsconfig")).unwrap(),
                tracked,
                "the conflicting config must really be tracked and materialised"
            );

            let guarded = checkout_config(URL);
            let (pairs, remainder) = guarded.as_chunks::<2>();
            assert!(
                remainder.is_empty(),
                "Git checkout config must consist only of -c/value pairs"
            );
            let mut control = Vec::new();
            let mut removed = 0;
            for pair in pairs {
                if pair[1] == "lfs.gitprotocol=https" {
                    removed += 1;
                } else {
                    control.extend_from_slice(pair);
                }
            }
            assert_eq!(removed, 1, "positive control must remove exactly the pin");

            let endpoint = |mut config: Vec<String>| {
                config.extend(["lfs".to_string(), "env".to_string()]);
                let args: Vec<std::ffi::OsString> =
                    config.into_iter().map(std::ffi::OsString::from).collect();
                let output = git_output(&dest, &args);
                assert!(
                    output.status.success(),
                    "git lfs env failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                String::from_utf8(output.stdout)
                    .expect("git lfs env output is UTF-8")
                    .lines()
                    .find(|line| line.starts_with("Endpoint="))
                    .expect("git lfs env reports its endpoint")
                    .to_string()
            };

            assert_eq!(
                endpoint(control),
                format!("Endpoint={protocol}://127.0.0.1:9418/repo.git/info/lfs (auth=none)"),
                "unpinned control must prove the tracked scheme is observable"
            );
            assert_eq!(
                endpoint(guarded),
                HTTPS_ENDPOINT,
                "command-line checkout pin must outrank tracked lfs.gitprotocol={protocol}"
            );
        }
    }

    #[cfg(unix)]
    async fn assert_tracked_lfsconfig_cannot_select_an_executable(scoped: bool, url: &str) {
        use super::super::network_exec::{
            lfs_checkout_command, network_command_without_credential, run_fixture_git,
        };
        use std::time::Duration;

        assert_ne!(program(), GIT_LFS_UNAVAILABLE, "#840 requires real git-lfs");
        let (clones, source) = lfs_fixture();
        let marker = clones.path().join("tracked-selector-ran");
        let selector = clones.path().join("tracked-selector.sh");
        executable(
            &selector,
            &format!("#!/bin/sh\nprintf RAN > '{}'\nexit 73\n", marker.display()),
        );
        let section = if scoped {
            format!("[lfs \"{}\"]", endpoint(url))
        } else {
            "[lfs]".to_string()
        };
        let config = format!(
            "{section}\n\tstandalonetransferagent = gv840\n\
             [lfs \"customtransfer.gv840\"]\n\tpath = {}\n\tconcurrent = false\n",
            selector.display()
        );
        std::fs::write(source.join(".lfsconfig"), &config).unwrap();
        run_fixture_git(&source, ["add", ".lfsconfig"]);
        run_fixture_git(&source, ["commit", "-qm", "tracked executable selector"]);
        assert!(
            !marker.exists(),
            "fixture add must not run the transfer selector"
        );
        let policy = super::super::policy_for_clone_checkout(clones.path()).unwrap();

        // Cover ordinary materialisation and the missing-object transfer path.
        // Each clone carries the actual committed .lfsconfig; it is not merely
        // repository-local config standing in for a fetched file.
        for cached in [true, false] {
            let dest = if cached {
                clone_with_cached_lfs_object(clones.path(), &source, "tracked-cached")
            } else {
                clone_without_lfs_object(clones.path(), &source, "tracked-missing")
            };
            assert!(!marker.exists(), "no-checkout transfer ran the selector");
            let output = tokio::time::timeout(
                Duration::from_secs(20),
                lfs_checkout_command(&policy, &dest, url)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("tracked checkout completes")
            .expect("tracked checkout starts");
            assert_eq!(
                output.status.success(),
                cached,
                "unexpected checkout result: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(dest.join(".lfsconfig")).unwrap(),
                config,
                "the hostile config must really be materialised"
            );
            assert!(
                !marker.exists(),
                "tracked .lfsconfig selected an executable (scoped={scoped}, cached={cached})"
            );
            if cached {
                assert_eq!(
                    std::fs::read(dest.join("asset.bin")).unwrap(),
                    b"materialised LFS bytes\0\xff\n"
                );
            } else {
                assert!(
                    !dest.join("asset.bin").exists(),
                    "failed checkout left a pointer"
                );
            }
        }

        // Model upstream admitting these same tracked keys: include the fetched
        // file as local config. This separately pins Git-Vista's command-line
        // resets, so git-lfs's current denylist cannot mask a reset regression.
        // A positive control under the SAME sandbox proves the executable and
        // marker are reachable; a denied exec/write cannot masquerade as safety.
        for guarded in [true, false] {
            let name = if guarded {
                "admitted-guarded"
            } else {
                "admitted-control"
            };
            let dest = clone_without_lfs_object(clones.path(), &source, name);
            std::fs::write(dest.join(".lfsconfig"), &config).unwrap();
            run_fixture_git(&dest, ["config", "include.path", "../.lfsconfig"]);
            let command = if guarded {
                lfs_checkout_command(&policy, &dest, url)
            } else {
                let args = [
                    "-c".to_string(),
                    format!("filter.lfs.process={} filter-process", program()),
                    "-c".to_string(),
                    format!("filter.lfs.smudge={} smudge", program()),
                    "-c".to_string(),
                    "filter.lfs.required=true".to_string(),
                    // Keep basic-only restrictions in the positive control:
                    // the standalone-selector resets are the mechanism at risk.
                    "-c".to_string(),
                    "lfs.basictransfersonly=true".to_string(),
                    "-c".to_string(),
                    format!("lfs.{}.basictransfersonly=true", endpoint(url)),
                    "-c".to_string(),
                    format!("lfs.url={}", endpoint(url)),
                    "checkout".to_string(),
                    "-f".to_string(),
                ];
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                network_command_without_credential(&policy, &dest, &refs)
            };
            let output =
                tokio::time::timeout(Duration::from_secs(20), command.kill_on_drop(true).output())
                    .await
                    .expect("admitted-key checkout completes")
                    .expect("admitted-key checkout starts");
            assert!(
                !output.status.success(),
                "absent object / failing selector must fail checkout"
            );
            assert_eq!(
                marker.exists(),
                !guarded,
                "selector effect differs from expected (scoped={scoped}, guarded={guarded}): {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !dest.join("asset.bin").exists(),
                "failed checkout left a pointer"
            );
        }
    }

    /// Retained #782 measurement: extensions have no transfer consumer under
    /// `clone --no-checkout`, but the same configured clean and smudge commands
    /// execute during add and checkout respectively.
    #[test]
    #[cfg(unix)]
    fn lfs_extensions_execute_on_clean_and_checkout_but_not_clone_transfer() {
        let root = tempfile::tempdir().expect("measurement root");
        let source = root.path().join("extension-source");
        std::fs::create_dir(&source).expect("create extension source");
        super::super::network_exec::run_fixture_git(&source, ["init", "-q"]);
        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "user.name", "git-vista-test"],
        );
        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "user.email", "test@example.invalid"],
        );

        let clean_marker = root.path().join("extension-clean-ran");
        let smudge_marker = root.path().join("extension-smudge-ran");
        let clean = root.path().join("extension-clean.sh");
        let smudge = root.path().join("extension-smudge.sh");
        executable(
            &clean,
            &format!(
                "#!/bin/sh\nprintf RAN > '{}'\nexec /usr/bin/tr 'a-z' 'A-Z'\n",
                clean_marker.display()
            ),
        );
        executable(
            &smudge,
            &format!(
                "#!/bin/sh\nprintf RAN > '{}'\nexec /usr/bin/tr 'A-Z' 'a-z'\n",
                smudge_marker.display()
            ),
        );
        super::super::network_exec::run_fixture_git(
            &source,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("lfs.extension.gv831.clean"),
                clean.as_os_str().to_owned(),
            ],
        );
        super::super::network_exec::run_fixture_git(
            &source,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("lfs.extension.gv831.smudge"),
                smudge.as_os_str().to_owned(),
            ],
        );
        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "lfs.extension.gv831.priority", "0"],
        );
        std::fs::write(
            source.join(".gitattributes"),
            "asset.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .expect("write extension attributes");
        std::fs::write(source.join("asset.bin"), b"extension bytes\n")
            .expect("write extension input");
        super::super::network_exec::run_fixture_git(&source, ["add", "."]);
        assert!(
            clean_marker.exists(),
            "clean/add must consume lfs.extension.<name>.clean"
        );
        assert!(
            !smudge_marker.exists(),
            "clean/add must not consume the smudge command"
        );
        let staged_pointer = git_output(
            &source,
            &[
                std::ffi::OsString::from("show"),
                std::ffi::OsString::from(":asset.bin"),
            ],
        );
        assert!(staged_pointer.status.success(), "read staged LFS pointer");
        assert!(
            String::from_utf8_lossy(&staged_pointer.stdout).contains("ext-0-gv831"),
            "clean command ran but did not record its extension in the pointer: {}",
            String::from_utf8_lossy(&staged_pointer.stdout)
        );
        super::super::network_exec::run_fixture_git(&source, ["commit", "-qm", "extension"]);
        std::fs::remove_file(&clean_marker).expect("clear clean marker");

        let transfer_dest = root.path().join("extension-transfer");
        let args = vec![
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from(format!("lfs.extension.gv831.clean={}", clean.display())),
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from(format!("lfs.extension.gv831.smudge={}", smudge.display())),
            std::ffi::OsString::from("clone"),
            std::ffi::OsString::from("--no-checkout"),
            std::ffi::OsString::from("--"),
            source.as_os_str().to_owned(),
            transfer_dest.as_os_str().to_owned(),
        ];
        let transfer = git_output(root.path(), &args);
        assert!(transfer.status.success(), "no-checkout transfer failed");
        assert!(
            !clean_marker.exists() && !smudge_marker.exists(),
            "clone --no-checkout must consume neither extension command"
        );

        std::fs::remove_file(source.join("asset.bin")).expect("remove extension worktree file");
        super::super::network_exec::run_fixture_git(&source, ["checkout", "--", "asset.bin"]);
        assert!(
            smudge_marker.exists(),
            "checkout must consume the extension recorded in the LFS pointer"
        );
        assert!(
            !clean_marker.exists(),
            "checkout must not consume the extension clean command"
        );
    }

    /// Retained #782 measurement: a configured standalone custom transfer is
    /// dormant during no-checkout transfer and clean/add, then is spawned by
    /// checkout when the required LFS object is absent.
    #[test]
    #[cfg(unix)]
    fn lfs_custom_transfer_executes_on_checkout_but_not_clone_transfer_or_clean_add() {
        let (clones, source) = lfs_fixture();
        let marker = clones.path().join("custom-transfer-ran");
        let agent = clones.path().join("custom-transfer.sh");
        executable(
            &agent,
            &format!("#!/bin/sh\nprintf RAN > '{}'\nexit 73\n", marker.display()),
        );

        super::super::network_exec::run_fixture_git(
            &source,
            ["config", "lfs.standalonetransferagent", "gv831"],
        );
        super::super::network_exec::run_fixture_git(
            &source,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("lfs.customtransfer.gv831.path"),
                agent.as_os_str().to_owned(),
            ],
        );
        std::fs::write(source.join("second.bin"), b"second LFS value\n")
            .expect("write second LFS file");
        std::fs::write(
            source.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .expect("widen LFS attributes");
        super::super::network_exec::run_fixture_git(&source, ["add", "."]);
        assert!(
            !marker.exists(),
            "clean/add stores locally and must not start a custom transfer"
        );
        super::super::network_exec::run_fixture_git(&source, ["commit", "-qm", "second asset"]);

        let dest = clones.path().join("custom-transfer-dest");
        let args = vec![
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from("lfs.standalonetransferagent=gv831"),
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from(format!("lfs.customtransfer.gv831.path={}", agent.display())),
            std::ffi::OsString::from("clone"),
            std::ffi::OsString::from("--no-checkout"),
            std::ffi::OsString::from("--"),
            source.as_os_str().to_owned(),
            dest.as_os_str().to_owned(),
        ];
        let transfer = git_output(clones.path(), &args);
        assert!(transfer.status.success(), "no-checkout transfer failed");
        assert!(
            !marker.exists(),
            "clone --no-checkout must not start a custom transfer"
        );
        super::super::network_exec::run_fixture_git(
            &dest,
            ["config", "lfs.standalonetransferagent", "gv831"],
        );
        super::super::network_exec::run_fixture_git(
            &dest,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("lfs.customtransfer.gv831.path"),
                agent.as_os_str().to_owned(),
            ],
        );
        let checkout = git_output(
            &dest,
            &[
                std::ffi::OsString::from("checkout"),
                std::ffi::OsString::from("-f"),
            ],
        );
        assert!(
            !checkout.status.success(),
            "the deliberately failing custom transfer must fail checkout"
        );
        assert!(
            marker.exists(),
            "checkout of an absent object must consume the custom transfer path"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn the_production_lfs_config_refuses_a_repo_local_custom_transfer() {
        let (clones, source) = lfs_fixture();
        let dest = clones.path().join("basic-only-dest");
        super::super::network_exec::run_fixture_git(
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
        let marker = clones.path().join("refused-custom-transfer-ran");
        let agent = clones.path().join("refused-custom-transfer.sh");
        executable(
            &agent,
            &format!("#!/bin/sh\nprintf RAN > '{}'\nexit 73\n", marker.display()),
        );
        super::super::network_exec::run_fixture_git(
            &dest,
            ["config", "lfs.standalonetransferagent", "gv831"],
        );
        super::super::network_exec::run_fixture_git(
            &dest,
            [
                std::ffi::OsString::from("config"),
                std::ffi::OsString::from("lfs.customtransfer.gv831.path"),
                agent.as_os_str().to_owned(),
            ],
        );
        super::super::network_exec::run_fixture_git(
            &dest,
            [
                "config",
                "lfs.https://127.0.0.1/repo.git/info/lfs.standalonetransferagent",
                "gv831",
            ],
        );

        let policy = super::super::policy_for_clone_checkout(clones.path())
            .expect("checkout policy must build");
        let output = super::super::network_exec::lfs_checkout_command(
            &policy,
            &dest,
            "https://127.0.0.1/repo.git",
        )
        .output()
        .await
        .expect("checkout starts");
        assert!(
            !output.status.success(),
            "the absent HTTPS object must fail when no server is listening"
        );
        assert!(
            !marker.exists(),
            "lfs.basictransfersonly=true must make the executable custom adapter unreachable"
        );
        assert!(
            !dest.join("asset.bin").exists(),
            "refusing the transfer must not leave an LFS pointer as apparent success"
        );
    }
}
