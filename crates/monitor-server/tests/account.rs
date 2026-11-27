//! Account lifecycle (D-027): an account changes its password, deletes itself, and a
//! workspace's owner exports the workspace's data.
//!
//! Each protection is asserted by its specific refusal and by the stored state read
//! back afterwards: a wrong password changes nothing, a deletion takes exactly the
//! account's own workspace, and an export holds one workspace's record and no secret.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use monitor_db::{Db, Observation, writer};
use monitor_domain::{
    CheckOutcome, Monitor, MonitorId, NewMonitor, Role, SlotObservation, WorkspaceId,
};
use monitor_server::auth;
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::watch;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";
const NEW_PASSWORD: &str = "a second, different passphrase";
const OPERATOR: &str = "test-operator-token";

const WRONG_PASSWORD: &str = "invalid credentials";
const NOT_AN_ACCOUNT: &str = "only a signed-in account can change its password or delete itself";

struct Rig {
    app: axum::Router,
    db: Db,
    /// What the coordinator would schedule: the last registry the API published.
    registry: watch::Receiver<Arc<Vec<Monitor>>>,
    writer: writer::WriterHandle,
    tmp: TempDir,
}

async fn rig() -> anyhow::Result<Rig> {
    rig_with(1).await
}

/// A rig whose store pool holds `connections`: more than one lets a test read the
/// store while a request's transaction is still open.
async fn rig_with(connections: u32) -> anyhow::Result<Rig> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("api.db").display()),
        connections,
    )
    .await?;
    db.migrate().await?;
    let (registry, receiver) = watch::channel(Arc::new(Vec::new()));
    let state = monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::default(),
        registry,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: Some(OPERATOR.into()),
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: Arc::default(),
        status_writes: Arc::default(),
        status_slugs: Arc::default(),
        status_slug_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: Arc::default(),
        monitor_writes: Arc::default(),
        invite_writes: Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: auth::CookieSecure::Auto,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    };
    let (writer, _task) = writer::spawn(
        db.clone(),
        state.statuses.clone(),
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    Ok(Rig {
        app: monitor_server::router(state),
        db,
        registry: receiver,
        writer,
        tmp,
    })
}

/// What carries the request's credential: a bearer (the operator token or a workspace
/// API token) or a session cookie, which rides with a matching Host/Origin pair.
#[derive(Clone, Copy)]
enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
}

/// One request: who sends it, in which workspace, from which address.
#[derive(Clone, Copy, Default)]
struct Who<'a> {
    cred: Option<Cred<'a>>,
    workspace: Option<&'a str>,
    peer: Option<SocketAddr>,
}

fn cookie(id: &str) -> Who<'_> {
    Who {
        cred: Some(Cred::Cookie(id)),
        ..Who::default()
    }
}

fn bearer(token: &str) -> Who<'_> {
    Who {
        cred: Some(Cred::Bearer(token)),
        ..Who::default()
    }
}

/// `cred`, acting in `workspace` (the `X-Smon-Workspace` pick).
fn within<'a>(cred: Cred<'a>, workspace: &'a str) -> Who<'a> {
    Who {
        cred: Some(cred),
        workspace: Some(workspace),
        peer: None,
    }
}

/// The response as sent: status, headers and the body's text.
#[expect(
    clippy::unwrap_used,
    reason = "test helper: a request that can't build is a failed test"
)]
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    from: Who<'_>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().method(method).uri(uri);
    match from.cred {
        Some(Cred::Bearer(token)) => {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        Some(Cred::Cookie(id)) => {
            request = request
                .header(header::COOKIE, format!("__Host-smon_session={id}"))
                .header(header::HOST, "api.test")
                .header(header::ORIGIN, "http://api.test");
        }
        None => {}
    }
    if let Some(workspace) = from.workspace {
        request = request.header("X-Smon-Workspace", workspace);
    }
    let payload = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&json).unwrap())
        }
        None => Body::empty(),
    };
    let mut request = request.body(payload).unwrap();
    if let Some(peer) = from.peer {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// [`send`], with the body parsed (`Null` when it is empty or not JSON).
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    from: Who<'_>,
) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, text) = send(app, method, uri, body, from).await;
    (
        status,
        headers,
        serde_json::from_str(&text).unwrap_or(Value::Null),
    )
}

/// The session id a sign-in's `Set-Cookie` carries, if it set one.
fn session_id(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::SET_COOKIE)?.to_str().ok()?;
    let value = raw
        .strip_prefix("__Host-smon_session=")?
        .split(';')
        .next()?;
    (!value.is_empty()).then(|| value.to_owned())
}

/// Sign in; the status and, on success, the session cookie's id.
async fn sign_in(
    app: &axum::Router,
    email: &str,
    password: &str,
    peer: Option<SocketAddr>,
) -> (StatusCode, Option<String>) {
    let from = Who {
        peer,
        ..Who::default()
    };
    let (status, headers, _) = call(
        app,
        "POST",
        "/api/login",
        Some(json!({"email": email, "password": password})),
        from,
    )
    .await;
    (status, session_id(&headers))
}

