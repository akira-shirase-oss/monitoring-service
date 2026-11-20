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
use axum::routing::{delete, get, patch, post};
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
    AccountId, ApiTokenId, Monitor, MonitorId, MonitorPatch, NewMonitor, Plan, Role, Status,
    ValidationError,
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

/// The request's store view comes from its credential *and* role (S1b/S1c): the
/// operator sees the default workspace (or one it picked), an account or API token
/// its own — a monitor in any other workspace is exactly as absent as one that never
/// existed, which is the isolation the API owes (D-020). `viewer_for` is the read
/// view every principal holds; `member_for` adds resource writes (member+ roles);
/// `owner_for` adds roster and token administration (owner, or the operator).
/// A view the role doesn't hold simply doesn't exist — denial by type, not a flag.
fn viewer_for<'a>(state: &'a AppState, principal: &auth::Principal) -> monitor_db::ViewerDb<'a> {
    state.db.viewer(principal.workspace())
}

fn member_for<'a>(
    state: &'a AppState,
    principal: &auth::Principal,
) -> Result<monitor_db::MemberDb<'a>, ApiError> {
    match principal {
        auth::Principal::Operator { .. } => Ok(state.db.member(principal.workspace())),
        auth::Principal::Account { role, .. } | auth::Principal::Token { role, .. }
            if role.at_least(Role::Member) =>
        {
            Ok(state.db.member(principal.workspace()))
        }
        _ => Err(ApiError::Forbidden(
            "a viewer can read this workspace but not change it".into(),
        )),
    }
}

fn owner_for<'a>(
    state: &'a AppState,
    principal: &auth::Principal,
) -> Result<monitor_db::OwnerDb<'a>, ApiError> {
    match principal {
        auth::Principal::Operator { .. } => Ok(state.db.owner(principal.workspace())),
        auth::Principal::Account { role, .. } if role.at_least(Role::Owner) => {
            Ok(state.db.owner(principal.workspace()))
        }
        _ => Err(ApiError::Forbidden(
            "roster and API tokens are owner work".into(),
        )),
    }
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
    /// How long a sign-in or signup may wait for a password-hashing slot before it is
    /// answered 503 `busy` (`auth::PASSWORD_WORK_WAIT` in production). A bounded queue
    /// keeps a credential flood from parking every connection the API has.
    pub password_work_wait: Duration,
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
    /// Pending API-token last-used stamps (S1c): the auth path writes here, the
    /// flusher lands them in one store write — never a write per request.
    pub token_use: auth::TokenUse,
    /// `SMON_STATUS_DIR`: where the public snapshots live — `status.json` plus each
    /// enabled workspace page at `s/<workspace>.json` (S2, D-021). The public route
    /// reads the files; the snapshot writer's heartbeat is their freshness. Files
    /// are named by workspace, not slug: a released slug can then never serve the
    /// previous owner's page (S2 review).
    pub status_dir: std::path::PathBuf,
    /// Enabled-slug → workspace cache for the public route: anonymous hits resolve
    /// the slug and read the file without touching the store (two SQL per hit on a
    /// 4-connection read pool would otherwise be the bottleneck — and uncached
    /// misses would be just as bad under a random-slug flood). `put_status_page`
    /// is the only writer of the mapping it caches, so invalidation lives there.
    pub status_slugs: StatusSlugs,
    /// Bumped on every status-page write; cached slug entries carry it and a
    /// stale version re-resolves — the commit/insert race leaves no stale entry.
    /// `Arc` so every cloned `AppState` shares the one counter.
    pub status_slug_version: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Per-workspace "send test alert" cooldowns (S2): the test is a real network
    /// send, so one per `TEST_SEND_EVERY` bounds what a member can aim at their
    /// receiver. Bounded like `token_use`: workspaces are bounded, and the map
    /// forgets spent cooldowns past a cap.
    pub test_sends: TestSends,
    /// Per-workspace status-page write budgets (S2 hardening): `workspace →
    /// (window start, writes spent)` — a config write can't amplify into a
    /// write loop against the store. Bounded and forgotten like `test_sends`.
    pub status_writes: WritePace,
    /// Per-workspace schedule-write budgets (S2 review): bounds a tenant's
    /// create/edit loop now that DNS waits happen before the schedule lock.
    pub monitor_writes: WritePace,
    /// Per-workspace invitation budgets (S2-4): a roster write can't amplify into
    /// an invitation loop, and the uniform 202 can't be used to enumerate fast.
    pub invite_writes: WritePace,
    /// S3: the in-flight probe share one workspace's schedule may claim — a
    /// create or edit whose own requirement would pass it is refused at write
    /// time (`capacity`), so one tenant's schedule can't crowd the engine.
    pub workspace_slots: usize,
    pub version: &'static str,
}

/// The pending test-alert cooldowns — `workspace → last send`.
pub type TestSends = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<monitor_domain::WorkspaceId, tokio::time::Instant>>,
>;

/// Status-page write budgets — `workspace → (window opened, writes spent)`.
pub type WritePace = std::sync::Arc<
    std::sync::Mutex<
        std::collections::HashMap<monitor_domain::WorkspaceId, (tokio::time::Instant, u32)>,
    >,
>;

/// The public route's slug → workspace resolution (`AppState::status_slugs`), one
/// entry per asked-for slug — `None` is a cached miss (unknown/disabled), so a
/// random-slug flood costs the in-memory map, not the small read pool (S2 review).
/// `version` is [`AppState::status_slug_version`] at insert time; a stale-version
/// entry is a miss, which closes the race where a reader's lookup ran before a
/// PUT committed but its insert landed after the PUT's invalidation.
#[derive(Debug, Clone, Copy)]
pub struct SlugEntry {
    pub workspace: Option<monitor_domain::WorkspaceId>,
    pub version: u64,
}

/// The slug map itself: bounded like every other per-key cache (the cap clears it
/// whole — entries are re-resolved on the next ask).
pub type StatusSlugs =
    std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, SlugEntry>>>;

