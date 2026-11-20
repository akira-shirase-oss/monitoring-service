//! S1d (D-017): a tenant's webhook URL is SSRF-guarded at save — scheme, IP literals,
//! and every address its name resolves to must clear the probe policy's public-address
//! rule before the store ever holds it. Send-time re-resolution lives with the
//! deliverer (covered in `tests/deliver.rs`); these tests pin the save gate and the
//! role surface: webhook administration is member work, the signing secret is shown
//! to members, and a clear removes the channel.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use monitor_db::Db;
use monitor_server::auth;
use tempfile::TempDir;
use tokio::sync::watch;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";
const OPERATOR: &str = "test-operator-token";

type Fixture = (axum::Router, monitor_server::AppState, Db, TempDir);

async fn fixture() -> anyhow::Result<Fixture> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("api.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    let (registry, _rx) = watch::channel(std::sync::Arc::new(Vec::new()));
    let state = monitor_server::AppState {
        db: db.clone(),
        statuses: std::sync::Arc::default(),
        registry,
        vitals: monitor_server::vitals::Vitals::new(
            std::sync::Arc::default(),
            Duration::from_secs(30),
        ),
        admin_token: Some(OPERATOR.into()),
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: auth::PASSWORD_WORK_WAIT,
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: auth::CookieSecure::Auto,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    };
    let app = monitor_server::router(state.clone());
    Ok((app, state, db, tmp))
}

enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
}

