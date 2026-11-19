//! The management API and the probe coordinator's host.
//!
//! `/livez`, `/readyz` and `GET|POST /api/monitors` for management; `probe` and `schedule`
//! carry the observing loop. Statuses served here come only from the committed-evidence map
//! the writer publishes — never from in-flight probes.

pub mod auth;
pub mod config;
pub mod deliver;
pub mod headers;
pub mod logfile;
pub mod origin;
pub mod probe;
pub mod redact;
pub mod schedule;
pub mod serve;
pub mod snapshot;
pub mod throttle;
pub mod vitals;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::watch;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use monitor_db::{
    CheckRow, Db, IncidentRow, MaintenanceRow, MonitorRow, QueryError, SharedStatuses,
};
use monitor_domain::{
    AccountId, Monitor, MonitorId, MonitorPatch, NewMonitor, Status, ValidationError,
};

/// Management requests are tiny JSON bodies; anything bigger is rejected before parsing.
const BODY_LIMIT: usize = 16 * 1024;
/// Every handler answers inside this; probes run on their own deadlines elsewhere.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The penalty a wrong login token pays (`throttle::PENALTY_SLOTS` at once at most).
const WRONG_TOKEN_DELAY: Duration = Duration::from_millis(300);

/// Wall-clock time for session expiry (D-016). Injected so tests can move it; the server
/// uses [`system_clock`].
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// The request's tenant scope comes from its credential (S1b): the operator sees the
/// default workspace, an account session its own — a monitor in any other workspace is
/// exactly as absent as one that never existed, which is the isolation the API owes
/// (D-020). Routes outside the auth layer (none call `tenant`) have no principal.
fn tenant<'a>(state: &'a AppState, principal: &auth::Principal) -> monitor_db::WorkspaceDb<'a> {
    state.db.workspace(principal.workspace())
}

/// The real clock.
pub fn system_clock() -> Clock {
    Arc::new(OffsetDateTime::now_utc)
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// Committed statuses, published by the writer after each commit.
    pub statuses: SharedStatuses,
    /// The live monitor set the coordinator schedules against. Management writes re-send it.
    pub registry: watch::Sender<Arc<Vec<Monitor>>>,
    /// Engine health: write failures, dead or stalled engine tasks, draining. One verdict
    /// for readiness, the statuses served here and the public snapshot — a monitor that
    /// is not observing must never serve its last statuses as current.
    pub vitals: vitals::Vitals,
    /// The management credential (M5.1). `None` only on a loopback listen — boot refuses
    /// a tokenless public listen.
    pub admin_token: auth::AdminToken,
    /// Dashboard session lifetime (M7.2) — the `expires_at` stamped on each new
    /// `sessions` row and the cookie's `Max-Age`. Store-side expiry is the authority;
    /// the cookie's copy is a convenience for the browser.
    pub session_ttl: Duration,
    /// `SMON_SESSION_IDLE_S`: a session unused for this long ends, inside its lifetime too
    /// (D-016). The dashboard's 15 s refresh keeps an open tab's session in use.
    pub session_idle: Duration,
    /// "Now" for session creation, expiry and idleness.
    pub clock: Clock,
    /// `SMON_COOKIE_SECURE`: when the session cookie carries `Secure` (M7.2 hardening).
    pub cookie_secure: auth::CookieSecure,
    /// `SMON_MAX_MONITORS`: creates past this count are refused (paused monitors count —
    /// they keep history). Bounds the schedule and every per-monitor table (D-012).
    pub max_monitors: u64,
    /// `SMON_TRUSTED_PROXY`: the reverse proxy whose right-most `X-Forwarded-For` entry
    /// names the client for credential throttling (`throttle.rs`). `None`: the TCP peer.
    pub trusted_proxy: Option<std::net::IpAddr>,
    /// `SMON_BACKUP_DIR` (D-019): the only directory `/api/backup` may write, created
    /// owner-only at boot. `None` switches the endpoint off (`backup_disabled`, 404).
    pub backup_dir: Option<std::path::PathBuf>,
    /// `SMON_PROFILE=lab`: local/private targets may be registered (fixtures). Otherwise
    /// a URL the SSRF guard would refuse is refused at registration (D-013).
    pub allow_local_targets: bool,
    pub version: &'static str,
}

/// Re-read the store and publish the monitor set the coordinator schedules against.
/// No receivers (e.g. a test router without a coordinator) is not an error.
pub async fn publish_registry(state: &AppState) -> Result<(), ApiError> {
    let monitors = load_registry(&state.db).await?;
    if state.registry.send(Arc::new(monitors)).is_err() {
        tracing::debug!("no coordinator listening for registry updates");
    }
    Ok(())
}

/// The registry is the stored monitor set converted through the domain — corrupt rows are
/// corruption, not a monitor to skip quietly. The engine sweeps *every* workspace: tenancy
/// scopes what a request can reach, never what the scheduler observes (D-020).
pub async fn load_registry(db: &Db) -> Result<Vec<Monitor>, QueryError> {
    db.list_monitors_engine()
        .await?
        .into_iter()
        .map(Monitor::try_from)
        .collect()
}

/// Boot rebuild: replay each monitor's stored tail through `Health` under its *current*
/// thresholds, seed the status map, and hand the machines to the writer. The rebuild tail
/// is sized from the monitor's own thresholds — a streak can never exceed them.
pub async fn rebuild_health(
    db: &Db,
    monitors: &[Monitor],
    statuses: &SharedStatuses,
) -> Result<std::collections::HashMap<MonitorId, monitor_domain::Health>, QueryError> {
    let mut health = std::collections::HashMap::with_capacity(monitors.len());
    for monitor in monitors {
        let scoped = db.workspace(monitor.workspace);
        let open = scoped.open_incident(monitor.id).await?.is_some();
        let tail = scoped
            .recent_outcomes(monitor.id, monitor.thresholds.rebuild_tail())
            .await?;
        let rebuilt = monitor_domain::Health::rebuild(open, &tail, monitor.thresholds);
        // Boot is single-threaded here: nothing else can have poisoned the map.
        statuses
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(monitor.id, rebuilt.status());
        health.insert(monitor.id, rebuilt);
    }
    Ok(health)
}

