//! Protocol version, the negotiation payload, and the compatibility check.
//!
//! The version negotiated here is the **wire-protocol** version — the shape of
//! the request/response contract — not the server's semver. It advances only on
//! a change to that contract, and it lets a long-lived (cached PWA) client and a
//! freshly restarted server detect that they no longer agree *before* the client
//! acts on a response it may misread.
//!
//! The mechanism (see `docs/adr/0002-versioned-api-contract.md`):
//!
//! - `GET /api/protocol` (the one unversioned endpoint) returns a
//!   [`ProtocolInfo`]: the server's current protocol version and the inclusive
//!   `[min, max]` window of client protocol versions it accepts, plus its semver.
//! - Every *other* `/api/*` request must carry the [`PROTOCOL_HEADER`] naming the
//!   protocol version the client speaks. The server checks it against its window
//!   with [`check_compatibility`] and refuses an out-of-window client with a
//!   structured error, which the frontend turns into an "Update Required" screen.

use serde::{Deserialize, Serialize};

/// The wire-protocol version this build speaks. Both roles reference it: the
/// server advertises it as its "current" version, and the client sends it in the
/// [`PROTOCOL_HEADER`] on every request. Bump this only when the request/response
/// contract changes in a way an older peer would misread.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest client protocol version this server build still accepts. Together
/// with [`MAX_CLIENT_PROTOCOL`] it is the compatibility window a client's version
/// must fall inside. Equal to [`PROTOCOL_VERSION`] until a compatible-but-older
/// contract must be supported.
pub const MIN_CLIENT_PROTOCOL: u32 = 1;

/// The newest client protocol version this server build can accept. A client
/// reporting a version above this is *ahead* of the server (the server was
/// downgraded, or the client cache is from a newer deploy) and is refused the
/// same way as one that is too old.
pub const MAX_CLIENT_PROTOCOL: u32 = 1;

/// Request header a client must send on every `/api/*` call **except**
/// `GET /api/protocol`, carrying the [`PROTOCOL_VERSION`] it was built against.
/// Lowercase because HTTP header names are case-insensitive and Axum/`http`
/// compare them lowercased; keeping the constant lowercase avoids surprises.
pub const PROTOCOL_HEADER: &str = "x-git-vista-protocol";

/// Response header the server sets on every `/api/*` response, echoing the
/// per-request correlation id (see [`crate::error::RequestId`]) so a client can
/// quote it when reporting a failure.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Parse the value of the [`PROTOCOL_HEADER`] a client sent. Returns `None` when
/// it is absent-shaped (empty) or not a base-10 `u32`; the server maps `None` to
/// a structured `invalid`/`missing` protocol error. Centralised (and tested)
/// here so the server and any future peer parse the header identically.
pub fn parse_protocol_header(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse::<u32>().ok()
}

/// Payload of `GET /api/protocol` — the one endpoint that needs no protocol
/// header, hit by a client before it trusts the rest of the API. It carries the
/// server's current protocol version, the `[min, max]` client window it accepts,
/// and its semver (for display/diagnostics only — compatibility turns on the
/// protocol numbers, never the semver).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolInfo {
    /// The server's current (native) protocol version.
    pub protocol_version: u32,
    /// Oldest client protocol version the server accepts.
    pub min_client_protocol: u32,
    /// Newest client protocol version the server accepts.
    pub max_client_protocol: u32,
    /// The server crate's semver (e.g. `"0.1.0"`) — informational only.
    pub server_version: String,
}

impl ProtocolInfo {
    /// The negotiation payload this build advertises. `server_version` is passed
    /// in (the server fills it from `CARGO_PKG_VERSION`) so this pure crate needs
    /// no build metadata of its own.
    pub fn advertise(server_version: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            min_client_protocol: MIN_CLIENT_PROTOCOL,
            max_client_protocol: MAX_CLIENT_PROTOCOL,
            server_version: server_version.into(),
        }
    }

    /// Whether a client speaking `client_protocol` falls inside this server's
    /// accepted window.
    pub fn compatibility(&self, client_protocol: u32) -> Compatibility {
        check_compatibility(
            client_protocol,
            self.min_client_protocol,
            self.max_client_protocol,
        )
    }
}

/// The verdict of comparing a client's protocol version against a server's
/// accepted `[min, max]` window. The three outcomes drive distinct handling: an
/// up-to-date client proceeds; a [`ClientTooOld`](Compatibility::ClientTooOld)
/// one must reload to catch up; a [`ClientTooNew`](Compatibility::ClientTooNew)
/// one is somehow ahead of the server. There is deliberately no ordering — the
/// question is only "in range or not, and which way out".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// The client's protocol version is within `[min, max]`.
    Compatible,
    /// The client is older than the server's minimum — reload to update.
    ClientTooOld,
    /// The client is newer than the server's maximum — the server is behind.
    ClientTooNew,
}

impl Compatibility {
    /// True only for [`Compatible`](Compatibility::Compatible).
    pub fn is_compatible(self) -> bool {
        matches!(self, Compatibility::Compatible)
    }
}

/// Is `client` within the inclusive `[min, max]` window the server accepts, and
/// if not, which side is it out on? Pure and total — the single definition of
/// "can these two talk", shared by the server's request check and the frontend's
/// startup negotiation.
pub fn check_compatibility(client: u32, min: u32, max: u32) -> Compatibility {
    if client < min {
        Compatibility::ClientTooOld
    } else if client > max {
        Compatibility::ClientTooNew
    } else {
        Compatibility::Compatible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_info_uses_the_build_constants() {
        let info = ProtocolInfo::advertise("0.1.0");
        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        assert_eq!(info.min_client_protocol, MIN_CLIENT_PROTOCOL);
        assert_eq!(info.max_client_protocol, MAX_CLIENT_PROTOCOL);
        assert_eq!(info.server_version, "0.1.0");
    }

    #[test]
    fn protocol_info_roundtrips_through_json() {
        let info = ProtocolInfo::advertise("1.2.3");
        let json = serde_json::to_string(&info).unwrap();
        let back: ProtocolInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn compatibility_covers_in_range_and_both_edges() {
        assert_eq!(check_compatibility(2, 1, 3), Compatibility::Compatible);
        assert_eq!(check_compatibility(1, 1, 3), Compatibility::Compatible);
        assert_eq!(check_compatibility(3, 1, 3), Compatibility::Compatible);
        assert_eq!(check_compatibility(0, 1, 3), Compatibility::ClientTooOld);
        assert_eq!(check_compatibility(4, 1, 3), Compatibility::ClientTooNew);
    }

    #[test]
    fn info_compatibility_matches_the_free_function() {
        let info = ProtocolInfo::advertise("0.1.0");
        assert!(info.compatibility(PROTOCOL_VERSION).is_compatible());
        assert_eq!(
            info.compatibility(MAX_CLIENT_PROTOCOL + 1),
            Compatibility::ClientTooNew
        );
    }

    #[test]
    fn header_parses_a_plain_integer_and_rejects_junk() {
        assert_eq!(parse_protocol_header("1"), Some(1));
        assert_eq!(parse_protocol_header("  7 "), Some(7));
        assert_eq!(parse_protocol_header(""), None);
        assert_eq!(parse_protocol_header("   "), None);
        assert_eq!(parse_protocol_header("v1"), None);
        assert_eq!(parse_protocol_header("-1"), None);
        assert_eq!(parse_protocol_header("1.0"), None);
    }
}