/// The slug map's bound — a flood of distinct public-slug guesses can't grow it
/// without limit.
pub const STATUS_SLUG_CAP: usize = 4096;

/// Monitors one status page may publish — the allowlist's bound (S2 hardening).
pub const PAGE_MAX_MONITORS: usize = 64;
/// Spend one unit of `workspace`'s write budget: `Err(wait)` when the rolling
/// window is spent — the caller turns it into `ApiError::TooMany`. Validation
/// refusals never reach this; a store refusal spends — that is what the budget
/// is for.
fn spend_write_budget(
    pace: &WritePace,
    workspace: monitor_domain::WorkspaceId,
    budget: u32,
    window: Duration,
    map_cap: usize,
) -> Result<(), Duration> {
    let now = tokio::time::Instant::now();
    let mut writes = pace
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if writes.len() >= map_cap {
        let cutoff = now - window;
        writes.retain(|_, (opened, _)| *opened > cutoff);
    }
    let (opened, spent) = writes.entry(workspace).or_insert((now, 0));
    if now.duration_since(*opened) >= window {
        *opened = now;
        *spent = 0;
    }
    if *spent >= budget {
        return Err(window.saturating_sub(now.duration_since(*opened)));
    }
    *spent += 1;
    Ok(())
}

/// Status-page writes per workspace per rolling window: a few saves are free,
/// a write loop is not (S2 hardening).
pub const STATUS_WRITE_BUDGET: u32 = 6;
/// The rolling window `STATUS_WRITE_BUDGET` spends within.
pub const STATUS_WRITE_WINDOW: Duration = Duration::from_secs(60);
/// Write-budget map bound: past it, quiet entries are forgotten before counting.
const STATUS_WRITE_CAP: usize = 4096;

/// Schedule writes per workspace per rolling window (S2 review): create/edit DNS
/// resolution happens before the schedule lock now, so the budget — not the lock —
/// is what bounds a tenant's write loop.
pub const MONITOR_WRITE_BUDGET: u32 = 12;
/// The rolling window `MONITOR_WRITE_BUDGET` spends within.
pub const MONITOR_WRITE_WINDOW: Duration = Duration::from_secs(60);
/// `monitor_writes` workspaces tracked at once; spent windows are pruned past it.
const MONITOR_WRITE_CAP: usize = 4096;

/// Member invitations per workspace per rolling window (S2 review, S2-4).
pub const INVITE_WRITE_BUDGET: u32 = 6;
/// The rolling window `INVITE_WRITE_BUDGET` spends within.
pub const INVITE_WRITE_WINDOW: Duration = Duration::from_secs(60);
/// `invite_writes` workspaces tracked at once.
const INVITE_WRITE_CAP: usize = 4096;

/// One test alert per workspace this often (S2). A second call inside the window is
/// 429 `too_many_requests` with `Retry-After`.
pub const TEST_SEND_EVERY: Duration = Duration::from_secs(15);
/// The test send's whole deadline — the outbox's own `attempt_deadline`.
pub const TEST_SEND_DEADLINE: Duration = Duration::from_secs(5);
/// Cooldown-map bound: past it, spent entries are forgotten before inserting.
const TEST_SEND_CAP: usize = 4096;

