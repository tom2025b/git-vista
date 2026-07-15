//! `GET /api/protocol` — the one unversioned endpoint (M1.02, #102).
//!
//! A client hits this before it trusts the rest of the API, to learn the server's
//! current protocol version and the client-version window it accepts. It is
//! exempt from the protocol-header requirement (see [`crate::middleware`]),
//! precisely because it exists to *bootstrap* that negotiation — a client can't be
//! required to already speak the protocol it's asking about.

use axum::{
    http::{header, HeaderValue},
    response::IntoResponse,
    Json,
};

use git_vista_protocol::ProtocolInfo;

/// Advertise this server's protocol version, its accepted client window, and its
/// semver ([`ProtocolInfo`]). Sent `no-store` like the live reads: a client must
/// always negotiate against the *current* contract, never a cached one.
pub(crate) async fn protocol_info() -> impl IntoResponse {
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    (
        no_store,
        Json(ProtocolInfo::advertise(env!("CARGO_PKG_VERSION"))),
    )
}
