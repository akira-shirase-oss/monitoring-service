//! Every management route denies a request without a valid credential — proved for the
//! whole inventory, not for one sample route.
//!
//! The inventory comes from the router itself, so a new route can't be missed:
//! - **paths** are read from `router()`'s source (every `.route("…")` literal, inside and
//!   outside the auth layer); a route added outside the layer that isn't on the short
//!   public list fails here;
//! - **methods** come from the live router: an authorized request with a method nobody
//!   routes gets axum's 405 with an `Allow` header naming every method routed on that
//!   path (without a credential even that 405 is a 401 — the route list isn't public).
//!
//! Then every (path, method) is sent with no credential, a wrong bearer, a malformed one
//! and an unknown session cookie, each carrying a body that would act if it got through.
//! Each must be 401 `unauthenticated`, and nothing may have happened in the store or on
//! disk.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use monitor_db::Db;
use tower::ServiceExt;

const ROUTER_SOURCE: &str = include_str!("../src/lib.rs");
const MAIN_SOURCE: &str = include_str!("../src/main.rs");
const TOKEN: &str = "inventory-token-0123456789abcdef0123456789";

/// The routes that are public by design: health, the build version, and the two
/// endpoints that exchange the token for a session (they enforce their own rules).
const PUBLIC: [&str; 5] = ["/livez", "/readyz", "/version", "/api/login", "/api/logout"];

/// The string literal each `marker` call opens with, in source order.
fn literals_after<'a>(source: &'a str, marker: &str) -> Vec<&'a str> {
    source
        .match_indices(marker)
        .filter_map(|(at, _)| {
            let rest = source[at + marker.len()..].trim_start();
            let rest = rest.strip_prefix('"')?;
            rest.split_once('"').map(|(literal, _)| literal)
        })
        .collect()
}

/// (protected paths, public paths) as `router()` declares them.
#[expect(
    clippy::expect_used,
    reason = "test helper: a source it can't read is a failed test"
)]
fn declared_routes() -> (Vec<String>, Vec<String>) {
    let start = ROUTER_SOURCE
        .find("pub fn router(")
        .expect("router() exists in lib.rs");
    let body = &ROUTER_SOURCE[start..];
    let body = &body[..body.find("\n}\n").expect("router() ends")];
    for unsupported in [".merge(", ".nest_service(", ".route_service(", ".fallback"] {
        assert!(
            !body.contains(unsupported),
            "router() uses `{unsupported}`: teach this inventory test about it first"
        );
    }
    let (api, outer) = body
        .split_once(".route_layer(")
        .expect("the API sits behind route_layer(require_auth)");
    assert_eq!(
        literals_after(outer, ".nest("),
        ["/api"],
        "the authed router is nested at /api, once"
    );
    let protected = literals_after(api, ".route(")
        .into_iter()
        .map(|path| format!("/api{path}"))
        .collect();
    let public = literals_after(outer, ".route(")
        .into_iter()
        .map(str::to_owned)
        .collect();
    (protected, public)
}

/// Every method the live router serves on `path`, read from the 405's `Allow` header.
#[expect(
    clippy::expect_used,
    reason = "test helper: a malformed probe is a failed test"
)]
async fn allowed_methods(app: &axum::Router, path: &str) -> BTreeSet<String> {
    let probe = || Method::from_bytes(b"INVENTORY").expect("a valid extension method");
    let anonymous = app
        .clone()
        .oneshot(
            Request::builder()
                .method(probe())
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(
        anonymous.status(),
        StatusCode::UNAUTHORIZED,
        "{path}: without a credential even an unrouted method is 401"
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(probe())
                .uri(path)
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "{path}: an unrouted method must be 405, so `Allow` lists the routed ones"
    );
    let allow = response
        .headers()
        .get("allow")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("{path}: 405 without an Allow header"));
    allow
        .split(',')
        .map(|method| method.trim().to_owned())
        .filter(|method| !method.is_empty())
        .collect()
}

