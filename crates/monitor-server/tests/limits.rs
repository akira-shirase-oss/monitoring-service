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
use monitor_domain::WorkspaceId;
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
        session_idle: Duration::from_hours(1),
        password_work_wait: monitor_server::auth::PASSWORD_WORK_WAIT,
        token_use: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
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
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_monitors()
            .await?
            .is_empty()
    );
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

/// `POST /api/login` is the one unauthenticated credential check. A fixed 300 ms delay per
/// wrong token slowed each guess but not the number in flight: 50 connections guessed 50
/// at a time. Now one address gets a burst of five guesses and the rest are refused with
/// 429 before the token is compared, however many connections it opens (throttle.rs).
#[tokio::test]
async fn parallel_wrong_logins_are_bounded_and_the_rest_get_429() -> anyhow::Result<()> {
    let (app, db, _tmp) = app().await?;
    let mut guesses = tokio::task::JoinSet::new();
    for n in 0..50 {
        let app = app.clone();
        guesses.spawn(async move {
            let response = app
                .oneshot(
                    Request::post("/api/login")
                        .header("content-type", "application/json")
                        .body(Body::from(format!(r#"{{"token":"guess-{n}"}}"#)))?,
                )
                .await?;
            let retry_after = response.headers().get("retry-after").cloned();
            anyhow::Ok((response.status(), retry_after))
        });
    }
    let (mut evaluated, mut refused) = (0, 0);
    while let Some(outcome) = guesses.join_next().await {
        let (status, retry_after) = outcome??;
        match status {
            StatusCode::UNAUTHORIZED => evaluated += 1,
            StatusCode::TOO_MANY_REQUESTS => {
                refused += 1;
                assert!(retry_after.is_some(), "429 says when to retry");
            }
            other => anyhow::bail!("unexpected {other}"),
        }
    }
    assert!(
        (1..=5).contains(&evaluated),
        "at most 5 guesses evaluated for one address, got {evaluated}"
    );
    assert_eq!(evaluated + refused, 50);

    // Not a lockout for anyone else: the operator's correct token, from their own address,
    // logs in during and after the flood.
    assert_eq!(
        login_from(&app, OWNER, TOKEN).await?.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(db.sweep_expired_sessions().await?, 1);
    Ok(())
}

/// L7: `Router::layer` makes the *last* call outermost, so the request-id layer sat
/// innermost: the propagating layer never saw a generated id, responses carried
/// `x-request-id` only when the client sent one, and the 408 came from outside the trace.
/// Every response — a plain read, a refusal, a timeout — now carries an id.
#[tokio::test]
async fn every_response_carries_a_request_id_even_a_timeout() -> anyhow::Result<()> {
    let (app, _db, _tmp) = app().await?;
    for request in [
        Request::get("/livez").body(Body::empty())?,
        Request::get("/api/monitors").body(Body::empty())?, // 401
    ] {
        let response = app.clone().oneshot(request).await?;
        assert!(
            response.headers().get("x-request-id").is_some(),
            "{}: no x-request-id",
            response.status()
        );
    }
    let stalled =
        Full::new(Bytes::from_static(br#"{"token":""#)).with_trailers(std::future::pending::<
            Option<Result<axum::http::HeaderMap, std::convert::Infallible>>,
        >());
    let request = Request::post("/api/login")
        .header("content-type", "application/json")
        .body(Body::new(stalled))?;
    tokio::time::pause();
    let response = tokio::time::timeout(Duration::from_secs(60), app.oneshot(request))
        .await
        .map_err(|_| anyhow::anyhow!("no answer within 60 s"))??;
    tokio::time::resume();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert!(
        response.headers().get("x-request-id").is_some(),
        "the 408 carries an id"
    );
    Ok(())
}

const OWNER: &str = "203.0.113.10:50000";
const ATTACKER: &str = "198.51.100.66:40000";

/// A login as the client at `peer` would send it (the server records the peer address in
/// `ConnectInfo`). Returns the status and `Retry-After`.
async fn login_from(
    app: &axum::Router,
    peer: &str,
    token: &str,
) -> anyhow::Result<(StatusCode, Option<String>)> {
    let mut request = Request::post("/api/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "token": token }).to_string(),
        ))?;
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>()?,
    ));
    let response = app.clone().oneshot(request).await?;
    let retry = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    Ok((response.status(), retry))
}

/// M1: the login throttle was one global pool of four slots, so an anonymous client that
/// kept four wrong logins in flight locked the owner out of the dashboard (19 of 20 correct
/// logins got 429). Throttling is per client address now: the attacker's guesses are its
/// own problem.
#[tokio::test]
async fn an_attackers_wrong_logins_never_lock_the_owner_out() -> anyhow::Result<()> {
    let (app, _db, _tmp) = app().await?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut attackers = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let app = app.clone();
        let stop = Arc::clone(&stop);
        attackers.spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = login_from(&app, ATTACKER, "wrong-guess").await;
                tokio::task::yield_now().await;
            }
        });
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut accepted = 0;
    for _ in 0..20 {
        if login_from(&app, OWNER, TOKEN).await?.0 == StatusCode::NO_CONTENT {
            accepted += 1;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    while attackers.join_next().await.is_some() {}
    assert_eq!(accepted, 20, "the owner's correct logins were refused");
    Ok(())
}

/// One address gets a short burst of guesses, then 429 (with `Retry-After`) — before its
/// token is even compared, so a throttled address can't learn whether a guess was right —
/// and one more guess per refill period after that. Other addresses are unaffected.
#[tokio::test]
async fn one_address_gets_a_short_burst_of_guesses_then_429_until_it_refills() -> anyhow::Result<()>
{
    let (app, _db, _tmp) = app().await?;
    tokio::time::pause();
    for n in 0..5 {
        assert_eq!(
            login_from(&app, ATTACKER, "wrong").await?.0,
            StatusCode::UNAUTHORIZED,
            "guess {n}"
        );
    }
    let (status, retry) = login_from(&app, ATTACKER, "wrong").await?;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(retry.is_some_and(|s| s.parse::<u64>().is_ok_and(|s| (1..=2).contains(&s))));
    assert_eq!(
        login_from(&app, ATTACKER, TOKEN).await?.0,
        StatusCode::TOO_MANY_REQUESTS,
        "a throttled address is not told whether its token is right"
    );
    assert_eq!(
        login_from(&app, "198.51.100.67:1", "wrong").await?.0,
        StatusCode::UNAUTHORIZED,
        "another address has its own budget"
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(
        login_from(&app, ATTACKER, "wrong").await?.0,
        StatusCode::UNAUTHORIZED,
        "a guess comes back per refill period"
    );
    // ...and only about one: the next few run dry again (each wrong guess's 300 ms
    // penalty refills a little while it sleeps).
    let mut more = 0;
    while login_from(&app, ATTACKER, "wrong").await?.0 == StatusCode::UNAUTHORIZED {
        more += 1;
        assert!(more <= 1, "the refill gave back more than it earned");
    }
    Ok(())
}

/// L1: a wrong `Authorization: Bearer` was never slowed or counted — 200 parallel guesses
/// were all evaluated in a few milliseconds. Bearer guesses draw on the same per-address
/// budget as logins.
#[tokio::test]
async fn wrong_bearer_tokens_are_throttled_per_address_too() -> anyhow::Result<()> {
    let (app, _db, _tmp) = app().await?;
    let bearer_from = |peer: &'static str, token: &'static str| {
        let app = app.clone();
        async move {
            let mut request = Request::get("/api/monitors")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())?;
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                peer.parse::<std::net::SocketAddr>()?,
            ));
            anyhow::Ok(app.oneshot(request).await?.status())
        }
    };
    let mut guesses = tokio::task::JoinSet::new();
    for _ in 0..200 {
        guesses.spawn(bearer_from(ATTACKER, "wrong"));
    }
    let (mut evaluated, mut refused) = (0, 0);
    while let Some(status) = guesses.join_next().await {
        match status?? {
            StatusCode::UNAUTHORIZED => evaluated += 1,
            StatusCode::TOO_MANY_REQUESTS => refused += 1,
            other => anyhow::bail!("unexpected {other}"),
        }
    }
    assert!(evaluated <= 5, "{evaluated} bearer guesses evaluated");
    assert_eq!(evaluated + refused, 200);
    assert_eq!(bearer_from(OWNER, TOKEN).await?, StatusCode::OK);
    Ok(())
}