/// Mint an invite as the operator, sign `email` up, and sign in: the session cookie.
#[expect(
    clippy::expect_used,
    reason = "test helper: a signup or sign-in that fails is a failed test"
)]
async fn sign_up(app: &axum::Router, email: &str) -> String {
    let (status, _, body) = call(
        app,
        "POST",
        "/api/invites",
        Some(json!({})),
        bearer(OPERATOR),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let code = body["invite"]["code"].as_str().expect("invite code");
    let (status, _, body) = call(
        app,
        "POST",
        "/api/signup",
        Some(json!({"email": email, "password": PASSWORD, "invite": code})),
        Who::default(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, id) = sign_in(app, email, PASSWORD, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    id.expect("a session cookie is set")
}

/// The workspace a signup created for `email`.
async fn workspace_of(db: &Db, email: &str) -> anyhow::Result<WorkspaceId> {
    let row = db
        .account_by_email(email)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no account for {email}"))?;
    Ok(WorkspaceId(uuid::Uuid::parse_str(&row.workspace_id)?))
}

/// The stored password hash behind `email`, or `None` once the account is gone.
async fn stored_hash(db: &Db, email: &str) -> anyhow::Result<Option<String>> {
    Ok(db
        .account_by_email(email)
        .await?
        .map(|row| row.password_hash))
}

async fn record(rig: &Rig, monitor: &Monitor, outcome: CheckOutcome) -> anyhow::Result<()> {
    rig.writer
        .record(Observation {
            monitor_id: monitor.id,
            workspace: monitor.workspace,
            generation: monitor.generation,
            slot_at: OffsetDateTime::now_utc(),
            slot: SlotObservation::Probe(outcome),
            http_status: Some(503),
            latency_ms: Some(12),
            detail: None,
        })
        .await?;
    Ok(())
}

/// A monitor with real evidence in `workspace`: a pass, then the three failures that
/// open an incident and queue its alert, and a maintenance window.
async fn seed(rig: &Rig, workspace: WorkspaceId, name: &str) -> anyhow::Result<Monitor> {
    let monitor = NewMonitor::new(name, url::Url::parse("https://target.example/health")?)?
        .into_monitor_for(workspace);
    rig.db.workspace(workspace).insert_monitor(&monitor).await?;
    record(rig, &monitor, CheckOutcome::Pass).await?;
    for _ in 0..3 {
        record(rig, &monitor, CheckOutcome::HttpFail { status: 503 }).await?;
    }
    let now = OffsetDateTime::now_utc();
    rig.db
        .member(workspace)
        .create_window(monitor.id, now, now + time::Duration::hours(1), "deploy")
        .await?;
    Ok(monitor)
}

/// What one workspace holds, counted through the store: monitors, checks, incidents,
/// queued alerts, maintenance windows, members and API tokens.
#[derive(Debug, PartialEq, Eq)]
struct Held {
    monitors: usize,
    checks: usize,
    incidents: usize,
    outbox: usize,
    windows: usize,
    members: usize,
    tokens: usize,
}

async fn held(db: &Db, workspace: WorkspaceId) -> anyhow::Result<Held> {
    let view = db.owner(workspace);
    let monitors = view.list_monitors().await?;
    let (mut checks, mut outbox, mut windows) = (0, 0, 0);
    for row in &monitors {
        let id = MonitorId(uuid::Uuid::parse_str(&row.id)?);
        checks += view.list_checks(id, 100_000).await?.len();
        outbox += view.list_outbox(id).await?.len();
        windows += view.list_windows(id).await?.len();
    }
    Ok(Held {
        monitors: monitors.len(),
        checks,
        incidents: view.list_incidents(None, None, 100_000).await?.len(),
        outbox,
        windows,
        members: view.list_members().await?.len(),
        tokens: view.list_api_tokens().await?.len(),
    })
}

/// Mint a workspace API token as `owner`; its secret.
#[expect(
    clippy::expect_used,
    reason = "test helper: a mint that fails is a failed test"
)]
async fn mint_token(app: &axum::Router, owner: &str, role: &str) -> String {
    let (status, _, body) = call(
        app,
        "POST",
        "/api/tokens",
        Some(json!({"name": format!("{role} token"), "role": role})),
        cookie(owner),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["token"]["secret"].as_str().expect("secret").to_owned()
}

/// Seat `email`'s account in `workspace` at `role`, as an owner's store write.
async fn seat(db: &Db, workspace: WorkspaceId, email: &str, role: Role) -> anyhow::Result<()> {
    let row = db
        .account_by_email(email)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no account for {email}"))?;
    let account = monitor_domain::AccountId(uuid::Uuid::parse_str(&row.id)?);
    db.owner(workspace)
        .add_member(account, role, OffsetDateTime::now_utc())
        .await?;
    Ok(())
}

async fn change(
    app: &axum::Router,
    from: Who<'_>,
    current: &str,
    new: &str,
) -> (StatusCode, Value) {
    let (status, _, body) = call(
        app,
        "POST",
        "/api/account/password",
        Some(json!({"current_password": current, "new_password": new})),
        from,
    )
    .await;
    (status, body)
}

async fn delete(
    app: &axum::Router,
    from: Who<'_>,
    password: &str,
) -> (StatusCode, HeaderMap, Value) {
    call(
        app,
        "DELETE",
        "/api/account",
        Some(json!({"password": password})),
        from,
    )
    .await
}

async fn signed_in(app: &axum::Router, session: &str) -> StatusCode {
    call(app, "GET", "/api/workspaces", None, cookie(session))
        .await
        .0
}

#[tokio::test]
async fn a_wrong_current_password_is_401_and_changes_nothing() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let (_, other_device) = sign_in(&rig.app, "ada@tenant.example", PASSWORD, None).await;
    let other_device = other_device.ok_or_else(|| anyhow::anyhow!("second session"))?;
    let before = stored_hash(&rig.db, "ada@tenant.example").await?;

    let (status, body) = change(&rig.app, cookie(&session), "not the password", NEW_PASSWORD).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "unauthenticated", "{body}");
    assert_eq!(body["error"]["message"], WRONG_PASSWORD, "{body}");
    assert_eq!(stored_hash(&rig.db, "ada@tenant.example").await?, before);
    assert_eq!(
        sign_in(&rig.app, "ada@tenant.example", NEW_PASSWORD, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "the refused password must not sign in"
    );
    // Neither session was touched by the refusal.
    assert_eq!(signed_in(&rig.app, &session).await, StatusCode::OK);
    assert_eq!(signed_in(&rig.app, &other_device).await, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_changed_password_replaces_the_old_one_and_ends_every_other_session() -> anyhow::Result<()>
{
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let (_, other_device) = sign_in(&rig.app, "ada@tenant.example", PASSWORD, None).await;
    let other_device = other_device.ok_or_else(|| anyhow::anyhow!("second session"))?;
    let bystander = sign_up(&rig.app, "bystander@tenant.example").await;
    assert_eq!(signed_in(&rig.app, &other_device).await, StatusCode::OK);

    let (status, body) = change(&rig.app, cookie(&session), PASSWORD, NEW_PASSWORD).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert_eq!(
        sign_in(&rig.app, "ada@tenant.example", PASSWORD, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "the old password still signs in"
    );
    assert_eq!(
        sign_in(&rig.app, "ada@tenant.example", NEW_PASSWORD, None)
            .await
            .0,
        StatusCode::NO_CONTENT,
        "the new password does not sign in"
    );
    assert_eq!(
        signed_in(&rig.app, &session).await,
        StatusCode::OK,
        "the session that changed the password was signed out"
    );
    assert_eq!(
        signed_in(&rig.app, &other_device).await,
        StatusCode::UNAUTHORIZED,
        "another session of the account outlived the password change"
    );
    // Someone else's session and password are theirs.
    assert_eq!(signed_in(&rig.app, &bystander).await, StatusCode::OK);
    assert_eq!(
        sign_in(&rig.app, "bystander@tenant.example", PASSWORD, None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    Ok(())
}

/// The new password follows the sign-up rule and nothing else: 8 to 256 bytes. A
/// refusal for shape is not a guess, so six in a row are all 422 and none is throttled.
#[tokio::test]
async fn a_new_password_follows_the_sign_up_rule() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let before = stored_hash(&rig.db, "ada@tenant.example").await?;
    let too_long = "p".repeat(257);

    for new in [
        "",
        "short",
        "7 chars",
        too_long.as_str(),
        "1234567",
        "é1234",
    ] {
        let (status, body) = change(&rig.app, cookie(&session), PASSWORD, new).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{new:?}: {body}");
        assert_eq!(body["error"]["code"], "invalid_request", "{body}");
        assert_eq!(
            body["error"]["message"], "`new_password` must be 8..=256 characters",
            "{body}"
        );
    }
    assert_eq!(stored_hash(&rig.db, "ada@tenant.example").await?, before);

    // The bounds themselves are allowed, exactly as at sign-up.
    let longest = "p".repeat(256);
    let (status, body) = change(&rig.app, cookie(&session), PASSWORD, "8 chars!").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = change(&rig.app, cookie(&session), "8 chars!", &longest).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        sign_in(&rig.app, "ada@tenant.example", &longest, None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    Ok(())
}

/// The account routes are for the person behind an account session. The operator's
/// token, the operator's session and a workspace API token are each refused 403 before
/// any password is looked at, and nothing is changed or deleted.
#[tokio::test]
async fn tokens_and_the_operator_get_403_on_both_account_routes() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let workspace = workspace_of(&rig.db, "ada@tenant.example").await?;
    seed(&rig, workspace, "ada-api").await?;
    let token = mint_token(&rig.app, &session, "member").await;
    let (status, headers, _) = call(
        &rig.app,
        "POST",
        "/api/login",
        Some(json!({"token": OPERATOR})),
        Who::default(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let operator_session =
        session_id(&headers).ok_or_else(|| anyhow::anyhow!("operator session"))?;
    let before = (
        stored_hash(&rig.db, "ada@tenant.example").await?,
        held(&rig.db, workspace).await?,
    );

    let picked = workspace.0.to_string();
    for (who, from) in [
        ("operator token", bearer(OPERATOR)),
        (
            "operator token in the account's workspace",
            Who {
                workspace: Some(&picked),
                ..bearer(OPERATOR)
            },
        ),
        ("operator session", cookie(&operator_session)),
        ("workspace API token", bearer(&token)),
    ] {
        let (status, body) = change(&rig.app, from, PASSWORD, NEW_PASSWORD).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {body}");
        assert_eq!(body["error"]["code"], "forbidden", "{who}: {body}");
        assert_eq!(body["error"]["message"], NOT_AN_ACCOUNT, "{who}: {body}");
        let (status, _, body) = delete(&rig.app, from, PASSWORD).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {body}");
        assert_eq!(body["error"]["message"], NOT_AN_ACCOUNT, "{who}: {body}");
    }

    let after = (
        stored_hash(&rig.db, "ada@tenant.example").await?,
        held(&rig.db, workspace).await?,
    );
    assert_eq!(after, before);
    assert_eq!(signed_in(&rig.app, &session).await, StatusCode::OK);
    Ok(())
}

/// Asking for the password again is a credential guess: it draws on the same
/// per-address budget as sign-in. The guesses go out together, so their admissions
/// cluster inside one refill period however slow the hashing is (the pattern
/// `accounts.rs` uses): a burst of five is evaluated, the rest are 429 before any
/// password is compared, and nothing is changed or deleted by any of them.
#[tokio::test]
async fn password_guesses_on_the_account_routes_are_throttled_like_sign_in() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let flood: SocketAddr = "198.51.100.7:40000".parse()?;
    let honest: SocketAddr = "198.51.100.8:40000".parse()?;
    let before = stored_hash(&rig.db, "ada@tenant.example").await?;

    let started = std::time::Instant::now();
    let mut guesses = tokio::task::JoinSet::new();
    for n in 0..8 {
        let (app, session) = (rig.app.clone(), session.clone());
        guesses.spawn(async move {
            let from = Who {
                peer: Some(flood),
                ..cookie(&session)
            };
            // Both routes spend the same budget: alternate them.
            if n % 2 == 0 {
                let (status, body) = change(&app, from, "a wrong guess", NEW_PASSWORD).await;
                (status, HeaderMap::new(), body)
            } else {
                delete(&app, from, "a wrong guess").await
            }
        });
    }
    let (mut evaluated, mut refused) = (0_u64, 0_u64);
    while let Some(outcome) = guesses.join_next().await {
        let (status, headers, body) = outcome?;
        match status {
            StatusCode::UNAUTHORIZED => {
                evaluated += 1;
                assert_eq!(body["error"]["message"], WRONG_PASSWORD, "{body}");
            }
            StatusCode::TOO_MANY_REQUESTS => {
                refused += 1;
                assert_eq!(body["error"]["code"], "too_many_requests", "{body}");
                // The password route's helper drops headers; the delete route's 429
                // must say when to retry.
                if !headers.is_empty() {
                    assert!(
                        headers.get(header::RETRY_AFTER).is_some(),
                        "429 without Retry-After"
                    );
                }
            }
            // A timed-out guess was still admitted: its token is spent either way.
            StatusCode::REQUEST_TIMEOUT => evaluated += 1,
            other => anyhow::bail!("unexpected {other}: {body}"),
        }
    }
    assert!(refused > 0, "eight wrong guesses and none was throttled");
    // The burst is five; the only slack is what real time refilled (one per 2 s).
    let refilled = started.elapsed().as_secs() / 2 + 1;
    assert!(
        evaluated <= 5 + refilled,
        "{evaluated} guesses were evaluated; the budget allows {}",
        5 + refilled
    );

    // Nothing was changed or deleted, and another address signs in as usual.
    assert_eq!(stored_hash(&rig.db, "ada@tenant.example").await?, before);
    assert_eq!(
        sign_in(&rig.app, "ada@tenant.example", PASSWORD, Some(honest))
            .await
            .0,
        StatusCode::NO_CONTENT,
        "one address's wrong guesses locked another address out"
    );
    Ok(())
}

/// A right password gives its guess back. Four changes in a row leave the address's
/// bucket full, so a burst of wrong guesses afterwards still gets five evaluations.
/// Without the give-back the four changes would have left one (plus the slow refill).
#[tokio::test]
async fn a_right_password_does_not_spend_the_address_budget() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let peer: SocketAddr = "198.51.100.9:40000".parse()?;
    let from = Who {
        peer: Some(peer),
        ..cookie(&session)
    };
    let (mut current, mut next) = (PASSWORD, NEW_PASSWORD);
    for round in 0..4 {
        let (status, body) = change(&rig.app, from, current, next).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "round {round}: {body}");
        std::mem::swap(&mut current, &mut next);
    }

    let mut guesses = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (app, session) = (rig.app.clone(), session.clone());
        guesses.spawn(async move {
            let from = Who {
                peer: Some(peer),
                ..cookie(&session)
            };
            change(&app, from, "a wrong guess", NEW_PASSWORD).await.0
        });
    }
    let mut evaluated = 0;
    while let Some(status) = guesses.join_next().await {
        if matches!(
            status?,
            StatusCode::UNAUTHORIZED | StatusCode::REQUEST_TIMEOUT
        ) {
            evaluated += 1;
        }
    }
    assert!(
        evaluated >= 5,
        "only {evaluated} guesses were admitted after four correct passwords: \
         a right password is being charged"
    );
    Ok(())
}

#[tokio::test]
async fn deleting_with_a_wrong_password_is_401_and_deletes_nothing() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let workspace = workspace_of(&rig.db, "ada@tenant.example").await?;
    seed(&rig, workspace, "ada-api").await?;
    let before = held(&rig.db, workspace).await?;

    let (status, headers, body) = delete(&rig.app, cookie(&session), "not the password").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "unauthenticated", "{body}");
    assert_eq!(body["error"]["message"], WRONG_PASSWORD, "{body}");
    assert!(
        headers.get(header::SET_COOKIE).is_none(),
        "a refused deletion must not expire the session cookie"
    );
    assert_eq!(held(&rig.db, workspace).await?, before);
    assert!(stored_hash(&rig.db, "ada@tenant.example").await?.is_some());
    assert_eq!(signed_in(&rig.app, &session).await, StatusCode::OK);
    assert_eq!(
        sign_in(&rig.app, "ada@tenant.example", PASSWORD, None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    Ok(())
}

/// Publish `workspace`'s status page at `slug` and put a snapshot file where the
/// writer would, so the public route has something to serve.
async fn publish(
    rig: &Rig,
    workspace: WorkspaceId,
    slug: &str,
    monitor: &Monitor,
) -> anyhow::Result<std::path::PathBuf> {
    rig.db
        .member(workspace)
        .set_status_page(Some(slug), Some("Status"), &[monitor.id])
        .await?;
    let dir = rig.tmp.path().join("status").join("s");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.json", workspace.0));
    std::fs::write(&file, br#"{"monitoring":"ok","services":[]}"#)?;
    Ok(file)
}

async fn public_page(app: &axum::Router, slug: &str) -> StatusCode {
    let uri = format!("/s/{slug}/status.json");
    call(app, "GET", &uri, None, Who::default()).await.0
}

/// Two accounts, each with a workspace that holds real evidence, an API token and a
/// published status page whose snapshot file is on disk.
struct Pair {
    rig: Rig,
    leaving: String,
    staying: String,
    gone: WorkspaceId,
    kept: WorkspaceId,
    gone_monitor: Monitor,
    kept_monitor: Monitor,
    gone_token: String,
    kept_token: String,
    gone_file: std::path::PathBuf,
    kept_file: std::path::PathBuf,
}

async fn pair() -> anyhow::Result<Pair> {
    let rig = rig().await?;
    let leaving = sign_up(&rig.app, "leaving@tenant.example").await;
    let staying = sign_up(&rig.app, "staying@tenant.example").await;
    let gone = workspace_of(&rig.db, "leaving@tenant.example").await?;
    let kept = workspace_of(&rig.db, "staying@tenant.example").await?;
    let gone_monitor = seed(&rig, gone, "leaving-api").await?;
    let kept_monitor = seed(&rig, kept, "staying-api").await?;
    let gone_token = mint_token(&rig.app, &leaving, "member").await;
    let kept_token = mint_token(&rig.app, &staying, "member").await;
    let gone_file = publish(&rig, gone, "leaving-status", &gone_monitor).await?;
    let kept_file = publish(&rig, kept, "staying-status", &kept_monitor).await?;
    // Both pages are served, which also fills the public route's slug cache.
    for slug in ["leaving-status", "staying-status"] {
        assert_eq!(public_page(&rig.app, slug).await, StatusCode::OK, "{slug}");
    }
    Ok(Pair {
        rig,
        leaving,
        staying,
        gone,
        kept,
        gone_monitor,
        kept_monitor,
        gone_token,
        kept_token,
        gone_file,
        kept_file,
    })
}

const NOTHING: Held = Held {
    monitors: 0,
    checks: 0,
    incidents: 0,
    outbox: 0,
    windows: 0,
    members: 0,
    tokens: 0,
};

/// The deletion itself: the account's own workspace goes with everything recorded in
/// it, and a second account's workspace is counted before and after and is the same.
#[tokio::test]
async fn deleting_an_account_removes_its_own_workspace_and_no_one_elses() -> anyhow::Result<()> {
    let pair = pair().await?;
    let (rig, gone, kept) = (&pair.rig, pair.gone, pair.kept);
    let before = held(&rig.db, kept).await?;
    assert_eq!(
        before,
        Held {
            monitors: 1,
            checks: 4,
            incidents: 1,
            outbox: 1,
            windows: 1,
            members: 1,
            tokens: 1
        },
        "the fixture seeds real evidence in the workspace that must survive"
    );
    assert_eq!(held(&rig.db, gone).await?, before);

    let (status, headers, body) = delete(&rig.app, cookie(&pair.leaving), PASSWORD).await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let expired = headers
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        expired.starts_with("__Host-smon_session=;") && expired.contains("Max-Age=0"),
        "the response must expire the session cookie: {expired:?}"
    );
    // The account's rows, and its workspace's, are gone from the store.
    assert!(
        rig.db
            .account_by_email("leaving@tenant.example")
            .await?
            .is_none()
    );
    assert!(rig.db.workspace_row(gone).await?.is_none());
    assert_eq!(held(&rig.db, gone).await?, NOTHING);
    let view = rig.db.workspace(gone);
    assert!(view.list_checks(pair.gone_monitor.id, 10).await?.is_empty());
    assert!(view.list_outbox(pair.gone_monitor.id).await?.is_empty());

    // The other account's workspace is what it was, row for row, and still works.
    assert_eq!(held(&rig.db, kept).await?, before);
    assert_eq!(signed_in(&rig.app, &pair.staying).await, StatusCode::OK);
    let (status, _, body) = call(
        &rig.app,
        "GET",
        "/api/monitors",
        None,
        bearer(&pair.kept_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["monitors"][0]["name"], "staying-api", "{body}");
    Ok(())
}

/// Nothing of a deleted account stays reachable: its session cookie, its password, its
/// workspace's API token, its public status page and that page's file, and its
/// monitors in the schedule. The other account's are all still there.
#[tokio::test]
async fn a_deleted_account_leaves_no_way_back_in() -> anyhow::Result<()> {
    let pair = pair().await?;
    let rig = &pair.rig;

    let (status, _, body) = delete(&rig.app, cookie(&pair.leaving), PASSWORD).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert_eq!(
        signed_in(&rig.app, &pair.leaving).await,
        StatusCode::UNAUTHORIZED,
        "the deleted account's session cookie still works"
    );
    assert_eq!(
        sign_in(&rig.app, "leaving@tenant.example", PASSWORD, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "the deleted account can still sign in"
    );
    let with_gone_token = bearer(&pair.gone_token);
    assert_eq!(
        call(&rig.app, "GET", "/api/monitors", None, with_gone_token)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "the deleted workspace's API token still works"
    );
    assert_eq!(
        public_page(&rig.app, "leaving-status").await,
        StatusCode::NOT_FOUND,
        "the deleted workspace's status page is still served"
    );
    assert!(
        !pair.gone_file.exists(),
        "the deleted workspace's snapshot file is still on disk"
    );
    let scheduled: Vec<MonitorId> = rig.registry.borrow().iter().map(|m| m.id).collect();
    assert_eq!(
        scheduled,
        vec![pair.kept_monitor.id],
        "the schedule still holds a deleted monitor"
    );

    assert_eq!(
        public_page(&rig.app, "staying-status").await,
        StatusCode::OK
    );
    assert!(pair.kept_file.exists());
    Ok(())
}

/// A deletion that committed is finished even when its request is not: the client
/// hung up, or the deadline passed. The request is polled by hand and dropped the
/// moment the store shows the account gone; the schedule must still be republished
/// without the deleted monitor, or the coordinator would go on probing a target whose
/// owner no longer exists.
#[tokio::test]
async fn a_deletion_whose_request_is_dropped_still_leaves_the_schedule() -> anyhow::Result<()> {
    let rig = rig_with(2).await?;
    let leaving = sign_up(&rig.app, "leaving@tenant.example").await;
    sign_up(&rig.app, "staying@tenant.example").await;
    let gone = workspace_of(&rig.db, "leaving@tenant.example").await?;
    let kept = workspace_of(&rig.db, "staying@tenant.example").await?;
    seed(&rig, gone, "leaving-api").await?;
    let kept_monitor = seed(&rig, kept, "staying-api").await?;
    let mut registry = rig.registry.clone();
    registry.borrow_and_update();

    let request = Request::builder()
        .method("DELETE")
        .uri("/api/account")
        .header(header::COOKIE, format!("__Host-smon_session={leaving}"))
        .header(header::HOST, "api.test")
        .header(header::ORIGIN, "http://api.test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"password": PASSWORD}).to_string()))?;
    {
        let mut pending = std::pin::pin!(rig.app.clone().oneshot(request));
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            // One poll of the request, then a look at the store.
            let answered = tokio::time::timeout(Duration::ZERO, &mut pending)
                .await
                .is_ok();
            let account = rig.db.account_by_email("leaving@tenant.example").await?;
            if answered || account.is_none() {
                break;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "the deletion never committed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The request is dropped here, answered or not.
    }

    tokio::time::timeout(Duration::from_secs(10), registry.changed())
        .await
        .map_err(|_| anyhow::anyhow!("the schedule was never republished after the deletion"))??;
    let scheduled: Vec<MonitorId> = registry.borrow().iter().map(|m| m.id).collect();
    assert_eq!(scheduled, vec![kept_monitor.id]);
    assert!(rig.db.workspace_row(gone).await?.is_none());
    Ok(())
}

/// The refusal, then the way out of it: the only owner of a workspace with other
/// members is told to hand it over first; once someone else is an owner the account
/// deletes and the shared workspace survives with everything in it.
#[tokio::test]
async fn the_last_owner_of_a_shared_workspace_must_hand_it_over_first() -> anyhow::Result<()> {
    let rig = rig().await?;
    let owner = sign_up(&rig.app, "owner@tenant.example").await;
    let colleague = sign_up(&rig.app, "colleague@tenant.example").await;
    let shared = workspace_of(&rig.db, "owner@tenant.example").await?;
    seed(&rig, shared, "shared-api").await?;
    seat(&rig.db, shared, "colleague@tenant.example", Role::Member).await?;
    let before = held(&rig.db, shared).await?;

    let (status, headers, body) = delete(&rig.app, cookie(&owner), PASSWORD).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict", "{body}");
    assert_eq!(
        body["error"]["message"],
        "the workspace \"Personal workspace\" needs an owner: make another member an owner, \
         then delete your account",
        "{body}"
    );
    assert!(headers.get(header::SET_COOKIE).is_none());
    assert_eq!(held(&rig.db, shared).await?, before);
    assert_eq!(
        signed_in(&rig.app, &owner).await,
        StatusCode::OK,
        "a refused deletion signed the account out"
    );

    // Hand the workspace over through the API, as the refusal asks.
    let colleague_id = rig
        .db
        .account_by_email("colleague@tenant.example")
        .await?
        .ok_or_else(|| anyhow::anyhow!("colleague"))?
        .id;
    let (status, _, body) = call(
        &rig.app,
        "PATCH",
        &format!("/api/members/{colleague_id}"),
        Some(json!({"role": "owner"})),
        cookie(&owner),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = delete(&rig.app, cookie(&owner), PASSWORD).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let after = held(&rig.db, shared).await?;
    assert_eq!(
        after,
        Held {
            members: before.members - 1,
            ..before
        }
    );
    let picked = shared.0.to_string();
    let in_shared = Who {
        workspace: Some(&picked),
        ..cookie(&colleague)
    };
    let (status, _, body) = call(&rig.app, "GET", "/api/monitors", None, in_shared).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["monitors"][0]["name"], "shared-api", "{body}");
    assert_eq!(signed_in(&rig.app, &owner).await, StatusCode::UNAUTHORIZED);
    Ok(())
}

/// A member who is not the last owner just leaves: the shared workspace keeps every
/// row, and only the leaver's own workspace is deleted.
#[tokio::test]
async fn a_shared_workspace_survives_a_member_deleting_their_account() -> anyhow::Result<()> {
    let rig = rig().await?;
    let owner = sign_up(&rig.app, "owner@tenant.example").await;
    let member = sign_up(&rig.app, "member@tenant.example").await;
    let shared = workspace_of(&rig.db, "owner@tenant.example").await?;
    let members_own = workspace_of(&rig.db, "member@tenant.example").await?;
    let shared_monitor = seed(&rig, shared, "shared-api").await?;
    seed(&rig, members_own, "members-api").await?;
    seat(&rig.db, shared, "member@tenant.example", Role::Member).await?;
    let before = held(&rig.db, shared).await?;

    let (status, _, body) = delete(&rig.app, cookie(&member), PASSWORD).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert_eq!(
        held(&rig.db, shared).await?,
        Held {
            members: before.members - 1,
            ..before
        }
    );
    assert!(rig.db.workspace_row(members_own).await?.is_none());
    let (status, _, body) = call(&rig.app, "GET", "/api/members", None, cookie(&owner)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let roster: Vec<&str> = body["members"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["email"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(roster, ["owner@tenant.example"]);
    let scheduled: Vec<MonitorId> = rig.registry.borrow().iter().map(|m| m.id).collect();
    assert_eq!(scheduled, vec![shared_monitor.id]);
    Ok(())
}

/// Every key of a JSON document, at any depth.
fn keys(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                found.push(key.clone());
                keys(inner, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|inner| keys(inner, found)),
        _ => {}
    }
}

/// A workspace with evidence, a webhook and an API token, beside a second account's
/// workspace: what an export must hold and what it must leave out.
struct Exported {
    rig: Rig,
    session: String,
    mine: WorkspaceId,
    theirs: WorkspaceId,
    monitor: Monitor,
    foreign: Monitor,
    token: String,
    webhook_secret: String,
    slug: String,
    headers: HeaderMap,
    text: String,
}

async fn exported() -> anyhow::Result<Exported> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    sign_up(&rig.app, "other@tenant.example").await;
    let mine = workspace_of(&rig.db, "ada@tenant.example").await?;
    let theirs = workspace_of(&rig.db, "other@tenant.example").await?;
    let monitor = seed(&rig, mine, "ada-api").await?;
    let foreign = seed(&rig, theirs, "someone-elses-api").await?;
    let token = mint_token(&rig.app, &session, "member").await;
    let webhook = rig
        .db
        .member(mine)
        .set_webhook("https://hooks.example/in")
        .await?;
    let slug = rig
        .db
        .workspace_row(mine)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workspace"))?
        .slug;
    let (status, headers, text) = send(
        &rig.app,
        "GET",
        "/api/workspace/export",
        None,
        cookie(&session),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    Ok(Exported {
        rig,
        session,
        mine,
        theirs,
        monitor,
        foreign,
        token,
        webhook_secret: webhook.secret,
        slug,
        headers,
        text,
    })
}

/// The export is the workspace's own record: the workspace, its roster, its monitors,
/// incidents, maintenance windows and checks, sent as a file named after its slug.
#[tokio::test]
async fn an_export_holds_the_workspaces_record() -> anyhow::Result<()> {
    let out = exported().await?;
    let header_of = |name| out.headers.get(name).and_then(|value| value.to_str().ok());
    assert_eq!(
        header_of(header::CONTENT_DISPOSITION),
        Some(
            format!(
                "attachment; filename=\"service-monitor-export-{}.json\"",
                out.slug
            )
            .as_str()
        )
    );
    assert_eq!(header_of(header::CACHE_CONTROL), Some("no-store"));
    let body: Value = serde_json::from_str(&out.text)?;
    let monitor_id = json!(out.monitor.id.0);
    let count = |field: &str| body[field].as_array().map(Vec::len);
    assert_eq!(
        body["workspace"],
        json!({
            "id": out.mine.0, "name": "Personal workspace", "slug": out.slug, "plan": "free",
        })
    );
    assert_eq!(
        body["members"],
        json!([{"email": "ada@tenant.example", "role": "owner"}])
    );
    assert_eq!(count("monitors"), Some(1), "{}", out.text);
    assert_eq!(body["monitors"][0]["name"], "ada-api");
    assert_eq!(body["monitors"][0]["id"], monitor_id);
    assert_eq!(body["monitors"][0]["url"], "https://target.example/health");
    assert_eq!(count("incidents"), Some(1), "{}", out.text);
    assert_eq!(body["incidents"][0]["monitor_id"], monitor_id);
    assert_eq!(body["incidents"][0]["state"], "open");
    assert_eq!(count("maintenance_windows"), Some(1));
    assert_eq!(body["maintenance_windows"][0]["reason"], "deploy");
    assert_eq!(body["daily_aggregates"], json!([]));
    assert_eq!(count("checks"), Some(1));
    assert_eq!(body["checks"][0]["monitor_id"], monitor_id);
    assert_eq!(body["checks"][0]["truncated"], false);
    assert_eq!(body["checks"][0]["rows"].as_array().map(Vec::len), Some(4));
    assert_eq!(
        body["truncated"],
        json!({"incidents": false, "maintenance_windows": false, "daily_aggregates": false})
    );
    Ok(())
}

/// The export never carries a credential in any form, and nothing of another
/// workspace: searched in the serialized text, and in every field name.
#[tokio::test]
async fn an_export_carries_no_secret_and_nothing_of_another_workspace() -> anyhow::Result<()> {
    let out = exported().await?;
    assert!(out.webhook_secret.starts_with("smon_whk_"));
    assert!(out.token.starts_with("smon_wst_"));
    let hash = stored_hash(&out.rig.db, "ada@tenant.example")
        .await?
        .unwrap_or_default();
    assert!(
        hash.starts_with("$argon2"),
        "the fixture's hash is a real one"
    );
    for (what, needle) in [
        ("another workspace's monitor", "someone-elses-api".into()),
        (
            "another workspace's monitor id",
            out.foreign.id.0.to_string(),
        ),
        ("another workspace's id", out.theirs.0.to_string()),
        ("another account's email", "other@tenant.example".into()),
        ("the webhook secret", out.webhook_secret.clone()),
        ("the webhook URL", "hooks.example".into()),
        ("an API token secret", out.token.clone()),
        ("an API token digest", auth::api_token_key(&out.token)),
        ("any smon_ secret", "smon_".into()),
        ("a password hash", "$argon2".into()),
        ("the password hash", hash),
        ("the session id", out.session.clone()),
        (
            "the session row's key",
            auth::account_session_key(&out.session),
        ),
    ] {
        let needle: String = needle;
        assert!(!out.text.contains(&needle), "the export contains {what}");
    }
    let mut found = Vec::new();
    keys(&serde_json::from_str(&out.text)?, &mut found);
    assert!(found.iter().any(|key| key == "monitors"));
    for key in &found {
        for word in [
            "secret", "password", "hash", "digest", "token", "session", "webhook",
        ] {
            assert!(!key.contains(word), "the export has a `{key}` field");
        }
    }
    Ok(())
}

/// Owner work: a member, a viewer and any workspace API token are refused 403, each by
/// name, and the operator (who acts at owner level in a workspace it picks) is served.
#[tokio::test]
async fn only_an_owner_exports_a_workspace() -> anyhow::Result<()> {
    let rig = rig().await?;
    let owner = sign_up(&rig.app, "owner@tenant.example").await;
    let member = sign_up(&rig.app, "member@tenant.example").await;
    let viewer = sign_up(&rig.app, "viewer@tenant.example").await;
    let workspace = workspace_of(&rig.db, "owner@tenant.example").await?;
    seed(&rig, workspace, "owner-api").await?;
    seat(&rig.db, workspace, "member@tenant.example", Role::Member).await?;
    seat(&rig.db, workspace, "viewer@tenant.example", Role::Viewer).await?;
    let member_token = mint_token(&rig.app, &owner, "member").await;
    let viewer_token = mint_token(&rig.app, &owner, "viewer").await;
    let picked = workspace.0.to_string();
    let here = |cred| within(cred, &picked);

    for (who, from) in [
        ("member", here(Cred::Cookie(&member))),
        ("viewer", here(Cred::Cookie(&viewer))),
        ("member token", bearer(&member_token)),
        ("viewer token", bearer(&viewer_token)),
    ] {
        let (status, headers, body) =
            call(&rig.app, "GET", "/api/workspace/export", None, from).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {body}");
        assert_eq!(body["error"]["code"], "forbidden", "{who}: {body}");
        assert_eq!(
            body["error"]["message"], "exporting a workspace's data is owner work",
            "{who}: {body}"
        );
        assert!(headers.get(header::CONTENT_DISPOSITION).is_none(), "{who}");
    }
    // A member's own workspace is theirs to export; the refusal above was the role's.
    let (status, _, body) = call(
        &rig.app,
        "GET",
        "/api/workspace/export",
        None,
        cookie(&member),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["monitors"],
        json!([]),
        "a member exported the owner's monitors"
    );

    let (status, _, body) = call(
        &rig.app,
        "GET",
        "/api/workspace/export",
        None,
        here(Cred::Bearer(OPERATOR)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["monitors"][0]["name"], "owner-api", "{body}");
    Ok(())
}

/// One export per workspace per cooldown: a second one straight away is 429 with the
/// wait, and another workspace's export is not held up by it.
#[tokio::test]
async fn a_second_export_inside_the_cooldown_is_429() -> anyhow::Result<()> {
    let rig = rig().await?;
    let first = sign_up(&rig.app, "first@tenant.example").await;
    let second = sign_up(&rig.app, "second@tenant.example").await;

    let (status, _, _) = call(
        &rig.app,
        "GET",
        "/api/workspace/export",
        None,
        cookie(&first),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, body) = call(
        &rig.app,
        "GET",
        "/api/workspace/export",
        None,
        cookie(&first),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "too_many_requests", "{body}");
    assert_eq!(
        body["error"]["message"],
        "one export every 10 seconds per workspace; retry shortly"
    );
    let wait: u64 = headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("429 without a numeric Retry-After"))?;
    assert!((1..=10).contains(&wait), "Retry-After {wait}");

    let (status, _, _) = call(
        &rig.app,
        "GET",
        "/api/workspace/export",
        None,
        cookie(&second),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "one workspace's cooldown held up another's export"
    );
    Ok(())
}

/// The raw checks are capped per monitor at the newest 5,000, and the export says so.
#[tokio::test]
async fn an_export_caps_a_monitors_checks_and_says_it_did() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let workspace = workspace_of(&rig.db, "ada@tenant.example").await?;
    let busy = NewMonitor::new("busy", url::Url::parse("https://busy.example/")?)?
        .into_monitor_for(workspace);
    let quiet = NewMonitor::new("quiet", url::Url::parse("https://quiet.example/")?)?
        .into_monitor_for(workspace);
    rig.db.workspace(workspace).insert_monitor(&busy).await?;
    rig.db.workspace(workspace).insert_monitor(&quiet).await?;
    let cap = monitor_server::account::EXPORT_CHECKS_PER_MONITOR;
    for _ in 0..=cap {
        record(&rig, &busy, CheckOutcome::Pass).await?;
    }
    record(&rig, &quiet, CheckOutcome::Pass).await?;

    let (status, _, body) = call(
        &rig.app,
        "GET",
        "/api/workspace/export",
        None,
        cookie(&session),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let checks = body["checks"].as_array().cloned().unwrap_or_default();
    let of = |monitor: &Monitor| {
        checks
            .iter()
            .find(|entry| entry["monitor_id"] == json!(monitor.id.0))
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(of(&busy)["truncated"], true);
    assert_eq!(of(&busy)["rows"].as_array().map(Vec::len), Some(cap));
    assert_eq!(of(&quiet)["truncated"], false);
    assert_eq!(of(&quiet)["rows"].as_array().map(Vec::len), Some(1));
    Ok(())
}

/// A right password gives its guess back, so the address budget does not bound a
/// signed-in account that loops the route. Its own small budget does: every pass costs
/// two rounds of password hashing on a pool other people's sign-ins share.
#[tokio::test]
async fn password_changes_have_their_own_small_budget() -> anyhow::Result<()> {
    let rig = rig().await?;
    let session = sign_up(&rig.app, "ada@tenant.example").await;
    let mut current = PASSWORD;
    let mut next = NEW_PASSWORD;
    for round in 0..monitor_server::account::PASSWORD_CHANGES {
        let (status, body) = change(&rig.app, cookie(&session), current, next).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "round {round}: {body}");
        std::mem::swap(&mut current, &mut next);
    }
    let (status, body) = change(&rig.app, cookie(&session), current, next).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "too_many_requests", "{body}");
    // The refused change changed nothing: the password is still the last accepted one.
    assert_eq!(
        sign_in(&rig.app, "ada@tenant.example", current, None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    Ok(())
}