/// A router with management auth on, over a scratch store.
#[expect(
    clippy::expect_used,
    reason = "test helper: a store that won't open is a failed test"
)]
async fn authed_router(dir: &std::path::Path) -> (axum::Router, Db) {
    let url = format!("sqlite:{}", dir.join("inventory.db").display());
    let db = Db::connect(&url, 1).await.expect("connect");
    db.migrate().await.expect("migrate");
    let app = monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: Some(Arc::from(TOKEN)),
        session_ttl: Duration::from_hours(1),
        session_idle: Duration::from_hours(1),
        cookie_secure: monitor_server::auth::CookieSecure::Auto,
        max_monitors: 100,
        allow_local_targets: false,
        version: "test",
    });
    (app, db)
}

/// `method path` with each kind of missing or bad credential: 401 `unauthenticated`.
#[expect(
    clippy::expect_used,
    reason = "test helper: a malformed request is a failed test"
)]
async fn assert_denied(app: &axum::Router, method: &str, template: &str, path: &str, body: &str) {
    for (label, header) in [
        ("no credential", None),
        ("wrong bearer", Some(("authorization", "Bearer wrong"))),
        ("lower-case scheme", Some(("authorization", "bearer x"))),
        ("bare token", Some(("authorization", TOKEN))),
        ("unknown session", Some(("cookie", "smon_session=deadbeef"))),
    ] {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).expect("method"))
            .uri(path)
            .header("content-type", "application/json");
        if let Some((name, value)) = header {
            request = request.header(name, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("infallible");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {template} ({label}) must be 401"
        );
        if method != "HEAD" {
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes();
            let json: serde_json::Value =
                serde_json::from_slice(&bytes).expect("an error envelope");
            assert_eq!(
                json["error"]["code"], "unauthenticated",
                "{method} {template} ({label})"
            );
        }
    }
}

#[tokio::test]
async fn every_api_route_and_method_denies_a_request_without_a_valid_credential() {
    let (protected, public) = declared_routes();

    // Nothing new is public by accident.
    let public: BTreeSet<&str> = public.iter().map(String::as_str).collect();
    assert_eq!(
        public,
        PUBLIC.into_iter().collect::<BTreeSet<_>>(),
        "a route outside the auth layer must be added to PUBLIC on purpose"
    );
    let main_public = literals_after(MAIN_SOURCE, ".route_service(");
    assert_eq!(
        main_public,
        ["/status.json"],
        "main.rs adds only the public snapshot outside the API"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let (app, db) = authed_router(tmp.path()).await;

    // A body that would *do* something if it got past the layer: create a monitor, write
    // a backup, open a maintenance window, edit, pause.
    let backup = tmp.path().join("leaked-backup.db");
    let body = serde_json::json!({
        "name": "unauthorized", "url": "https://example.com/", "paused": true,
        "path": backup.display().to_string(),
        "starts_at": "2026-01-01T00:00:00Z", "ends_at": "2027-01-01T00:00:00Z",
    })
    .to_string();
    let id = "0190a000-0000-7000-8000-000000000001";

    let mut inventory: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for template in &protected {
        let path = template.replace("{id}", id);
        let methods = allowed_methods(&app, &path).await;
        assert!(!methods.is_empty(), "{template}: no methods routed");
        for method in &methods {
            assert_denied(&app, method, template, &path, &body).await;
        }
        inventory.insert(template.clone(), methods);
    }

    // The inventory really is the management surface (the parser and `Allow` both work).
    for (template, method) in [
        ("/api/monitors", "GET"),
        ("/api/monitors", "POST"),
        ("/api/monitors/{id}", "GET"),
        ("/api/monitors/{id}", "PATCH"),
        ("/api/monitors/{id}", "DELETE"),
        ("/api/monitors/{id}/checks", "GET"),
        ("/api/monitors/{id}/stats", "GET"),
        ("/api/monitors/{id}/maintenance", "GET"),
        ("/api/monitors/{id}/maintenance", "POST"),
        ("/api/maintenance/{id}", "DELETE"),
        ("/api/incidents", "GET"),
        ("/api/backup", "POST"),
    ] {
        assert!(
            inventory.get(template).is_some_and(|m| m.contains(method)),
            "{method} {template} missing from the inventory {inventory:?}"
        );
    }

    // Nothing happened: no monitor, no backup file.
    assert!(db.list_monitors().await.expect("list").is_empty());
    assert!(!backup.exists(), "an unauthenticated backup was written");

    // And the same requests with the token are not refused by the layer — the 401s above
    // are the credential's doing, not a broken route.
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/monitors")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
}
