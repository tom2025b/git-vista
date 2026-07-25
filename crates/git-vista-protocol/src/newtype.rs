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
            PlanFieldError::NotToken(field) => write!(
                f,
                "{field} may only contain letters, digits, '-' and '_'"
            ),
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
}
