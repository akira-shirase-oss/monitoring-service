//! S1c: workspace members carry roles (owner > member > viewer) and per-workspace API
//! tokens. Roles are enforced at the store facade — a viewer's view has no write
//! methods to call — so these tests prove what the API shows: a viewer gets 403 on a
//! write, a member writes monitors but administers nothing, tokens live in exactly one
//! workspace and die on revocation.

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

/// The router over a real store, with an operator token configured — the API-token
/// bearer path only runs when a token exists (tokenless dev mode would trust it).
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

/// What carries the request's credential: a bearer string (operator token or workspace
/// API token) or an account session cookie — cookies ride with a matching Host/Origin
/// pair, the CSRF contract.
enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
}

/// `call` with `X-Smon-Workspace: ws` — the workspace picker an account uses to act in
/// any workspace it belongs to.
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

/// Mint an invite as the operator, sign up `email`, and sign in — the account's
/// session cookie.
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

/// The workspace a signup created for `email` — the value `X-Smon-Workspace` carries.
#[expect(
    clippy::expect_used,
    reason = "test helper: the account was just signed up"
)]
async fn workspace_of(db: &Db, email: &str) -> anyhow::Result<String> {
    let row = db.account_by_email(email).await?.expect("account exists");
    Ok(row.workspace_id)
}

/// The account id behind `email` — the `/api/members/{id}` path parameter.
#[expect(
    clippy::expect_used,
    reason = "test helper: the account was just signed up"
)]
async fn account_of(db: &Db, email: &str) -> anyhow::Result<String> {
    Ok(db
        .account_by_email(email)
        .await?
        .expect("account exists")
        .id)
}

/// Three accounts — an owner, a member and a viewer of the owner's workspace — plus a
/// stranger in a fourth. Returns (owner, member, viewer) cookies, the shared workspace
/// id and the stranger's.
async fn seed_roster(
    app: &axum::Router,
    db: &Db,
) -> anyhow::Result<(String, String, String, String, String)> {
    let owner = sign_up(app, "owner@a.example").await;
    let member = sign_up(app, "member@b.example").await;
    let viewer = sign_up(app, "viewer@c.example").await;
    let ws = workspace_of(db, "owner@a.example").await?;
    let _stranger = sign_up(app, "s@d.example").await;
    let stranger_ws = workspace_of(db, "s@d.example").await?;
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
    Ok((owner, member, viewer, ws, stranger_ws))
}

fn monitor_body(name: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "url": "https://example.com/health" })
}

/// An owner adds an account as a member; the member writes monitors in that workspace
/// (chosen with `X-Smon-Workspace`) but administers nothing — members and tokens are
/// owner work, and the viewer role is read-only.
#[tokio::test]
async fn members_write_monitors_viewers_only_read_and_neither_administers() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture().await?;
    let (_owner, member, viewer, ws, _stranger_ws) = seed_roster(&app, &db).await?;

    // A member writes into the owner's workspace through the picker.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(monitor_body("member's monitor")),
        Some(Cred::Cookie(&member)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // … but administers nothing: roster and token work are owner-only.
    for (method, uri, body) in [
        (
            "POST",
            "/api/members",
            Some(serde_json::json!({"email": "s@d.example", "role": "viewer"})),
        ),
        (
            "POST",
            "/api/tokens",
            Some(serde_json::json!({"name": "ci", "role": "member"})),
        ),
        ("GET", "/api/tokens", None),
    ] {
        let (status, _, body) = call(
            &app,
            method,
            uri,
            body,
            Some(Cred::Cookie(&member)),
            Some(&ws),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "member {method} {uri}: {body}"
        );
    }

    // A viewer reads the shared workspace — including the member's monitor and the
    // roster itself — but every write, roster or resource, is denied.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/monitors",
        None,
        Some(Cred::Cookie(&viewer)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["monitors"].as_array().map(Vec::len), Some(1));
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/members",
        None,
        Some(Cred::Cookie(&viewer)),
        Some(&ws),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["members"].as_array().map(Vec::len), Some(3));

    for (method, uri, payload) in [
        // Valid bodies: the extractor runs before the role check — a denied request
        // must get past parsing to prove the facade, not the shape, refused it.
        (
            "POST",
            "/api/monitors",
            serde_json::json!({"name": "x", "url": "https://a.example/", "interval_s": 15}),
        ),
        (
            "PATCH",
            "/api/monitors/00000000-0000-0000-0000-000000000000",
            serde_json::json!({"paused": true}),
        ),
        (
            "POST",
            "/api/members",
            serde_json::json!({"email": "x@y.example", "role": "viewer"}),
        ),
        (
            "POST",
            "/api/tokens",
            serde_json::json!({"name": "t", "role": "viewer"}),
        ),
    ] {
        let (status, _, body) = call(
            &app,
            method,
            uri,
            Some(payload),
            Some(Cred::Cookie(&viewer)),
            Some(&ws),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "viewer {method} {uri}: {body}"
        );
    }
    Ok(())
}

