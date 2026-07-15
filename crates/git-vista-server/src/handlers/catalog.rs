//! `GET /api/catalog` (M1.03): the capability report of servable repositories.
//!
//! The browser learns *what* it may open — and the opaque id to address each by —
//! without ever seeing a filesystem path (unless the operator opted into path
//! exposure). This is the read side of the allowlisted-catalog model: selection
//! is by id, resolved server-side against a set the server itself registered.

use axum::http::{header, HeaderValue};
use axum::response::IntoResponse;
use axum::Json;

use crate::state::catalog_descriptors;

/// List the repositories this server will serve, each addressed by opaque
/// repository/worktree ids and classified (bare / main / linked worktree).
/// Absolute paths are omitted unless the operator set `GIT_VISTA_EXPOSE_PATHS`.
///
/// Sent `no-store` like the other live reads: the catalog changes at runtime (a
/// clone opened from a URL adds an entry), so a cached copy would be misleading.
pub(crate) async fn catalog_list() -> impl IntoResponse {
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    (no_store, Json(catalog_descriptors()))
}