/// Restart honesty: while the process was down, slots passed unobserved — but no row said
/// so, so the boot rebuild replayed the last pre-outage observations as if they were
/// current (a stale "up" on the first snapshot) and counted failures before and after the
/// outage as one consecutive streak.
///
/// For every unpaused monitor whose newest stored slot is older than one interval plus the
/// dispatch grace, commit one `missed` row — at the first slot that never ran — through the
/// writer: streaks reset, the status reads `unknown` until a fresh probe, and the gap is
/// part of the record (D-007). One row per gap, not one per lost slot: coverage already
/// counts expected slots from elapsed time, and a catch-up burst is exactly what the
/// schedule refuses elsewhere. Returns how many monitors had a gap.
pub async fn record_downtime(
    writer: &monitor_db::WriterHandle,
    db: &Db,
    monitors: &[Monitor],
    now: OffsetDateTime,
    dispatch_grace: Duration,
) -> Result<usize, monitor_db::WriterError> {
    let mut gaps = 0;
    for monitor in monitors.iter().filter(|monitor| !monitor.paused) {
        let Some(last) = db
            .workspace(monitor.workspace)
            .last_slot_at(monitor.id)
            .await?
        else {
            continue; // never observed: nothing to be stale about
        };
        let interval = time::Duration::try_from(monitor.interval).unwrap_or(time::Duration::MAX);
        let grace = time::Duration::try_from(monitor.interval.min(dispatch_grace))
            .unwrap_or(time::Duration::ZERO);
        if now - last <= interval + grace {
            continue; // the next slot was not yet overdue: no gap
        }
        writer
            .record(monitor_db::Observation {
                monitor_id: monitor.id,
                workspace: monitor.workspace,
                generation: monitor.generation,
                slot_at: last + interval,
                slot: monitor_domain::SlotObservation::Missed,
                http_status: None,
                latency_ms: None,
                detail: None,
            })
            .await?;
        gaps += 1;
    }
    Ok(gaps)
}

/// The management API under its service stack ([`layered`]) — what the in-process tests
/// drive. [`app`] is the same plus the static routes, which is what the server answers.
pub fn router(state: AppState) -> Router {
    layered(routes(state))
}

/// Every API and health route, unlayered. With [`app`], the only place a route is added
/// (`tests/auth_inventory.rs`).
fn routes(state: AppState) -> Router {
    let trusted_proxy = state.trusted_proxy;
    let api = Router::new()
        .route("/monitors", get(list_monitors).post(create_monitor))
        .route(
            "/monitors/{id}",
            get(get_monitor).patch(patch_monitor).delete(delete_monitor),
        )
        .route("/monitors/{id}/checks", get(list_checks))
        .route("/monitors/{id}/stats", get(monitor_stats))
        .route(
            "/monitors/{id}/maintenance",
            get(list_windows).post(create_window),
        )
        .route("/maintenance/{id}", delete(delete_window))
        .route("/incidents", get(list_incidents))
        // Invites are minted inside the authed API — operator-only in the handler.
        .route("/invites", post(create_invite))
        .route("/backup", post(backup))
        // Auth wraps the whole API: /livez, /readyz and static files stay open —
        // the dashboard must load to *hold* a token, and readiness is not a secret.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        // Login/signup/logout live outside the auth layer: a credential can't be
        // required to earn a credential. All enforce their own rules inside.
        .route("/api/login", post(login))
        .route("/api/signup", post(signup))
        .route("/api/logout", post(logout))
        .nest("/api", api)
        .with_state(state)
        .layer(axum::Extension(throttle::Guard::new(trusted_proxy)))
        .layer(axum::Extension(ScheduleLock::default()))
}

/// Serializes the writes that change the schedule (create, edit, pause), so the capacity
/// check and the write it guards can't interleave with another such write (D-018).
#[derive(Clone, Default)]
struct ScheduleLock(Arc<tokio::sync::Mutex<()>>);

/// D-018: refuse a write whose resulting schedule the coordinator can't serve — named,
/// instead of accepted and then silently missed. `change` turns the stored monitors into
/// the proposed set.
async fn check_capacity(
    state: &AppState,
    principal: &auth::Principal,
    change: impl FnOnce(&mut Vec<Monitor>),
) -> Result<(), ApiError> {
    let mut monitors = load_registry(&state.db).await?;
    change(&mut monitors);
    // Per-origin capacity counts resolved addresses (origin.rs): resolve the names not
    // known yet — the one being written above all — within a bound the request can afford.
    origin::refresh(
        monitors.iter().map(|monitor| monitor.url.clone()),
        Duration::from_secs(3),
    )
    .await;
    let config = schedule::Config::default();
    match schedule::capacity(&monitors, &config) {
        None => Ok(()),
        Some(schedule::Overload::Global { required }) => Err(ApiError::Capacity(format!(
            "this schedule would need {required:.0} probes in flight; the monitor runs at most {} \
             at once. Lengthen intervals or shorten timeouts",
            schedule::MAX_GLOBAL_SLOTS
        ))),
        Some(schedule::Overload::Origin { origin, per_tick }) => {
            // The origin name would tell a tenant about another tenant's targets —
            // operators get it, accounts get the shape of the limit instead (D-020).
            let detail = match principal {
                auth::Principal::Operator => {
                    format!("{origin} would need {per_tick:.1} probe starts a second")
                }
                auth::Principal::Account { .. } => {
                    "one origin would exceed its probe rate".to_owned()
                }
            };
            Err(ApiError::Capacity(format!(
                "{detail}; one origin gets at most {}. Lengthen the intervals of its monitors",
                config.per_origin_slots
            )))
        }
    }
}

