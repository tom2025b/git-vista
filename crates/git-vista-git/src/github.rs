//! Turning a repository's `origin` remote URL into a GitHub web base URL, so the
//! UI can link commits and refs to their GitHub pages.

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

/// Parse a git remote URL into its GitHub web base (`https://github.com/owner/repo`),
/// or `None` if it isn't a github.com remote. Handles the common forms:
/// `git@github.com:owner/repo.git`, `https://github.com/owner/repo(.git)`, and
/// `ssh://git@github.com/owner/repo.git`. Pure (no I/O) so it's unit-testable.
fn web_base_from_remote(remote: &str) -> Option<String> {
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
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    // Strip a trailing "/" and the ".git" suffix, then require owner + repo.
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.splitn(3, '/');
    let owner = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    Some(format!("https://github.com/{owner}/{repo}"))
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
        assert_eq!(web_base_from_remote("https://github.com/owner/repo.git"), want);
        assert_eq!(web_base_from_remote("https://github.com/owner/repo/"), want);
        // ssh:// URL form
        assert_eq!(web_base_from_remote("ssh://git@github.com/owner/repo.git"), want);
        // Case-insensitive host.
        assert_eq!(web_base_from_remote("git@GitHub.com:owner/repo.git"), want);
    }

    #[test]
    fn rejects_non_github_or_malformed_remotes() {
        assert_eq!(web_base_from_remote("git@gitlab.com:owner/repo.git"), None);
        assert_eq!(web_base_from_remote("https://example.com/owner/repo.git"), None);
        assert_eq!(web_base_from_remote("/local/path/repo.git"), None);
        assert_eq!(web_base_from_remote("git@github.com:owner.git"), None); // no repo
        assert_eq!(web_base_from_remote(""), None);
    }
}
