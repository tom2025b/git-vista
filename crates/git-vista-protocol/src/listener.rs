//! Listener capability profiles and their HTTP header representation.
//!
//! The server has two deliberately different route tables: [`Full`](ListenerProfile::Full)
//! listeners register the complete API, while [`ReadOnly`](ListenerProfile::ReadOnly)
//! listeners omit every route in the write/selection profile (ADR 0005).  A
//! client cannot infer that distinction from its hostname or from an ordinary
//! HTTP failure, so every listener declares it in [`LISTENER_PROFILE_HEADER`].

/// Response header declaring which route profile served a request.
///
/// Lowercase for the same reason as the other shared header names: HTTP header
/// names are case-insensitive, while both Axum and `gloo-net` expose their
/// canonical lowercase spelling.
pub const LISTENER_PROFILE_HEADER: &str = "x-git-vista-listener-profile";

/// The route-capability profile of the listener that served a response.
///
/// This is about routes, not network topology.  `ReadOnly` currently belongs
/// to the LAN listener and `Full` to loopback, but clients consume the declared
/// capability rather than reconstructing it from the address they happened to
/// use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerProfile {
    /// The complete route table, including repository selection and writes.
    Full,
    /// ADR 0005's structurally read-only route table.
    ReadOnly,
}

impl ListenerProfile {
    /// Derive the declaration from the exact switch that constructs the route
    /// table.  Keeping this conversion here gives the server one mapping to use
    /// for both routing and the response header.
    pub const fn from_write_routes(write_routes: bool) -> Self {
        if write_routes {
            Self::Full
        } else {
            Self::ReadOnly
        }
    }

    /// Stable ASCII value carried by [`LISTENER_PROFILE_HEADER`].
    pub const fn as_header_value(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ReadOnly => "read-only",
        }
    }

    /// Parse one header value.  Unknown values are rejected rather than
    /// treated as `Full`: an unfamiliar profile must never create controls the
    /// listener may not be able to honour.
    pub fn from_header_value(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "read-only" => Some(Self::ReadOnly),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_switch_and_header_declaration_are_one_mapping() {
        for (write_routes, expected, wire) in [
            (true, ListenerProfile::Full, "full"),
            (false, ListenerProfile::ReadOnly, "read-only"),
        ] {
            let profile = ListenerProfile::from_write_routes(write_routes);
            assert_eq!(profile, expected);
            assert_eq!(profile.as_header_value(), wire);
            assert_eq!(ListenerProfile::from_header_value(wire), Some(expected));
        }
    }

    #[test]
    fn an_unknown_or_weakened_declaration_never_defaults_to_full() {
        for value in ["", "readonly", "lan", "FULL", "future-profile"] {
            assert_eq!(
                ListenerProfile::from_header_value(value),
                None,
                "unknown listener profile {value:?} was accepted"
            );
        }
    }

    #[test]
    fn the_profile_header_name_is_canonical_http_ascii() {
        assert_eq!(LISTENER_PROFILE_HEADER, "x-git-vista-listener-profile");
        assert_eq!(
            LISTENER_PROFILE_HEADER,
            LISTENER_PROFILE_HEADER.to_ascii_lowercase()
        );
    }
}
