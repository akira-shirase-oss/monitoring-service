//! Every management route denies a request without a valid credential — proved for the
//! whole inventory, not for one sample route.
//!
//! The inventory comes from the router itself, so a new route can't be missed:
//! - **paths** are read from `routes()`'s source (every `.route("…")` literal, inside and
//!   outside the auth layer) and `app()`'s (the snapshot and the dashboard fallback); a
//!   route added outside the layer that isn't on the short public list fails here, and a
//!   routing call anywhere else in the crate fails `routes_are_assembled_only_in_routes_and_app`;
//! - **methods** come from the live router: an authorized request with a method nobody
//!   routes gets axum's 405 with an `Allow` header naming every method routed on that
//!   path (without a credential even that 405 is a 401 — the route list isn't public).
//!
//! Then every (path, method) is sent to the assembled `app()` — exactly what `main.rs`
//! serves — with no credential, a wrong bearer, a malformed one
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
use monitor_domain::WorkspaceId;
use tower::ServiceExt;

const ROUTER_SOURCE: &str = include_str!("../src/lib.rs");
const TOKEN: &str = "inventory-token-0123456789abcdef0123456789";

/// The routes that are public by design: health, the build version, and the
/// endpoints that exchange a credential for a session (login, signup, logout —
/// they enforce their own rules: invite validation, throttling, password work).
const PUBLIC: [&str; 7] = [
    "/livez",
    "/readyz",
    "/version",
    "/api/login",
    "/api/signup",
    "/api/logout",
    "/s/{slug}/status.json",
];

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

/// (protected paths, public paths) as `routes()` declares them.
#[expect(
    clippy::expect_used,
    reason = "test helper: a source it can't read is a failed test"
)]
fn declared_routes() -> (Vec<String>, Vec<String>) {
    let start = ROUTER_SOURCE
        .find("fn routes(")
        .expect("routes() exists in lib.rs");
    let body = &ROUTER_SOURCE[start..];
    let body = &body[..body.find("\n}\n").expect("routes() ends")];
    for unsupported in [".merge(", ".nest_service(", ".route_service(", ".fallback"] {
        assert!(
            !body.contains(unsupported),
            "routes() uses `{unsupported}`: teach this inventory test about it first"
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
    std::fs::create_dir_all(dir.join("dashboard")).expect("dashboard dir");
    std::fs::write(dir.join("dashboard/200.html"), "<!doctype html>").expect("fallback page");
    let state = monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry: tokio::sync::watch::channel(Arc::new(Vec::new())).0,
        vitals: monitor_server::vitals::Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: Some(Arc::from(TOKEN)),
        session_ttl: Duration::from_hours(1),
        session_idle: Duration::from_hours(1),
        password_work_wait: Duration::from_secs(600),
        token_use: std::sync::Arc::default(),
        status_dir: dir.join("status"),
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
    // The assembled app, exactly as main.rs serves it (snapshot route, dashboard fallback,
    // hardening headers).
    let app = monitor_server::app(
        state,
        &dir.join("status"),
        &dir.join("dashboard"),
        &monitor_server::headers::dashboard_csp(&dir.join("dashboard")),
    );
    (app, db)
}

/// A fresh client address for every probe.
fn next_peer() -> std::net::SocketAddr {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::net::SocketAddr::from((std::net::Ipv4Addr::from(0x0A00_0000 + n), 40000))
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
        (
            "unknown session",
            Some(("cookie", "__Host-smon_session=deadbeef")),
        ),
    ] {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).expect("method"))
            .uri(path)
            .header("content-type", "application/json");
        if let Some((name, value)) = header {
            request = request.header(name, value);
        }
        let mut request = request.body(Body::from(body.to_owned())).expect("request");
        // Each probe from its own address: what is under test here is the credential
        // check, not the per-address guessing budget (that is limits.rs).
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(next_peer()));
        let response = app.clone().oneshot(request).await.expect("infallible");
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
    let (_, app_body) = fn_body(ROUTER_SOURCE, "app").expect("lib.rs app()");
    assert_eq!(
        literals_after(app_body, ".route_service("),
        ["/status.json"],
        "app() adds only the public snapshot (and the dashboard fallback) outside the API"
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
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_monitors()
            .await
            .expect("list")
            .is_empty()
    );
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

/// Calls that add a route, a nested router or a fallback.
const ROUTING: [&str; 8] = [
    ".route(",
    ".route_service(",
    ".nest(",
    ".nest_service(",
    ".merge(",
    ".fallback(",
    ".fallback_service(",
    ".method_not_allowed_fallback(",
];

/// The body of `fn <name>(` in `source`, up to the closing brace at column 0.
fn fn_body<'a>(source: &'a str, name: &str) -> Option<(usize, &'a str)> {
    let start = source.find(&format!("fn {name}("))?;
    let len = source[start..].find("\n}\n")?;
    Some((start, &source[start..start + len]))
}

/// L3: the inventory read `router()` only, so a route added where the app is assembled
/// (a `.route` in `main.rs`) was public and went unnoticed. Every routing call in the
/// crate — any file under `src/`, found by listing it — must sit in `routes()` or `app()`
/// in lib.rs, which is what the inventory reads and what `main.rs` serves.
#[test]
fn routes_are_assembled_only_in_routes_and_app() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .unwrap_or_else(|e| panic!("read src/: {e}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    files.sort();
    let allowed: Vec<(usize, usize)> = ["routes", "app"]
        .iter()
        .filter_map(|name| fn_body(ROUTER_SOURCE, name).map(|(at, body)| (at, at + body.len())))
        .collect();
    assert_eq!(allowed.len(), 2, "lib.rs defines routes() and app()");
    for path in files {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let full = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        // Unit-test modules (at the end of a file, by convention) build fixture routers of
        // their own; they are not served.
        let text = full
            .find("#[cfg(test)]\nmod tests")
            .map_or(full.as_str(), |at| &full[..at]);
        for call in ROUTING {
            for (at, _) in text.match_indices(call) {
                let line = text[..at].lines().count();
                let inside =
                    name == "lib.rs" && allowed.iter().any(|(s, e)| (*s..*e).contains(&at));
                assert!(
                    inside,
                    "{name}:{line}: `{call}` outside lib.rs routes()/app() escapes the auth inventory"
                );
            }
        }
    }
}