/// The service stack, as one `ServiceBuilder` — its *first* layer is the outermost
/// (rust-mastery ch. 12): set the request id → propagate it to the response → trace →
/// catch panics → time out → limit bodies. So every response, a 408 or a caught panic
/// included, carries `x-request-id` and passes through the trace. (`Router::layer` calls
/// chain the other way round — the last is outermost — which once left the id innermost.)
fn layered(app: Router) -> Router {
    app.layer(
        tower::ServiceBuilder::new()
            .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(TraceLayer::new_for_http())
            .layer(CatchPanicLayer::new())
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                REQUEST_TIMEOUT,
            ))
            .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT)),
    )
}

/// Everything the server answers: [`router`] plus the public snapshot at `/status.json`
/// (outside `/api`, so public viewers never touch the management surface; expiry is the
/// file's own claim) and the dashboard build as the fallback when it has been built, all
/// under the browser hardening headers (`csp` from [`headers::dashboard_csp`]). This is
/// what `main.rs` serves; every route in the crate is added here or in [`router`]
/// (`tests/auth_inventory.rs` holds the crate to it).
pub fn app(
    state: AppState,
    status_dir: &std::path::Path,
    dashboard_dir: &std::path::Path,
    csp: &str,
) -> Router {
    use tower_http::services::{ServeDir, ServeFile};
    let app = routes(state).route_service(
        "/status.json",
        ServeFile::new(status_dir.join("status.json")),
    );
    let app = if dashboard_dir.is_dir() {
        let index = ServeFile::new(dashboard_dir.join("200.html"));
        app.fallback_service(ServeDir::new(dashboard_dir).fallback(index))
    } else {
        app
    };
    // The static routes get the same stack as the API: ids, trace, timeout.
    headers::harden(layered(app), csp)
}

async fn livez() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "alive" }))
}

/// Which build answers — an operator checks this after an upgrade or rollback, so it
/// reports the binary's own version, never a configured string.
async fn version(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": state.version }))
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    /// Operator sign-in (existing): the admin token. Exactly one of `token` or
    /// (`email`, `password`) must be set — anything else is a malformed request.
    pub token: Option<String>,
    /// Account sign-in (S1b).
    pub email: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SignupBody {
    pub email: String,
    pub password: String,
    /// The invite code the operator handed out — shown once, stored only as a digest.
    pub invite: String,
}

#[derive(Debug, Deserialize)]
pub struct InviteBody {
    /// Hours the code stays usable; default three days, hard bound thirty.
    pub ttl_hours: Option<u64>,
}

/// Turn the admin token into a session: the browser presents the token once, the
/// server answers with an `HttpOnly` cookie whose id is a `sessions` row. The token
/// itself never touches the dashboard's storage — it can be rotated or forgotten
/// without a stray copy in localStorage.
async fn login(
    State(state): State<AppState>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    // Same CSRF rule as the middleware: Origin, when sent, must match the Host.
    let authority = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok());
    if let Some(origin) = headers.get(axum::http::header::ORIGIN)
        && !auth::origin_matches_host(origin, authority)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": { "code": "origin_mismatch", "message": "Origin does not match Host" }
            })),
        )
            .into_response();
    }

    let Some(guard) = extensions.get::<throttle::Guard>() else {
        tracing::error!("invariant: routes() installs the credential guard");
        return ApiError::Corrupt.into_response();
    };
    let attempt = match guard.throttle.admit(guard.client(&extensions, &headers)) {
        Ok(attempt) => attempt,
        Err(wait) => return throttle::too_many(wait),
    };
    let penalties = guard.penalties.clone();
    if let Some(token) = body.token.as_deref() {
        return login_operator(&state, token, &headers, attempt, &penalties).await;
    }
    let (Some(email), Some(password)) = (body.email.as_deref(), body.password.as_deref()) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": { "code": "invalid_request",
                           "message": "send `token`, or `email` and `password`" }
            })),
        )
            .into_response();
    };
    login_account(&state, email, password, &headers, attempt, &penalties).await
}

async fn login_operator(
    state: &AppState,
    token: &str,
    headers: &axum::http::HeaderMap,
    attempt: throttle::Attempt,
    penalties: &std::sync::Arc<tokio::sync::Semaphore>,
) -> Response {
    let Some(admin) = &state.admin_token else {
        // Tokenless dev mode has no credential to exchange — say so plainly.
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": { "code": "auth_disabled",
                           "message": "management auth is not enabled (no SMON_ADMIN_TOKEN)" }
            })),
        )
            .into_response();
    };
    if !auth::digests_match(token.as_bytes(), admin.as_bytes()) {
        drop(attempt); // stays spent
        // A courtesy delay, bounded across all addresses; the buckets bound the rate.
        if let Ok(_penalty) = penalties.try_acquire() {
            tokio::time::sleep(WRONG_TOKEN_DELAY).await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "code": "unauthenticated", "message": "invalid token" }
            })),
        )
            .into_response();
    }
    attempt.succeeded();

    // The cookie gets the id; the store only its token-bound digest (D-016).
    let id = Uuid::new_v4().simple().to_string();
    let now = (state.clock)();
    let expires = now
        + time::Duration::seconds(i64::try_from(state.session_ttl.as_secs()).unwrap_or(i64::MAX));
    if let Err(error) = state
        .db
        .create_session_at(&auth::session_key(admin, &id), now, expires)
        .await
    {
        tracing::error!(%error, "session insert failed");
        return ApiError::Db(error).into_response();
    }
    (
        StatusCode::NO_CONTENT,
        [(
            axum::http::header::SET_COOKIE,
            format!(
                "{}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
                auth::SESSION_COOKIE,
                state.session_ttl.as_secs(),
                state.cookie_secure.attribute(headers),
            ),
        )],
    )
        .into_response()
}

