//! Management auth (M5.1 + M7.2): every `/api/**` route requires a credential once
//! `SMON_ADMIN_TOKEN` is configured — either `Authorization: Bearer <token>` (CLI,
//! scripts) or the `HttpOnly` `smon_session` cookie the dashboard earns at `/api/login`.
//! Denied means denied — there is no bypass path inside the middleware; the only
//! unauthenticated mode is dev-on-loopback, and boot refuses a tokenless
//! non-loopback listen rather than leave the store open.
//!
//! Comparison is done on SHA-256 digests: fixed-length, content-independent, so the
//! compare can't leak the token's prefix or length through timing.
//!
//! CSRF contract (M7.2): a request that carries an `Origin` header must send one that
//! matches the `Host` — a cross-site browser request is rejected no matter which
//! credential it rides. A *cookie*-authed mutation must send `Origin` at all:
//! browsers always do on writes, so a missing header is not a browser. Bearer
//! requests need no Origin — `curl` and the CLI don't send one.

use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::AppState;

/// The configured credential. `None` is only reachable on a loopback listen — main.rs
/// refuses to boot otherwise.
pub type AdminToken = Option<Arc<str>>;

/// The `HttpOnly` cookie name the session id rides on.
pub const SESSION_COOKIE: &str = "smon_session";

/// Which credential authorized the request — decides whether Origin is mandatory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Credential {
    Bearer,
    Cookie,
}

pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(token) = &state.admin_token else {
        // Dev-on-loopback mode: boot logged the warning; nothing here is a decision.
        return next.run(request).await;
    };

    // An Origin that doesn't match the Host is a cross-site browser request —
    // rejected before the credential is even considered. `Host` may be absent on
    // HTTP/2 — the URI's authority is the same information.
    let authority = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()));
    if let Some(origin) = request.headers().get(header::ORIGIN)
        && !origin_matches_host(origin, authority.as_deref())
    {
        return deny(
            StatusCode::FORBIDDEN,
            "origin_mismatch",
            "Origin does not match Host",
        );
    }

    let credential = if bearer_matches(request.headers().get(header::AUTHORIZATION), token) {
        Some(Credential::Bearer)
    } else if let Some(session) = session_cookie(request.headers().get(header::COOKIE)) {
        match state.db.session_valid(session).await {
            Ok(true) => Some(Credential::Cookie),
            Ok(false) => None,
            Err(error) => {
                // Failing closed would masquerade a broken store as "wrong credential";
                // the honest answer is that authorization cannot be decided right now.
                tracing::error!(%error, "session lookup failed");
                return deny(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "internal",
                    "the session store is unavailable",
                );
            }
        }
    } else {
        None
    };

    match credential {
        Some(Credential::Bearer) => next.run(request).await,
        Some(Credential::Cookie) => {
            if is_mutation(request.method()) && request.headers().get(header::ORIGIN).is_none() {
                return deny(
                    StatusCode::FORBIDDEN,
                    "origin_required",
                    "cookie-authenticated writes require a matching Origin",
                );
            }
            request.extensions_mut().insert(CookieAuth);
            next.run(request).await
        }
        None => deny(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "a valid Bearer token or session cookie is required",
        ),
    }
}

/// Request marker: this call was authorized by the session cookie, not a bearer token.
/// Handlers that care (none yet — parity is the point) can read it from extensions.
#[derive(Debug, Clone, Copy)]
pub struct CookieAuth;

/// `Origin: http://host[:port]` must name the same authority the request was sent to —
/// host plus port, not scheme (a TLS-terminating proxy still terminates the same site).
pub fn origin_matches_host(origin: &header::HeaderValue, authority: Option<&str>) -> bool {
    let (Ok(origin), Some(authority)) = (origin.to_str(), authority) else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(origin) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed
            .host_str()
            .is_some_and(|h| authority_matches(h, parsed.port(), authority))
}

/// `authority` is `host[:port]` from the URL; `host_header` is the raw Host value
/// (same shape). Compare case-insensitively on host and exactly on the effective port —
/// default ports are equal by URL rules (`http://x` == `x:80` is NOT assumed here: the
/// browser writes the explicit port it dialed, so string authority compare is right).
fn authority_matches(host: &str, port: Option<u16>, host_header: &str) -> bool {
    let normalized = match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    normalized.eq_ignore_ascii_case(host_header)
}

fn is_mutation(method: &Method) -> bool {
    !matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

/// The session id from the Cookie header — name-prefix match, value must be non-empty.
/// `pub` so `logout` parses the cookie through the same code the middleware trusts.
pub fn session_cookie(cookie: Option<&header::HeaderValue>) -> Option<&str> {
    cookie?.to_str().ok()?.split(';').find_map(|pair| {
        let pair = pair.trim();
        let value = pair.strip_prefix(SESSION_COOKIE)?.strip_prefix('=')?;
        (!value.is_empty()).then_some(value)
    })
}

fn bearer_matches(header: Option<&header::HeaderValue>, token: &Arc<str>) -> bool {
    header
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| digests_match(presented.as_bytes(), token.as_bytes()))
}

fn deny(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

/// Hash-then-compare: both sides are 32-byte digests, so the equality check runs in
/// data-independent time and reveals nothing about *where* the inputs differ.
pub fn digests_match(presented: &[u8], configured: &[u8]) -> bool {
    let presented = Sha256::digest(presented);
    let configured = Sha256::digest(configured);
    presented == configured
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

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

    fn origin(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    #[test]
    fn a_matching_origin_is_same_authority() {
        assert!(origin_matches_host(
            &origin("http://127.0.0.1:58400"),
            Some("127.0.0.1:58400")
        ));
        assert!(origin_matches_host(
            &origin("https://mon.example.com"),
            Some("mon.example.com")
        ));
        // An IPv6 authority keeps its brackets in both places.
        assert!(origin_matches_host(
            &origin("http://[::1]:9000"),
            Some("[::1]:9000")
        ));
    }

    #[test]
    fn a_cross_site_origin_is_rejected() {
        let host = "127.0.0.1:58400";
        for bad in [
            "http://127.0.0.2:58400",
            "http://127.0.0.1:58401",
            "https://evil.example.com",
            "javascript:alert(1)",
            "null",
            "not a url",
        ] {
            assert!(
                !origin_matches_host(&origin(bad), Some(host)),
                "{bad} must not match"
            );
        }
        assert!(!origin_matches_host(&origin("http://x"), None));
        assert!(!origin_matches_host(
            &origin("https://example.com"),
            Some("example.com:443")
        ));
        // The browser spells default ports explicitly; a port-ful Host can't claim a
        // port-less Origin.
        assert!(!origin_matches_host(
            &origin("https://example.com"),
            Some("example.com:8443")
        ));
    }

    #[test]
    fn session_cookie_parses_its_own_name_only() {
        let header = HeaderValue::from_static("other=1; smon_session=abc-123 ; theme=dark");
        assert_eq!(session_cookie(Some(&header)), Some("abc-123"));
        assert_eq!(
            session_cookie(Some(&HeaderValue::from_static("smon_session="))),
            None
        );
        assert_eq!(
            session_cookie(Some(&HeaderValue::from_static("smon_sessionx=abc"))),
            None
        );
        assert_eq!(session_cookie(None), None);
    }

    #[test]
    fn mutations_are_everything_but_safe_reads() {
        assert!(is_mutation(&Method::POST));
        assert!(is_mutation(&Method::PATCH));
        assert!(is_mutation(&Method::DELETE));
        assert!(!is_mutation(&Method::GET));
        assert!(!is_mutation(&Method::HEAD));
    }
}
