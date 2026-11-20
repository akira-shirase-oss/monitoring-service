//! S1b: invite-gated signup, account sign-in and account sessions (D-016 digest rule
//! reused). Invites are single-use, hashed and expiring; password verification is
//! argon2id behind a process-wide work limit; an account session resolves its own
//! workspace — never the operator's default.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use monitor_db::Db;
use monitor_domain::{NewMonitor, WorkspaceId};
use monitor_server::auth;
use monitor_server::vitals::Vitals;
use tempfile::TempDir;
use tokio::sync::watch;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";

type Fixture = (axum::Router, Db, TempDir);

async fn fixture() -> anyhow::Result<Fixture> {
    // Tests wait out the argon2 pool instead of ever seeing `busy`: the permits are
    // process-global, so parallel sign-ups/sign-ins queue behind each other and any
    // fixed wait is a flake under load. The busy answer has its own coverage — the
    // sign-in flood test below passes a one-millisecond wait.
    fixture_with_wait(Duration::from_secs(600)).await
}

/// The production fixture, with the password-work queue deadline the caller picks —
/// the flood test shortens it so a queued acquire overflows at once.
async fn fixture_with_wait(password_work_wait: Duration) -> anyhow::Result<Fixture> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("api.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    let (registry, _rx) = watch::channel(Arc::new(Vec::new()));
    let app = monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry,
        vitals: Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait,
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    });
    Ok((app, db, tmp))
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    cookie: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    call_from(app, None, method, uri, body, cookie).await
}

/// `call` as seen from `peer` — what makes the per-address throttle believe the
/// request came from that address (`auth_inventory` uses the same `ConnectInfo`
/// hook).
#[expect(
    clippy::unwrap_used,
    reason = "test helper: a request that can't build is a failed test"
)]
async fn call_from(
    app: &axum::Router,
    peer: Option<std::net::SocketAddr>,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    cookie: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let payload = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&json).unwrap())
        }
        None => Body::empty(),
    };
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, format!("__Host-smon_session={cookie}"));
    }
    let mut request = request.body(payload).unwrap();
    if let Some(peer) = peer {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

#[expect(
    clippy::expect_used,
    reason = "test helper: a sign-in without its cookie is a failed test"
)]
fn set_cookie(headers: &axum::http::HeaderMap) -> String {
    let raw = headers
        .get(header::SET_COOKIE)
        .expect("a session cookie is set")
        .to_str()
        .expect("cookie header is text")
        .to_owned();
    raw.strip_prefix("__Host-smon_session=")
        .expect("the __Host-smon_session cookie")
        .split(';')
        .next()
        .expect("cookie id")
        .to_owned()
}

/// Operator helper: mint an invite through the API (dev mode ⇒ Operator principal).
#[expect(
    clippy::expect_used,
    reason = "test helper: a mint without a code is a failed test"
)]
async fn mint_invite(app: &axum::Router) -> String {
    let (status, _, body) = call(
        app,
        "POST",
        "/api/invites",
        Some(serde_json::json!({})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["invite"]["code"].as_str().expect("code").to_owned()
}

#[tokio::test]
async fn signup_needs_a_valid_unused_unexpired_invite() -> anyhow::Result<()> {
    let (app, db, _tmp) = fixture().await?;
    let signup = |invite: &str| {
        call(
            &app,
            "POST",
            "/api/signup",
            Some(serde_json::json!({
                "email": "new@tenant.example",
                "password": PASSWORD,
                "invite": invite,
            })),
            None,
        )
    };

    // No code, wrong code, right code — only the last mints an account.
    let (status, _, _) = signup("").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _, body) = signup("smon_inv_bogus").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "invalid_invite");

    let code = mint_invite(&app).await;
    let (status, _, body) = signup(&code).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["account"]["email"], "new@tenant.example");

    // Single-use: a second signup with the same code gets the same denial as a bad one.
    let (status, _, body) = signup(&code).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "invalid_invite");

    // Expired: an invite minted with an already-past expiry reads as invalid — the row
    // is real, the code is right, only time failed it.
    let expired_digest = auth::invite_key("smon_inv_expired");
    db.create_invite(
        &expired_digest,
        time::OffsetDateTime::now_utc() - time::Duration::days(2),
        time::OffsetDateTime::now_utc() - time::Duration::days(1),
    )
    .await?;
    let (status, _, body) = signup("smon_inv_expired").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    Ok(())
}