/// Drain the pending token-use stamps into the store in one transaction — the batched
/// counterpart to `AppState::token_use` (S1c). The flusher task calls this on its
/// interval; tests call it directly.
pub async fn flush_token_uses(state: &AppState) -> Result<u64, QueryError> {
    let pending: Vec<(ApiTokenId, OffsetDateTime)> = {
        let mut map = state
            .token_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.drain().collect()
    };
    if pending.is_empty() {
        return Ok(0);
    }
    state.db.touch_api_tokens(&pending).await
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
        // Roster and workspace API tokens (S1c): reads any member, writes owner-only —
        // `member_for`/`owner_for` decide by the request's role, not by route table.
        .route("/members", get(list_members).post(add_member))
        .route("/members/invitations", get(list_workspace_invitations))
        .route("/members/invitations/{id}", delete(revoke_invitation))
        .route(
            "/members/{id}",
            patch(set_member_role).delete(remove_member),
        )
        // Invitations addressed to the caller's account (S2-4): the seat is the
        // invitee's to take. Account work — tokens and the operator get 403.
        .route("/invitations", get(list_invitations))
        .route("/invitations/{id}/accept", post(accept_invitation))
        .route("/invitations/{id}/decline", post(decline_invitation))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", delete(revoke_token))
        // The workspace itself (S2): rename, the memberships the switcher lists, and
        // the public status page's configuration (D-021).
        .route("/workspaces", get(list_workspaces))
        .route("/workspaces/{id}/plan", patch(patch_workspace_plan))
        .route("/workspace", patch(patch_workspace))
        .route(
            "/workspace/status-page",
            get(read_status_page).put(put_status_page),
        )
        // The workspace's own webhook (S1d, D-017): member work — the URL is
        // SSRF-validated at save and revalidated inside every send.
        .route(
            "/workspace/webhook",
            get(read_webhook).put(put_webhook).delete(delete_webhook),
        )
        .route("/workspace/webhook/test", post(test_webhook))
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
        // The public status pages (S2, D-021): no credential — the slug is the page's
        // public identity, the file's own expiry is its freshness, and the per-address
        // throttle bounds what one client can ask for.
        .route("/s/{slug}/status.json", get(public_status))
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
    let before = load_registry(&state.db).await?;
    let mut monitors = before.clone();
    change(&mut monitors);
    // No name is resolved inside this lock (S2 review): the caller resolves the
    // proposed URL above it, so foreign workspaces' monitors answer here from the
    // origin cache — or their own spelling, a stable capacity key either way. A
    // black-hole hostname in another tenant's registry must never hold every
    // writer's lock for 2–3 s.
    let config = schedule::Config::default();
    // Refuse only when *this* change adds load to an overloaded budget (S2
    // review): an origin another tenant already pushed over must not block an
    // unrelated write, and a pause — which only ever frees — is never refused.
    let required_after = schedule::required_slots(monitors.iter(), config.tick);
    if required_after > schedule::slots_f64(schedule::MAX_GLOBAL_SLOTS)
        && required_after > schedule::required_slots(before.iter(), config.tick)
    {
        return Err(ApiError::Capacity(format!(
            "this schedule would need {required_after:.0} probes in flight; the monitor runs at most {} \
             at once. Lengthen intervals or shorten timeouts",
            schedule::MAX_GLOBAL_SLOTS
        )));
    }
    // Every origin the proposed schedule overloads is judged, not just the worst
    // `capacity` would report — a change pushing a *second* origin over its bound
    // must not pass because another origin is already saturated (S2 review).
    let origin_limit = schedule::slots_f64(config.per_origin_slots);
    let before_origins = schedule::per_origin_loads(before.iter(), config.tick);
    let worsened = schedule::per_origin_loads(monitors.iter(), config.tick)
        .into_iter()
        .filter(|(origin, per_tick)| {
            *per_tick > origin_limit
                && *per_tick > before_origins.get(origin).copied().unwrap_or(0.0)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((origin, per_tick)) = worsened {
        // The origin name would tell a tenant about another tenant's targets —
        // operators get it, accounts get the shape of the limit instead (D-020).
        let detail = match principal {
            auth::Principal::Operator { .. } => {
                format!("{origin} would need {per_tick:.1} probe starts a second")
            }
            auth::Principal::Account { .. } | auth::Principal::Token { .. } => {
                "one origin would exceed its probe rate".to_owned()
            }
        };
        return Err(ApiError::Capacity(format!(
            "{detail}; one origin gets at most {}. Lengthen the intervals of its monitors",
            config.per_origin_slots
        )));
    }
    // S3: a workspace's *own* share of the probe budget — named here, at the
    // write, instead of one tenant's schedule quietly crowding the engine's
    // global slots later. Paused monitors cost nothing (`required_slots` skips
    // them), so pausing frees the share — and a schedule already past the cap is
    // refused only when this write makes it worse (S2 review).
    let own = schedule::required_slots(
        monitors
            .iter()
            .filter(|monitor| monitor.workspace == principal.workspace()),
        config.tick,
    );
    if own > schedule::slots_f64(state.workspace_slots)
        && own
            > schedule::required_slots(
                before
                    .iter()
                    .filter(|monitor| monitor.workspace == principal.workspace()),
                config.tick,
            )
    {
        return Err(ApiError::Capacity(format!(
            "this workspace's schedule would need {own:.1} probes in flight; a workspace gets \
             at most {}. Pause monitors, lengthen intervals or shorten timeouts",
            state.workspace_slots
        )));
    }
    Ok(())
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
                state.cookie_secure.name(),
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
    match auth::verify_password(hash, password, state.password_work_wait).await {
        Ok(true) => {}
        Ok(false) => {
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
        Err(auth::PasswordWorkError::Busy) => {
            drop(attempt);
            return password_busy();
        }
        Err(auth::PasswordWorkError::Hash(_)) => {
            unreachable!("verification never hashes")
        }
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
                state.cookie_secure.name(),
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
    if !matches!(principal, auth::Principal::Operator { .. }) {
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

/// The roster as the API shows it — account id, email, role, when they joined.
fn member_view(row: &monitor_db::MemberRow) -> Result<serde_json::Value, ApiError> {
    let account = row.account().ok_or(ApiError::Corrupt)?;
    let role = row.role().ok_or(ApiError::Corrupt)?;
    Ok(serde_json::json!({
        "account": account.0,
        "email": row.email,
        "role": role.as_str(),
        "created_at": row.created_at,
    }))
}

/// Token metadata as the API shows it — never the digest, and the secret itself only
/// ever existed in the mint response.
fn token_view(row: &monitor_db::ApiTokenRow) -> Result<serde_json::Value, ApiError> {
    let token = row.token().ok_or(ApiError::Corrupt)?;
    let role = row.role().ok_or(ApiError::Corrupt)?;
    Ok(serde_json::json!({
        "id": token.0,
        "name": row.name,
        "role": role.as_str(),
        "created_at": row.created_at,
        "revoked_at": row.revoked_at,
        "last_used_at": row.last_used_at,
    }))
}

/// The workspace roster — every member may read it (S1c).
async fn list_members(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let members = viewer_for(&state, &principal)
        .list_members()
        .await?
        .iter()
        .map(member_view)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(serde_json::json!({ "members": members })))
}

#[derive(Debug, Deserialize)]
pub struct MemberBody {
    pub email: Option<String>,
    pub role: Option<Role>,
}

/// Invite `email` to this workspace at `role` — owner work (S2-4). The seat is
/// earned by the invitee's acceptance, never seated by the inviter's say-so: the
/// answer is a uniform 202 whether the address has an account, is already a member,
/// or belongs to no one — nothing about the roster or the directory leaks through
/// the response. The workspace's write budget bounds the asking. Known residual
/// (re-review): an invitation to an address with no account yet can be claimed by
/// whoever registers that address inside the 72 h expiry — the expiry bounds the
/// window, and the owner can withdraw a mistyped ask (`revoke_invitation`).
async fn add_member(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(body): Json<MemberBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = owner_for(&state, &principal)?;
    let (Some(email), Some(role)) = (body.email, body.role) else {
        return Err(ApiError::Invalid("email and role are required".into()));
    };
    let email = email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::Invalid("email must be an email address".into()));
    }
    spend_write_budget(
        &state.invite_writes,
        principal.workspace(),
        INVITE_WRITE_BUDGET,
        INVITE_WRITE_WINDOW,
        INVITE_WRITE_CAP,
    )
    .map_err(ApiError::TooMany)?;
    view.invite_member(&email, role, &principal_label(&principal))
        .await
        .map_err(|e| match e {
            QueryError::Invalid(msg) => ApiError::Invalid(msg),
            other => ApiError::Db(other),
        })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "invited": true })),
    ))
}

