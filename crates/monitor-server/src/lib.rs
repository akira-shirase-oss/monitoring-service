//! The management API and the probe coordinator's host.
//!
//! `/livez`, `/readyz` and `GET|POST /api/monitors` for management; `probe` and `schedule`
//! carry the observing loop. Statuses served here come only from the committed-evidence map
//! the writer publishes — never from in-flight probes.

pub mod deliver;
pub mod probe;
pub mod schedule;
pub mod snapshot;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
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

use monitor_db::{CheckRow, Db, IncidentRow, MonitorRow, QueryError, SharedStatuses};
use monitor_domain::{Monitor, MonitorId, MonitorPatch, NewMonitor, Status, ValidationError};

/// Management requests are tiny JSON bodies; anything bigger is rejected before parsing.
const BODY_LIMIT: usize = 16 * 1024;
/// Every handler answers inside this; probes run on their own deadlines elsewhere.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// Committed statuses, published by the writer after each commit.
    pub statuses: SharedStatuses,
    /// The live monitor set the coordinator schedules against. Management writes re-send it.
    pub registry: watch::Sender<Arc<Vec<Monitor>>>,
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
/// corruption, not a monitor to skip quietly.
pub async fn load_registry(db: &Db) -> Result<Vec<Monitor>, QueryError> {
    db.list_monitors()
        .await?
        .into_iter()
        .map(Monitor::try_from)
        .collect()
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/monitors", get(list_monitors).post(create_monitor))
        .route(
            "/monitors/{id}",
            get(get_monitor).patch(patch_monitor).delete(delete_monitor),
        )
        .route("/monitors/{id}/checks", get(list_checks))
        .route("/monitors/{id}/stats", get(monitor_stats))
        .route("/incidents", get(list_incidents));

    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .nest("/api", api)
        .with_state(state)
        // Order matters: request id first, then tracing, panic catch, timeout, body limit.
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
}

async fn livez() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "alive" }))
}

/// Readiness means the store answers. If the disk write path has failed, readiness must say so —
/// never paint an unsaved state green.
async fn readyz(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    state.db.ping().await?;
    Ok(Json(serde_json::json!({ "status": "ready" })))
}

#[derive(Debug, Serialize)]
struct MonitorView {
    id: Uuid,
    name: String,
    url: String,
    interval_s: u64,
    timeout_s: u64,
    generation: u64,
    paused: bool,
    /// From the committed-evidence map only — a monitor with no committed observations is
    /// `unknown`, and an unwritten result can never appear here.
    status: Status,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

async fn list_monitors(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let rows = state.db.list_monitors().await?;
    let monitors: Vec<MonitorView> = rows
        .into_iter()
        .map(|row| view_from_row(row, &state.statuses))
        .collect::<Result<_, _>>()?;
    Ok(Json(serde_json::json!({ "monitors": monitors })))
}

async fn get_monitor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<MonitorView>, ApiError> {
    let row = find_monitor(&state, &id).await?;
    Ok(Json(view_from_row(row, &state.statuses)?))
}

#[derive(Debug, Deserialize)]
pub struct CreateMonitor {
    pub name: String,
    pub url: String,
    pub interval_s: Option<u64>,
    pub timeout_s: Option<u64>,
}

async fn create_monitor(
    State(state): State<AppState>,
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
    let monitor = new.into_monitor();
    state.db.insert_monitor(&monitor).await?;
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
    Path(id): Path<String>,
    Json(input): Json<PatchMonitor>,
) -> Result<Json<MonitorView>, ApiError> {
    let row = find_monitor(&state, &id).await?;
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
        paused: input.paused,
    };
    let merged = patch.apply(&monitor).map_err(ApiError::Validation)?;
    if state
        .db
        .update_monitor(&merged, monitor.generation)
        .await?
        .is_none()
    {
        return Err(ApiError::Conflict(
            "monitor changed since it was read; re-read and retry".into(),
        ));
    }
    publish_registry(&state).await?;
    let row = find_monitor(&state, &id).await?;
    Ok(Json(view_from_row(row, &state.statuses)?))
}

/// Delete a monitor with no recorded evidence. A monitor with history can't be deleted —
/// checks and incidents are the record; pause stops monitoring without erasing it.
async fn delete_monitor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let row = find_monitor(&state, &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    if !state.db.delete_monitor(monitor.id).await? {
        return Err(ApiError::Conflict(
            "monitor has recorded history; pause it instead".into(),
        ));
    }
    publish_registry(&state).await?;
    Ok(StatusCode::NO_CONTENT)
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
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row = find_monitor(&state, &id).await?;
    let monitor = Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let checks = state
        .db
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
    Path(id): Path<String>,
    Query(params): Query<StatsParams>,
) -> Result<Json<StatsView>, ApiError> {
    let row = find_monitor(&state, &id).await?;
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
    let mut counts = state.db.check_stats(monitor.id, since).await?;
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
            resolved_at: row.resolved_at,
            state: row.state,
        }
    }
}

/// The incident timeline — open and resolved. `?monitor_id=` and `?state=open|resolved`
/// filter; a bogus state value is a bad request, not silently "all".
async fn list_incidents(
    State(state): State<AppState>,
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
    let incidents = state
        .db
        .list_incidents(
            monitor_id,
            state_filter,
            bounded_limit(params.limit, 50, 500),
        )
        .await?;
    let incidents: Vec<IncidentView> = incidents.into_iter().map(IncidentView::from).collect();
    Ok(Json(serde_json::json!({ "incidents": incidents })))
}

async fn find_monitor(state: &AppState, id: &str) -> Result<MonitorRow, ApiError> {
    let id = Uuid::parse_str(id).map_err(|_| ApiError::NotFound)?;
    state
        .db
        .list_monitors()
        .await?
        .into_iter()
        .find(|row| row.id == id.to_string())
        .ok_or(ApiError::NotFound)
}

fn view_from_row(row: MonitorRow, statuses: &SharedStatuses) -> Result<MonitorView, ApiError> {
    let created_at = row.created_at;
    let monitor = monitor_domain::Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    let status = statuses.read().map_or(Status::Unknown, |map| {
        map.get(&monitor.id).copied().unwrap_or(Status::Unknown)
    });
    Ok(MonitorView {
        id: monitor.id.0,
        name: monitor.name,
        url: monitor.url.to_string(),
        interval_s: monitor.interval.as_secs(),
        timeout_s: monitor.timeout.as_secs(),
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
    Conflict(String),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("stored data violates the contract")]
    Corrupt,
    #[error(transparent)]
    Db(#[from] QueryError),
}

impl ApiError {
    fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::BadId | Self::BadUrl | Self::Validation(_) => "invalid_request",
            Self::Conflict(_) => "conflict",
            Self::Corrupt => "corrupt_store",
            Self::Db(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadId | Self::BadUrl | Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Corrupt | Self::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(code = self.code(), error = %self, "request failed");
        }
        let body =
            serde_json::json!({ "error": { "code": self.code(), "message": self.to_string() } });
        (status, Json(body)).into_response()
    }
}