#[tokio::test]
async fn signup_then_signin_then_a_workspace_of_ones_own() -> anyhow::Result<()> {
    let (app, db, _tmp) = fixture().await?;
    // The operator's monitor lives in the default workspace.
    let operator_monitor =
        NewMonitor::new("operator's", url::Url::parse("https://ops.example/health")?)?
            .into_monitor();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&operator_monitor)
        .await?;

    let code = mint_invite(&app).await;
    let (status, _, _) = call(
        &app,
        "POST",
        "/api/signup",
        Some(serde_json::json!({
            "email": "Tenant@Example.com",   // normalized on the way in
            "password": PASSWORD,
            "invite": code,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Sign in: 204 + cookie; the sessions row holds a digest, never the id itself.
    let (status, headers, _) = call(
        &app,
        "POST",
        "/api/login",
        Some(serde_json::json!({
            "email": "tenant@example.com",
            "password": PASSWORD,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cookie_id = set_cookie(&headers);
    let key = auth::account_session_key(&cookie_id);
    let rows = db.list_workspaces().await?;
    assert_eq!(rows.len(), 2, "default + the account's own");

    // The account's list is its own workspace: empty — the operator's monitor is not
    // merely unnamed, it is unreachable through this credential (D-020).
    let (status, _, body) = call(&app, "GET", "/api/monitors", None, Some(&cookie_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["monitors"], serde_json::json!([]));
    // And the operator's monitor by id is a 404, not a 200 with a foreign name.
    let (status, _, _) = call(
        &app,
        "GET",
        &format!("/api/monitors/{}", operator_monitor.id.0),
        None,
        Some(&cookie_id),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The digest key in the store is not the cookie id (D-016).
    assert_ne!(key, cookie_id);
    assert!(
        db.account_session_at(
            &key,
            time::OffsetDateTime::now_utc(),
            Duration::from_secs(60)
        )
        .await?
        .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn signin_is_one_envelope_for_every_miss() -> anyhow::Result<()> {
    let (app, _db, _tmp) = fixture().await?;
    let code = mint_invite(&app).await;
    call(
        &app,
        "POST",
        "/api/signup",
        Some(serde_json::json!({
            "email": "real@tenant.example",
            "password": PASSWORD,
            "invite": code,
        })),
        None,
    )
    .await;

    // Wrong password and unknown email answer identically — "invalid credentials".
    for (email, password) in [
        ("real@tenant.example", "not the password"),
        ("nobody@tenant.example", PASSWORD),
    ] {
        let (status, _, body) = call(
            &app,
            "POST",
            "/api/login",
            Some(serde_json::json!({ "email": email, "password": password })),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{email}");
        assert_eq!(body["error"]["message"], "invalid credentials");
    }
    Ok(())
}

#[tokio::test]
async fn logout_ends_the_account_session() -> anyhow::Result<()> {
    let (app, db, _tmp) = fixture().await?;
    let code = mint_invite(&app).await;
    call(
        &app,
        "POST",
        "/api/signup",
        Some(serde_json::json!({
            "email": "gone@tenant.example",
            "password": PASSWORD,
            "invite": code,
        })),
        None,
    )
    .await;
    let (status, headers, _) = call(
        &app,
        "POST",
        "/api/login",
        Some(serde_json::json!({
            "email": "gone@tenant.example",
            "password": PASSWORD,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cookie_id = set_cookie(&headers);
    let (_, _, _) = call(&app, "POST", "/api/logout", None, Some(&cookie_id)).await;
    assert!(
        db.account_session_at(
            &auth::account_session_key(&cookie_id),
            time::OffsetDateTime::now_utc(),
            Duration::from_secs(60)
        )
        .await?
        .is_none(),
        "logout deletes the account session row"
    );
    let (status, _, _) = call(&app, "GET", "/api/monitors", None, Some(&cookie_id)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "dev mode still serves — without the workspace"
    );
    Ok(())
}

/// The credential bucket is per address: one address that spends its guesses gets
/// 429 + Retry-After, and the spend never touches another address's sign-in.
#[tokio::test]
async fn one_address_runs_out_of_guesses_and_another_still_signs_in() -> anyhow::Result<()> {
    let (app, _db, _tmp) = fixture().await?;
    let flood: std::net::SocketAddr = "198.51.100.7:40000".parse()?;
    let honest: std::net::SocketAddr = "198.51.100.8:40000".parse()?;

    // An account exists (signing up from the honest address spends none of the flood's
    // budget), so the flood's misses are real wrong-password sign-in attempts.
    let code = mint_invite(&app).await;
    let (status, _, _) = call_from(
        &app,
        Some(honest),
        "POST",
        "/api/signup",
        Some(serde_json::json!({
            "email": "honest@tenant.example",
            "password": PASSWORD,
            "invite": code,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The flood address spends its burst on a wrong password. The token is spent at
    // admission, *before* the argon2 compare — so the guesses go out together and all
    // admissions cluster inside one refill period on any machine, however slow the
    // hashing (the old serial loop let the 0.5/s refill keep pace: flake, not rule).
    let wrong = serde_json::json!({
        "email": "honest@tenant.example", "password": "not the password",
    });
    let started = std::time::Instant::now();
    let mut guesses = tokio::task::JoinSet::new();
    for attempt_no in 1..=8 {
        let (app, wrong) = (app.clone(), wrong.clone());
        guesses.spawn(async move {
            let (status, headers, body) =
                call_from(&app, Some(flood), "POST", "/api/login", Some(wrong), None).await;
            anyhow::Ok((attempt_no, status, headers, body))
        });
    }
    let (mut evaluated, mut refused) = (0_u64, false);
    while let Some(outcome) = guesses.join_next().await {
        let (attempt_no, status, headers, body) = outcome??;
        match status {
            StatusCode::UNAUTHORIZED => {
                evaluated += 1;
                assert_eq!(body["error"]["message"], "invalid credentials", "{body}");
            }
            StatusCode::TOO_MANY_REQUESTS => {
                assert_eq!(body["error"]["code"], "too_many_requests", "{body}");
                assert!(
                    headers.get(header::RETRY_AFTER).is_some(),
                    "a 429 must say when to retry"
                );
                refused = true;
            }
            // A timed-out request was still admitted — its token is spent either way.
            StatusCode::REQUEST_TIMEOUT => evaluated += 1,
            other => anyhow::bail!("guess {attempt_no}: unexpected {other}"),
        }
    }
    assert!(
        refused,
        "the bucket still had a token after eight wrong guesses"
    );
    // The burst is five tokens; the only slack is what real elapsed time refilled
    // between the first admit and the last join (0.5/s — REFILL_EVERY is 2 s).
    let refilled = started
        .elapsed()
        .as_millis()
        .div_ceil(monitor_server::throttle::REFILL_EVERY.as_millis());
    assert!(
        u128::from(evaluated) <= 5 + refilled,
        "{evaluated} guesses reached the password check (burst 5 + {refilled} refilled)"
    );

    // The other address's bucket is untouched: it still reaches the credential check
    // and signs in.
    let (status, _, _) = call_from(
        &app,
        Some(honest),
        "POST",
        "/api/login",
        Some(serde_json::json!({
            "email": "honest@tenant.example", "password": PASSWORD,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    Ok(())
}

/// Password work is bounded: at most two hashes run, and the queue in front of them
/// is bounded by a deadline. A sign-in burst beyond the queue gets 503 `busy` with
/// Retry-After — a waiting request must never hold its connection permit forever —
/// while the rest of the API keeps answering.
#[tokio::test]
async fn a_signin_flood_gets_busy_answers_and_the_api_stays_up() -> anyhow::Result<()> {
    // A 1 ms deadline: every queued acquire overflows at once. How many requests win
    // a permit first depends on what the other tests' parallel threads hold, so the
    // assertion is that busy answers happen at all — not an exact split.
    let (app, _db, _tmp) = fixture_with_wait(Duration::from_millis(1)).await?;

    // Twelve addresses sign in at once. Two permits run; the queue deadline decides
    // the rest — every request that cannot start inside the wait answers busy instead
    // of parking on a connection forever.
    let mut tasks = Vec::new();
    for n in 0..12_u32 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let peer: std::net::SocketAddr =
                std::net::SocketAddr::from((std::net::Ipv4Addr::from(0xC633_6400 + n), 40000));
            call_from(
                &app,
                Some(peer),
                "POST",
                "/api/login",
                Some(serde_json::json!({
                    "email": format!("flood{n}@tenant.example"),
                    "password": "any password at all",
                })),
                None,
            )
            .await
            .0
        }));
    }
    let mut busy = 0_u32;
    let mut denied = 0_u32;
    for task in tasks {
        match task.await? {
            StatusCode::SERVICE_UNAVAILABLE => busy += 1,
            StatusCode::UNAUTHORIZED => denied += 1,
            other => panic!("a flood answer is busy or denied, not {other}"),
        }
    }
    assert_eq!(denied + busy, 12, "every request answered");
    assert!(
        busy >= 1,
        "a 12-deep flood must overflow the queue: all {denied} denied"
    );

    // And while password work was saturated, an unrelated route never noticed.
    let (status, _, _) = call(&app, "GET", "/livez", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(&app, "GET", "/api/monitors", None, None).await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}