/// Who an invitation names as its inviter — an account id, a token id, or the
/// operator; never an email or secret.
fn principal_label(principal: &auth::Principal) -> String {
    match principal {
        auth::Principal::Operator { .. } => "operator".to_owned(),
        auth::Principal::Account { account, .. } => format!("account:{}", account.0),
        auth::Principal::Token { token, .. } => format!("token:{}", token.0),
    }
}

/// The workspace's pending invitations — owner visibility, so the roster view shows
/// who has been asked but not yet seated (S2-4).
async fn list_workspace_invitations(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<impl IntoResponse, ApiError> {
    let view = owner_for(&state, &principal)?;
    let invitations = view
        .list_invitations(time::OffsetDateTime::now_utc())
        .await?;
    Ok(Json(serde_json::json!({
        "invitations": invitations.iter().map(|i| serde_json::json!({
            "id": i.id, "email": i.email, "role": i.role, "created_at": i.created_at,
        })).collect::<Vec<_>>()
    })))
}

/// The account principal behind this request — invitations belong to accounts,
/// never to tokens or the operator (S2-4).
fn account_for(principal: &auth::Principal) -> Result<AccountId, ApiError> {
    match principal {
        auth::Principal::Account { account, .. } => Ok(*account),
        _ => Err(ApiError::Forbidden(
            "invitations belong to accounts, not tokens or the operator".into(),
        )),
    }
}

/// Every invitation addressed to this account's email (S2-4): the workspace's name
/// and the role it offers — the match is on the account's stored email.
async fn list_invitations(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<impl IntoResponse, ApiError> {
    let account = account_for(&principal)?;
    let invitations = state
        .db
        .invitations_for_account(account, time::OffsetDateTime::now_utc())
        .await?;
    Ok(Json(serde_json::json!({
        "invitations": invitations.iter().map(|i| serde_json::json!({
            "id": i.id, "workspace": i.workspace_name, "role": i.role,
            "created_at": i.created_at,
        })).collect::<Vec<_>>()
    })))
}

/// Accept an invitation: it must be addressed to this account's email, and the seat
/// it grants is the invitation's stored role — never anything the request asked for.
/// Unknown and someone-else's read the same 404 (S2-4).
async fn accept_invitation(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let account = account_for(&principal)?;
    let role = state
        .db
        .accept_invitation(&id, account, (state.clock)())
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(serde_json::json!({ "member": { "role": role } })))
}

/// Decline an invitation — same addressing rule as accept, the row is simply gone.
/// Unknown and someone-else's read the same 404 (S2-4).
async fn decline_invitation(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let account = account_for(&principal)?;
    if !state.db.decline_invitation(&id, account).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Withdraw one of this workspace's pending invitations — the owner's undo for a
/// mistyped address or a changed mind before the invitee ever saw it (S3 review).
/// A member can read the roster but only an owner can unsend an ask.
async fn revoke_invitation(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let view = owner_for(&state, &principal)?;
    if !view.revoke_invitation(&id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct MemberRoleBody {
    pub role: Option<Role>,
}

/// Change a member's role. The last-owner rule lives in the store transaction — a
/// demotion that would orphan the workspace comes back a conflict (S1c).
async fn set_member_role(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
    Json(body): Json<MemberRoleBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = owner_for(&state, &principal)?;
    let account = Uuid::parse_str(&id).map_err(|_| ApiError::NotFound)?;
    let Some(role) = body.role else {
        return Err(ApiError::Invalid("role is required".into()));
    };
    let changed = view
        .set_member_role(AccountId(account), role)
        .await
        .map_err(|e| match e {
            QueryError::Invalid(msg) => ApiError::Conflict(msg),
            other => ApiError::Db(other),
        })?;
    if !changed {
        return Err(ApiError::NotFound);
    }
    Ok(Json(
        serde_json::json!({ "member": { "account": account, "role": role.as_str() } }),
    ))
}

/// Take a member's seat away — same last-owner guard as a demotion (S1c).
async fn remove_member(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let view = owner_for(&state, &principal)?;
    let account = Uuid::parse_str(&id).map_err(|_| ApiError::NotFound)?;
    let removed = view
        .remove_member(AccountId(account))
        .await
        .map_err(|e| match e {
            QueryError::Invalid(msg) => ApiError::Conflict(msg),
            other => ApiError::Db(other),
        })?;
    if !removed {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// This workspace's token metadata — owner-only: names and lifetimes are automation
/// intel a member doesn't need (S1c).
async fn list_tokens(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tokens = owner_for(&state, &principal)?
        .list_api_tokens()
        .await?
        .iter()
        .map(token_view)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(serde_json::json!({ "tokens": tokens })))
}

#[derive(Debug, Deserialize)]
pub struct TokenBody {
    pub name: Option<String>,
    pub role: Option<Role>,
}

/// Mint a workspace API token (S1c): the secret is shown once here and kept as a
/// digest — same rule as invites. Tokens hold member or viewer, never owner: a
/// credential must not administer the workspace that issued it.
async fn create_token(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(body): Json<TokenBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = owner_for(&state, &principal)?;
    let (Some(name), Some(role)) = (body.name, body.role) else {
        return Err(ApiError::Invalid("name and role are required".into()));
    };
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(ApiError::Invalid("name must be 1..=80 chars".into()));
    }
    if role == Role::Owner {
        return Err(ApiError::Invalid(
            "tokens hold member or viewer, never owner".into(),
        ));
    }
    let created_by = match principal {
        auth::Principal::Account { account, .. } => Some(account),
        _ => None,
    };
    let secret = format!("smon_wst_{}", Uuid::new_v4().simple());
    let row = view
        .create_api_token(
            &name,
            role,
            &auth::api_token_key(&secret),
            created_by,
            (state.clock)(),
        )
        .await?;
    let mut view_json = token_view(&row)?;
    view_json["secret"] = serde_json::Value::String(secret);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "token": view_json })),
    ))
}

