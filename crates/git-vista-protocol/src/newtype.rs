//! The validating-newtype machinery shared by every string-shaped wire value.
//!
//! A wire value that is "a string" is almost never *any* string: a branch name
//! must not be option-shaped, an object id is fixed-length lowercase hex, an
//! idempotency key must be safe to put in a header and a log line. Encoding
//! those rules in the type — and running them from `Deserialize` — makes a
//! malformed value a hard wire error (a 400) instead of something a handler
//! might act on.
//!
//! Extracted from [`plan`](crate::plan) when [`operation`](crate::operation)
//! needed the same guarantees (M1.08, #61); the validators and the macro are
//! the module's whole content, so there is exactly one definition of each rule.

use std::fmt;

/// Why a wire field failed validation, typed (used as the serde error message
/// when a malformed value arrives on the wire).
///
/// Named for the plan schema it was introduced with (M1.06a); it is now the
/// shared error of every validated newtype in the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFieldError {
    /// The field is empty (or whitespace-only) where a value is required.
    Empty(&'static str),
    /// The value starts with `-`, so git could read it as an option.
    OptionShaped(&'static str),
    /// The value is not the required lowercase-hex shape.
    NotHex {
        field: &'static str,
        expected: &'static str,
    },
    /// The value is longer than the field's cap — a bound on anything a client
    /// chooses the contents of, so it can't be used to grow server-side state.
    TooLong { field: &'static str, max: usize },
    /// The value carries something outside the field's allowed character set
    /// (ASCII letters, digits, `-` and `_` for the token-shaped fields).
    NotToken(&'static str),
    /// The value is not a path relative to the worktree root: it is absolute,
    /// carries a `..` component, or embeds a NUL byte (#219).
    NotWorktreeRelative(&'static str),
    /// The value is not a plain remote *name*: it carries a character no
    /// remote nickname may (`:`, `/`, `@`, `~`, whitespace, …), starts with
    /// `.`, or embeds `..` — i.e. it is URL-shaped or path-shaped, and git
    /// would resolve it as a transport target rather than looking it up in
    /// the repository's configuration (ADR 0044).
    NotRemoteName(&'static str),
}

impl fmt::Display for PlanFieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanFieldError::Empty(field) => write!(f, "{field} can't be empty"),
            PlanFieldError::OptionShaped(field) => write!(f, "{field} can't start with '-'"),
            PlanFieldError::NotHex { field, expected } => {
                write!(f, "{field} must be {expected} lowercase hex characters")
            }
            PlanFieldError::TooLong { field, max } => {
                write!(f, "{field} can't be longer than {max} characters")
            }
            PlanFieldError::NotToken(field) => {
                write!(f, "{field} may only contain letters, digits, '-' and '_'")
            }
            PlanFieldError::NotWorktreeRelative(field) => {
                write!(
                    f,
                    "{field} must be relative to the worktree root — no leading '/' and \
                     no '..' component"
                )
            }
            PlanFieldError::NotRemoteName(field) => {
                write!(
                    f,
                    "{field} must be the name of a remote configured in this repository \
                     (letters, digits, '.', '-' and '_'), not a URL or a path"
                )
            }
        }
    }
}

impl std::error::Error for PlanFieldError {}

/// Non-empty after trimming — the same test every write handler applies.
pub(crate) fn require_non_empty(value: &str, field: &'static str) -> Result<(), PlanFieldError> {
    if value.trim().is_empty() {
        return Err(PlanFieldError::Empty(field));
    }
    Ok(())
}

/// Non-empty and not option-shaped — the belt-and-braces check the handlers
/// run before a name goes anywhere near a git argv.
pub(crate) fn require_git_safe(value: &str, field: &'static str) -> Result<(), PlanFieldError> {
    require_non_empty(value, field)?;
    if value.starts_with('-') {
        return Err(PlanFieldError::OptionShaped(field));
    }
    Ok(())
}

pub(crate) fn require_hex(
    value: &str,
    lens: &[usize],
    field: &'static str,
    expected: &'static str,
) -> Result<(), PlanFieldError> {
    let hex_ok = lens.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex_ok {
        Ok(())
    } else {
        Err(PlanFieldError::NotHex { field, expected })
    }
}

/// The wire-boundary gate for a path a discard/delete operation names (#219,
/// M2.18a): [`require_git_safe`]'s non-empty/not-option-shaped check (the same
/// argv-injection defense every other name gets — a path can never be read by
/// git as a flag), plus three checks specific to a *path*: no embedded NUL (a
/// path containing one can never name a real filesystem entry, and a NUL in a
/// Rust `String` handed to a process argv is itself a spawn-time error
/// waiting to happen), no leading `/` (never absolute), and no `..` path
/// component (never able to walk out of the worktree lexically).
///
/// **Necessary but not sufficient.** A relative, `..`-free, NUL-free path can
/// still resolve outside the worktree through a symlinked component or a
/// symlinked final entry — no lexical check can see that, because a symlink's
/// target is not spelled in the path string at all. That is caught
/// separately, at execution time, by re-resolving the live filesystem with
/// `std::fs::canonicalize` — see `git-vista-server`'s
/// `planner::symlink_containment_guard`, which reuses the exact
/// canonicalize-and-compare pattern `bin/gv-sandbox/main.rs`'s
/// `resolve_excludes` already established for the same reason.
pub(crate) fn require_worktree_relative_path(
    value: &str,
    field: &'static str,
) -> Result<(), PlanFieldError> {
    require_git_safe(value, field)?;
    if value.contains('\0') || value.starts_with('/') {
        return Err(PlanFieldError::NotWorktreeRelative(field));
    }
    // `..` names the parent, `.` names the worktree root itself — neither is
    // a single file/entry this type may point at. Rejected here as the
    // newtype's own guarantee (review finding): today the execution-time
    // status re-check happens to refuse `.` too, since `git status` never
    // emits it as an entry path — but that is a coincidence of git's output
    // shape, not something this type should depend on a downstream check to
    // enforce, the same belt-and-braces posture `require_git_safe` documents
    // for itself.
    if value
        .split('/')
        .any(|component| component == ".." || component == ".")
    {
        return Err(PlanFieldError::NotWorktreeRelative(field));
    }
    Ok(())
}

/// The wire-boundary gate for the *name of a configured remote* (ADR 0044):
/// [`require_git_safe`]'s non-empty/not-option-shaped check, a length cap, and
/// then the rule that matters — the value must be a plain nickname, never a
/// transport target.
///
/// # Why a name-shaped value is a security boundary, not tidiness
///
/// `git fetch <arg>` does **not** refuse an argument that is not a configured
/// remote. It falls through to treating it as a URL or a path: verified
/// against git 2.43.0, `git fetch https://attacker.example/r.git` connects,
/// and `git fetch ghost.git` inside a repository with no such remote fetches
/// from the *directory* `ghost.git`. So an unvalidated `remote` field is a
/// request field that chooses which host this server — and whatever
/// credential helper or SSH agent the host offers it — opens a socket to.
/// `docs/adr/0002-versioned-api-contract.md` already refuses a request that
/// names a repository *path* for the same reason; this is the same refusal
/// one field over.
///
/// # The rule
///
/// ASCII letters, digits, `.`, `-` and `_`, with no leading `.` and no `..`
/// anywhere. That is a superset of every remote nickname `git remote add`
/// produces in practice (`origin`, `upstream`, `fork-2`, `remote.v2`) and a
/// strict subset of the character sets every transport target needs:
///
/// | Shape | Refused by |
/// |---|---|
/// | `https://host/r.git`, `git://…`, `ssh://…`, `file://…` | `:` and `/` |
/// | `git@host:r.git` (scp-style) | `@` and `:` |
/// | `/abs/path`, `./rel`, `../sibling` | `/`, leading `.` |
/// | `~/private.git`, `~user/r.git` | `~` |
/// | `ext::sh -c …` (git's command transport) | `:` and space |
/// | `-u`, `--upload-pack=…` | [`require_git_safe`] |
///
/// **Necessary but not sufficient**, in exactly the sense
/// [`require_worktree_relative_path`] documents for itself: a name that
/// passes here can still be one the repository has never configured
/// (`ghost.git` above), and git resolves *that* as a relative path. No
/// string rule can see the repository's config. That half is enforced at
/// execution time by `git-vista-server`'s `planner::enforce_fresh`, which
/// refuses when `Precondition::RemoteConfigured` did not hold — see
/// `planner::refuses_when_unmet_at_build`.
pub(crate) fn require_remote_name(
    value: &str,
    field: &'static str,
    max: usize,
) -> Result<(), PlanFieldError> {
    require_git_safe(value, field)?;
    if value.len() > max {
        return Err(PlanFieldError::TooLong { field, max });
    }
    let charset_ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_');
    // A leading `.` is refused for the same reason `require_worktree_relative_path`
    // refuses a `.` component: `.` and `..` are paths, and `.anything` is the
    // shape a hidden relative path takes. `..` is refused anywhere, not only at
    // the start, so `a..b` cannot become a traversal in some later joined path.
    if !charset_ok || value.starts_with('.') || value.contains("..") {
        return Err(PlanFieldError::NotRemoteName(field));
    }
    Ok(())
}

/// Non-empty after trimming and at most `max` bytes — the gate for a free-text
/// field whose *contents* the client chooses but whose *size* must not be able
/// to grow server-side state without bound (M2.21a, #235: `TagMessage`). The
/// same two rules as [`require_non_empty`] plus [`require_token`]'s length cap,
/// without the character-set restriction — a tag message is prose, not a token,
/// and never travels in a header, URL, or log line unescaped.
pub(crate) fn require_non_empty_bounded(
    value: &str,
    field: &'static str,
    max: usize,
) -> Result<(), PlanFieldError> {
    require_non_empty(value, field)?;
    if value.len() > max {
        return Err(PlanFieldError::TooLong { field, max });
    }
    Ok(())
}

/// A bounded, opaque token: non-empty, at most `max` characters, and made only
/// of ASCII letters, digits, `-` and `_`.
///
/// The character set is deliberately narrow because these values travel in HTTP
/// headers, URL path segments and log lines: nothing here needs escaping in any
/// of those places, and no control character, newline or `%` can be smuggled
/// through one. The length cap matters because the *client* chooses an
/// idempotency key, and a key becomes a map entry on the server (M1.08, #61).
pub(crate) fn require_token(
    value: &str,
    field: &'static str,
    max: usize,
) -> Result<(), PlanFieldError> {
    require_non_empty(value, field)?;
    if value.len() > max {
        return Err(PlanFieldError::TooLong { field, max });
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(PlanFieldError::NotToken(field));
    }
    Ok(())
}

/// Declare a validated, string-backed newtype: serializes as a bare JSON
/// string, and `Deserialize` runs the same validator as `new` so a malformed
/// value is a hard wire error, never a smuggled payload.
macro_rules! validated_string {
    ($(#[$doc:meta])* $name:ident, $validate:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap a raw value.
            pub fn new(value: impl Into<String>) -> Result<Self, $crate::newtype::PlanFieldError> {
                let value = value.into();
                let validate: fn(&str) -> Result<(), $crate::newtype::PlanFieldError> = $validate;
                validate(&value)?;
                Ok(Self(value))
            }

            /// The raw wire value.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation_bounds_length_and_character_set() {
        assert!(require_token("abc-123_XYZ", "key", 64).is_ok());
        assert_eq!(
            require_token("", "key", 64),
            Err(PlanFieldError::Empty("key"))
        );
        assert_eq!(
            require_token(&"a".repeat(65), "key", 64),
            Err(PlanFieldError::TooLong {
                field: "key",
                max: 64
            })
        );
        // The shapes that would matter in a header, a URL, or a log line.
        for bad in ["a b", "a\nb", "a/b", "a%2f", "a:b", "a\"b", "é"] {
            assert_eq!(
                require_token(bad, "key", 64),
                Err(PlanFieldError::NotToken("key")),
                "should have been refused: {bad:?}"
            );
        }
    }

    #[test]
    fn non_empty_bounded_enforces_both_rules_and_nothing_else() {
        assert!(require_non_empty_bounded("release notes\n\nwith prose.", "msg", 64).is_ok());
        // Prose characters a token would refuse are fine here.
        assert!(require_non_empty_bounded("v1.0 — stable (see #74)", "msg", 64).is_ok());
        assert_eq!(
            require_non_empty_bounded("", "msg", 64),
            Err(PlanFieldError::Empty("msg"))
        );
        assert_eq!(
            require_non_empty_bounded("   \n ", "msg", 64),
            Err(PlanFieldError::Empty("msg"))
        );
        assert_eq!(
            require_non_empty_bounded(&"a".repeat(65), "msg", 64),
            Err(PlanFieldError::TooLong {
                field: "msg",
                max: 64
            })
        );
        // The cap is in bytes, not chars: multi-byte text can't sneak past it.
        assert_eq!(
            require_non_empty_bounded(&"é".repeat(33), "msg", 64),
            Err(PlanFieldError::TooLong {
                field: "msg",
                max: 64
            })
        );
    }

    #[test]
    fn worktree_relative_path_accepts_ordinary_relative_paths() {
        assert!(require_worktree_relative_path("a.txt", "path").is_ok());
        assert!(require_worktree_relative_path("dir/sub/file.rs", "path").is_ok());
        // A leading dot component (not "..") is a perfectly normal filename.
        assert!(require_worktree_relative_path(".env", "path").is_ok());
    }

    #[test]
    fn worktree_relative_path_rejects_absolute_and_traversal_and_argv_injection() {
        assert_eq!(
            require_worktree_relative_path("/etc/passwd", "path"),
            Err(PlanFieldError::NotWorktreeRelative("path"))
        );
        assert_eq!(
            require_worktree_relative_path("../outside.txt", "path"),
            Err(PlanFieldError::NotWorktreeRelative("path"))
        );
        assert_eq!(
            require_worktree_relative_path("dir/../../outside.txt", "path"),
            Err(PlanFieldError::NotWorktreeRelative("path"))
        );
        assert_eq!(
            require_worktree_relative_path("a\0b", "path"),
            Err(PlanFieldError::NotWorktreeRelative("path"))
        );
        // A bare "." names the worktree root itself, not a single entry —
        // review finding: this must be the newtype's own guarantee, not
        // something left for a downstream re-check to happen to catch.
        assert_eq!(
            require_worktree_relative_path(".", "path"),
            Err(PlanFieldError::NotWorktreeRelative("path"))
        );
        assert_eq!(
            require_worktree_relative_path("dir/.", "path"),
            Err(PlanFieldError::NotWorktreeRelative("path"))
        );
        // Same argv-injection defense every other name gets — a path that
        // starts with '-' could be read by git as a flag.
        assert_eq!(
            require_worktree_relative_path("-rf", "path"),
            Err(PlanFieldError::OptionShaped("path"))
        );
        assert_eq!(
            require_worktree_relative_path("", "path"),
            Err(PlanFieldError::Empty("path"))
        );
    }
}
