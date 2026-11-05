//! Black-box tests against the real router on an in-memory database.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use monitor_db::Db;
use monitor_server::{AppState, router};
use tokio::sync::watch;
use tower::ServiceExt;

/// Builds the router and returns it with the shared pieces tests drive directly.
async fn app() -> anyhow::Result<(Router, Arc<RwLock<monitor_domain::Statuses>>, Db)> {
    // One connection: pooled checkouts of sqlite::memory: would each see a different database.
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (registry, _rx) = watch::channel(Arc::new(Vec::new()));
    let app = router(AppState {
        db: db.clone(),
        statuses: Arc::clone(&statuses),
        registry,
        version: "test",
    });
    Ok((app, statuses, db))
}

async fn json_body(response: axum::response::Response) -> anyhow::Result<serde_json::Value> {
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::test]
async fn livez_and_readyz_answer() {
    let (app, _statuses, _db) = app().await.unwrap();
    for path in ["/livez", "/readyz"] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn create_then_list_a_monitor() {
    let (app, _statuses, _db) = app().await.unwrap();

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
    let (app, _statuses, _db) = app().await.unwrap();
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
    let (app, _statuses, _db) = app().await.unwrap();
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
    let (app, _statuses, _db) = app().await.unwrap();
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
    let (app, _statuses, _db) = app().await.unwrap();
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
    let (writer, _task) =
        monitor_db::writer::spawn(db.clone(), Arc::clone(statuses), HashMap::new(), 8);
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
    let (app, statuses, db) = app().await.unwrap();
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
    let (app, statuses, db) = app().await.unwrap();
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
    let (app, statuses, db) = app().await.unwrap();
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
    let (writer, _task) = monitor_db::writer::spawn(db.clone(), statuses, HashMap::new(), 8);
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
    let (app, statuses, db) = app().await.unwrap();
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
    let (app, _statuses, _db) = app().await.unwrap();
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
