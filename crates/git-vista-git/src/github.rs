//! Turning a repository's `origin` remote URL into a browsable web base URL —
//! GitHub-only for the existing commit/ref links ([`github_web_base`]), and
//! any-host for the general forge links ([`remote_web_base`], ADR 0010).

use std::path::Path;

/// The GitHub web base URL for a repository's `origin` remote, e.g.
/// `"https://github.com/owner/repo"`, or `None` when there's no `origin`, the URL
/// can't be parsed, or the host isn't github.com. The UI turns this into per-commit
/// and per-ref links; `None` means it leaves the labels as plain text.
pub fn github_web_base(path: &Path) -> Option<String> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).ok()?;
    let url = repo.config_snapshot().string("remote.origin.url")?;
    web_base_from_remote(&url.to_string())
}

/// The web base URL for a repository's `origin` remote on ANY host (ADR 0010):
/// `https://<host>/<owner>/<repo>`, or `None` when there's no origin or the URL
/// has no owner/repo shape. Unlike [`github_web_base`] this does not filter by
/// host — unknown forges get a best-effort base link rather than nothing.
pub fn remote_web_base(path: &Path) -> Option<String> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).ok()?;
    let url = repo.config_snapshot().string("remote.origin.url")?;
    any_web_base_from_remote(&url.to_string())
}

/// Parse a git remote URL into its GitHub web base (`https://github.com/owner/repo`),
/// or `None` if it isn't a github.com remote. The host filter keeps the existing
/// GitHub-only link behavior (pushed-commit dot links, PR compare pages) intact;
/// the any-host normalization lives in [`any_web_base_from_remote`].
fn web_base_from_remote(remote: &str) -> Option<String> {
    any_web_base_from_remote(remote).filter(|base| base.starts_with("https://github.com/"))
}

/// Host-agnostic remote-URL normalization (ADR 0010): reduce the common forms —
/// `git@host:owner/repo.git`, `https://host/owner/repo(.git)`,
/// `ssh://git@host/owner/repo.git` — to `https://host/owner/repo`. Requires an
/// owner + repo shape; anything else (local paths, bare hosts) is `None`.
/// Pure (no I/O) so it's unit-testable.
fn any_web_base_from_remote(remote: &str) -> Option<String> {
    let s = remote.trim();
    // Reduce every form to "host/owner/repo…" by stripping scheme and any user@.
    let host_and_path = if let Some(idx) = s.find("://") {
        // scheme://[user@]host/path
        let after = &s[idx + 3..];
        after.split_once('@').map_or(after, |(_, h)| h).to_string()
    } else if let Some((user_host, path)) = s.split_once(':') {
        // scp-like: [user@]host:path
        let host = user_host.split_once('@').map_or(user_host, |(_, h)| h);
        format!("{host}/{path}")
    } else {
        return None;
    };

    let (host, path) = host_and_path.split_once('/')?;
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    // Drop a port if present (host:8443) — forge web URLs are https-standard —
    // and lowercase the host (hostnames are case-insensitive).
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    // Strip a trailing "/" and the ".git" suffix, then require owner + repo.
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.splitn(3, '/');
    let owner = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    Some(format!("https://{host}/{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_remote_urls_to_a_web_base() {
        let want = Some("https://github.com/owner/repo".to_string());
        // SSH (scp-like), with and without .git
        assert_eq!(web_base_from_remote("git@github.com:owner/repo.git"), want);
        assert_eq!(web_base_from_remote("git@github.com:owner/repo"), want);
        // HTTPS, with .git / trailing slash
        assert_eq!(
            web_base_from_remote("https://github.com/owner/repo.git"),
            want
        );
        assert_eq!(web_base_from_remote("https://github.com/owner/repo/"), want);
        // ssh:// URL form
        assert_eq!(
            web_base_from_remote("ssh://git@github.com/owner/repo.git"),
            want
        );
        // Case-insensitive host.
        assert_eq!(web_base_from_remote("git@GitHub.com:owner/repo.git"), want);
    }

    #[test]
    fn any_host_remotes_normalize_to_a_web_base() {
        let f = any_web_base_from_remote;
        assert_eq!(
            f("git@gitlab.com:owner/repo.git"),
            Some("https://gitlab.com/owner/repo".into())
        );
        assert_eq!(
            f("https://codeberg.org/owner/repo.git"),
            Some("https://codeberg.org/owner/repo".into())
        );
        // Unknown host still yields the normalized base (ADR 0010: best-effort).
        assert_eq!(
            f("ssh://git@git.example.net/owner/repo.git"),
            Some("https://git.example.net/owner/repo".into())
        );
        // A port on the git transport is dropped for the web URL.
        assert_eq!(
            f("ssh://git@forge.lan:2222/owner/repo.git"),
            Some("https://forge.lan/owner/repo".into())
        );
        // Owner-only, local paths, empty: no link.
        assert_eq!(f("git@host.com:owner.git"), None);
        assert_eq!(f("/local/path/repo.git"), None);
        assert_eq!(f(""), None);
    }

    #[test]
    fn rejects_non_github_or_malformed_remotes() {
        assert_eq!(web_base_from_remote("git@gitlab.com:owner/repo.git"), None);
        assert_eq!(
            web_base_from_remote("https://example.com/owner/repo.git"),
            None
        );
        assert_eq!(web_base_from_remote("/local/path/repo.git"), None);
        assert_eq!(web_base_from_remote("git@github.com:owner.git"), None); // no repo
        assert_eq!(web_base_from_remote(""), None);
    }
}