/// Account sign-in (S1b): argon2id verify behind the password-work limit, the same
/// per-address guessing budget, and the same cookie contract as the operator session —
/// but the session row carries the account, so the request resolves its own workspace.
async fn login_account(
    state: &AppState,
    email: &str,
    password: &str,
    headers: &axum::http::HeaderMap,
    attempt: throttle::Attempt,
    penalties: &std::sync::Arc<tokio::sync::Semaphore>,
) -> Response {
    let email = email.trim().to_ascii_lowercase();
    let account = match state.db.account_by_email(&email).await {
        Ok(row) => row,
        Err(error) => return ApiError::Db(error).into_response(),
    };
    // Unknown address verifies against the dummy hash — same work, no existence oracle.
    let hash = account
        .as_ref()
        .map_or(auth::DUMMY_LOGIN_HASH, |row| row.password_hash.as_str());
    if !auth::verify_password(hash, password).await {
        drop(attempt);
        if let Ok(_penalty) = penalties.try_acquire() {
            tokio::time::sleep(WRONG_TOKEN_DELAY).await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "code": "unauthenticated", "message": "invalid credentials" }
            })),
        )
            .into_response();
    }
    let Some(account) = account else {
        // Dummy-hash path passed? A correct guess on a nonexistent address still has
        // no account to sign into — same envelope as a wrong password.
        drop(attempt);
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "code": "unauthenticated", "message": "invalid credentials" }
            })),
        )
            .into_response();
    };
    attempt.succeeded();
    let Ok(account_id) = Uuid::parse_str(&account.id) else {
        return ApiError::Corrupt.into_response();
    };
    let id = Uuid::new_v4().simple().to_string();
    let now = (state.clock)();
    let expires = now
        + time::Duration::seconds(i64::try_from(state.session_ttl.as_secs()).unwrap_or(i64::MAX));
    if let Err(error) = state
        .db
        .create_account_session_at(
            &auth::account_session_key(&id),
            AccountId(account_id),
            now,
            expires,
        )
        .await
    {
        tracing::error!(%error, "account session insert failed");
        return ApiError::Db(error).into_response();
    }
    (
        StatusCode::NO_CONTENT,
        [(
            axum::http::header::SET_COOKIE,
            format!(
                "{}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
                auth::SESSION_COOKIE,
                state.session_ttl.as_secs(),
                state.cookie_secure.attribute(headers),
            ),
        )],
    )
        .into_response()
}

/// Mint an invite code (S1b): the operator asks, the code is shown once in this
/// response, and the store only ever holds its digest — single-use and expiring.
async fn create_invite(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(body): Json<InviteBody>,
) -> Result<impl IntoResponse, ApiError> {
    const MAX_TTL_HOURS: u64 = 24 * 30;
    if !matches!(principal, auth::Principal::Operator) {
        return Err(ApiError::Forbidden(
            "invites are an operator operation".into(),
        ));
    }
    let ttl_hours = body.ttl_hours.unwrap_or(72);
    if ttl_hours == 0 || ttl_hours > MAX_TTL_HOURS {
        return Err(ApiError::Invalid(format!(
            "ttl_hours must be 1..={MAX_TTL_HOURS}"
        )));
    }
    let code = format!("smon_inv_{}", Uuid::new_v4().simple());
    let now = (state.clock)();
    let expires = now + time::Duration::hours(i64::try_from(ttl_hours).unwrap_or(i64::MAX));
    let row = state
        .db
        .create_invite(&auth::invite_key(&code), now, expires)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "invite": { "code": code, "expires_at": row.expires_at }
        })),
    ))
}

/// One 422 shape answer for every malformed signup field.
fn signup_invalid(message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({
            "error": { "code": "invalid_request", "message": message }
        })),
    )
        .into_response()
}

/// One 403 for every unusable code — unknown, spent and expired read the same.
fn invite_denied(attempt: throttle::Attempt) -> Response {
    drop(attempt);
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": { "code": "invalid_invite",
                       "message": "invite code is invalid or expired" }
        })),
    )
        .into_response()
}

/// The field-shape gate before anything is looked up; the normalized email is what
/// persists (lowercase, trimmed — `accounts.email` is UNIQUE on that form). `Err` is
/// the message the 422 carries.
fn check_signup_fields(body: &SignupBody) -> Result<String, &'static str> {
    let email = body.email.trim().to_ascii_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return Err("`email` must be an email address");
    }
    if body.password.len() < 8 || body.password.len() > 256 {
        return Err("`password` must be 8..=256 characters");
    }
    if body.invite.is_empty() || body.invite.len() > 128 {
        return Err("`invite` is required");
    }
    Ok(email)
}

/// The workspace and account a valid signup mints. The slug derives from the email's
/// local part plus account-id entropy, so two signups can't collide on it.
fn new_account(email: &str, password_hash: String) -> monitor_db::NewAccount {
    let local = email.split('@').next().unwrap_or("ws");
    let slug_base: String = local
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(20)
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    let id = AccountId::new();
    monitor_db::NewAccount {
        id,
        workspace: monitor_domain::WorkspaceId::new(),
        workspace_slug: format!(
            "{}-{}",
            if slug_base.is_empty() {
                "ws"
            } else {
                &slug_base
            },
            &id.0.simple().to_string()[..6]
        ),
        workspace_name: email.to_owned(),
        email: email.to_owned(),
        password_hash,
    }
}

