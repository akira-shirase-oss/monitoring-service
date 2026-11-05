//! The management API and, later, the probe coordinator's host.
//!
//! Scaffold scope: `/livez`, `/readyz` and `GET|POST /api/monitors`. The probe contract's
//! scheduling rules (one attempt per slot, bounded everything, SSRF guards) arrive with the
//! coordinator — nothing here probes yet, so no management route can touch the network.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use monitor_db::{Db, MonitorRow, QueryError};
use monitor_domain::{NewMonitor, Status, ValidationError};

/// Management requests are tiny JSON bodies; anything bigger is rejected before parsing.
const BODY_LIMIT: usize = 16 * 1024;
/// Every handler answers inside this; probes will run on their own deadlines elsewhere.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub version: &'static str,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/monitors", get(list_monitors).post(create_monitor))
        .route("/monitors/{id}", get(get_monitor));

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
    /// No probe history is read yet, so the honest answer is always `unknown`.
    status: Status,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

async fn list_monitors(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let rows = state.db.list_monitors().await?;
    let monitors: Vec<MonitorView> = rows
        .into_iter()
        .map(view_from_row)
        .collect::<Result<_, _>>()?;
    Ok(Json(serde_json::json!({ "monitors": monitors })))
}

async fn get_monitor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<MonitorView>, ApiError> {
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::NotFound)?;
    let monitors = state.db.list_monitors().await?;
    let row = monitors
        .into_iter()
        .find(|row| row.id == id.to_string())
        .ok_or(ApiError::NotFound)?;
    Ok(Json(view_from_row(row)?))
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
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": monitor.id.0 })),
    ))
}

fn view_from_row(row: MonitorRow) -> Result<MonitorView, ApiError> {
    let created_at = row.created_at;
    let monitor = monitor_domain::Monitor::try_from(row).map_err(|_| ApiError::Corrupt)?;
    Ok(MonitorView {
        id: monitor.id.0,
        name: monitor.name,
        url: monitor.url.to_string(),
        interval_s: monitor.interval.as_secs(),
        timeout_s: monitor.timeout.as_secs(),
        generation: monitor.generation,
        paused: monitor.paused,
        status: Status::Unknown,
        created_at,
    })
}

/// The error envelope: a stable `code` for clients, a safe `message` for humans — internal
/// details stay in the logs.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("monitor not found")]
    NotFound,
    #[error("`url` must be a valid absolute URL")]
    BadUrl,
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
            Self::BadUrl | Self::Validation(_) => "invalid_request",
            Self::Corrupt => "corrupt_store",
            Self::Db(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadUrl | Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
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