/// Revoke a token: `revoked_at` stamps it dead and the row stays as history (S1c).
async fn revoke_token(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let view = owner_for(&state, &principal)?;
    let token = Uuid::parse_str(&id).map_err(|_| ApiError::NotFound)?;
    if !view
        .revoke_api_token(ApiTokenId(token), (state.clock)())
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The workspace's own webhook as members see it — URL and the signing secret a
/// receiver verifies with. Viewer principals have no method for it (member work).
async fn read_webhook(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let view = member_for(&state, &principal)?;
    let webhook = view
        .webhook()
        .await?
        .map(|row| serde_json::json!({ "url": row.url, "secret": row.secret }));
    Ok(Json(serde_json::json!({ "webhook": webhook })))
}

#[derive(Debug, Deserialize)]
pub struct WebhookBody {
    pub url: String,
}

/// Store a tenant webhook (S1d, D-017): the URL passes the probe policy *and* the
/// validating resolver before it lands — scheme, literal addresses, and every address
/// the name currently resolves to. The send side re-runs the resolver on every
/// attempt, so what is proven public today is re-proven before each POST.
async fn put_webhook(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(body): Json<WebhookBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let view = member_for(&state, &principal)?;
    let url = url::Url::parse(&body.url).map_err(|_| ApiError::BadUrl)?;
    check_webhook_target(&state, &url).await?;
    let row = view.set_webhook(url.as_str()).await?;
    Ok(Json(serde_json::json!({
        "webhook": { "url": row.url, "secret": row.secret }
    })))
}

/// Remove the workspace's webhook — pending `tenant_webhook` rows then fail their
/// next attempt visibly (`no_target`) instead of being silently dropped.
async fn delete_webhook(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<StatusCode, ApiError> {
    let view = member_for(&state, &principal)?;
    view.clear_webhook().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The D-017 save gate: scheme and literal checks, then a bounded DNS resolution —
/// every returned address must be public, the same gate the send-time resolver runs.
/// `Unresolvable` is its own message: a name that cannot resolve is not proof of
/// a public target, but it isn't a policy refusal either.
async fn check_webhook_target(state: &AppState, url: &url::Url) -> Result<(), ApiError> {
    let policy = probe::ProbePolicy {
        allow_local_targets: state.allow_local_targets,
        extra_roots: Vec::new(),
    };
    match policy.check_resolved(url).await {
        Ok(()) => Ok(()),
        Err(probe::Rejected::Unresolvable) => Err(ApiError::Invalid(
            "the webhook host did not resolve to a public address".into(),
        )),
        Err(_) => Err(ApiError::RefusedTarget),
    }
}

/// The caller's workspaces (S2) — the memberships the sidebar switcher lists. Each
/// principal answers for its own claim: an account its `members` rows plus the account
/// identity the menu shows, a token the one workspace it was minted for, the operator
/// the whole inventory at operator level. There is no wider view to ask for.
async fn list_workspaces(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut account = serde_json::Value::Null;
    let workspaces = match principal {
        auth::Principal::Operator { .. } => state
            .db
            .list_workspaces()
            .await?
            .into_iter()
            .map(|w| {
                serde_json::json!({"id": w.id, "slug": w.slug, "name": w.name, "plan": w.plan, "role": "operator"})
            })
            .collect::<Vec<_>>(),
        auth::Principal::Account { account: id, .. } => {
            if let Some(row) = state.db.account_by_id(id).await? {
                account = serde_json::json!({"id": row.id, "email": row.email});
            }
            state
                .db
                .memberships(id)
                .await?
                .into_iter()
                .map(|m| {
                    serde_json::json!({"id": m.id, "slug": m.slug, "name": m.name, "plan": m.plan, "role": m.role})
                })
                .collect()
        }
        auth::Principal::Token {
            workspace, role, ..
        } => state
            .db
            .workspace_row(workspace)
            .await?
            .into_iter()
            .map(|w| {
                serde_json::json!({"id": w.id, "slug": w.slug, "name": w.name, "plan": w.plan, "role": role.as_str()})
            })
            .collect(),
    };
    Ok(Json(
        serde_json::json!({ "workspaces": workspaces, "account": account }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceBody {
    pub name: String,
}

/// Rename the workspace (S2 onboarding step 1) — member work. The signup name is the
/// account's email; the first-run flow names it something its owner recognizes.
async fn patch_workspace(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(body): Json<WorkspaceBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let view = member_for(&state, &principal)?;
    let name = body.name.trim();
    if name.is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
        return Err(ApiError::Invalid(
            "a workspace name is 1-80 visible characters".into(),
        ));
    }
    let row = view.rename_workspace(name).await?;
    Ok(Json(serde_json::json!({
        "workspace": { "id": row.id, "slug": row.slug, "name": row.name }
    })))
}

#[derive(Debug, Deserialize)]
pub struct PlanBody {
    pub plan: String,
}

/// Move a workspace between plan tiers (S3, D-022) — operator work, never tenant
/// work: a member can't lift its own limits. The new tier gates the workspace's
/// next write; nothing already running is paused or deleted by moving it.
async fn patch_workspace_plan(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Path(id): Path<String>,
    Json(body): Json<PlanBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !matches!(principal, auth::Principal::Operator { .. }) {
        return Err(ApiError::Forbidden(
            "only the operator moves a workspace between plans".into(),
        ));
    }
    let workspace = Uuid::parse_str(&id)
        .map(monitor_domain::WorkspaceId)
        .map_err(|_| ApiError::BadId)?;
    let Some(plan) = Plan::parse(body.plan.trim()) else {
        return Err(ApiError::Invalid("plan is `free` or `operator`".into()));
    };
    if !state.db.set_workspace_plan(workspace, plan).await? {
        return Err(ApiError::NotFound);
    }
    let row = state
        .db
        .workspace_row(workspace)
        .await?
        .ok_or(ApiError::Corrupt)?;
    Ok(Json(serde_json::json!({
        "workspace": { "id": row.id, "slug": row.slug, "name": row.name, "plan": row.plan }
    })))
}

/// The status-page config as the editor reads it (S2): `title: null` is the page's
/// off switch; `monitor_ids` is the allowlist in display order.
async fn read_status_page(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let view = viewer_for(&state, &principal);
    let page = view.status_page().await?;
    Ok(Json(serde_json::json!({
        "status_page": {
            "slug": page.slug,
            "title": page.title,
            "monitor_ids": page.monitors.iter().map(|m| &m.monitor_id).collect::<Vec<_>>(),
        }
    })))
}

#[derive(Debug, Deserialize)]
pub struct StatusPageBody {
    /// New public slug — optional; absent keeps the current one.
    pub slug: Option<String>,
    /// `null` or empty turns the page off (the monitor set is kept).
    pub title: Option<String>,
    /// The monitors the owner publishes, in display order.
    pub monitor_ids: Vec<String>,
}

/// The public slug's charset — what `new_account` mints and the file name the writer
/// uses (`s/<slug>.json`); anything else is refused rather than sanitized. `default`
/// is a real slug (the seeded workspace's) — a tenant asking for it answers the same
/// "already taken" conflict as any other owned slug.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
}

/// Write the status page (S2, D-021) — member work. The response is the *effective*
/// config read back, so the client always shows what was stored.
async fn put_status_page(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    Json(body): Json<StatusPageBody>,
) -> Result<Response, ApiError> {
    // Owner-only: the status page is the workspace's public face — what it names and
    // publishes is the same tier of decision as roster and webhook secrets.
    let view = owner_for(&state, &principal)?;
    if let Some(slug) = body.slug.as_deref()
        && !valid_slug(slug)
    {
        return Err(ApiError::Invalid(
            "a slug is 1-64 lowercase letters, digits and hyphens".into(),
        ));
    }
    if let Some(title) = body.title.as_deref()
        && (title.len() > 120 || title.chars().any(char::is_control))
    {
        return Err(ApiError::Invalid(
            "a status page title is at most 120 visible characters".into(),
        ));
    }
    if body.monitor_ids.len() > PAGE_MAX_MONITORS {
        return Err(ApiError::Invalid(format!(
            "a status page publishes at most {PAGE_MAX_MONITORS} monitors"
        )));
    }
    // One row per monitor: the first occurrence wins, so a repeated id is a
    // no-op rather than a `status_monitors` primary-key violation.
    let mut monitors = Vec::with_capacity(body.monitor_ids.len());
    for id in &body.monitor_ids {
        let uuid = Uuid::parse_str(id).map_err(|_| ApiError::BadId)?;
        let id = MonitorId(uuid);
        if !monitors.contains(&id) {
            monitors.push(id);
        }
    }
    // The workspace's write budget — a few saves in a row are free, a write
    // loop against the store is not. Validation refusals never spend: they
    // weren't writes. The budget is spent before the store call, so a store
    // refusal costs it too — that is what the budget is for.
    {
        spend_write_budget(
            &state.status_writes,
            principal.workspace(),
            STATUS_WRITE_BUDGET,
            STATUS_WRITE_WINDOW,
            STATUS_WRITE_CAP,
        )
        .map_err(ApiError::TooMany)?;
    }
    view.set_status_page(
        body.slug.as_deref(),
        body.title.as_deref().map(str::trim),
        &monitors,
    )
    .await
    .map_err(|error| match error {
        QueryError::Invalid(msg) => ApiError::Invalid(msg),
        QueryError::Conflict(msg) => ApiError::Conflict(msg),
        other => ApiError::Db(other),
    })?;
    let page = viewer_for(&state, &principal).status_page().await?;
    // Keep the public route's slug map honest with what just committed: the
    // version bump alone retires every entry a reader may have raced in, then
    // this workspace's one page means dropping every key that resolves to it and
    // caching the current slug when the page is enabled. A disabled page is gone
    // from the map and from disk at once — never served until the writer's next
    // beat notices.
    {
        let ws = principal.workspace();
        let version = state
            .status_slug_version
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        let mut slugs = state
            .status_slugs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slugs.retain(|_, entry| entry.workspace != Some(ws));
        if page.title.is_some() {
            slugs.insert(
                page.slug.clone(),
                SlugEntry {
                    workspace: Some(ws),
                    version,
                },
            );
        } else {
            let _ = std::fs::remove_file(state.status_dir.join("s").join(format!("{}.json", ws.0)));
        }
    }
    Ok(Json(serde_json::json!({
        "status_page": {
            "slug": page.slug,
            "title": page.title,
            "monitor_ids": page.monitors.iter().map(|m| &m.monitor_id).collect::<Vec<_>>(),
        }
    }))
    .into_response())
}

/// "Send test alert" (S2): one real send through the D-017 gate — the same
/// literal/scheme check, a client built from the validating resolver, the
/// workspace's own secret on the signature, the outbox's deadline — answered as a
/// classified result, never an outbox row. One per `TEST_SEND_EVERY` per workspace.
async fn test_webhook(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
) -> Result<Response, ApiError> {
    let view = member_for(&state, &principal)?;
    let Some(webhook) = view.webhook().await? else {
        return Err(ApiError::Invalid("save a webhook URL first".into()));
    };
    // The cooldown buys a real send: a call that can't reach the wire never spends it.
    {
        let now = tokio::time::Instant::now();
        let mut sends = state
            .test_sends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sends.len() >= TEST_SEND_CAP {
            let cutoff = now - TEST_SEND_EVERY;
            sends.retain(|_, last| *last > cutoff);
        }
        if let Some(last) = sends.get(&principal.workspace()) {
            let wait = TEST_SEND_EVERY.saturating_sub(now.duration_since(*last));
            if !wait.is_zero() {
                return Ok((
                    StatusCode::TOO_MANY_REQUESTS,
                    [(
                        axum::http::header::RETRY_AFTER,
                        throttle::retry_after_secs(wait).to_string(),
                    )],
                    Json(serde_json::json!({
                        "error": { "code": "too_many_requests",
                                   "message": "one test alert every 15 seconds; retry shortly" }
                    })),
                )
                    .into_response());
            }
        }
        sends.insert(principal.workspace(), now);
    }
    let target = url::Url::parse(&webhook.url).map_err(|_| ApiError::BadUrl)?;
    let result = deliver::test_send(
        state.allow_local_targets,
        &target,
        Some(webhook.secret.as_bytes()),
        TEST_SEND_DEADLINE,
    )
    .await;
    Ok(Json(serde_json::json!({
        "test": {
            "ok": result.is_ok(),
            "detail": result.as_ref().err().map_or("delivered", String::as_str),
        }
    }))
    .into_response())
}

/// The public status page's payload (S2, D-021): no credential — the slug is the
/// page's public identity. The address spends a token like any other anonymous
/// route; the file's own `expires_at` is the freshness claim, and a missing file
/// answers born-expired so "nothing written" can never read as healthy.
async fn public_status(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(guard) = extensions.get::<throttle::Guard>() else {
        tracing::error!("invariant: routes() installs the credential guard");
        return ApiError::Corrupt.into_response();
    };
    // Anonymous reads spend from their own budget, never the credential one — a
    // read loop can't lock out a sign-in, a guessing flood can't starve readers.
    if let Err(wait) = guard.reads.admit(guard.client(&extensions, &headers)) {
        return throttle::too_many(wait);
    }
    // Every non-beat answer is `no-store`: a shared cache must not keep a 404
    // past the page's enablement, nor an "unavailable" past the outage. The
    // beat file alone is cacheable — and only until the next beat lands; the
    // payload's own `expires_at` stays the freshness claim a reader trusts.
    let unavailable = |code: StatusCode, body: serde_json::Value| {
        (
            code,
            [
                (axum::http::header::CONTENT_TYPE, "application/json"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            body.to_string(),
        )
            .into_response()
    };
    if !valid_slug(&slug) {
        return unavailable(
            StatusCode::NOT_FOUND,
            serde_json::json!({"error": {"code": "not_found", "message": "no status page here"}}),
        );
    }
    // Slug → workspace: the in-memory map first (anonymous hits shouldn't pay SQL
    // on the small read pool). The version is read *before* the store lookup — an
    // entry inserted under a version a PUT has since bumped is a miss, so the
    // commit/insert race can't leave the map serving a name the writer released.
    // A cached `None` answers unknown/disabled without a query at all.
    let version = state
        .status_slug_version
        .load(std::sync::atomic::Ordering::Acquire);
    let workspace = {
        let cached = state
            .status_slugs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&slug)
            .copied();
        match cached {
            Some(entry) if entry.version == version => entry.workspace,
            _ => match state.db.status_page_by_slug(&slug).await {
                Ok(page) => {
                    let workspace = page.map(|page| page.workspace);
                    let mut slugs = state
                        .status_slugs
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if slugs.len() >= STATUS_SLUG_CAP {
                        slugs.clear();
                    }
                    slugs.insert(slug.clone(), SlugEntry { workspace, version });
                    workspace
                }
                Err(error) => return ApiError::Db(error).into_response(),
            },
        }
    };
    let Some(workspace) = workspace else {
        return unavailable(
            StatusCode::NOT_FOUND,
            serde_json::json!({"error": {"code": "not_found", "message": "no status page here"}}),
        );
    };
    // The file is named by workspace: a slug a previous owner released resolves to
    // the new owner's file — never stale content under a recycled name.
    if let Ok(bytes) = std::fs::read(
        state
            .status_dir
            .join("s")
            .join(format!("{}.json", workspace.0)),
    ) {
        return (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "application/json"),
                (axum::http::header::CACHE_CONTROL, "public, max-age=30"),
            ],
            bytes,
        )
            .into_response();
    }
    // Enabled but never written — the writer died or hasn't beaten yet. The
    // answer is the born-expired shape, timestamps RFC3339 like the writer's:
    // a reader that trusts `expires_at` reads "monitoring unavailable".
    let now = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
    unavailable(
        StatusCode::OK,
        serde_json::json!({
            "generated_at": now,
            "expires_at": now,
            "monitoring": "unavailable",
            "services": [],
        }),
    )
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

/// 503 for a full password-work queue: `busy` + Retry-After — the client may retry in
/// a moment; the API is not down, only the hashing slots are.
fn password_busy() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(axum::http::header::RETRY_AFTER, "1")],
        Json(serde_json::json!({
            "error": { "code": "busy",
                       "message": "too many sign-ins in flight; retry in a moment" }
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

/// The workspace and account a valid signup mints. The public slug is random — an
/// email-derived name would tell anyone who can guess an address which page is
/// theirs (S2 review) — and the workspace's name starts neutral, never the email.
fn new_account(email: &str, password_hash: String) -> monitor_db::NewAccount {
    let id = AccountId::new();
    monitor_db::NewAccount {
        id,
        workspace: monitor_domain::WorkspaceId::new(),
        workspace_slug: format!("ws-{}", &Uuid::now_v7().simple().to_string()[20..]),
        workspace_name: "Personal workspace".to_owned(),
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
    let password_hash = match auth::hash_password(&body.password, state.password_work_wait).await {
        Ok(hash) => hash,
        Err(auth::PasswordWorkError::Busy) => {
            drop(attempt);
            return password_busy();
        }
        Err(auth::PasswordWorkError::Hash(error)) => {
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
    if let Some(id) = auth::session_cookie(
        headers.get(axum::http::header::COOKIE),
        state.cookie_secure.name(),
    ) {
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
                state.cookie_secure.name(),
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
    let rows = viewer_for(&state, &principal).list_monitors().await?;
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
    let row = find_monitor(&viewer_for(&state, &principal), &id).await?;
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
    let view = member_for(&state, &principal)?;
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
    let limits = view.plan().await?.limits();
    if new.interval < limits.min_interval {
        return Err(ApiError::PlanLimit(format!(
            "interval {}s is below the {}s minimum on the workspace's plan",
            new.interval.as_secs(),
            limits.min_interval.as_secs()
        )));
    }
    let monitor = new.into_monitor_for(principal.workspace());
    // Everything that can wait or refuse happens BEFORE the schedule lock (S2
    // review): the write budget, the cheap count pre-check, and the candidate's
    // DNS resolution. A tenant refused at the cap — or looping a black-hole name —
    // must never hold the lock everyone's writes queue behind.
    spend_write_budget(
        &state.monitor_writes,
        principal.workspace(),
        MONITOR_WRITE_BUDGET,
        MONITOR_WRITE_WINDOW,
        MONITOR_WRITE_CAP,
    )
    .map_err(ApiError::TooMany)?;
    // The plan caps the workspace's own count first; the server's `SMON_MAX_MONITORS`
    // ceiling still bounds every tier (S3). `insert_monitor_within` re-checks this
    // atomically inside its transaction — the pre-check only keeps the refusal cheap.
    let cap = limits.max_monitors.min(state.max_monitors);
    if view.monitor_count().await? >= cap {
        return Err(ApiError::LimitReached(cap));
    }
    origin::refresh(std::iter::once(monitor.url.clone()), Duration::from_secs(3)).await;
    let _schedule = lock.0.lock().await;
    check_capacity(&state, &principal, |monitors| {
        monitors.push(monitor.clone());
    })
    .await?;
    if !view.insert_monitor_within(&monitor, cap).await? {
        return Err(ApiError::LimitReached(cap));
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
    let view = member_for(&state, &principal)?;
    let row = find_monitor(&view, &id).await?;
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
    // The plan floor gates the proposed interval, not the stored one — a rename or
    // pause on a monitor already below the floor isn't made worse by landing (S3).
    // It sits after `apply`, so an interval below the absolute domain floor stays a
    // 422 whatever the tier: the plan floor only governs the band above it.
    if input.interval_s.is_some() {
        let limits = view.plan().await?.limits();
        if merged.interval < limits.min_interval {
            return Err(ApiError::PlanLimit(format!(
                "interval {}s is below the {}s minimum on the workspace's plan",
                merged.interval.as_secs(),
                limits.min_interval.as_secs()
            )));
        }
    }
    // Same ordering as create (S2 review): the write budget and a changed name's
    // DNS resolution happen before the schedule lock, never under it.
    spend_write_budget(
        &state.monitor_writes,
        principal.workspace(),
        MONITOR_WRITE_BUDGET,
        MONITOR_WRITE_WINDOW,
        MONITOR_WRITE_CAP,
    )
    .map_err(ApiError::TooMany)?;
    origin::refresh(std::iter::once(merged.url.clone()), Duration::from_secs(3)).await;
    let _schedule = lock.0.lock().await;
    check_capacity(&state, &principal, |monitors| {
        for stored in monitors.iter_mut().filter(|stored| stored.id == merged.id) {
            *stored = merged.clone();
        }
    })
    .await?;
    if view
        .update_monitor(&merged, monitor.generation)
        .await?
        .is_none()
    {
        return Err(ApiError::Conflict(
            "monitor changed since it was read; re-read and retry".into(),
        ));
    }
    publish_registry(&state).await?;
    let row = find_monitor(&view, &id).await?;
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
    let view = member_for(&state, &principal)?;
    let row = find_monitor(&view, &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    if !view.delete_monitor(monitor.id).await? {
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
    let row = find_monitor(&viewer_for(&state, &principal), &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let checks = viewer_for(&state, &principal)
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
    let row = find_monitor(&viewer_for(&state, &principal), &id).await?;
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
    let mut counts = viewer_for(&state, &principal)
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
    let view = member_for(&state, &principal)?;
    let row = find_monitor(&view, &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    if input.ends_at <= input.starts_at {
        return Err(ApiError::Invalid("ends_at must be after starts_at".into()));
    }
    let created = view
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
    let row = find_monitor(&viewer_for(&state, &principal), &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let now = OffsetDateTime::now_utc();
    let windows: Vec<MaintenanceView> = viewer_for(&state, &principal)
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
    if !member_for(&state, &principal)?.delete_window(id).await? {
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
    let incidents = viewer_for(&state, &principal)
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
    if !matches!(principal, auth::Principal::Operator { .. }) {
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

async fn find_monitor(view: &monitor_db::ViewerDb<'_>, id: &str) -> Result<MonitorRow, ApiError> {
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
    /// S3: the write crossed the workspace's own plan tier (interval floor today).
    #[error("{0}")]
    PlanLimit(String),
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
    /// A per-workspace write budget is spent (S2 review): the wait until one unit
    /// returns is the `Retry-After`.
    #[error("too many writes from this workspace; retry shortly")]
    TooMany(Duration),
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
            Self::PlanLimit(_) => "plan_limit",
            Self::LimitReached(_) => "limit_reached",
            Self::Capacity(_) => "capacity_exceeded",
            Self::RefusedTarget => "refused_target",
            Self::BackupDisabled => "backup_disabled",
            Self::TooMany(_) => "too_many_requests",
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
            Self::Conflict(_) | Self::PlanLimit(_) | Self::LimitReached(_) | Self::Capacity(_) => {
                StatusCode::CONFLICT
            }
            Self::TooMany(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Corrupt | Self::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        if let Self::TooMany(wait) = self {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(
                    axum::http::header::RETRY_AFTER,
                    throttle::retry_after_secs(wait).to_string(),
                )],
                Json(serde_json::json!({
                    "error": { "code": self.code(), "message": self.to_string() }
                })),
            )
                .into_response();
        }
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
