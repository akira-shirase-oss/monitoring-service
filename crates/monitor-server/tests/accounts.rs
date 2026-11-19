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

async fn call(
    app: &axum::Router,
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
        request = request.header(header::COOKIE, format!("smon_session={cookie}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(payload).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

fn set_cookie(headers: &axum::http::HeaderMap) -> String {
    let raw = headers
        .get(header::SET_COOKIE)
        .expect("a session cookie is set")
        .to_str()
        .expect("cookie header is text")
        .to_owned();
    raw.strip_prefix("smon_session=")
        .expect("the smon_session cookie")
        .split(';')
        .next()
        .expect("cookie id")
        .to_owned()
}

/// Operator helper: mint an invite through the API (dev mode ⇒ Operator principal).
async fn mint_invite(app: &axum::Router) -> String {
    let (status, _, body) =
        call(app, "POST", "/api/invites", Some(serde_json::json!({})), None).await;
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
    assert_eq!(body["monitors"].as_array().unwrap().len(), 0);
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
    assert!(db.account_session_at(&key, time::OffsetDateTime::now_utc(), Duration::from_secs(60)).await?.is_some());
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
    assert_eq!(status, StatusCode::OK, "dev mode still serves — without the workspace");
    Ok(())
}