/// Invite-gated signup (S1b): a valid, unused, unexpired code mints a workspace and an
/// account in one store transaction. The invite is consumed atomically, so two signups
/// racing one code get one account; unknown, used and expired all read the same 403.
async fn signup(
    State(state): State<AppState>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    Json(body): Json<SignupBody>,
) -> Response {
    // Same CSRF rule as login: Origin, when sent, must match the Host.
    let authority = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok());
    if let Some(origin) = headers.get(axum::http::header::ORIGIN)
        && !auth::origin_matches_host(origin, authority)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": { "code": "origin_mismatch", "message": "Origin does not match Host" }
            })),
        )
            .into_response();
    }
    let Some(guard) = extensions.get::<throttle::Guard>() else {
        tracing::error!("invariant: routes() installs the credential guard");
        return ApiError::Corrupt.into_response();
    };
    let attempt = match guard.throttle.admit(guard.client(&extensions, &headers)) {
        Ok(attempt) => attempt,
        Err(wait) => return throttle::too_many(wait),
    };
    let email = match check_signup_fields(&body) {
        Ok(email) => email,
        Err(message) => {
            drop(attempt);
            return signup_invalid(message);
        }
    };
    let digest = auth::invite_key(&body.invite);
    let invite = match state.db.invite_by_digest(&digest).await {
        Ok(Some(row)) if auth::digests_match(digest.as_bytes(), row.code_digest.as_bytes()) => row,
        Ok(_) => return invite_denied(attempt),
        Err(error) => return ApiError::Db(error).into_response(),
    };
    // Used and expired deny the same as unknown — before the email check can say
    // anything else, so a spent code never reads differently than a forged one.
    if invite.used_at.is_some() || invite.expires_at <= (state.clock)() {
        return invite_denied(attempt);
    }
    // A taken email is told before the invite burns — the invite stays spendable.
    match state.db.account_by_email(&email).await {
        Ok(Some(_)) => {
            drop(attempt);
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": { "code": "conflict", "message": "email is already registered" }
                })),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(error) => return ApiError::Db(error).into_response(),
    }
    let password_hash = match auth::hash_password(&body.password).await {
        Ok(hash) => hash,
        Err(error) => {
            tracing::error!(%error, "password hashing failed");
            return ApiError::Corrupt.into_response();
        }
    };
    let new = new_account(&email, password_hash);
    match state.db.signup(&invite.id, &new, (state.clock)()).await {
        Ok(_) => {
            attempt.succeeded();
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "account": {
                        "id": new.id.0,
                        "email": email,
                        "workspace": new.workspace.0,
                    }
                })),
            )
                .into_response()
        }
        Err(QueryError::Invalid(message))
            if message.contains("email") || message.contains("slug") =>
        {
            drop(attempt);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": { "code": "conflict", "message": message }
                })),
            )
                .into_response()
        }
        Err(QueryError::Invalid(_)) => invite_denied(attempt),
        Err(error) => ApiError::Db(error).into_response(),
    }
}

/// Delete the session row and expire the cookie. Idempotent — clearing an already-gone
/// session answers the same way, so a browser can always reset its own state.
async fn logout(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    // Tokenless dev mode never issues sessions, so there is no row to delete.
    if let Some(id) = auth::session_cookie(headers.get(axum::http::header::COOKIE)) {
        if let Some(admin) = &state.admin_token
            && let Err(error) = state.db.delete_session(&auth::session_key(admin, id)).await
        {
            tracing::error!(%error, "session delete failed");
            return ApiError::Db(error).into_response();
        }
        if let Err(error) = state
            .db
            .delete_session(&auth::account_session_key(id))
            .await
        {
            tracing::error!(%error, "account session delete failed");
            return ApiError::Db(error).into_response();
        }
    }
    (
        StatusCode::NO_CONTENT,
        [(
            axum::http::header::SET_COOKIE,
            format!(
                "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
                auth::SESSION_COOKIE,
                state.cookie_secure.attribute(&headers),
            ),
        )],
    )
        .into_response()
}

/// Readiness means the store answers, the write path lands commits, and every engine task
/// (writer, coordinator, deliverers, snapshot, retention) is running and not stalled. A
/// read can succeed while every write fails (disk full), and the HTTP server can answer
/// while the writer is dead — the vitals verdict is the truth about the engine, never a
/// green that frozen statuses would contradict.
async fn readyz(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    if let Err(error) = state.db.ping().await {
        // Public endpoint: fixed text, the detail in the log only.
        tracing::error!(%error, "readiness: the store does not answer");
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": vitals::Availability::Unavailable,
                "reason": "store unavailable",
                "faults": ["store unavailable"],
            })),
        )
            .into_response());
    }
    let verdict = state.vitals.verdict();
    if verdict.status == vitals::Availability::Degraded {
        // Still observing and serving: ready, with the faults named (D-011). Only a
        // monitor that is not observing is taken out of rotation.
        return Ok(Json(serde_json::json!({
            "status": verdict.status,
            "faults": verdict.faults,
        }))
        .into_response());
    }
    if verdict.status != vitals::Availability::Ok {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": verdict.status,
                "reason": verdict.faults.first(),
                "faults": verdict.faults,
                "consecutive_failures": verdict.consecutive_write_failures,
            })),
        )
            .into_response());
    }
    Ok(Json(serde_json::json!({ "status": "ready" })).into_response())
}

