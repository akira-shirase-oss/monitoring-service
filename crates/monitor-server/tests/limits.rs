//! The request bounds, against the real router: every body is capped at 16 KiB before it
//! is parsed — including the unauthenticated `POST /api/login` — and every request is
//! answered within 10 s (408), however slowly its client sends.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use http_body_util::{BodyExt, Full};
use monitor_db::Db;
use tower::ServiceExt;

const TOKEN: &str = "limits-token-0123456789abcdef0123456789ab";
const LIMIT: usize = 16 * 1024;

/// An authed router whose sessions expire the moment they are made, so
/// `sweep_expired_sessions` counts exactly the session rows a request created.
async fn app() -> anyhow::Result<(axum::Router, Db, tempfile::TempDir)> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("limits.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let app = monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: Some(Arc::from(TOKEN)),
        session_ttl: Duration::ZERO,
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100,
        allow_local_targets: false,
        version: "test",
    });
    Ok((app, db, tmp))
}

/// A login body carrying the *correct* token, padded to `total` bytes — parsed, it would
/// succeed, so only the size limit can refuse it.
fn padded_login(total: usize) -> String {
    let head = format!(r#"{{"token":"{TOKEN}","pad":""#);
    let pad = total - head.len() - 2;
    format!("{head}{}\"}}", "x".repeat(pad))
}

#[tokio::test]
async fn an_oversized_login_is_413_before_parsing_and_creates_no_session() -> anyhow::Result<()> {
    let (app, db, _tmp) = app().await?;
    let over = padded_login(LIMIT + 1024);
    assert!(over.len() > LIMIT);

    // With the length declared, and streamed without one (a chunked client).
    for declared in [true, false] {
        let mut request = Request::post("/api/login").header("content-type", "application/json");
        if declared {
            request = request.header("content-length", over.len());
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(over.clone()))?)
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "declared length: {declared}"
        );
        assert!(response.headers().get("set-cookie").is_none());
    }
    assert_eq!(db.sweep_expired_sessions().await?, 0, "no session row");

    // Control: the same token with padding under the limit logs in — the padding is not
    // what refused the big one.
    let under = padded_login(LIMIT - 1024);
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(under))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        db.sweep_expired_sessions().await?,
        1,
        "the control made one"
    );
    Ok(())
}

#[tokio::test]
async fn an_oversized_authed_write_is_413_and_stores_nothing() -> anyhow::Result<()> {
    let (app, db, _tmp) = app().await?;
    let body = serde_json::json!({
        "name": "x".repeat(LIMIT), "url": "https://example.com/",
    })
    .to_string();
    let response = app
        .oneshot(
            Request::post("/api/monitors")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from(body))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(db.list_monitors().await?.is_empty());
    Ok(())
}

/// A client that sends the start of its login body and then nothing (a slow-body client)
/// must not hold the handler forever: the request times out with 408 at 10 s. Time is
/// paused, so the 10 s pass instantly — and a 60 s guard turns a missing timeout into a
/// failure instead of a hung test.
#[tokio::test]
async fn a_request_whose_body_never_finishes_is_408_at_the_deadline() -> anyhow::Result<()> {
    let (app, db, _tmp) = app().await?;
    let stalled =
        Full::new(Bytes::from_static(br#"{"token":""#)).with_trailers(std::future::pending::<
            Option<Result<axum::http::HeaderMap, std::convert::Infallible>>,
        >());
    let request = Request::post("/api/login")
        .header("content-type", "application/json")
        .body(Body::new(stalled))?;

    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let response = tokio::time::timeout(Duration::from_secs(60), app.oneshot(request))
        .await
        .map_err(|_| anyhow::anyhow!("no answer within 60 s: the request timeout is missing"))??;
    let waited = started.elapsed();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert!(
        waited >= Duration::from_secs(10) && waited < Duration::from_secs(11),
        "answered after {waited:?}, expected the 10 s deadline"
    );
    tokio::time::resume();
    assert_eq!(db.sweep_expired_sessions().await?, 0, "no session row");
    Ok(())
}