/// `X-Smon-Workspace` never reaches a workspace the account isn't a member of — a real
/// one or an invented id answer identically — and without the picker each account is
/// home.
#[tokio::test]
async fn the_workspace_picker_never_leaves_membership() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture().await?;
    let (_owner, member, _viewer, _ws, stranger_ws) = seed_roster(&app, &db).await?;

    for ws in [&stranger_ws, "00000000-0000-0000-0000-000000000000"] {
        let (status, _, body) = call(
            &app,
            "GET",
            "/api/monitors",
            None,
            Some(Cred::Cookie(&member)),
            Some(ws),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{ws}: {body}");
    }

    // And without the picker each account still sees only its own workspace.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/monitors",
        None,
        Some(Cred::Cookie(&member)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["monitors"].as_array().map(Vec::len), Some(0));
    Ok(())
}

/// A workspace must always have an owner: demoting or removing the last one conflicts;
/// with a second owner seated, the first may step down.
#[tokio::test]
async fn the_last_owner_cannot_be_demoted_or_removed() -> anyhow::Result<()> {
    let (app, _state, db, _tmp) = fixture().await?;
    let owner = sign_up(&app, "owner@a.example").await;
    let _other = sign_up(&app, "second@b.example").await;
    let owner_id = account_of(&db, "owner@a.example").await?;

    for (method, uri, body) in [
        (
            "PATCH",
            format!("/api/members/{owner_id}"),
            serde_json::json!({"role": "member"}),
        ),
        (
            "DELETE",
            format!("/api/members/{owner_id}"),
            serde_json::json!(null),
        ),
    ] {
        let (status, _, body) = call(
            &app,
            method,
            &uri,
            if body.is_null() { None } else { Some(body) },
            Some(Cred::Cookie(&owner)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}: {body}");
    }

    // Seat a second owner; now the first may stand down.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/members",
        Some(serde_json::json!({"email": "second@b.example", "role": "owner"})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = call(
        &app,
        "PATCH",
        &format!("/api/members/{owner_id}"),
        Some(serde_json::json!({"role": "member"})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

/// A workspace token: shown once at mint, held as a digest in the store, scoped to its
/// workspace, revocable, and its last-used stamp lands in a batch — not per request.
#[tokio::test]
async fn a_workspace_token_is_shown_once_scoped_and_revocable() -> anyhow::Result<()> {
    let (app, state, db, _tmp) = fixture().await?;
    let owner = sign_up(&app, "owner@a.example").await;
    let outsider = sign_up(&app, "o@b.example").await;
    let outsider_ws = workspace_of(&db, "o@b.example").await?;
    drop(outsider);

    let (status, _, body) = call(
        &app,
        "POST",
        "/api/tokens",
        Some(serde_json::json!({"name": "ci", "role": "member"})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret = body["token"]["secret"]
        .as_str()
        .expect("shown once")
        .to_owned();
    assert!(secret.starts_with("smon_wst_"));
    let token_id = body["token"]["id"].as_str().expect("id").to_owned();

    // The secret is never shown again — the list carries metadata only.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/tokens",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = &body["tokens"][0];
    assert_eq!(listed["id"], token_id);
    assert!(listed.get("secret").is_none(), "never re-shown: {listed}");
    assert!(listed["last_used_at"].is_null());

    // A member-role token works in its own workspace — writes included.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(monitor_body("by token")),
        Some(Cred::Bearer(&secret)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Its scope is fixed: the picker cannot move it into another workspace.
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/monitors",
        None,
        Some(Cred::Bearer(&secret)),
        Some(&outsider_ws),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // And owner work is beyond a token — even a member-role one.
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/members",
        Some(serde_json::json!({"email": "o@b.example", "role": "viewer"})),
        Some(Cred::Bearer(&secret)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // last_used is batched, not written per request: after those calls the row still
    // reads untouched until the flush lands them in one write.
    monitor_server::flush_token_uses(&state).await?;
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/tokens",
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["tokens"][0]["last_used_at"].is_null(), "{body}");

    // Revoke and the token is dead immediately.
    let (status, _, _) = call(
        &app,
        "DELETE",
        &format!("/api/tokens/{token_id}"),
        None,
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, body) = call(
        &app,
        "GET",
        "/api/monitors",
        None,
        Some(Cred::Bearer(&secret)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    Ok(())
}

/// A viewer-role token reads its workspace but cannot write — the role bound at mint
/// follows the token, not the account that created it.
#[tokio::test]
async fn a_viewer_token_reads_but_cannot_write() -> anyhow::Result<()> {
    let (app, _state, _db, _tmp) = fixture().await?;
    let owner = sign_up(&app, "owner@a.example").await;
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/tokens",
        Some(serde_json::json!({"name": "readonly", "role": "viewer"})),
        Some(Cred::Cookie(&owner)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret = body["token"]["secret"]
        .as_str()
        .expect("shown once")
        .to_owned();

    let (status, _, _) = call(
        &app,
        "GET",
        "/api/monitors",
        None,
        Some(Cred::Bearer(&secret)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/monitors",
        Some(monitor_body("nope")),
        Some(Cred::Bearer(&secret)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    Ok(())
}