#[expect(
    clippy::unwrap_used,
    reason = "test helper: a request that can't build is a failed test"
)]
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    cred: Option<Cred<'_>>,
    workspace: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(uri);
    match cred {
        Some(Cred::Bearer(token)) => {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        Some(Cred::Cookie(cookie)) => {
            request = request
                .header(header::COOKIE, format!("smon_session={cookie}"))
                .header(header::HOST, "api.test")
                .header(header::ORIGIN, "http://api.test");
        }
        None => {}
    }
    if let Some(ws) = workspace {
        request = request.header("X-Smon-Workspace", ws);
    }
    let payload = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&json).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(payload).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[expect(
    clippy::expect_used,
    reason = "test helper: a signup or sign-in that fails is a failed test"
)]
async fn sign_up(app: &axum::Router, email: &str) -> String {
    let (status, _, body) = call(
        app,
        "POST",
        "/api/invites",
        Some(serde_json::json!({})),
        Some(Cred::Bearer(OPERATOR)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let code = body["invite"]["code"]
        .as_str()
        .expect("invite code")
        .to_owned();
    let (status, _, body) = call(
        app,
        "POST",
        "/api/signup",
        Some(serde_json::json!({"email": email, "password": PASSWORD, "invite": code})),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = call(
        app,
        "POST",
        "/api/login",
        Some(serde_json::json!({"email": email, "password": PASSWORD})),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
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

#[expect(
    clippy::expect_used,
    reason = "test helper: the account was just signed up"
)]
async fn workspace_of(db: &Db, email: &str) -> anyhow::Result<String> {
    let row = db.account_by_email(email).await?.expect("account exists");
    Ok(row.workspace_id)
}

/// Owner plus a member and a viewer seated in the owner's workspace.
async fn roster(app: &axum::Router) -> anyhow::Result<(String, String, String)> {
    let owner = sign_up(app, "owner@a.example").await;
    let member = sign_up(app, "member@b.example").await;
    let viewer = sign_up(app, "viewer@c.example").await;
    for (email, role) in [
        ("member@b.example", "member"),
        ("viewer@c.example", "viewer"),
    ] {
        let (status, _, body) = call(
            app,
            "POST",
            "/api/members",
            Some(serde_json::json!({"email": email, "role": role})),
            Some(Cred::Cookie(&owner)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{role}: {body}");
    }
    Ok((owner, member, viewer))
}

fn put_webhook(url: &str) -> serde_json::Value {
    serde_json::json!({ "url": url })
}

/// Every literal form the guard must refuse at save — IANA non-global ranges, loopback,
/// link-local + cloud metadata, CGNAT, multicast, and the IPv4-mapped IPv6 spellings of
/// the same. None of these need DNS: the literal check refuses before any lookup.
#[tokio::test]
async fn webhook_save_refuses_every_blocked_literal() -> anyhow::Result<()> {
    let (app, _state, _db, _tmp) = fixture().await?;
    let owner = sign_up(&app, "owner@a.example").await;

    for url in [
        // Loopback, unspecified, broadcast.
        "http://127.0.0.1/hook",
        "http://127.0.0.1:8443/hook",
        "http://0.0.0.0/hook",
        "http://255.255.255.255/hook",
        // Private ranges.
        "http://10.1.2.3/hook",
        "http://172.16.5.4/hook",
        "http://192.168.1.1/hook",
        // Link-local, including the cloud metadata address D-017 names.
        "http://169.254.0.1/hook",
        "http://169.254.169.254/latest/meta-data",
        // CGNAT / shared address space.
        "http://100.64.1.1/hook",
        // Documentation and benchmarking ranges.
        "http://192.0.2.1/hook",
        "http://198.51.100.7/hook",
        "http://203.0.113.9/hook",
        "http://198.18.0.1/hook",
        // IETF protocol assignments and deprecated 6to4 anycast.
        "http://192.0.0.9/hook",
        "http://192.88.99.1/hook",
        // Multicast and reserved.
        "http://224.0.0.1/hook",
        "http://239.255.255.250/hook",
        "http://240.0.0.1/hook",
        // IPv6: loopback, unspecified, link-local, unique-local, the metadata name
        // D-017 names, multicast — and IPv4-mapped spellings of blocked IPv4.
        "http://[::1]/hook",
        "http://[::]/hook",
        "http://[fe80::1]/hook",
        "http://[fd12:3456::1]/hook",
        "http://[fd00:ec2::254]/hook",
        "http://[ff02::1]/hook",
        "http://[::ffff:127.0.0.1]/hook",
        "http://[::ffff:10.0.0.1]/hook",
        "http://[::ffff:169.254.169.254]/hook",
        // Not http(s) at all.
        "ftp://hooks.example.com/x",
        "file:///etc/passwd",
        "gopher://hooks.example.com/",
    ] {
        let (status, _, body) = call(
            &app,
            "PUT",
            "/api/workspace/webhook",
            Some(put_webhook(url)),
            Some(Cred::Cookie(&owner)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {body}");
        assert_eq!(body["error"]["code"], "refused_target", "{url}: {body}");
    }
    // And nothing landed — the channel stayed unconfigured through every refusal.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspace/webhook",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["webhook"].is_null(), "{body}");
    Ok(())
}

/// A hostname that *resolves* local is refused at save, not just at send: the save gate
/// runs the validating resolver, so `localhost` — public-looking scheme, private
/// answer — never lands. A name that cannot resolve at all is refused too (an
/// unproven host is not a proven-public host), with its own honest code.
#[tokio::test]
async fn webhook_save_refuses_names_that_resolve_local_or_not_at_all() -> anyhow::Result<()> {
    let (app, _state, _db, _tmp) = fixture().await?;
    let owner = sign_up(&app, "owner@a.example").await;

    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/webhook",
        Some(put_webhook("http://localhost:9000/hook")),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "refused_target", "{body}");

    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/webhook",
        Some(put_webhook("https://nonexistent.invalid/hook")),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    Ok(())
}

/// The channel itself: PUT stores the validated URL and answers URL + signing secret;
/// GET shows them to members; DELETE removes both. A viewer has no method for any of
/// it — webhook administration is member work, and a member-role API token does it too.
#[tokio::test]
async fn the_webhook_channel_is_member_work_end_to_end() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture().await?;
    let (owner, member, viewer) = roster(&app).await?;
    let ws = workspace_of(&db, "owner@a.example").await?;

    // Unconfigured reads as an honest null, not a 404.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspace/webhook",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["webhook"].is_null(), "{body}");

    // PUT: a public literal target stores — and answers the secret once per read.
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/webhook",
        Some(put_webhook("https://93.184.216.34/hooks/incident")),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["webhook"]["url"],
        "https://93.184.216.34/hooks/incident"
    );
    let secret = body["webhook"]["secret"]
        .as_str()
        .expect("secret shown")
        .to_owned();
    assert!(secret.starts_with("smon_whk_"), "{body}");

    // GET shows the same pair to a member through the workspace picker — and the
    // store holds the same values the API returned.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspace/webhook",
        None,
        Some(Cred::Cookie(&member)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["webhook"]["secret"], secret, "{body}");
    let stored = db
        .member(monitor_domain::WorkspaceId::DEFAULT)
        .webhook()
        .await?;
    assert!(stored.is_none(), "another workspace is untouched");
    let stored = db
        .member(monitor_domain::WorkspaceId(uuid::Uuid::parse_str(&ws)?))
        .webhook()
        .await?
        .expect("stored webhook");
    assert_eq!(stored.secret, secret);

    // A member-role API token administers the channel in its own workspace.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/tokens",
        Some(serde_json::json!({"name": "hooks-ci", "role": "member"})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let token = body["token"]["secret"].as_str().expect("token").to_owned();
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/webhook",
        Some(put_webhook("http://127.0.0.1/hook")),
        Some(Cred::Bearer(&token)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A viewer is denied on all three methods — valid bodies, so it is the facade
    // that refuses, not the extractor.
    for (method, payload) in [
        ("GET", None),
        ("PUT", Some(put_webhook("https://93.184.216.34/x"))),
        ("DELETE", None),
    ] {
        let (status, _, body) = call(
            &app,
            method,
            "/api/workspace/webhook",
            payload,
            Some(Cred::Cookie(&viewer)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}: {body}");
    }

    Ok(())
}

/// DELETE removes the channel — the next read is null, and configuring again mints a
/// fresh signing secret (a clear is a rotation boundary, not a pause).
#[tokio::test]
async fn a_cleared_webhook_rotates_its_secret() -> anyhow::Result<()> {
    let (app, _state, _db, _tmp) = fixture().await?;
    let owner = sign_up(&app, "owner@a.example").await;

    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/webhook",
        Some(put_webhook("https://93.184.216.34/hooks/incident")),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let secret = body["webhook"]["secret"]
        .as_str()
        .expect("secret shown")
        .to_owned();

    let (status, _, _) = call(
        &app,
        "DELETE",
        "/api/workspace/webhook",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspace/webhook",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["webhook"].is_null(), "{body}");

    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/webhook",
        Some(put_webhook("https://93.184.216.34/rotated")),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(body["webhook"]["secret"], secret, "a clear rotates");
    Ok(())
}
