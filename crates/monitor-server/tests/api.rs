//! Black-box tests against the real router on an in-memory database.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use monitor_db::Db;
use monitor_server::{AppState, router};
use tower::ServiceExt;

async fn app() -> anyhow::Result<Router> {
    // One connection: pooled checkouts of sqlite::memory: would each see a different database.
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    Ok(router(AppState {
        db,
        version: "test",
    }))
}

async fn json_body(response: axum::response::Response) -> anyhow::Result<serde_json::Value> {
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::test]
async fn livez_and_readyz_answer() {
    let app = app().await.unwrap();
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
    let app = app().await.unwrap();

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

#[tokio::test]
async fn invalid_input_gets_an_error_envelope() {
    let app = app().await.unwrap();
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
