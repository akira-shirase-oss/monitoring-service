//! Black-box tests against the real router on an in-memory database.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use monitor_db::Db;
use monitor_domain::WorkspaceId;
use monitor_server::{AppState, router};
use tokio::sync::watch;
use tower::ServiceExt;

/// Builds the router and returns it with the shared pieces tests drive directly.
async fn app() -> anyhow::Result<(
    Router,
    Arc<RwLock<monitor_domain::Statuses>>,
    Db,
    monitor_db::WriteHealth,
    tempfile::TempDir,
)> {
    // A file-backed scratch database: `VACUUM INTO` backups can't run on :memory:, and
    // one pooled connection keeps every test honest anyway.
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("api.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    // The confinement dir a serving boot would have created (D-019).
    let backup_dir = tmp.path().join("backups");
    std::fs::create_dir_all(&backup_dir)?;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let write_health: monitor_db::WriteHealth = Arc::default();
    let (registry, _rx) = watch::channel(Arc::new(Vec::new()));
    let app = router(AppState {
        db: db.clone(),
        statuses: Arc::clone(&statuses),
        registry,
        vitals: monitor_server::vitals::Vitals::new(
            Arc::clone(&write_health),
            Duration::from_secs(30),
        ),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: Some(backup_dir),
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    });
    Ok((app, statuses, db, write_health, tmp))
}

async fn json_body(response: axum::response::Response) -> anyhow::Result<serde_json::Value> {
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::test]
async fn livez_and_readyz_answer() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    for path in ["/livez", "/readyz"] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

/// Disk guardrail (M4.2): while the writer reports consecutive store-write failures, a
/// readable store still isn't ready — readiness is a real 503, never a green lie.
#[tokio::test]
async fn readyz_reports_unavailable_when_writes_fail() {
    let (app, _statuses, _db, write_health, _tmp) = app().await.unwrap();
    write_health.store(2, std::sync::atomic::Ordering::Relaxed);
    let response = app
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await.unwrap();
    assert_eq!(body["status"], "unavailable");
    assert_eq!(body["consecutive_failures"], 2);
}

#[tokio::test]
async fn create_then_list_a_monitor() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/monitors")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "example",
                        "url": "https://example.com/health"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = app
        .oneshot(Request::get("/api/monitors").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = json_body(response).await.unwrap();
    let monitors = body["monitors"].as_array().unwrap();
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0]["name"], "example");
    // No probe has run; the only honest status is unknown.
    assert_eq!(monitors[0]["status"], "unknown");
}

/// M6.2: thresholds ride the same create/patch contract — defaults are 3/2, explicit values
/// are validated, and an invalid threshold is a `validation` error, never a stored monitor.
#[tokio::test]
async fn thresholds_are_validated_on_create_and_patch() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();

    // Explicit thresholds on create round-trip through the view.
    let (status, body) = send(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({
            "name": "strict", "url": "https://example.com/health",
            "failures_to_open": 1, "passes_to_recover": 4
        })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::CREATED);
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, view) = send(&app, "GET", &format!("/api/monitors/{id}"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["failures_to_open"], 1);
    assert_eq!(view["passes_to_recover"], 4);

    // Defaults stay 3/2.
    let id2 = create(&app, "defaulted").await.unwrap();
    let (_, view) = send(&app, "GET", &format!("/api/monitors/{id2}"), None)
        .await
        .unwrap();
    assert_eq!(view["failures_to_open"], 3);
    assert_eq!(view["passes_to_recover"], 2);

    // Out-of-range values are rejected on both create and patch.
    for (method, path) in [
        ("POST", "/api/monitors".to_owned()),
        ("PATCH", format!("/api/monitors/{id}")),
    ] {
        let payload = if method == "POST" {
            serde_json::json!({"name": "bad", "url": "https://x.example.com", "failures_to_open": 0})
        } else {
            serde_json::json!({"passes_to_recover": 0})
        };
        let (status, body) = send(&app, method, &path, Some(payload)).await.unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{method} {path}");
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    // A valid patch merges: change one threshold, keep the other.
    let (status, view) = send(
        &app,
        "PATCH",
        &format!("/api/monitors/{id}"),
        Some(serde_json::json!({"failures_to_open": 2})),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["failures_to_open"], 2);
    assert_eq!(view["passes_to_recover"], 4);
}

/// M6.11: windows CRUD through the real router — `active` is computed server-side,
/// backwards windows are `invalid_request`, and deleting tells 204 from 404 honestly.
#[tokio::test]
async fn maintenance_windows_create_list_and_cancel() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "windowed").await.unwrap();
    let now = time::OffsetDateTime::now_utc();
    let rfc = |t: time::OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };

    // Backwards window → invalid_request, never stored.
    let (status, body) = send(
        &app,
        "POST",
        &format!("/api/monitors/{id}/maintenance"),
        Some(serde_json::json!({
            "starts_at": rfc(now + time::Duration::hours(2)),
            "ends_at": rfc(now),
        })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_request");

    // An active window round-trips with `active: true`.
    let (status, body) = send(
        &app,
        "POST",
        &format!("/api/monitors/{id}/maintenance"),
        Some(serde_json::json!({
            "starts_at": rfc(now - time::Duration::minutes(1)),
            "ends_at": rfc(now + time::Duration::hours(2)),
            "reason": "kernel rebuild",
        })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["active"], true);
    assert_eq!(body["reason"], "kernel rebuild");
    let window_id = body["id"].as_i64().unwrap();

    // A past window lists with `active: false` — records are kept, not hidden.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/monitors/{id}/maintenance"),
        Some(serde_json::json!({
            "starts_at": rfc(now - time::Duration::hours(3)),
            "ends_at": rfc(now - time::Duration::hours(1)),
        })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = send(
        &app,
        "GET",
        &format!("/api/monitors/{id}/maintenance"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    let windows = body["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2);
    let actives: Vec<bool> = windows
        .iter()
        .map(|w| w["active"].as_bool().unwrap())
        .collect();
    assert!(actives.contains(&true) && actives.contains(&false));

    // Windows on an unknown monitor are 404, as is deleting a nonexistent window.
    let (status, _) = send(
        &app,
        "POST",
        "/api/monitors/00000000-0000-0000-0000-000000000000/maintenance",
        Some(serde_json::json!({
            "starts_at": rfc(now),
            "ends_at": rfc(now + time::Duration::hours(1)),
        })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "DELETE", "/api/maintenance/99999", None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Cancel the active window: 204, and the list shows it gone.
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/api/maintenance/{window_id}"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = send(
        &app,
        "GET",
        &format!("/api/monitors/{id}/maintenance"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(body["windows"].as_array().unwrap().len(), 1);
}

/// Create a monitor and return its id.
async fn create(app: &Router, name: &str) -> anyhow::Result<String> {
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/monitors")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "name": name,
                    "url": "https://example.com/health"
                }))?))?,
        )
        .await?;
    let body = json_body(response).await?;
    Ok(body["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("create returned no id"))?
        .to_owned())
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    payload: Option<serde_json::Value>,
) -> anyhow::Result<(StatusCode, serde_json::Value)> {
    let mut builder = match method {
        "GET" => Request::get(path),
        "PATCH" => Request::patch(path),
        "DELETE" => Request::delete(path),
        _ => Request::post(path),
    };
    if payload.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = payload.map_or_else(Body::empty, |v| {
        Body::from(serde_json::to_vec(&v).unwrap_or_default())
    });
    let response = app.clone().oneshot(builder.body(body)?).await?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok((status, body))
}

#[tokio::test]
async fn patch_edits_a_monitor_and_bumps_generation() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "before").await.unwrap();

    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/monitors/{id}"),
        Some(serde_json::json!({ "name": "after", "paused": true })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "after");
    assert_eq!(body["paused"], true);
    assert_eq!(
        body["generation"], 2,
        "any management write bumps generation"
    );

    // A stale-generation guard is a conflict, not a blind last-writer-wins.
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/monitors/{id}"),
        Some(serde_json::json!({ "name": "third", "generation": 1 })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "conflict");

    // The name did not change — the conflicted write did not apply.
    let (status, body) = send(&app, "GET", &format!("/api/monitors/{id}"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "after");
}

#[tokio::test]
async fn patch_revalidates_against_create_rules() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "m").await.unwrap();

    for payload in [
        serde_json::json!({ "name": "   " }),
        serde_json::json!({ "url": "ftp://example.com" }),
        serde_json::json!({ "url": "not a url" }),
        serde_json::json!({ "interval_s": 5 }),
        serde_json::json!({ "timeout_s": 30, "interval_s": 20 }),
    ] {
        let (status, body) = send(&app, "PATCH", &format!("/api/monitors/{id}"), Some(payload))
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{status} for {body}"
        );
    }
}

