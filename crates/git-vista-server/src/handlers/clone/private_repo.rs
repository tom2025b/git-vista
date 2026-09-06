//! Credential-aware interpretation of Git's failed GitHub HTTP transfers (#585).
//! Pure/native: no framework, transport, token store, or credential values.
//!
//! Git exposes these failures as exit 128 plus stderr, not an HTTP response.
//! Markers are therefore a heuristic (as in `planner::fetch::classify_failure`).
//! A 404 proves neither repository existence nor token validity. The access
//! message deliberately leaves scope/account access unresolved; unmatched errors
//! retain Git's original, redacted diagnostic.

pub(super) fn failure_message(url: &str, has_token: bool, stderr: &str) -> Option<&'static str> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let authority = rest.split('/').next()?;
    if !authority.eq_ignore_ascii_case("github.com")
        && !authority.eq_ignore_ascii_case("github.com:443")
        && !authority.eq_ignore_ascii_case("github.com:80")
    {
        return None;
    }

    let s = stderr.to_ascii_lowercase();
    let rejected = s.contains("the requested url returned error: 401")
        || s.contains("authentication failed")
        || s.contains("invalid username or password")
        || s.contains("invalid username or token");
    let refused = s.contains("the requested url returned error: 403")
        || s.contains("the requested url returned error: 404")
        || s.contains("remote: repository not found")
        || (s.contains("fatal: repository '") && s.contains("' not found"));
    let missing = s.contains("could not read username") || s.contains("could not read password");

    if !has_token && (rejected || refused || missing) {
        Some("private repo — no token configured")
    } else if has_token && rejected {
        Some("private repo — token invalid or expired")
    } else if has_token && refused {
        Some("private repo — token lacks `repo` scope, or this account cannot see it")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://github.com/example/private.git";

    #[test]
    fn no_token_message_is_distinct() {
        for stderr in [
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
            "fatal: could not read Password for 'https://github.com': terminal prompts disabled",
            "fatal: Authentication failed for 'https://github.com/example/private.git/'",
            "fatal: The requested URL returned error: 401",
            "fatal: The requested URL returned error: 403",
            "fatal: The requested URL returned error: 404",
            "remote: Repository not found.",
        ] {
            assert_eq!(
                failure_message(URL, false, stderr),
                Some("private repo — no token configured")
            );
        }
    }

    #[test]
    fn rejected_token_message_is_distinct() {
        for stderr in [
            "fatal: The requested URL returned error: 401",
            "fatal: Authentication failed for 'https://github.com/example/private.git/'",
            "remote: Invalid username or password.",
            "remote: Invalid username or token. Password authentication is not supported for Git operations.",
        ] {
            assert_eq!(failure_message(URL, true, stderr), Some("private repo — token invalid or expired"));
        }
    }

    #[test]
    fn refused_access_message_keeps_scope_and_account_ambiguous() {
        for stderr in [
            "fatal: The requested URL returned error: 403",
            "fatal: The requested URL returned error: 404",
            "remote: Repository not found.",
            "fatal: repository 'https://github.com/example/private.git/' not found",
        ] {
            assert_eq!(
                failure_message(URL, true, stderr),
                Some("private repo — token lacks `repo` scope, or this account cannot see it")
            );
        }
    }

    #[test]
    fn other_hosts_and_transports_do_not_get_a_github_token_diagnosis() {
        for url in [
            "https://gitlab.com/example/private.git",
            "https://github.com.example.org/example/private.git",
            "https://example.org/github.com/example/private.git",
            "https://github.com@evil.example/example/private.git",
            "git://github.com/example/private.git",
            "ssh://git@github.com/example/private.git",
            "/local/repo",
        ] {
            assert_eq!(
                failure_message(url, true, "remote: Repository not found."),
                None
            );
        }
        assert_eq!(
            failure_message(
                "https://GitHub.COM:443/example/private",
                false,
                "remote: Repository not found."
            ),
            Some("private repo — no token configured")
        );
    }

    #[test]
    fn unrelated_failures_do_not_invent_authentication_evidence() {
        for stderr in [
            "fatal: unable to access 'https://github.com/example/private.git/': Could not resolve host: github.com",
            "fatal: Failed to connect to github.com port 443: Connection refused",
            "fatal: The requested URL returned error: 500",
            "fatal: The requested URL returned error: 429",
            "fatal: SSL certificate problem: certificate has expired",
            "fatal: unable to create file: Permission denied",
            "",
        ] {
            for has_token in [false, true] {
                assert_eq!(failure_message(URL, has_token, stderr), None);
            }
        }
        // A configured token that Git couldn't obtain is not proof of rejection.
        assert_eq!(
            failure_message(
                URL,
                true,
                "fatal: could not read Username: terminal prompts disabled"
            ),
            None
        );
    }
}
