//! Pure forge-URL builders (ADR 0010): given a normalized web base
//! (`https://host/owner/repo`), produce commit/branch page URLs. GitLab nests
//! repo pages under `/-/`; GitHub, Gitea/Codeberg and most others don't. Pure
//! string work, shared by the wasm frontend, so it's host-unit-tested here.

/// The commit page URL for `id` under `base`.
pub fn commit_url(base: &str, id: &str) -> String {
    if is_gitlab(base) {
        format!("{base}/-/commit/{id}")
    } else {
        format!("{base}/commit/{id}")
    }
}

/// The branch (tree) page URL for `branch` under `base`.
pub fn branch_url(base: &str, branch: &str) -> String {
    if is_gitlab(base) {
        format!("{base}/-/tree/{branch}")
    } else {
        format!("{base}/tree/{branch}")
    }
}

/// The bare host of `base` for UI labels ("View commit on github.com");
/// `"remote"` when the base doesn't parse as an http(s) URL.
pub fn host_label(base: &str) -> String {
    base.strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "remote".to_string())
}

/// GitLab (gitlab.com or a self-hosted `gitlab.*` host) is the one major forge
/// whose web paths differ; everything else gets the GitHub/Gitea shape.
fn is_gitlab(base: &str) -> bool {
    let host = host_label(base);
    host == "gitlab.com" || host.starts_with("gitlab.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_and_branch_urls_use_each_forges_path_shape() {
        assert_eq!(
            commit_url("https://github.com/o/r", "abc"),
            "https://github.com/o/r/commit/abc"
        );
        // GitLab inserts /-/ before commit/tree paths.
        assert_eq!(
            commit_url("https://gitlab.com/o/r", "abc"),
            "https://gitlab.com/o/r/-/commit/abc"
        );
        assert_eq!(
            branch_url("https://gitlab.com/o/r", "main"),
            "https://gitlab.com/o/r/-/tree/main"
        );
        assert_eq!(
            branch_url("https://codeberg.org/o/r", "dev"),
            "https://codeberg.org/o/r/tree/dev"
        );
        // Self-hosted GitLab counts as GitLab.
        assert_eq!(
            commit_url("https://gitlab.example.com/o/r", "abc"),
            "https://gitlab.example.com/o/r/-/commit/abc"
        );
    }

    #[test]
    fn host_label_is_the_bare_host() {
        assert_eq!(host_label("https://github.com/o/r"), "github.com");
        assert_eq!(host_label("http://forge.lan/o/r"), "forge.lan");
        assert_eq!(host_label("not a url"), "remote");
    }
}