#[derive(Debug, Serialize)]
struct MonitorView {
    id: Uuid,
    name: String,
    url: String,
    interval_s: u64,
    timeout_s: u64,
    failures_to_open: u8,
    passes_to_recover: u8,
    generation: u64,
    paused: bool,
    /// From the committed-evidence map only — a monitor with no committed observations is
    /// `unknown`, and an unwritten result can never appear here.
    status: Status,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// The monitor list carries the engine verdict alongside it (`monitoring`), read once so
/// the statuses and the verdict can't disagree: while the engine is not observing, every
/// status is `unknown` — the last committed value is history, not current evidence.
async fn list_monitors(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let verdict = state.vitals.verdict();
    let rows = tenant(&state, &principal).list_monitors().await?;
    let monitors: Vec<MonitorView> = rows
        .into_iter()
        .map(|row| view_from_row(row, &state.statuses, verdict.observing()))
        .collect::<Result<_, _>>()?;
    Ok(Json(serde_json::json!({
        "monitoring": { "status": verdict.status, "faults": verdict.faults },
        "monitors": monitors,
    })))
}

async fn get_monitor(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<Json<MonitorView>, ApiError> {
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    Ok(Json(view_from_row(
        row,
        &state.statuses,
        state.vitals.verdict().observing(),
    )?))
}

#[derive(Debug, Deserialize)]
pub struct CreateMonitor {
    pub name: String,
    pub url: String,
    pub interval_s: Option<u64>,
    pub timeout_s: Option<u64>,
    /// Optional incident sensitivity — both fall back to the contract's 3/2 when absent.
    pub failures_to_open: Option<u8>,
    pub passes_to_recover: Option<u8>,
}

async fn create_monitor(
    State(state): State<AppState>,
    axum::Extension(lock): axum::Extension<ScheduleLock>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(input): Json<CreateMonitor>,
) -> Result<impl IntoResponse, ApiError> {
    let url = url::Url::parse(&input.url).map_err(|_| ApiError::BadUrl)?;
    let new = match (input.interval_s, input.timeout_s) {
        (None, None) => NewMonitor::new(input.name, url),
        (interval, timeout) => NewMonitor::with_schedule(
            input.name,
            url,
            Duration::from_secs(interval.unwrap_or(monitor_domain::DEFAULT_INTERVAL.as_secs())),
            Duration::from_secs(timeout.unwrap_or(monitor_domain::DEFAULT_TIMEOUT.as_secs())),
        ),
    }
    .map_err(ApiError::Validation)?;
    check_target(&state, &new.url)?;
    let new = match (input.failures_to_open, input.passes_to_recover) {
        (None, None) => new,
        (fail, recover) => new
            .with_thresholds(
                fail.unwrap_or(monitor_domain::DEFAULT_FAILURES_TO_OPEN),
                recover.unwrap_or(monitor_domain::DEFAULT_PASSES_TO_RECOVER),
            )
            .map_err(ApiError::Validation)?,
    };
    let monitor = new.into_monitor_for(principal.workspace());
    let _schedule = lock.0.lock().await;
    check_capacity(&state, &principal, |monitors| {
        monitors.push(monitor.clone());
    })
    .await?;
    if !tenant(&state, &principal)
        .insert_monitor_within(&monitor, state.max_monitors)
        .await?
    {
        return Err(ApiError::LimitReached(state.max_monitors));
    }
    publish_registry(&state).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": monitor.id.0 })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct PatchMonitor {
    pub name: Option<String>,
    pub url: Option<String>,
    pub interval_s: Option<u64>,
    pub timeout_s: Option<u64>,
    pub failures_to_open: Option<u8>,
    pub passes_to_recover: Option<u8>,
    pub paused: Option<bool>,
    /// Optional optimistic-concurrency guard: when present and stale, the write is a
    /// conflict rather than a blind last-writer-wins.
    pub generation: Option<u64>,
}

/// Apply a management write. Any field change bumps `generation` atomically, which is what
/// makes a probe that was already in flight stale — the write and the invalidation are the
/// same committed fact.
async fn patch_monitor(
    State(state): State<AppState>,
    axum::Extension(lock): axum::Extension<ScheduleLock>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
    Json(input): Json<PatchMonitor>,
) -> Result<Json<MonitorView>, ApiError> {
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    if input
        .generation
        .is_some_and(|expected| expected != monitor.generation)
    {
        return Err(ApiError::Conflict(
            "monitor changed since it was read; re-read and retry".into(),
        ));
    }
    let patch = MonitorPatch {
        name: input.name,
        url: input
            .url
            .map(|value| url::Url::parse(&value))
            .transpose()
            .map_err(|_| ApiError::BadUrl)?,
        interval: input.interval_s.map(Duration::from_secs),
        timeout: input.timeout_s.map(Duration::from_secs),
        failures_to_open: input.failures_to_open,
        passes_to_recover: input.passes_to_recover,
        paused: input.paused,
    };
    if let Some(url) = &patch.url {
        check_target(&state, url)?;
    }
    let merged = patch.apply(&monitor).map_err(ApiError::Validation)?;
    let _schedule = lock.0.lock().await;
    check_capacity(&state, &principal, |monitors| {
        for stored in monitors.iter_mut().filter(|stored| stored.id == merged.id) {
            *stored = merged.clone();
        }
    })
    .await?;
    if tenant(&state, &principal)
        .update_monitor(&merged, monitor.generation)
        .await?
        .is_none()
    {
        return Err(ApiError::Conflict(
            "monitor changed since it was read; re-read and retry".into(),
        ));
    }
    publish_registry(&state).await?;
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    Ok(Json(view_from_row(
        row,
        &state.statuses,
        state.vitals.verdict().observing(),
    )?))
}

/// Delete a monitor with no recorded evidence. A monitor with history can't be deleted —
/// checks and incidents are the record; pause stops monitoring without erasing it.
async fn delete_monitor(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    if !tenant(&state, &principal)
        .delete_monitor(monitor.id)
        .await?
    {
        return Err(ApiError::Conflict(
            "monitor has recorded history; pause it instead".into(),
        ));
    }
    publish_registry(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A target the SSRF guard would refuse can never be observed — registering it would only
/// turn the monitor's own refusal into a "down" target and a page. Names are checked again
/// at every probe (the DNS answer can change); this catches literals up front.
fn check_target(state: &AppState, url: &url::Url) -> Result<(), ApiError> {
    let policy = probe::ProbePolicy {
        allow_local_targets: state.allow_local_targets,
        extra_roots: Vec::new(),
    };
    policy.check(url).map_err(|_| ApiError::RefusedTarget)
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
}

fn bounded_limit(limit: Option<i64>, default: i64, cap: i64) -> i64 {
    limit.unwrap_or(default).clamp(1, cap)
}

#[derive(Debug, Serialize)]
struct CheckView {
    #[serde(with = "time::serde::rfc3339")]
    slot_at: OffsetDateTime,
    outcome: String,
    http_status: Option<i64>,
    latency_ms: Option<i64>,
    detail: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    recorded_at: OffsetDateTime,
}

impl From<CheckRow> for CheckView {
    fn from(row: CheckRow) -> Self {
        Self {
            slot_at: row.slot_at,
            outcome: row.outcome,
            http_status: row.http_status,
            latency_ms: row.latency_ms,
            detail: row.detail,
            recorded_at: row.recorded_at,
        }
    }
}

/// Recent slots for a monitor — the evidence strip. Includes `missed` rows: a gap is part
/// of the record, not something the API hides.
async fn list_checks(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let checks = tenant(&state, &principal)
        .list_checks(monitor.id, bounded_limit(params.limit, 50, 500))
        .await?;
    let checks: Vec<CheckView> = checks.into_iter().map(CheckView::from).collect();
    Ok(Json(serde_json::json!({ "checks": checks })))
}

/// Coverage vs success over a recent window — separate numbers, per the contract: coverage
/// is "how much scheduled monitoring actually ran" (observed/expected slots), success is
/// "of what ran, how much passed" (passed/observed). A window of misses is 0% coverage with
/// undefined success — never "down by silence".
#[derive(Debug, Deserialize)]
pub struct StatsParams {
    /// Window in seconds; default 24h, bounded to [60s, 7d].
    pub window_s: Option<u64>,
}

const DEFAULT_STATS_WINDOW: u64 = 86_400;
const MAX_STATS_WINDOW: u64 = 7 * 86_400;

#[derive(Debug, Serialize)]
struct StatsView {
    window_s: u64,
    expected: u64,
    observed: u64,
    missed: u64,
    passed: u64,
    /// Basis points; `null` when the denominator is undefined — "no data" is an answer,
    /// not a number.
    coverage_bps: Option<u32>,
    success_bps: Option<u32>,
}

async fn monitor_stats(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
    Query(params): Query<StatsParams>,
) -> Result<Json<StatsView>, ApiError> {
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    let created_at = row.created_at;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let window_s = params
        .window_s
        .unwrap_or(DEFAULT_STATS_WINDOW)
        .clamp(60, MAX_STATS_WINDOW);
    let now = OffsetDateTime::now_utc();
    let window = time::Duration::seconds(i64::try_from(window_s).unwrap_or(i64::MAX));
    let since = now - window;
    // The window starts at the later of `since` and the monitor's creation — a monitor born
    // an hour ago can't owe 24h of slots.
    let window_start = if created_at > since {
        created_at
    } else {
        since
    };
    let elapsed = u64::try_from((now - window_start).whole_seconds().max(0)).unwrap_or(0);
    let expected = elapsed / monitor.interval.as_secs().max(1);
    let mut counts = tenant(&state, &principal)
        .check_stats(monitor.id, since)
        .await?;
    counts.expected = expected;
    Ok(Json(StatsView {
        window_s,
        expected: counts.expected,
        observed: counts.observed,
        missed: counts.missed,
        passed: counts.passed,
        coverage_bps: counts.coverage_bps(),
        success_bps: counts.success_bps(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct IncidentParams {
    pub monitor_id: Option<String>,
    pub state: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct IncidentView {
    id: i64,
    monitor_id: String,
    #[serde(with = "time::serde::rfc3339")]
    opened_at: OffsetDateTime,
    /// When the streak started — `null` on incidents opened before M6.3.
    #[serde(with = "time::serde::rfc3339::option")]
    first_fail_at: Option<OffsetDateTime>,
    /// Latest failing slot while open — `null` on incidents opened before M6.3.
    #[serde(with = "time::serde::rfc3339::option")]
    last_fail_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    resolved_at: Option<OffsetDateTime>,
    state: String,
}

impl From<IncidentRow> for IncidentView {
    fn from(row: IncidentRow) -> Self {
        Self {
            id: row.id,
            monitor_id: row.monitor_id,
            opened_at: row.opened_at,
            first_fail_at: row.first_fail_at,
            last_fail_at: row.last_fail_at,
            resolved_at: row.resolved_at,
            state: row.state,
        }
    }
}

/// A maintenance window as the API reports it. `active` is computed server-side against
/// the same clock the gate uses — clients never re-derive "is this suppressing now".
#[derive(Debug, Serialize)]
struct MaintenanceView {
    id: i64,
    monitor_id: String,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    ends_at: OffsetDateTime,
    reason: String,
    active: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl MaintenanceView {
    fn from_row(row: MaintenanceRow, now: OffsetDateTime) -> Self {
        Self {
            id: row.id,
            monitor_id: row.monitor_id,
            active: row.starts_at <= now && row.ends_at > now,
            starts_at: row.starts_at,
            ends_at: row.ends_at,
            reason: row.reason,
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateWindow {
    /// RFC 3339 timestamps. A window wholly in the past is accepted — it documents why
    /// checks failed without paging, the same reason resolved incidents are kept.
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Register suppression for a monitor. The gate reads windows per committed observation,
/// so a new window suppresses starting with the *next* slot — never retroactively.
async fn create_window(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
    Json(input): Json<CreateWindow>,
) -> Result<(StatusCode, Json<MaintenanceView>), ApiError> {
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    if input.ends_at <= input.starts_at {
        return Err(ApiError::Invalid("ends_at must be after starts_at".into()));
    }
    let created = tenant(&state, &principal)
        .create_window(
            monitor.id,
            input.starts_at,
            input.ends_at,
            input.reason.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| match e {
            QueryError::Invalid(msg) => ApiError::Invalid(msg),
            other => ApiError::Db(other),
        })?;
    Ok((
        StatusCode::CREATED,
        Json(MaintenanceView::from_row(
            created,
            OffsetDateTime::now_utc(),
        )),
    ))
}

async fn list_windows(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row = find_monitor(&tenant(&state, &principal), &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let now = OffsetDateTime::now_utc();
    let windows: Vec<MaintenanceView> = tenant(&state, &principal)
        .list_windows(monitor.id)
        .await?
        .into_iter()
        .map(|row| MaintenanceView::from_row(row, now))
        .collect();
    Ok(Json(serde_json::json!({ "windows": windows })))
}

/// Cancel a window. An active one resumes alerting on the next slot; deleting a
/// nonexistent window is a 404, not a silent success.
async fn delete_window(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: i64 = id.parse().map_err(|_| ApiError::NotFound)?;
    if !tenant(&state, &principal).delete_window(id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The incident timeline — open and resolved. `?monitor_id=` and `?state=open|resolved`
/// filter; a bogus state value is a bad request, not silently "all".
async fn list_incidents(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Query(params): Query<IncidentParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let monitor_id = params
        .monitor_id
        .map(|value| Uuid::parse_str(&value).map(MonitorId))
        .transpose()
        .map_err(|_| ApiError::BadId)?;
    let state_filter = match params.state.as_deref() {
        None => None,
        Some("open") => Some("open"),
        Some("resolved") => Some("resolved"),
        Some(_) => return Err(ApiError::BadId),
    };
    let incidents = tenant(&state, &principal)
        .list_incidents(
            monitor_id,
            state_filter,
            bounded_limit(params.limit, 50, 500),
        )
        .await?;
    let incidents: Vec<IncidentView> = incidents.into_iter().map(IncidentView::from).collect();
    Ok(Json(serde_json::json!({ "incidents": incidents })))
}

/// The backup file's UTC stamp: `monitor-<stamp>-<random>.db`, second precision —
/// enough for an operator to sort and old enough that a redraw suffix is the uniquifier.
fn backup_stamp(now: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Online snapshot of the store via `VACUUM INTO` (M4.3): the restore drill proves open
/// incidents and pending outbox rows survive the round trip.
///
/// D-019: the file lands in `SMON_BACKUP_DIR` under a server-drawn name; the client
/// supplies nothing but an empty object. The previous contract — any absolute `path` —
/// was a write-anywhere primitive one admin-token leak away. A name collision redraws
/// (the suffix is 32 random bits per draw); after three, something else is wrong.
async fn backup(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The backup holds every workspace's rows — it is operator infrastructure (D-019),
    // never something a tenant credential may pull.
    if !matches!(principal, auth::Principal::Operator) {
        return Err(ApiError::Forbidden(
            "backups are an operator operation".into(),
        ));
    }
    if !input.as_object().is_some_and(serde_json::Map::is_empty) {
        return Err(ApiError::Invalid(
            "POST an empty object — backups land in SMON_BACKUP_DIR under a \
             server-chosen name; a client `path` is refused"
                .into(),
        ));
    }
    let Some(dir) = &state.backup_dir else {
        return Err(ApiError::BackupDisabled);
    };
    let stamp = backup_stamp((state.clock)());
    for _draw in 0..3 {
        let name = format!(
            "monitor-{stamp}-{}.db",
            &Uuid::new_v4().simple().to_string()[..8]
        );
        match state.db.backup(&dir.join(&name)).await {
            Ok(bytes) => {
                return Ok(Json(serde_json::json!({ "name": name, "bytes": bytes })));
            }
            // the name was already taken — redraw
            Err(QueryError::Invalid(_)) => {}
            Err(error) => return Err(ApiError::Db(error)),
        }
    }
    Err(ApiError::Corrupt)
}

async fn find_monitor(
    view: &monitor_db::WorkspaceDb<'_>,
    id: &str,
) -> Result<MonitorRow, ApiError> {
    let id = Uuid::parse_str(id).map_err(|_| ApiError::NotFound)?;
    view.monitor_row(MonitorId(id))
        .await?
        .ok_or(ApiError::NotFound)
}

/// `observing = false` (the engine verdict) forces `unknown`: a status the engine is no
/// longer producing is not a status.
fn view_from_row(
    row: MonitorRow,
    statuses: &SharedStatuses,
    observing: bool,
) -> Result<MonitorView, ApiError> {
    let created_at = row.created_at;
    let monitor = monitor_domain::Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let status = if observing {
        statuses.read().map_or(Status::Unknown, |map| {
            map.get(&monitor.id).copied().unwrap_or(Status::Unknown)
        })
    } else {
        Status::Unknown
    };
    Ok(MonitorView {
        id: monitor.id.0,
        name: monitor.name,
        url: monitor.url.to_string(),
        interval_s: monitor.interval.as_secs(),
        timeout_s: monitor.timeout.as_secs(),
        failures_to_open: monitor.thresholds.failures_to_open,
        passes_to_recover: monitor.thresholds.passes_to_recover,
        generation: monitor.generation,
        paused: monitor.paused,
        status,
        created_at,
    })
}

/// The error envelope: a stable `code` for clients, a safe `message` for humans — internal
/// details stay in the logs.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("monitor not found")]
    NotFound,
    #[error("invalid parameter")]
    BadId,
    #[error("`url` must be a valid absolute URL")]
    BadUrl,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("monitor limit reached ({0}); delete an unused monitor first")]
    LimitReached(u64),
    /// D-018: the schedule, with this write, is more than the coordinator can start.
    #[error("{0}")]
    Capacity(String),
    #[error(
        "the monitor refuses to probe local, private or reserved addresses (SSRF policy); \
         SMON_PROFILE=lab allows them for local fixtures"
    )]
    RefusedTarget,
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// D-019: `SMON_BACKUP_DIR` unset — the endpoint is off, like `auth_disabled` on login.
    #[error("backups are not configured on this server (SMON_BACKUP_DIR unset)")]
    BackupDisabled,
    #[error("stored data violates the contract")]
    Corrupt,
    /// The store failed. Clients get fixed text; the detail (SQLite and sqlx messages:
    /// table names, pool state) goes to the log only.
    #[error("internal error")]
    Db(#[from] QueryError),
}

impl ApiError {
    fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::BadId | Self::BadUrl | Self::Validation(_) | Self::Invalid(_) => {
                "invalid_request"
            }
            Self::Conflict(_) => "conflict",
            Self::Forbidden(_) => "forbidden",
            Self::LimitReached(_) => "limit_reached",
            Self::Capacity(_) => "capacity_exceeded",
            Self::RefusedTarget => "refused_target",
            Self::BackupDisabled => "backup_disabled",
            Self::Corrupt => "corrupt_store",
            Self::Db(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound | Self::BackupDisabled => StatusCode::NOT_FOUND,
            Self::BadId
            | Self::BadUrl
            | Self::Validation(_)
            | Self::Invalid(_)
            | Self::RefusedTarget => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Conflict(_) | Self::LimitReached(_) | Self::Capacity(_) => StatusCode::CONFLICT,
            Self::Corrupt | Self::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status();
        if status.is_server_error() {
            let detail = std::error::Error::source(&self).map(ToString::to_string);
            tracing::error!(code = self.code(), error = %self, ?detail, "request failed");
        }
        let body =
            serde_json::json!({ "error": { "code": self.code(), "message": self.to_string() } });
        (status, Json(body)).into_response()
    }
}