#[tokio::test]
async fn patch_and_delete_on_missing_or_malformed_monitors() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let missing = uuid::Uuid::now_v7();

    for path in [
        format!("/api/monitors/{missing}"),
        "/api/monitors/not-a-uuid".to_owned(),
    ] {
        for method in ["PATCH", "DELETE"] {
            let (status, body) = send(
                &app,
                method,
                &path,
                (method == "PATCH").then(|| serde_json::json!({ "paused": true })),
            )
            .await
            .unwrap();
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
            assert_eq!(body["error"]["code"], "not_found");
        }
    }
    // Malformed ids fail the same way on the nested routes.
    let (status, _body) = send(&app, "GET", "/api/monitors/nope/checks", None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_removes_a_monitor_without_history() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "ephemeral").await.unwrap();

    let (status, _body) = send(&app, "DELETE", &format!("/api/monitors/{id}"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _body) = send(&app, "GET", &format!("/api/monitors/{id}"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Seed committed checks for the monitor through one writer — the writer owns the `Health`
/// machine, so a streak can only accumulate while the same writer lives. Returns the handle
/// plus task so the caller keeps them alive for the test's duration.
fn seed_writer(
    db: &Db,
    statuses: &Arc<RwLock<monitor_domain::Statuses>>,
) -> monitor_db::writer::WriterHandle {
    let (writer, _task) = monitor_db::writer::spawn(
        db.clone(),
        Arc::clone(statuses),
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    // The JoinHandle detaches on drop; the writer lives as long as the handle is used.
    writer
}

async fn seed_check(
    db: &Db,
    writer: &monitor_db::writer::WriterHandle,
    id: &str,
    outcome: monitor_domain::CheckOutcome,
) -> anyhow::Result<monitor_db::WriteOutcome> {
    let monitor_id = monitor_domain::MonitorId(uuid::Uuid::parse_str(id)?);
    let generation = db
        .workspace(WorkspaceId::DEFAULT)
        .monitor_generation(monitor_id)
        .await?
        .unwrap_or(1);
    Ok(writer
        .record(monitor_db::Observation {
            monitor_id,
            workspace: WorkspaceId::DEFAULT,
            generation,
            slot_at: time::OffsetDateTime::now_utc(),
            slot: monitor_domain::SlotObservation::Probe(outcome),
            http_status: None,
            latency_ms: Some(10),
            detail: None,
        })
        .await?)
}

#[tokio::test]
async fn a_monitor_with_history_cannot_be_deleted() {
    let (app, statuses, db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "keeper").await.unwrap();
    let writer = seed_writer(&db, &statuses);
    seed_check(&db, &writer, &id, monitor_domain::CheckOutcome::Pass)
        .await
        .unwrap();

    let (status, body) = send(&app, "DELETE", &format!("/api/monitors/{id}"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "conflict");

    // The record survives — evidence is not erasable through the API.
    let (status, _body) = send(&app, "GET", &format!("/api/monitors/{id}"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn checks_and_incidents_endpoints_answer() {
    let (app, statuses, db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "watched").await.unwrap();
    let writer = seed_writer(&db, &statuses);
    for _ in 0..3 {
        seed_check(&db, &writer, &id, monitor_domain::CheckOutcome::Transport)
            .await
            .unwrap();
    }

    // The checks strip carries the recorded evidence, newest first.
    let (status, body) = send(&app, "GET", &format!("/api/monitors/{id}/checks"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    let checks = body["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|c| c["outcome"] == "transport"));

    // Three transport failures = one open incident, visible both filtered and unfiltered.
    let (status, body) = send(&app, "GET", "/api/incidents", None).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let incidents = body["incidents"].as_array().unwrap();
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0]["state"], "open");
    assert_eq!(incidents[0]["monitor_id"], id);

    let (status, body) = send(
        &app,
        "GET",
        &format!("/api/incidents?monitor_id={id}&state=open"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["incidents"].as_array().unwrap().len(), 1);

    let (_status, body) = send(
        &app,
        "GET",
        &format!("/api/incidents?monitor_id={id}&state=resolved"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(body["incidents"].as_array().unwrap().len(), 0);

    // A bogus state filter is a bad request, not silently "all".
    let (status, body) = send(&app, "GET", "/api/incidents?state=bogus", None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn a_stale_observation_after_edit_is_dropped() {
    let (app, statuses, db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "stale-check").await.unwrap();
    let monitor_id = monitor_domain::MonitorId(uuid::Uuid::parse_str(&id).unwrap());

    // Edit bumps generation 1 -> 2.
    let (status, _body) = send(
        &app,
        "PATCH",
        &format!("/api/monitors/{id}"),
        Some(serde_json::json!({ "interval_s": 30 })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);

    // An observation claimed under generation 1 arrives late: the writer must drop it.
    let (writer, _task) = monitor_db::writer::spawn(
        db.clone(),
        statuses,
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    let outcome = writer
        .record(monitor_db::Observation {
            monitor_id,
            workspace: WorkspaceId::DEFAULT,
            generation: 1,
            slot_at: time::OffsetDateTime::now_utc(),
            slot: monitor_domain::SlotObservation::Probe(monitor_domain::CheckOutcome::Pass),
            http_status: Some(200),
            latency_ms: Some(10),
            detail: None,
        })
        .await
        .unwrap();
    assert_eq!(outcome, monitor_db::WriteOutcome::DroppedStaleGeneration);
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn stats_separates_coverage_from_success() {
    let (app, statuses, db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "stats").await.unwrap();
    let writer = seed_writer(&db, &statuses);
    let monitor_id = monitor_domain::MonitorId(uuid::Uuid::parse_str(&id).unwrap());

    // In-window rows: two passes, one transport failure, one missed slot.
    for outcome in [
        monitor_domain::CheckOutcome::Pass,
        monitor_domain::CheckOutcome::Pass,
        monitor_domain::CheckOutcome::Transport,
    ] {
        seed_check(&db, &writer, &id, outcome).await.unwrap();
    }
    let generation = db
        .workspace(WorkspaceId::DEFAULT)
        .monitor_generation(monitor_id)
        .await
        .unwrap()
        .unwrap_or(1);
    writer
        .record(monitor_db::Observation {
            monitor_id,
            workspace: WorkspaceId::DEFAULT,
            generation,
            slot_at: time::OffsetDateTime::now_utc(),
            slot: monitor_domain::SlotObservation::Missed,
            http_status: None,
            latency_ms: None,
            detail: None,
        })
        .await
        .unwrap();

    let (status, body) = send(
        &app,
        "GET",
        &format!("/api/monitors/{id}/stats?window_s=3600"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["observed"], 3);
    assert_eq!(body["missed"], 1);
    assert_eq!(body["passed"], 2);
    // Success = passed/observed = 2/3; missed slots never enter the denominator.
    assert_eq!(body["success_bps"], 6_666);
    // The monitor was created moments ago — the window owes ~0 slots, so coverage is
    // honestly undefined (null), never 0% or 100% guessed.
    assert!(body["coverage_bps"].is_null(), "{body}");

    // Window bounds: too-large windows clamp rather than scan unbounded history.
    let (_status, body) = send(
        &app,
        "GET",
        &format!("/api/monitors/{id}/stats?window_s=99999999"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(body["window_s"], 7 * 86_400);

    // Missing monitor still gets the envelope.
    let missing = uuid::Uuid::now_v7();
    let (status, body) = send(&app, "GET", &format!("/api/monitors/{missing}/stats"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn invalid_input_gets_an_error_envelope() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let response = app
        .oneshot(
            Request::post("/api/monitors")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "bad",
                        "url": "ftp://example.com"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(response).await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_request");
}

/// Backup endpoint (M4.3, D-019): `{}` asks for a snapshot; it lands inside the
/// configured `SMON_BACKUP_DIR` under a server-drawn name, owner-only like the store.
#[tokio::test]
async fn backup_writes_a_server_named_snapshot_into_the_configured_dir() -> anyhow::Result<()> {
    let (app, _statuses, _db, _wh, tmp) = app().await?;
    let (status, body) = send(&app, "POST", "/api/backup", Some(serde_json::json!({}))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let name = body["name"].as_str().unwrap_or_default();
    assert!(
        name.starts_with("monitor-")
            && std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("db")),
        "{name}"
    );
    let file = tmp.path().join("backups").join(name);
    assert!(file.exists(), "{name} not in the backup dir");
    assert_eq!(
        std::fs::metadata(&file)?.len(),
        body["bytes"].as_u64().unwrap_or_default()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file)?.permissions().mode() & 0o777,
            0o600
        );
    }
    Ok(())
}

/// Two backups stamped in the same second still land as two files — the random suffix
/// draws distinct names, and `create_new` turns a collision into a redraw, never an
/// overwrite.
#[tokio::test]
async fn two_backups_in_one_second_get_distinct_names() -> anyhow::Result<()> {
    let (_app, statuses, db, write_health, tmp) = app().await?;
    // A frozen clock forces the same second stamp: the suffix is what must differ.
    let stamp = time::OffsetDateTime::from_unix_timestamp(1_763_000_000)?;
    let app = monitor_server::router(monitor_server::AppState {
        db,
        statuses,
        registry: watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(write_health, Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: std::env::temp_dir(),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: Arc::new(move || stamp),
        trusted_proxy: None,
        backup_dir: Some(tmp.path().join("backups")),
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    });
    let mut names = std::collections::HashSet::new();
    for _ in 0..2 {
        let (status, body) = send(&app, "POST", "/api/backup", Some(serde_json::json!({}))).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        names.insert(body["name"].as_str().unwrap_or_default().to_owned());
    }
    assert_eq!(names.len(), 2, "same-stamp backups collided");
    assert_eq!(
        std::fs::read_dir(tmp.path().join("backups"))?.count(),
        2,
        "one backup overwrote the other"
    );
    Ok(())
}

/// Without `SMON_BACKUP_DIR` the endpoint is off — 404 `backup_disabled`, like
/// `auth_disabled` on login, never a write into an implied default directory.
#[tokio::test]
async fn backup_is_disabled_without_a_configured_dir() -> anyhow::Result<()> {
    let (_app, statuses, db, write_health, _tmp) = app().await?;
    let app = monitor_server::router(monitor_server::AppState {
        db,
        statuses,
        registry: watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(write_health, Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: std::env::temp_dir(),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
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
    let (status, body) = send(&app, "POST", "/api/backup", Some(serde_json::json!({}))).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "backup_disabled");
    Ok(())
}

/// D-019: where a backup lands is the server's choice — `SMON_BACKUP_DIR` confines it
/// and the server names the file. A client-supplied `path` (or any other body field)
/// is a refusal, never an instruction: the old contract was a write-anywhere primitive
/// one admin-token leak away.
#[tokio::test]
async fn a_client_supplied_backup_path_is_refused() -> anyhow::Result<()> {
    let (app, _statuses, _db, _wh, _tmp) = app().await?;
    let out = tempfile::tempdir()?;
    for body in [
        serde_json::json!({ "path": out.path().join("stolen.db").display().to_string() }),
        serde_json::json!({ "path": "../escape.db" }),
        serde_json::json!({ "path": "relative.db" }),
        serde_json::json!({ "path": "/tmp/smon-trap.db", "name": "x" }),
        serde_json::json!({ "unrelated": true }),
        serde_json::json!("backup please"),
    ] {
        let (status, envelope) = send(&app, "POST", "/api/backup", Some(body.clone())).await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(envelope["error"]["code"], "invalid_request", "{body}");
    }
    assert_eq!(
        std::fs::read_dir(out.path())?.count(),
        0,
        "a refused backup was written"
    );
    Ok(())
}

/// Management auth (M5.1): with a token configured, every /api route denies a missing,
/// wrong, or malformed credential — and accepts exactly `Bearer <token>`. /livez and
/// /readyz stay open: readiness is not a secret.
#[tokio::test]
async fn auth_denies_without_a_valid_bearer_token() {
    let (_app, statuses, db, write_health, _tmp) = app().await.unwrap();
    // Rebuild the router with a token configured — `app()` runs the dev (None) path.
    let state = monitor_server::AppState {
        db,
        statuses,
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(write_health, Duration::from_secs(30)),
        admin_token: Some(Arc::from("s3cret")),
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: std::env::temp_dir(),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
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
    };
    let app = monitor_server::router(state);

    for headers in [
        vec![],
        vec![("authorization", "Bearer wrong")],
        vec![("authorization", "Bearer s3cret-extra")],
        vec![("authorization", "bearer s3cret")],
        vec![("authorization", "s3cret")],
    ] {
        let mut request = Request::get("/api/monitors");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/monitors")
                .header("authorization", "Bearer s3cret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Health stays open — probes and orchestrators don't carry admin credentials.
    for path in ["/livez", "/readyz"] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

/// Builds an authed router (admin token + configurable session ttl) on a scratch store —
/// the M7.2 session tests all share this rig.
async fn authed(token: &str, ttl: Duration) -> anyhow::Result<(Router, Db, tempfile::TempDir)> {
    authed_with(token, ttl, monitor_server::auth::CookieSecure::Auto).await
}

async fn authed_with(
    token: &str,
    ttl: Duration,
    cookie_secure: monitor_server::auth::CookieSecure,
) -> anyhow::Result<(Router, Db, tempfile::TempDir)> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("auth.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let state = monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: Some(Arc::from(token)),
        session_ttl: ttl,
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    };
    Ok((monitor_server::router(state), db, tmp))
}

/// The Set-Cookie value from a login response — the assertions all key off it.
fn session_cookie_from(response: &axum::response::Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next().map(str::trim).map(str::to_owned))
        .unwrap_or_else(|| panic!("login must set the session cookie"))
}

/// M7.2: a correct token earns an `HttpOnly` session cookie; the cookie alone authorizes
/// reads. The token itself never leaves the login request.
#[tokio::test]
async fn login_issues_an_http_only_cookie_and_the_cookie_authorizes() {
    let (app, _db, _tmp) = authed("s3cret", Duration::from_secs(3600)).await.unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .header("host", "127.0.0.1:58400")
                .header("origin", "http://127.0.0.1:58400")
                .body(Body::from(r#"{"token":"s3cret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains("Max-Age=3600"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/"), "{set_cookie}");

    let cookie = session_cookie_from(&response);
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/monitors")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the session cookie authorizes a read"
    );
}

#[tokio::test]
async fn a_wrong_token_gets_401_and_no_cookie() {
    let (app, _db, _tmp) = authed("s3cret", Duration::from_secs(3600)).await.unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("set-cookie").is_none());
    let body = json_body(response).await.unwrap();
    assert_eq!(body["error"]["code"], "unauthenticated");
}

#[tokio::test]
async fn login_with_auth_disabled_says_so() {
    let (app, _s, _d, _w, _tmp) = app().await.unwrap(); // dev-mode: no admin token
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"anything"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await.unwrap();
    assert_eq!(body["error"]["code"], "auth_disabled");
}

/// Cookie-authed mutations are CSRF-guarded: no Origin → 403, wrong Origin → 403,
/// matching Origin → the write lands. Bearer carries the same power with no Origin —
/// that is the CLI contract.
#[tokio::test]
async fn a_cookie_authed_mutation_needs_a_matching_origin() {
    let (app, _db, _tmp) = authed("s3cret", Duration::from_secs(3600)).await.unwrap();
    let login = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"s3cret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = session_cookie_from(&login);

    let create = |origin: Option<&str>| {
        let mut request = Request::post("/api/monitors")
            .header("content-type", "application/json")
            .header("cookie", cookie.clone())
            .header("host", "127.0.0.1:58400");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        request
            .body(Body::from(r#"{"name":"m","url":"https://example.com/"}"#))
            .unwrap()
    };

    // No Origin at all — a browser always sends one on writes, so this is not a browser.
    let response = app.clone().oneshot(create(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = json_body(response).await.unwrap();
    assert_eq!(body["error"]["code"], "origin_required");

    // A cross-site Origin is rejected before the credential matters.
    let response = app
        .clone()
        .oneshot(create(Some("https://evil.example.com")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = json_body(response).await.unwrap();
    assert_eq!(body["error"]["code"], "origin_mismatch");

    // Same-authority Origin — the real browser shape.
    let response = app
        .clone()
        .oneshot(create(Some("http://127.0.0.1:58400")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Bearer never needs Origin — curl and the CLI don't send one.
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/monitors")
                .header("content-type", "application/json")
                .header("authorization", "Bearer s3cret")
                .body(Body::from(r#"{"name":"m2","url":"https://example.org/"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}

/// And a mismatched Origin is rejected even on a valid bearer request — the header is
/// a cross-site marker no matter the credential.
#[tokio::test]
async fn a_mismatched_origin_is_rejected_for_any_credential() {
    let (app, _db, _tmp) = authed("s3cret", Duration::from_secs(3600)).await.unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/monitors")
                .header("authorization", "Bearer s3cret")
                .header("host", "127.0.0.1:58400")
                .header("origin", "https://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_expires_the_session() {
    let (app, _db, _tmp) = authed("s3cret", Duration::from_secs(3600)).await.unwrap();
    let login = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"s3cret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = session_cookie_from(&login);

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/logout")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cleared = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    // The same cookie now denies — the row is gone, not just the browser's copy.
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/monitors")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Store-side expiry is the authority: a zero-ttl session is dead the moment it exists.
#[tokio::test]
async fn an_expired_session_is_denied() {
    let (app, _db, _tmp) = authed("s3cret", Duration::from_secs(0)).await.unwrap();
    let login = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"s3cret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::NO_CONTENT);
    let cookie = session_cookie_from(&login);
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/monitors")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Honest unavailability: the writer is the only path to the store. When it dies, the
/// committed status map freezes — a monitor whose last commit was a pass would keep
/// serving "up" forever. The supervisor owns the writer's `JoinHandle`; its exit must turn
/// readiness 503 and every served status `unknown`, with the cause named.
#[tokio::test]
async fn a_dead_writer_makes_the_monitor_report_unavailable() -> anyhow::Result<()> {
    use monitor_server::vitals::{Role, Supervisor, Vitals};

    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("dead-writer.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let monitor =
        monitor_domain::NewMonitor::new("watched", url::Url::parse("https://example.com/health")?)?
            .into_monitor();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;

    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let write_health: monitor_db::WriteHealth = Arc::default();
    let vitals = Vitals::new(Arc::clone(&write_health), Duration::from_secs(30));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let supervisor = Supervisor::new(vitals.clone(), shutdown.clone());
    let (writer, writer_task) = monitor_db::writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        HashMap::new(),
        write_health,
        8,
        false,
    );
    supervisor.watch("writer", Role::Observing, writer_task);
    let app = router(AppState {
        db: db.clone(),
        statuses: Arc::clone(&statuses),
        registry: watch::channel(Arc::new(Vec::new())).0,
        vitals,
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
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

    // One committed pass: the monitor is honestly up.
    writer
        .record(monitor_db::Observation {
            monitor_id: monitor.id,
            workspace: monitor.workspace,
            generation: monitor.generation,
            slot_at: time::OffsetDateTime::now_utc(),
            slot: monitor_domain::SlotObservation::Probe(monitor_domain::CheckOutcome::Pass),
            http_status: Some(200),
            latency_ms: Some(3),
            detail: None,
        })
        .await?;
    let (status, body) = send(&app, "GET", "/api/monitors", None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["monitors"][0]["status"], "up");
    assert_eq!(body["monitoring"]["status"], "ok");

    // The writer stops (every handle gone — the same exit a crash in its loop produces).
    drop(writer);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let ready = loop {
        let response = app
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty())?)
            .await?;
        if response.status() != StatusCode::OK || std::time::Instant::now() > deadline {
            break response;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    let ready = json_body(ready).await?;
    assert_eq!(ready["status"], "unavailable");
    assert_eq!(ready["reason"], "writer exited");

    let (_, body) = send(&app, "GET", "/api/monitors", None).await?;
    assert_eq!(
        body["monitors"][0]["status"], "unknown",
        "a frozen status is not current evidence"
    );
    assert_eq!(body["monitoring"]["status"], "unavailable");
    assert_eq!(body["monitoring"]["faults"][0], "writer exited");
    let (_, one) = send(
        &app,
        "GET",
        &format!("/api/monitors/{}", monitor.id.0),
        None,
    )
    .await?;
    assert_eq!(one["status"], "unknown");
    Ok(())
}

/// Retention (D-012): the monitor count is bounded — every per-monitor table and the
/// schedule scale with it. The cap is one conditional insert, so it holds under races.
#[tokio::test]
async fn creating_past_the_monitor_cap_is_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("cap.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let app = router(AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 2,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    });
    let create = |n: usize| serde_json::json!({ "name": format!("m{n}"), "url": format!("https://m{n}.example.com/") });
    for n in 0..2 {
        let (status, _) = send(&app, "POST", "/api/monitors", Some(create(n))).await?;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, body) = send(&app, "POST", "/api/monitors", Some(create(2))).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "limit_reached");
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT).monitor_count().await?,
        2,
        "nothing was inserted past the cap"
    );

    // Deleting one (no history yet) frees a slot.
    let (_, list) = send(&app, "GET", "/api/monitors", None).await?;
    let id = list["monitors"][0]["id"].as_str().unwrap().to_owned();
    let (status, _) = send(&app, "DELETE", &format!("/api/monitors/{id}"), None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "POST", "/api/monitors", Some(create(3))).await?;
    assert_eq!(status, StatusCode::CREATED);
    Ok(())
}

/// The `Set-Cookie` header of one login (or logout) call with the given extra headers.
async fn cookie_header(
    app: &Router,
    path: &str,
    headers: &[(&str, &str)],
) -> anyhow::Result<String> {
    let mut request = Request::post(path)
        .header("content-type", "application/json")
        .header("host", "mon.example.com");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(r#"{"token":"s3cret"}"#))?)
        .await?;
    anyhow::ensure!(response.status() == StatusCode::NO_CONTENT, "{path}");
    Ok(response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned())
}

/// The session cookie is `__Host-` pinned (S2 review): `Secure`, `Path=/`, no
/// `Domain`, on every response — a subdomain can't shadow it and a plain-http hop
/// can never carry it. The contract no longer depends on what scheme the proxy
/// reported, so plain-http loopback dev still works (localhost is a trustworthy
/// origin; browsers store `Secure` cookies there).
#[tokio::test]
async fn the_session_cookie_is_host_pinned_and_always_secure() {
    let (app, _db, _tmp) = authed("s3cret", Duration::from_secs(3600)).await.unwrap();
    for headers in [
        vec![],
        vec![("x-forwarded-proto", "http")],
        vec![("x-forwarded-proto", "https")],
    ] {
        for path in ["/api/login", "/api/logout"] {
            let cookie = cookie_header(&app, path, &headers).await.unwrap();
            assert!(cookie.starts_with("__Host-smon_session="), "{cookie}");
            assert!(cookie.contains("; Secure"), "{cookie}");
            assert!(cookie.contains("; Path=/"), "{cookie}");
            assert!(!cookie.contains("Domain="), "{cookie}");
        }
    }
}

/// Refused-target honesty, at registration: a target the SSRF policy will refuse can't be
/// registered (or edited in) under the strict profile — it would only ever report the
/// monitor's own refusal as the target being down. The lab profile still allows fixtures.
#[tokio::test]
async fn refused_targets_cannot_be_registered_outside_the_lab_profile() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    for url in [
        "http://127.0.0.1:9/health",
        "http://[::1]/",
        "http://169.254.169.254/latest/meta-data",
        "http://10.0.0.7:8080/",
        "http://100.64.0.1/",
    ] {
        let (status, body) = send(
            &app,
            "POST",
            "/api/monitors",
            Some(serde_json::json!({ "name": "internal", "url": url })),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {body}");
        assert_eq!(body["error"]["code"], "refused_target", "{url}");
    }

    // Editing a public monitor onto a refused address is refused the same way.
    let (status, created) = send(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({ "name": "public", "url": "https://example.com/health" })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_owned();
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/monitors/{id}"),
        Some(serde_json::json!({ "url": "http://192.168.1.1/" })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "refused_target");

    // The lab profile is the explicit opt-in for local fixtures.
    let (lab, _db, _tmp) = lab_app().await.unwrap();
    let (status, _) = send(
        &lab,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({ "name": "fixture", "url": "http://127.0.0.1:9/health" })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::CREATED);
}

async fn lab_app() -> anyhow::Result<(Router, Db, tempfile::TempDir)> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("lab.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let app = router(AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: true,
        version: "test",
    });
    Ok((app, db, tmp))
}

/// Degraded is not unready: the monitor still observes and serves, so `/readyz` answers 200
/// with the faults (load balancers and the RUNBOOK's `curl -sf` gate keep working); only
/// `unavailable` (not observing) is 503.
#[tokio::test]
async fn readyz_is_200_and_says_degraded_when_only_a_supporting_task_stopped() {
    let vitals = monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30));
    vitals.task_stopped(
        "snapshot",
        monitor_server::vitals::Role::Supporting,
        "exited",
    );
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::connect(&format!("sqlite:{}", tmp.path().join("d.db").display()), 1)
        .await
        .unwrap();
    db.migrate().await.unwrap();
    let app = router(AppState {
        db,
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: watch::channel(Arc::new(Vec::new())).0,
        vitals,
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
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
    let response = app
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await.unwrap();
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["faults"][0], "snapshot exited");
}

/// `/readyz` is unauthenticated: a failed retention pass may say *that* it failed, never
/// the store's error text (paths, SQL, SQLite messages). The detail goes to the log.
#[tokio::test]
async fn readyz_reports_a_failed_retention_pass_without_its_error_text() {
    let vitals = monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30));
    let error = monitor_db::QueryError::Io(
        "disk I/O error on /opt/smon/data/monitor.db: SELECT * FROM checks".into(),
    );
    vitals.retention_outcome(Err(&error));
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::connect(&format!("sqlite:{}", tmp.path().join("r.db").display()), 1)
        .await
        .unwrap();
    db.migrate().await.unwrap();
    let app = router(AppState {
        db,
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: watch::channel(Arc::new(Vec::new())).0,
        vitals,
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
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
    let response = app
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = json_body(response).await.unwrap();
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["faults"][0], "retention failing (see the server log)");
    let text = body.to_string();
    assert!(
        !text.contains("/opt/smon") && !text.contains("SELECT"),
        "{text}"
    );
}

/// An authed router over an existing store — two of these on one store are "the same
/// server restarted with a different `SMON_ADMIN_TOKEN`".
fn authed_on(db: &Db, token: &str) -> Router {
    monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: Some(Arc::from(token)),
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: std::env::temp_dir(),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
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
    })
}

async fn login_cookie(app: &Router, token: &str) -> anyhow::Result<String> {
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "token": token }).to_string(),
                ))?,
        )
        .await?;
    anyhow::ensure!(response.status() == StatusCode::NO_CONTENT, "login failed");
    Ok(session_cookie_from(&response))
}

async fn read_with_cookie(app: &Router, cookie: &str) -> anyhow::Result<StatusCode> {
    Ok(app
        .clone()
        .oneshot(
            Request::get("/api/monitors")
                .header("cookie", cookie)
                .body(Body::empty())?,
        )
        .await?
        .status())
}

/// Rotating `SMON_ADMIN_TOKEN` is how an operator revokes access — it must end every
/// dashboard session too, not leave cookies earned with the old token working for their
/// remaining 12 hours.
#[tokio::test]
async fn rotating_the_admin_token_ends_every_session() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("rotate.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    let old = authed_on(&db, "old-token-0123456789abcdef0123456789abcd");
    let cookie = login_cookie(&old, "old-token-0123456789abcdef0123456789abcd").await?;
    assert_eq!(read_with_cookie(&old, &cookie).await?, StatusCode::OK);

    let rotated = authed_on(&db, "new-token-0123456789abcdef0123456789abcd");
    assert_eq!(
        read_with_cookie(&rotated, &cookie).await?,
        StatusCode::UNAUTHORIZED,
        "a cookie earned with the old token must not survive the rotation"
    );
    // The new token earns a session of its own.
    let fresh = login_cookie(&rotated, "new-token-0123456789abcdef0123456789abcd").await?;
    assert_eq!(read_with_cookie(&rotated, &fresh).await?, StatusCode::OK);
    Ok(())
}

/// Session rows ride along in every backup. The store keeps only a digest of each session
/// id, so a copied store or backup holds nothing that works as a cookie.
#[tokio::test]
async fn the_store_and_its_backups_hold_no_usable_session_id() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("digest.db");
    let db = Db::connect(&format!("sqlite:{}", db_path.display()), 1).await?;
    db.migrate().await?;
    let app = authed_on(&db, "digest-token-0123456789abcdef0123456789ab");
    let cookie = login_cookie(&app, "digest-token-0123456789abcdef0123456789ab").await?;
    let id = cookie
        .strip_prefix("__Host-smon_session=")
        .ok_or_else(|| anyhow::anyhow!("cookie shape: {cookie}"))?;
    assert_eq!(read_with_cookie(&app, &cookie).await?, StatusCode::OK);

    let backup = tmp.path().join("backup.db");
    db.backup(&backup).await?;
    for file in [
        backup.clone(),
        db_path.clone(),
        db_path.with_extension("db-wal"),
    ] {
        if let Ok(bytes) = std::fs::read(&file) {
            assert!(
                !bytes
                    .windows(id.len())
                    .any(|window| window == id.as_bytes()),
                "{} holds the raw session id",
                file.display()
            );
        }
    }
    Ok(())
}

/// D-016: a dashboard session also ends when it goes unused for `SMON_SESSION_IDLE_S`,
/// inside its 12 h lifetime; use keeps it alive. L8: this used wall-clock sleeps (a 1 s
/// limit, use every 0.6 s) and failed on a loaded CI runner; the clock is injected now and
/// moved by hand.
#[tokio::test]
async fn an_unused_session_ends_at_the_idle_limit_and_use_keeps_it_alive() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("idle.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    let token = "idle-token-0123456789abcdef0123456789abcd";
    let offset_s = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let clock: monitor_server::Clock = {
        let offset_s = Arc::clone(&offset_s);
        Arc::new(move || {
            time::OffsetDateTime::now_utc()
                + time::Duration::seconds(offset_s.load(std::sync::atomic::Ordering::Relaxed))
        })
    };
    let advance = |seconds: i64| {
        offset_s.fetch_add(seconds, std::sync::atomic::Ordering::Relaxed);
    };
    let app = monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: Some(Arc::from(token)),
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_secs(60),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock,
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    });
    let cookie = login_cookie(&app, token).await?;
    for _ in 0..4 {
        advance(40);
        assert_eq!(
            read_with_cookie(&app, &cookie).await?,
            StatusCode::OK,
            "used every 40 s: alive past the 60 s limit"
        );
    }
    advance(61);
    assert_eq!(
        read_with_cookie(&app, &cookie).await?,
        StatusCode::UNAUTHORIZED,
        "unused for 61 s: ended"
    );
    Ok(())
}

async fn status_with(
    app: &Router,
    method: &str,
    headers: &[(&str, &str)],
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<StatusCode> {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(json) => {
            request = request.header("content-type", "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    Ok(app.clone().oneshot(request.body(body)?).await?.status())
}

/// M3: tokenless dev mode is open to whoever reaches the loopback listener — which, by DNS
/// rebinding, includes any web page the operator visits. It skipped the Origin and Host
/// checks entirely: a rebound `Host` read the monitors and wrote a backup. Tokenless mode
/// now serves only loopback `Host` values, and a present `Origin` must match `Host` in
/// every mode.
#[tokio::test]
async fn tokenless_mode_serves_only_loopback_hosts_and_same_origin_browsers() -> anyhow::Result<()>
{
    let (app, _statuses, _db, _wh, _tmp) = app().await?;
    let out = tempfile::tempdir()?;
    let backup = out.path().join("exfil.db");
    let backup_body = || Some(serde_json::json!({ "path": backup.display().to_string() }));

    // A rebound hostname, with the Origin a rebound page really sends: refused.
    let rebound = [
        ("host", "rebound.attacker.example:58465"),
        ("origin", "http://rebound.attacker.example:58465"),
    ];
    assert_eq!(
        status_with(&app, "GET", &rebound, "/api/monitors", None).await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status_with(&app, "POST", &rebound, "/api/backup", backup_body()).await?,
        StatusCode::FORBIDDEN
    );
    // A cross-site page posting to the loopback address: refused.
    let cross_site = [
        ("host", "127.0.0.1:58465"),
        ("origin", "https://evil.example"),
    ];
    assert_eq!(
        status_with(&app, "POST", &cross_site, "/api/backup", backup_body()).await?,
        StatusCode::FORBIDDEN
    );
    assert!(!backup.exists(), "a forged request wrote a backup");

    // The CLI (no Origin) and the dashboard (same origin) on loopback still work.
    for host in ["127.0.0.1:58465", "localhost:58465", "[::1]:58465"] {
        assert_eq!(
            status_with(&app, "GET", &[("host", host)], "/api/monitors", None).await?,
            StatusCode::OK,
            "{host}"
        );
    }
    let same_origin = [
        ("host", "localhost:58465"),
        ("origin", "http://localhost:58465"),
    ];
    assert_eq!(
        status_with(
            &app,
            "POST",
            &same_origin,
            "/api/monitors",
            Some(serde_json::json!({ "name": "m", "url": "https://example.com/" })),
        )
        .await?,
        StatusCode::CREATED
    );
    Ok(())
}

/// L2: a store failure answered with the database's own text — sqlx/SQLite messages
/// (table names, pool state) reached clients, including unauthenticated ones through
/// `/readyz` and `POST /api/logout`. Every 5xx body is now fixed text; the detail goes to
/// the log only.
#[tokio::test]
async fn store_failures_answer_fixed_text_never_the_databases_own() -> anyhow::Result<()> {
    let leaks = |body: &serde_json::Value| {
        let text = body.to_string().to_ascii_lowercase();
        [
            "pool",
            "sqlite",
            "database",
            "table",
            "connection",
            "closed",
        ]
        .into_iter()
        .find(|word| text.contains(word))
    };

    let (app, _statuses, db, _wh, _tmp) = app().await?;
    db.close().await;
    let (status, body) = send(&app, "GET", "/api/monitors", None).await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "internal");
    assert_eq!(leaks(&body), None, "{body}");

    let (status, body) = send(&app, "GET", "/readyz", None).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["status"], "unavailable");
    assert_eq!(leaks(&body), None, "{body}");

    let (authed_app, authed_db, _tmp2) = authed("s3cret", Duration::from_secs(3600)).await?;
    authed_db.close().await;
    let response = authed_app
        .oneshot(
            Request::post("/api/logout")
                .header("cookie", "__Host-smon_session=whatever")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = json_body(response).await?;
    assert_eq!(leaks(&body), None, "{body}");
    Ok(())
}

/// A router whose monitor cap is out of the way, so the schedule's capacity is what is
/// tested.
fn uncapped(db: &Db) -> Router {
    monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: std::env::temp_dir(),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100_000,
        workspace_slots: usize::MAX,
        allow_local_targets: false,
        version: "test",
    })
}

/// D-018: a schedule past what the coordinator can start is refused when it is written —
/// 409 `capacity_exceeded`, naming the bound — instead of being accepted and then missing
/// slots in silence. Globally: 480 monitors at 15 s whose probes may each take the full
/// 15 s would fill the 512 slots; the 480th is refused. Per origin: 30 monitors at 15 s are the
/// 2 starts a tick one origin allows; a 31st on that origin is refused, elsewhere it fits.
#[tokio::test]
async fn a_schedule_the_coordinator_cannot_serve_is_refused_by_name() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("cap.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    let slow = |name: String, url: &str| -> anyhow::Result<monitor_domain::Monitor> {
        Ok(monitor_domain::NewMonitor::with_schedule(
            name,
            url::Url::parse(url)?,
            Duration::from_secs(15),
            Duration::from_secs(15),
        )?
        .into_monitor())
    };
    for i in 0..479 {
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&slow(format!("m{i}"), &format!("https://o{i}.example/"))?)
            .await?;
    }
    let app = uncapped(&db);
    // 479 × 16/15 ≈ 510.9 slots: a cheap hourly monitor still fits ...
    let (status, body) = send(
        &app,
        "POST",
        "/api/monitors",
        Some(serde_json::json!({ "name": "hourly", "url": "https://another.example/", "interval_s": 3600 })),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // ... one more 15 s monitor that may take its full 15 s does not.
    let one_more = serde_json::json!({
        "name": "one-more", "url": "https://another.example/", "interval_s": 15, "timeout_s": 15
    });
    let (status, body) = send(&app, "POST", "/api/monitors", Some(one_more)).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "capacity_exceeded");
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_monitors()
            .await?
            .len(),
        480,
        "nothing was stored"
    );

    // Per origin, in a fresh store.
    let tmp2 = tempfile::tempdir()?;
    let db = Db::connect(&format!("sqlite:{}", tmp2.path().join("o.db").display()), 1).await?;
    db.migrate().await?;
    for i in 0..30 {
        let mut monitor = slow(format!("p{i}"), &format!("https://api.example/{i}"))?;
        monitor.timeout = Duration::from_secs(3);
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&monitor)
            .await?;
    }
    let app = uncapped(&db);
    let same_origin = serde_json::json!({
        "name": "p30", "url": "https://api.example/30", "interval_s": 15, "timeout_s": 3
    });
    let (status, body) = send(&app, "POST", "/api/monitors", Some(same_origin)).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "capacity_exceeded");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("api.example")),
        "the refusal names the origin: {body}"
    );
    let elsewhere = serde_json::json!({
        "name": "q", "url": "https://other.example/", "interval_s": 15, "timeout_s": 3
    });
    let (status, _) = send(&app, "POST", "/api/monitors", Some(elsewhere)).await?;
    assert_eq!(status, StatusCode::CREATED);

    // An edit that would overload the origin is refused the same way.
    let faster = serde_json::json!({ "interval_s": 15, "url": "https://api.example/again" });
    let other_id = db
        .workspace(WorkspaceId::DEFAULT)
        .list_monitors()
        .await?
        .into_iter()
        .find(|row| row.url.contains("other.example"))
        .map(|row| row.id)
        .ok_or_else(|| anyhow::anyhow!("the other monitor"))?;
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/monitors/{other_id}"),
        Some(faster),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "capacity_exceeded");
    Ok(())
}

/// Origins are (address, host) pairs for the per-workspace *share*, but the
/// write-time ceiling is the resolved **address** summed across hosts (N1
/// re-review): 30 monitors at 15 s on `127.0.0.1:9` spend that address's whole
/// 2-starts-a-tick share for this workspace, so a monitor spelled `localhost:9` —
/// a different host, the *same* IP — is refused, and so is one more literal.
/// A different address is unaffected either way. (Lab profile, so loopback is
/// allowed.)
#[tokio::test]
async fn an_alias_of_a_full_address_is_refused_too() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("alias.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    for i in 0..30 {
        let monitor = monitor_domain::NewMonitor::with_schedule(
            format!("l{i}"),
            url::Url::parse(&format!("http://127.0.0.1:9/{i}"))?,
            Duration::from_secs(15),
            Duration::from_secs(3),
        )?
        .into_monitor();
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&monitor)
            .await?;
    }
    let app = monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        status_changed: std::sync::Arc::default(),
        monitor_writes: std::sync::Arc::default(),
        invite_writes: std::sync::Arc::default(),
        clock: monitor_server::system_clock(),
        trusted_proxy: None,
        backup_dir: None,
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100_000,
        workspace_slots: usize::MAX,
        allow_local_targets: true,
        version: "test",
    });
    // `localhost` resolves to the saturated 127.0.0.1 — a different host spelling
    // of the *same address*. The per-(address, host) share would give it an empty
    // bucket, but the workspace's load on the address is already spent, so the
    // write is refused: host aliasing can't inflate one IP's rate (N1).
    let alias = serde_json::json!({
        "name": "alias", "url": "http://localhost:9/30", "interval_s": 15, "timeout_s": 3
    });
    let (status, body) = send(&app, "POST", "/api/monitors", Some(alias)).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "capacity_exceeded");
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_monitors()
            .await?
            .len(),
        30,
        "nothing was stored"
    );
    // One more literal of the same address is refused the same way.
    let same_pair = serde_json::json!({
        "name": "same", "url": "http://127.0.0.1:9/30", "interval_s": 15, "timeout_s": 3
    });
    let (status, body) = send(&app, "POST", "/api/monitors", Some(same_pair)).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "capacity_exceeded");
    let other_address = serde_json::json!({
        "name": "other", "url": "http://127.0.0.2:9/", "interval_s": 15, "timeout_s": 3
    });
    let (status, body) = send(&app, "POST", "/api/monitors", Some(other_address)).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Ok(())
}

/// A stored body expectation can be removed: an explicit `null` clears it, while a
/// patch that doesn't mention the field keeps it. Without the first, a monitor that
/// ever had a marker could never be switched to `HEAD`.
#[tokio::test]
async fn patch_null_clears_the_body_expectation_and_absent_keeps_it() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "marker").await.unwrap();
    let path = format!("/api/monitors/{id}");

    let (status, body) = send(
        &app,
        "PATCH",
        &path,
        Some(serde_json::json!({ "expect_body": "ready" })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");

    // Absent: an unrelated edit leaves the marker where it was.
    let (status, body) = send(
        &app,
        "PATCH",
        &path,
        Some(serde_json::json!({ "timeout_s": 4 })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, stored) = send(&app, "GET", &path, None).await.unwrap();
    assert_eq!(stored["expect_body"], "ready", "{stored}");

    // HEAD while the marker is stored is refused: there is no body to search.
    let (status, body) = send(
        &app,
        "PATCH",
        &path,
        Some(serde_json::json!({ "method": "HEAD" })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Null: the marker goes, and the same request may switch the method.
    let (status, body) = send(
        &app,
        "PATCH",
        &path,
        Some(serde_json::json!({ "method": "HEAD", "expect_body": null })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, stored) = send(&app, "GET", &path, None).await.unwrap();
    assert!(stored["expect_body"].is_null(), "{stored}");
    assert_eq!(stored["method"], "HEAD", "{stored}");
}

/// Deleting a monitor together with what was recorded about it is a separate,
/// explicit request (D-026): the plain delete still refuses, the opt-in removes the
/// monitor, its checks, its incident and its queued alerts, and nothing of another
/// monitor's.
#[tokio::test]
async fn a_monitor_is_deleted_with_its_evidence_only_when_asked() {
    let (app, statuses, db, _wh, _tmp) = app().await.unwrap();
    let doomed = create(&app, "doomed").await.unwrap();
    let kept = create(&app, "kept").await.unwrap();
    let writer = seed_writer(&db, &statuses);
    // Three failures: checks, an open incident and an outbox row for each monitor.
    for id in [&doomed, &kept] {
        for _ in 0..3 {
            seed_check(&db, &writer, id, monitor_domain::CheckOutcome::Transport)
                .await
                .unwrap();
        }
    }
    let path = format!("/api/monitors/{doomed}");

    // Anything but the one word is a malformed request, not a quiet plain delete.
    let (status, body) = send(&app, "DELETE", &format!("{path}?evidence=yes"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = send(&app, "DELETE", &path, None).await.unwrap();
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the plain delete still refuses"
    );

    let (status, body) = send(&app, "DELETE", &format!("{path}?evidence=delete"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = send(&app, "GET", &path, None).await.unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Its rows are gone from every table that named it.
    let scoped = db.workspace(WorkspaceId::DEFAULT);
    let gone = monitor_domain::MonitorId(uuid::Uuid::parse_str(&doomed).unwrap());
    assert!(!scoped.has_history(gone).await.unwrap());
    let (_, incidents) = send(&app, "GET", "/api/incidents", None).await.unwrap();
    let incidents = incidents["incidents"].as_array().unwrap();
    assert_eq!(incidents.len(), 1, "{incidents:?}");
    assert_eq!(incidents[0]["monitor_id"], kept);

    // The other monitor's evidence is untouched.
    let (_, checks) = send(&app, "GET", &format!("/api/monitors/{kept}/checks"), None)
        .await
        .unwrap();
    assert_eq!(checks["checks"].as_array().unwrap().len(), 3);

    // A second ask finds nothing to delete.
    let (status, _) = send(&app, "DELETE", &format!("{path}?evidence=delete"), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Two write-time refusals the dashboard's form can now reach. A redirect status
/// named as passing would report a broken redirect chain as up, because working
/// redirects are followed and never the final answer. A name is bounded and plain:
/// it is shown in lists, in alerts and on public pages.
#[tokio::test]
async fn redirect_statuses_and_unbounded_names_are_refused_at_write() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let create = |body: serde_json::Value| {
        let app = app.clone();
        async move {
            send(&app, "POST", "/api/monitors", Some(body))
                .await
                .unwrap()
        }
    };
    let base = |name: &str, statuses: serde_json::Value| {
        serde_json::json!({
            "name": name, "url": "https://example.com/health", "expect_statuses": statuses,
        })
    };

    for statuses in [
        serde_json::json!(["200-399"]),
        serde_json::json!(["200-299", "302"]),
        serde_json::json!(["308"]),
    ] {
        let (status, body) = create(base("redirects", statuses.clone())).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{statuses}: {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("redirects are followed")),
            "{body}"
        );
    }
    // 304 and 300 are not followed, so they stay nameable; so do codes around the gap.
    let (status, body) = create(base(
        "not-followed",
        serde_json::json!(["200-299", "300", "304-306", "401"]),
    ))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = create(base(&"n".repeat(121), serde_json::json!(["200-299"]))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = create(base("two\nlines", serde_json::json!(["200-299"]))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = create(base(&"n".repeat(120), serde_json::json!(["200-299"]))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The edit path holds the same line.
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/monitors/{id}"),
        Some(serde_json::json!({"expect_statuses": ["200-308"]})),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

/// The name and redirect rules are for what a write names, not for what is stored. A
/// monitor an earlier version accepted with a longer name must still pause, resume and
/// reschedule; only an edit that names the field again has to satisfy the rule.
#[tokio::test]
async fn an_edit_is_not_refused_over_a_stored_value_it_does_not_touch() {
    let (app, _statuses, db, _wh, _tmp) = app().await.unwrap();
    let mut old = monitor_domain::NewMonitor::new(
        "short",
        url::Url::parse("https://example.com/health").unwrap(),
    )
    .unwrap()
    .into_monitor();
    old.name = "n".repeat(300);
    old.expect.statuses = monitor_domain::parse_statuses("200-399").unwrap();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&old)
        .await
        .unwrap();
    let path = format!("/api/monitors/{}", old.id.0);

    for body in [
        serde_json::json!({ "paused": true }),
        serde_json::json!({ "paused": false }),
        serde_json::json!({ "timeout_s": 4 }),
    ] {
        let (status, answer) = send(&app, "PATCH", &path, Some(body.clone()))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}: {answer}");
    }

    // Naming either field again brings its rule with it.
    let (status, answer) = send(
        &app,
        "PATCH",
        &path,
        Some(serde_json::json!({ "name": "n".repeat(200) })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    let (status, answer) = send(
        &app,
        "PATCH",
        &path,
        Some(serde_json::json!({ "expect_statuses": ["200-302"] })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");

    // And a corrected value is accepted.
    let (status, answer) = send(
        &app,
        "PATCH",
        &path,
        Some(serde_json::json!({ "name": "billing", "expect_statuses": ["200-299"] })),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// A maintenance window's reason is one bounded line, and creating windows spends the
/// workspace's monitor write budget like every other monitor write. Unbounded, a member
/// could store megabytes per window, without limit, in rows a later delete removes in
/// one cascade under the store's write lock.
#[tokio::test]
async fn a_window_reason_is_bounded_and_windows_spend_the_write_budget() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let id = create(&app, "windowed").await.unwrap();
    let path = format!("/api/monitors/{id}/maintenance");
    let window = |reason: &str| {
        serde_json::json!({
            "starts_at": "2099-01-01T10:00:00Z", "ends_at": "2099-01-01T11:00:00Z",
            "reason": reason,
        })
    };

    for reason in ["r".repeat(201), "two\nlines".to_owned()] {
        let (status, body) = send(&app, "POST", &path, Some(window(&reason)))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let (status, body) = send(&app, "POST", &path, Some(window(&"r".repeat(200))))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Refusals spent nothing; the create above and the monitor's own create spent two.
    let left = monitor_server::MONITOR_WRITE_BUDGET - 2;
    for _ in 0..left {
        let (status, body) = send(&app, "POST", &path, Some(window("deploy")))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = send(&app, "POST", &path, Some(window("deploy")))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}
