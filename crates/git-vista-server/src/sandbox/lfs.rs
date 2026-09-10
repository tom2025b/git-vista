//! #831's explicit Git LFS checkout configuration.
//!
//! Fresh-clone checkout cannot read system or global Git config: a fetched
//! `.gitattributes` can choose any filter name, so restoring either scope would
//! also restore every operator-selected executable filter. On this host that
//! hides Git LFS's system-only `filter.lfs.*` settings and makes Git silently
//! write pointer text. This module restores only the values the server authors.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Reviewed installation locations, never `PATH`. The first executable file
/// wins. These are the same system prefixes already read-granted by the sandbox.
/// A package installed elsewhere is unavailable to this boundary until its
/// absolute location is reviewed and added here.
const GIT_LFS_CANDIDATES: &[&str] = &["/usr/bin/git-lfs", "/bin/git-lfs", "/usr/local/bin/git-lfs"];

/// Deliberately absent. When no candidate exists, keeping a required filter
/// with this program makes an LFS-attributed checkout fail at the exact file
/// instead of silently leaving its pointer. Non-LFS repositories never invoke
/// it and remain cloneable.
const GIT_LFS_UNAVAILABLE: &str = "/dev/null/git-vista-lfs-unavailable";

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn program() -> &'static str {
    GIT_LFS_CANDIDATES
        .iter()
        .copied()
        .find(|candidate| is_executable_file(Path::new(candidate)))
        .unwrap_or(GIT_LFS_UNAVAILABLE)
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
/// and exact-URL standalone selectors are therefore reset, and the exact URL's
/// basic-only value is pinned as well so URL-match specificity cannot outrank
/// the generic setting. `skipdownloaderrors`, `fetchinclude`, and
/// `fetchexclude` are also pinned: Git LFS otherwise accepts each from a
/// tracked `.lfsconfig`, can deliberately leave pointer text for a selected
/// path, and still report filter success. Pinning `lfs.url` prevents a fetched
/// `.lfsconfig` from switching checkout to SSH and thereby selecting `ssh` or
/// `git-lfs-authenticate` as another executable path.
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
        "lfs.basictransfersonly=true".into(),
        "-c".into(),
        format!("lfs.{endpoint}.basictransfersonly=true"),
        "-c".into(),
        "lfs.standalonetransferagent=".into(),
        "-c".into(),
        format!("lfs.{endpoint}.standalonetransferagent="),
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
    use std::process::Output;

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

    fn git_output(repo: &Path, args: &[std::ffi::OsString]) -> Output {
        super::super::network_exec::fixture_git_output(repo, args)
    }

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write executable fixture");
        let mut permissions = std::fs::metadata(path)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("make fixture executable");
    }

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

    #[test]
    fn endpoint_discards_query_fragment_and_appends_the_standard_path() {
        assert_eq!(
            endpoint("https://user:secret@example.invalid/org/repo.git/?token=data#fragment"),
            "https://example.invalid/org/repo.git/info/lfs"
        );
    }

    #[test]
    fn checkout_config_pins_every_lfs_executable_and_transfer_selector() {
        let config = checkout_config("https://example.invalid/repo.git");
        let joined = config.join("\n");
        let selected = program();
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
        assert!(joined.contains("lfs.basictransfersonly=true"));
        assert!(joined.contains("lfs.standalonetransferagent="));
        assert!(joined
            .contains("lfs.https://example.invalid/repo.git/info/lfs.standalonetransferagent="));
        assert!(joined.contains("lfs.url=https://example.invalid/repo.git/info/lfs"));
        assert!(
            !selected.contains(' '),
            "reviewed candidates need no shell quoting"
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
    async fn tracked_lfsconfig_skipdownloaderrors_cannot_make_checkout_succeed() {
        assert_tracked_lfsconfig_cannot_skip_missing_object(
            "skip-errors-dest",
            "[lfs]\n\tskipdownloaderrors = true\n",
        )
        .await;
    }

    #[tokio::test]
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

    /// Retained #782 measurement: extensions have no transfer consumer under
    /// `clone --no-checkout`, but the same configured clean and smudge commands
    /// execute during add and checkout respectively.
    #[test]
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
