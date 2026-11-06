//! Black-box tests against the real router on an in-memory database.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use monitor_db::Db;
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
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let write_health: monitor_db::WriteHealth = Arc::default();
    let (registry, _rx) = watch::channel(Arc::new(Vec::new()));
    let app = router(AppState {
        db: db.clone(),
        statuses: Arc::clone(&statuses),
        registry,
        write_health: Arc::clone(&write_health),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
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
    let generation = db.monitor_generation(monitor_id).await?.unwrap_or(1);
    Ok(writer
        .record(monitor_db::Observation {
            monitor_id,
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
    assert!(db.list_checks(monitor_id, 10).await.unwrap().is_empty());
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
        .monitor_generation(monitor_id)
        .await
        .unwrap()
        .unwrap_or(1);
    writer
        .record(monitor_db::Observation {
            monitor_id,
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

/// Backup endpoint (M4.3): absolute paths produce a snapshot file; relative paths are a
/// bad request — where a backup lands is the operator's explicit choice.
#[tokio::test]
async fn backup_writes_a_snapshot_to_an_absolute_path() {
    let (app, _statuses, _db, _wh, _tmp) = app().await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let backup = tmp.path().join("snapshot.db");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/backup")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"path": "relative.db"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let response = app
        .oneshot(
            Request::post("/api/backup")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"path": backup.to_str().unwrap()}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await.unwrap();
    assert!(body["bytes"].as_u64().unwrap() > 0);
    assert!(backup.exists());
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
        write_health,
        admin_token: Some(Arc::from("s3cret")),
        session_ttl: Duration::from_hours(12),
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
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("auth.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let state = monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        write_health: Arc::default(),
        admin_token: Some(Arc::from(token)),
        session_ttl: ttl,
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
