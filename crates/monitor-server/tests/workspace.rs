//! S2 backend: the workspace endpoints the dashboard and onboarding need — rename,
//! the memberships the sidebar switcher lists, the status-page configuration behind
//! `/api/workspace/status-page`, "send test alert" through the D-017 gate, and the
//! public `/s/<slug>/status.json` whose freshness is the snapshot file's own claim.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use monitor_db::Db;
use monitor_domain::{MonitorId, Status, WorkspaceId};
use monitor_server::auth;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";
const OPERATOR: &str = "test-operator-token";

type Fixture = (axum::Router, monitor_server::AppState, Db, TempDir);

async fn fixture(lab: bool) -> anyhow::Result<Fixture> {
    fixture_with(lab, usize::MAX).await
}

async fn fixture_with(lab: bool, workspace_slots: usize) -> anyhow::Result<Fixture> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("api.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    let (registry, _rx) = watch::channel(Arc::new(Vec::new()));
    let state = monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::default(),
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
        workspace_slots,
        allow_local_targets: lab,
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
    // Password work is process-global — a sibling test's argon2 can hold both
    // permits, answering 503 `busy`. That answer is the retry contract: use it.
    let busy_retried = |want: StatusCode| {
        move |(status, headers, body): (StatusCode, HeaderMap, serde_json::Value)| {
            if status == StatusCode::SERVICE_UNAVAILABLE {
                return None;
            }
            assert_eq!(status, want, "{body}");
            Some((status, headers, body))
        }
    };
    let mut signup = None;
    for _ in 0..20 {
        let reply = call(
            app,
            "POST",
            "/api/signup",
            Some(serde_json::json!({"email": email, "password": PASSWORD, "invite": code})),
            None,
            None,
        )
        .await;
        if let Some(done) = busy_retried(StatusCode::CREATED)(reply) {
            signup = Some(done);
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(signup.is_some(), "signup never got a slot");
    let mut headers = axum::http::HeaderMap::new();
    for _ in 0..20 {
        let reply = call(
            app,
            "POST",
            "/api/login",
            Some(serde_json::json!({"email": email, "password": PASSWORD})),
            None,
            None,
        )
        .await;
        if let Some((_, h, _)) = busy_retried(StatusCode::NO_CONTENT)(reply) {
            headers = h;
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(!headers.is_empty(), "login never got a slot");
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

/// Lowercase hex — the `x-smon-signature` encoding the receiver verifies.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[expect(
    clippy::expect_used,
    reason = "test helper: the monitor was just created"
)]
async fn monitor(app: &axum::Router, cookie: &str, name: &str) -> String {
    let (status, _, body) = call(
        app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({"name": name, "url": "https://example.com/health"})),
        Some(Cred::Cookie(cookie)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().expect("monitor id").to_owned()
}

/// Rename is member work: owner and member both rename, a viewer is refused by the
/// facade, and a name that isn't a name is a 422 — never silently stored.
#[tokio::test]
async fn rename_is_member_work_and_validated() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(false).await?;
    let (owner, member, viewer) = roster(&app).await?;
    let ws = workspace_of(&db, "owner@a.example").await?;

    for (name, label) in [("Acme status", "owner"), ("Acme prod", "member")] {
        let cookie = if label == "owner" { &owner } else { &member };
        let (status, _, body) = call(
            &app,
            "PATCH",
            "/api/workspace",
            Some(serde_json::json!({"name": name})),
            Some(Cred::Cookie(cookie)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
        assert_eq!(body["workspace"]["name"], name, "{body}");
    }
    let row = db
        .workspace_row(WorkspaceId(uuid::Uuid::parse_str(&ws)?))
        .await?
        .expect("workspace");
    assert_eq!(row.name, "Acme prod");

    for bad in ["", "   ", &"x".repeat(81), "line\nbreak"] {
        let (status, _, body) = call(
            &app,
            "PATCH",
            "/api/workspace",
            Some(serde_json::json!({"name": bad})),
            Some(Cred::Cookie(&owner)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}: {body}");
    }
    let (status, _, body) = call(
        &app,
        "PATCH",
        "/api/workspace",
        Some(serde_json::json!({"name": "viewer rename"})),
        Some(Cred::Cookie(&viewer)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    Ok(())
}

/// The switcher rows are each principal's own claim: an account its memberships
/// (with the role it holds *there*), a token its one workspace, and nobody sees a
/// workspace they weren't seated in.
#[tokio::test]
async fn workspaces_lists_each_principals_own_claim() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(false).await?;
    let (owner, member, _viewer) = roster(&app).await?;
    let owner_ws = workspace_of(&db, "owner@a.example").await?;
    let member_ws = workspace_of(&db, "member@b.example").await?;

    // The owner belongs to exactly its home workspace.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspaces",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["workspaces"].as_array().expect("workspaces array");
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["id"], owner_ws);
    assert_eq!(rows[0]["role"], "owner");

    // The member sees both: home (owner) and the owner's workspace (member) — and
    // nothing else.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspaces",
        None,
        Some(Cred::Cookie(&member)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["workspaces"].as_array().expect("workspaces array");
    assert_eq!(rows.len(), 2, "{body}");
    let role_of = |id: &str| {
        rows.iter()
            .find(|r| r["id"] == id)
            .map(|r| r["role"].as_str().unwrap_or_default().to_owned())
    };
    assert_eq!(role_of(&member_ws).as_deref(), Some("owner"), "{body}");
    assert_eq!(role_of(&owner_ws).as_deref(), Some("member"), "{body}");

    // A member-role API token sees exactly the workspace it was minted for.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/tokens",
        Some(serde_json::json!({"name": "switcher", "role": "member"})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let token = body["token"]["secret"].as_str().expect("token").to_owned();
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspaces",
        None,
        Some(Cred::Bearer(&token)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["workspaces"].as_array().expect("workspaces array");
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["id"], owner_ws);
    assert_eq!(rows[0]["role"], "member");
    Ok(())
}

/// The editor round-trip: configure slug, title and the published monitors; read
/// back exactly what stored; taken slugs conflict; a foreign monitor can never be
/// published through another workspace's PUT; and turning the page off keeps the
/// saved set without serving it.
#[tokio::test]
async fn status_page_config_round_trip_and_guards() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(false).await?;
    let (owner, _member, viewer) = roster(&app).await?;
    let ws = workspace_of(&db, "owner@a.example").await?;
    let mon_a = monitor(&app, &owner, "api").await;
    let mon_b = monitor(&app, &owner, "site").await;
    // An outsider's monitor — seated nowhere near the owner's workspace.
    let outsider = sign_up(&app, "outsider@d.example").await;
    let foreign = monitor(&app, &outsider, "other").await;

    // Untouched: page off, empty allowlist, the minted slug.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspace/status-page",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["status_page"]["title"].is_null(), "{body}");
    assert_eq!(body["status_page"]["monitor_ids"], serde_json::json!([]));

    // The write: title + slug + both monitors, in the order given.
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({
            "slug": "acme-status",
            "title": "Acme status",
            "monitor_ids": [mon_a.clone(), mon_b.clone()],
        })),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status_page"]["slug"], "acme-status");
    assert_eq!(body["status_page"]["title"], "Acme status");
    assert_eq!(
        body["status_page"]["monitor_ids"],
        serde_json::json!([mon_a.clone(), mon_b.clone()]),
        "{body}"
    );

    // A member's PUT can never publish another workspace's monitor — refused, and
    // the stored set is unchanged.
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({
            "title": "Acme status", "monitor_ids": [foreign.clone()],
        })),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Title empty = page off; the allowlist survives for re-enable.
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({"title": "", "monitor_ids": [mon_a.clone()]})),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["status_page"]["title"].is_null(), "{body}");
    assert_eq!(
        body["status_page"]["monitor_ids"],
        serde_json::json!([mon_a]),
        "{body}"
    );

    // A viewer can read the config but never write it.
    let (status, _, _) = call(
        &app,
        "GET",
        "/api/workspace/status-page",
        None,
        Some(Cred::Cookie(&viewer)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({"title": "nope", "monitor_ids": []})),
        Some(Cred::Cookie(&viewer)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    Ok(())
}

/// Slug rules: a bad charset is a 422 before the store; a slug another workspace
/// holds is a 409 — "taken", never which tenant holds it.
#[tokio::test]
async fn status_page_slug_rules() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(false).await?;
    let (owner, _member, _viewer) = roster(&app).await?;
    let ws = workspace_of(&db, "owner@a.example").await?;
    let member_ws = workspace_of(&db, "member@b.example").await?;

    for bad in ["UPPER", "has space", "-lead", "trail-", "dot.com", "../x"] {
        let (status, _, body) = call(
            &app,
            "PUT",
            "/api/workspace/status-page",
            Some(serde_json::json!({"slug": bad, "title": "x", "monitor_ids": []})),
            Some(Cred::Cookie(&owner)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    let member_slug: String = db
        .workspace_row(WorkspaceId(uuid::Uuid::parse_str(&member_ws)?))
        .await?
        .expect("member workspace")
        .slug;
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({"slug": member_slug, "title": "x", "monitor_ids": []})),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    Ok(())
}

/// "Send test alert" posts a signed `kind:"test"` payload to the workspace's own
/// receiver through the D-017 gate, answers the classified result, and holds the
/// workspace to one send per window — the second call is 429 + Retry-After.
#[tokio::test]
async fn test_alert_sends_signed_once_then_waits() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(true).await?;
    let (owner, _member, viewer) = roster(&app).await?;
    let ws = workspace_of(&db, "owner@a.example").await?;
    let (url, mut hits) = receiver().await;

    // No channel yet: the test send says so, in the ordinary error envelope.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/workspace/webhook/test",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");

    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/webhook",
        Some(serde_json::json!({"url": url})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let secret = body["webhook"]["secret"]
        .as_str()
        .expect("secret")
        .to_owned();

    let (status, _, body) = call(
        &app,
        "POST",
        "/api/workspace/webhook/test",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["test"]["ok"], true, "{body}");
    assert_eq!(body["test"]["detail"], "delivered", "{body}");

    // What the receiver got: a signed POST of a `test` event — the HMAC over the
    // exact body verifies with the workspace secret.
    let (headers, bytes) = hits.recv().await.expect("the test POST landed");
    let payload: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(payload["kind"], "test", "{payload}");
    let signature = headers
        .get("x-smon-signature")
        .and_then(|v| v.to_str().ok())
        .expect("signed")
        .to_owned();
    let expected = {
        use hmac::{Hmac, KeyInit, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())?;
        mac.update(&bytes);
        format!("sha256={}", hex_lower(&mac.finalize().into_bytes()))
    };
    assert_eq!(signature, expected);
    assert!(
        headers
            .get("x-smon-event")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|id| id.starts_with("test_")),
        "{headers:?}"
    );

    // The workspace cooldown: an immediate second send is 429 with Retry-After.
    let (status, headers, body) = call(
        &app,
        "POST",
        "/api/workspace/webhook/test",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "too_many_requests", "{body}");
    assert!(headers.get(header::RETRY_AFTER).is_some());
    assert!(hits.try_recv().is_err(), "no second send left the box");

    // Viewer work is forbidden by the facade, before any send is even considered.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/workspace/webhook/test",
        None,
        Some(Cred::Cookie(&viewer)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    Ok(())
}

/// The public page: an enabled page answers the writer's last beat verbatim —
/// allowlisted fields only; an unknown slug and a disabled page are the same 404;
/// enabled-but-never-written answers born-expired so absence reads as
/// "monitoring unavailable", never as healthy.
#[tokio::test]
async fn public_status_page_serves_the_beat_or_unavailable() -> anyhow::Result<()> {
    let (app, state, db, _tmp) = fixture(true).await?;
    let owner = sign_up(&app, "owner@a.example").await;
    let mon = monitor(&app, &owner, "api").await;
    // Never published — the page must not show it even exists.
    monitor(&app, &owner, "internal").await;
    let ws = workspace_of(&db, "owner@a.example").await?;

    // Enabled but no beat yet: born-expired, monitoring "unavailable".
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({
            "slug": "acme-status", "title": "Acme status", "monitor_ids": [mon.clone()],
        })),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = call(&app, "GET", "/s/acme-status/status.json", None, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["monitoring"], "unavailable", "{body}");
    // Same wire shape as the writer's file: RFC3339 strings, parseable by readers.
    assert!(
        body["generated_at"]
            .as_str()
            .is_some_and(|stamp| stamp.contains('T') && stamp.ends_with('Z')),
        "born-expired timestamps are RFC3339 strings: {body}"
    );

    // A committed status seeded, then one beat: the page answers it, with only the
    // allowlisted monitor — the unpublished one is absent by construction.
    state
        .statuses
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(MonitorId(uuid::Uuid::parse_str(&mon)?), Status::Up);
    let config = monitor_server::snapshot::Config {
        dir: state.status_dir.clone(),
        interval: Duration::from_secs(30),
        ttl: Duration::from_secs(90),
    };
    monitor_server::snapshot::write_beat(&db, &state.statuses, &config, true).await?;

    // The writer's file is the fresh answer — the beat verbatim.
    let (status, _, body) = call(&app, "GET", "/s/acme-status/status.json", None, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "Acme status");
    assert_eq!(body["monitoring"], "available", "{body}");
    let services = body["services"].as_array().expect("services");
    assert_eq!(services.len(), 1, "{body}");
    assert_eq!(services[0]["name"], "api");
    assert_eq!(services[0]["status"], "up");
    assert!(
        services[0].get("url").is_none(),
        "no URLs leave: {services:?}"
    );
    assert!(
        services[0].get("id").is_none(),
        "no ids leave: {services:?}"
    );

    // An unknown slug and a workspace with no page answer identically.
    for slug in ["no-such-page", "default"] {
        let (status, _, body) = call(
            &app,
            "GET",
            &format!("/s/{slug}/status.json"),
            None,
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{slug}: {body}");
        assert_eq!(body["error"]["code"], "not_found", "{body}");
    }

    // Page off: the next beat removes the file and the route is 404 — never
    // last-known data dressed as current.
    let (status, _, _) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({"title": "", "monitor_ids": [mon.clone()]})),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    monitor_server::snapshot::write_beat(&db, &state.statuses, &config, true).await?;
    assert!(!state.status_dir.join("s/acme-status.json").exists());
    let (status, _, body) = call(&app, "GET", "/s/acme-status/status.json", None, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let _ = ws;
    Ok(())
}

/// The public route's cache contract (S2 hardening): the served beat is cacheable
/// only until the next beat lands (`public, max-age=30` — its own `expires_at`
/// stays the freshness claim), and every other answer is `no-store` — a shared
/// cache must never keep "unavailable" or "missing" past the truth.
#[tokio::test]
async fn public_status_cache_headers() -> anyhow::Result<()> {
    let (app, state, db, _tmp) = fixture(true).await?;
    let owner = sign_up(&app, "owner@a.example").await;
    let mon = monitor(&app, &owner, "api").await;
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({
            "slug": "acme-status", "title": "Acme", "monitor_ids": [mon],
        })),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let cache_of = |headers: &HeaderMap| {
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    // Enabled but never written — born-expired, never cached.
    let (status, headers, _) =
        call(&app, "GET", "/s/acme-status/status.json", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache_of(&headers).as_deref(), Some("no-store"));

    // The writer's file — cacheable for one beat interval.
    let config = monitor_server::snapshot::Config {
        dir: state.status_dir.clone(),
        interval: Duration::from_secs(30),
        ttl: Duration::from_secs(90),
    };
    monitor_server::snapshot::write_beat(&db, &state.statuses, &config, true).await?;
    let (status, headers, _) =
        call(&app, "GET", "/s/acme-status/status.json", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache_of(&headers).as_deref(), Some("public, max-age=30"));

    // Unknown and malformed slugs — the 404s are never cached either.
    for slug in ["no-such-page", "UPPER"] {
        let (status, headers, _) = call(
            &app,
            "GET",
            &format!("/s/{slug}/status.json"),
            None,
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{slug}");
        assert_eq!(cache_of(&headers).as_deref(), Some("no-store"), "{slug}");
    }
    Ok(())
}

/// The status-page write's bounds (S2 hardening): `monitor_ids` past the page's
/// bound is a 422 *before* store work — and the refusal names the bound; a title
/// too long or carrying a control character is a 422; duplicate ids collapse to
/// their first occurrence instead of tripping the `status_monitors` primary key
/// into a 500; and writes past the workspace's per-window budget answer 429 with
/// `Retry-After` — a config write can't amplify.
#[tokio::test]
async fn status_page_input_bounds_and_write_pacing() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(false).await?;
    let (owner, _member, _viewer) = roster(&app).await?;
    let ws = workspace_of(&db, "owner@a.example").await?;
    let mon = monitor(&app, &owner, "api").await;

    // The bound check runs before ids are even parsed — the message names the cap.
    let too_many: Vec<String> = (0..65).map(|_| uuid::Uuid::now_v7().to_string()).collect();
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({"title": "x", "monitor_ids": too_many})),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(message.contains("64"), "{message}");

    for title in ["x".repeat(121), "with\u{7}bell".to_owned()] {
        let (status, _, body) = call(
            &app,
            "PUT",
            "/api/workspace/status-page",
            Some(serde_json::json!({"title": title, "monitor_ids": []})),
            Some(Cred::Cookie(&owner)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{title}: {body}");
    }

    // A duplicate id is a no-op on the echo — one row, not a constraint error.
    let (status, _, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({
            "slug": "acme-status", "title": "Acme",
            "monitor_ids": [mon.clone(), mon.clone()],
        })),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["status_page"]["monitor_ids"],
        serde_json::json!([mon.clone()]),
        "{body}"
    );

    // The per-workspace write budget: six writes a rolling window, then 429.
    // The refusals above never counted — they weren't writes.
    for n in 0..5 {
        let (status, _, body) = call(
            &app,
            "PUT",
            "/api/workspace/status-page",
            Some(serde_json::json!({"title": format!("Acme {n}"), "monitor_ids": [mon.clone()]})),
            Some(Cred::Cookie(&owner)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "write {n}: {body}");
    }
    let (status, headers, body) = call(
        &app,
        "PUT",
        "/api/workspace/status-page",
        Some(serde_json::json!({"title": "one too many", "monitor_ids": [mon.clone()]})),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "too_many_requests", "{body}");
    assert!(headers.get(header::RETRY_AFTER).is_some());
    Ok(())
}

/// S3's per-workspace probe share: a workspace's own schedule can't claim more
/// than `workspace_slots` in-flight probes — the create is a named 409 `capacity`
/// refusal, not a slot quietly missed later. Pausing frees the share (it counts
/// unpaused monitors only) and another tenant's creates are unmoved.
#[tokio::test]
async fn a_workspace_cannot_claim_the_whole_probe_budget() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture_with(false, 4).await?;
    let owner = sign_up(&app, "owner@a.example").await;
    let neighbour = sign_up(&app, "neighbour@b.example").await;
    // The share check needs 15 s monitors — below the free plan's 60 s floor — so
    // both workspaces run on the operator tier for this test (S3, D-022).
    for email in ["owner@a.example", "neighbour@b.example"] {
        let ws = workspace_of(&db, email).await?;
        db.set_workspace_plan(
            monitor_domain::WorkspaceId(uuid::Uuid::parse_str(&ws)?),
            monitor_domain::Plan::Operator,
        )
        .await?;
    }

    // interval = timeout = 15 s: each monitor needs (tick + 15)/15 ≈ 1.07 slots.
    // Three fit under a 4-slot share; the fourth makes the workspace need 4.27.
    let create = |cookie: &str, name: &str| {
        let app = app.clone();
        let cookie = cookie.to_owned();
        let name = name.to_owned();
        async move {
            call(
                &app,
                "POST",
                "/api/monitors",
                Some(serde_json::json!({
                    "name": name, "url": "https://example.com/health",
                    "interval_s": 15, "timeout_s": 15,
                })),
                Some(Cred::Cookie(&cookie)),
                None,
            )
            .await
        }
    };
    for n in 0..3 {
        let (status, _, body) = create(&owner, &format!("m{n}")).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, _, body) = create(&owner, "one too loud").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "capacity_exceeded", "{body}");

    // Pausing frees the share — the refused create succeeds once a monitor sleeps.
    let ws = workspace_of(&db, "owner@a.example").await?;
    let first = db
        .workspace(WorkspaceId(uuid::Uuid::parse_str(&ws)?))
        .list_monitors()
        .await?
        .into_iter()
        .next()
        .expect("a monitor");
    let (status, _, body) = call(
        &app,
        "PATCH",
        &format!("/api/monitors/{}", first.id),
        Some(serde_json::json!({"paused": true})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = create(&owner, "within budget now").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Another tenant's share is its own: the neighbour creates while the owner is at it.
    for n in 0..3 {
        let (status, _, body) = create(&neighbour, &format!("n{n}")).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    Ok(())
}

/// S3 plan tiers (D-022): a signed-up workspace starts `free` — ten monitors and a
/// 60 s interval floor, enforced on the server at write time with a named refusal.
/// Only the operator moves a workspace between plans; a tenant can't.
#[tokio::test]
async fn a_workspace_s_plan_gates_its_limits() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(false).await?;
    let owner = sign_up(&app, "plan-owner@a.example").await;
    let ws = workspace_of(&db, "plan-owner@a.example").await?;

    // The plan rides in the switcher row — a workspace always knows its tier.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/workspaces",
        None,
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workspaces"][0]["plan"], "free", "{body}");

    // At the floor a create lands; below it a named 409 refuses.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({
            "name": "at the floor", "url": "https://example.com/a", "interval_s": 60
        })),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let first = body["id"].as_str().expect("monitor id").to_owned();

    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({
            "name": "below the floor", "url": "https://example.com/b", "interval_s": 59
        })),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "plan_limit", "{body}");

    // An edit below the floor is refused the same way; one that doesn't touch the
    // schedule still lands.
    for (patch, want) in [
        (serde_json::json!({"interval_s": 30}), StatusCode::CONFLICT),
        (serde_json::json!({"name": "renamed"}), StatusCode::OK),
    ] {
        let (status, _, body) = call(
            &app,
            "PATCH",
            &format!("/api/monitors/{first}"),
            Some(patch),
            Some(Cred::Cookie(&owner)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, want, "{body}");
        if want == StatusCode::CONFLICT {
            assert_eq!(body["error"]["code"], "plan_limit", "{body}");
        }
    }

    // The plan's count cap: nine more monitors fill the ten; the next is refused.
    for n in 2..=10 {
        let (status, _, body) = call(
            &app,
            "POST",
            "/api/monitors",
            Some(serde_json::json!({
                "name": format!("m{n}"), "url": "https://example.com/m"
            })),
            Some(Cred::Cookie(&owner)),
            Some(&ws),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "m{n}: {body}");
    }
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({"name": "over the cap", "url": "https://example.com/over"})),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "limit_reached", "{body}");
    Ok(())
}

/// `PATCH /api/workspaces/{id}/plan` is operator work: a member can't lift its own
/// limits, a nonsense tier is a 422, an unknown workspace a 404 — and once moved,
/// the operator tier's floors (15 s, the server's count cap) apply to new writes.
#[tokio::test]
async fn only_the_operator_moves_a_workspace_between_plans() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture(false).await?;
    let owner = sign_up(&app, "move-owner@a.example").await;
    let ws = workspace_of(&db, "move-owner@a.example").await?;

    // A below-floor create is refused first, so the upgrade has something to prove.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({
            "name": "too fast", "url": "https://example.com/a", "interval_s": 15
        })),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "plan_limit", "{body}");

    for (cred, payload, want) in [
        // A member asking for its own workspace's upgrade is forbidden.
        (
            Cred::Cookie(&owner),
            serde_json::json!({"plan": "operator"}),
            StatusCode::FORBIDDEN,
        ),
        // The operator with a nonsense tier is a 422.
        (
            Cred::Bearer(OPERATOR),
            serde_json::json!({"plan": "gold"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, _, body) = call(
            &app,
            "PATCH",
            &format!("/api/workspaces/{ws}/plan"),
            Some(payload),
            Some(cred),
            Some(&ws),
        )
        .await;
        assert_eq!(status, want, "{body}");
    }
    let (status, _, body) = call(
        &app,
        "PATCH",
        "/api/workspaces/00000000-0000-0000-0000-000000000099/plan",
        Some(serde_json::json!({"plan": "operator"})),
        Some(Cred::Bearer(OPERATOR)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // The operator's move lands — and the refused write now succeeds (the 15 s
    // interval is the domain floor's business again, and the count cap the server's).
    let (status, _, body) = call(
        &app,
        "PATCH",
        &format!("/api/workspaces/{ws}/plan"),
        Some(serde_json::json!({"plan": "operator"})),
        Some(Cred::Bearer(OPERATOR)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workspace"]["plan"], "operator", "{body}");
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({
            "name": "fine now", "url": "https://example.com/c", "interval_s": 15
        })),
        Some(Cred::Cookie(&owner)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Ok(())
}

/// A loopback receiver that records (headers, body) per hit.
#[expect(
    clippy::expect_used,
    reason = "test helper: a receiver that won't bind is a failed test"
)]
async fn receiver() -> (String, mpsc::Receiver<(HeaderMap, Vec<u8>)>) {
    async fn record(
        axum::extract::State(hits): axum::extract::State<mpsc::Sender<(HeaderMap, Vec<u8>)>>,
        request: Request<Body>,
    ) -> axum::response::Response {
        let (parts, body) = request.into_parts();
        let bytes = body
            .collect()
            .await
            .map(|c| c.to_bytes().to_vec())
            .unwrap_or_default();
        let _ = hits.send((parts.headers, bytes)).await;
        axum::response::Response::new(Body::empty())
    }
    let (tx, rx) = mpsc::channel(8);
    let app = axum::Router::new()
        .route("/", axum::routing::post(record))
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}/"), rx)
}
