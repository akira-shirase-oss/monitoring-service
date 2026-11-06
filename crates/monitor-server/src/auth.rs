//! Management auth (M5.1): every `/api/**` route requires `Authorization: Bearer <token>`
//! once `SMON_ADMIN_TOKEN` is configured. Denied means denied — there is no bypass path
//! inside the middleware; the only unauthenticated mode is dev-on-loopback, and boot
//! refuses a tokenless non-loopback listen rather than leave the store open.
//!
//! Comparison is done on SHA-256 digests: fixed-length, content-independent, so the
//! compare can't leak the token's prefix or length through timing.

use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::AppState;

/// The configured credential. `None` is only reachable on a loopback listen — main.rs
/// refuses to boot otherwise.
pub type AdminToken = Option<Arc<str>>;

pub async fn require_auth(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(token) = &state.admin_token else {
        // Dev-on-loopback mode: boot logged the warning; nothing here is a decision.
        return next.run(request).await;
    };
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| digests_match(presented.as_bytes(), token.as_bytes()));
    if authorized {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": { "code": "unauthenticated", "message": "a valid Bearer token is required" }
        })),
    )
        .into_response()
}

/// Hash-then-compare: both sides are 32-byte digests, so the equality check runs in
/// data-independent time and reveals nothing about *where* the inputs differ.
fn digests_match(presented: &[u8], configured: &[u8]) -> bool {
    let presented = Sha256::digest(presented);
    let configured = Sha256::digest(configured);
    presented == configured
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_compare_accepts_the_same_bytes() {
        assert!(digests_match(b"s3cret", b"s3cret"));
    }

    #[test]
    fn digest_compare_rejects_any_difference() {
        assert!(!digests_match(b"s3cret", b"s3creT"));
        assert!(!digests_match(b"s3cret", b"s3cret-extra"));
        assert!(!digests_match(b"", b"s3cret"));
    }
}
