//! S1a tenancy isolation (D-020): a workspace can never read, edit, list, count or be
//! alerted about another workspace's monitors — through the store view or through the
//! API. The engine's sweep stays workspace-agnostic: observing is infrastructure, not a
//! tenant operation, and one workspace's slot coverage is unaffected by another's.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{Router, get};
use http_body_util::BodyExt;
use monitor_db::{Db, Observation, writer};
use monitor_domain::{CheckOutcome, Monitor, NewMonitor, SlotObservation, WorkspaceId};
use monitor_server::probe::{ProbeClient, ProbePolicy};
use monitor_server::schedule::{Config, Coordinator};
use monitor_server::vitals::Vitals;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use url::Url;

const A: WorkspaceId = WorkspaceId::DEFAULT;

async fn status_of(app: &axum::Router, method: &str, uri: &str) -> anyhow::Result<StatusCode> {
    let mut request = Request::builder().method(method).uri(uri);
    let body = if method == "GET" || method == "DELETE" {
        Body::empty()
    } else {
        request = request.header("content-type", "application/json");
        Body::from("{}")
    };
    Ok(app.clone().oneshot(request.body(body)?).await?.status())
}

async fn store() -> anyhow::Result<Db> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    Ok(db)
}

/// A second workspace, with a monitor that exists only in it.
async fn second_workspace(db: &Db) -> anyhow::Result<(WorkspaceId, Monitor)> {
    let ws = db.create_workspace("tenant-b", "Tenant B").await?;
    let monitor = NewMonitor::new("b's monitor", Url::parse("https://b.example/health")?)?
        .into_monitor_for(ws);
    db.workspace(ws).insert_monitor(&monitor).await?;
    Ok((ws, monitor))
}

fn own_monitor() -> anyhow::Result<Monitor> {
    Ok(NewMonitor::new("a's monitor", Url::parse("https://a.example/health")?)?.into_monitor())
}

async fn record(
    db: &Db,
    monitor: &Monitor,
    outcome: CheckOutcome,
) -> anyhow::Result<monitor_db::WriteOutcome> {
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (handle, _task) = writer::spawn(
        db.clone(),
        statuses,
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    Ok(handle
        .record(Observation {
            monitor_id: monitor.id,
            workspace: monitor.workspace,
            generation: monitor.generation,
            slot_at: OffsetDateTime::now_utc(),
            slot: SlotObservation::Probe(outcome),
            http_status: Some(500),
            latency_ms: Some(12),
            detail: None,
        })
        .await?)
}

/// Reads: list, get, checks, stats, incidents, windows and outbox are all scoped.
#[tokio::test]
async fn a_workspace_cannot_read_another_workspaces_rows() -> anyhow::Result<()> {
    let db = store().await?;
    let mine = own_monitor()?;
    db.workspace(A).insert_monitor(&mine).await?;
    let (b, foreign) = second_workspace(&db).await?;
    assert_ne!(A, b);

    for _ in 0..3 {
        record(&db, &foreign, CheckOutcome::HttpFail { status: 503 }).await?;
    }

    let a = db.workspace(A);
    let mine_ids: Vec<String> = a
        .list_monitors()
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(mine_ids, vec![mine.id.0.to_string()]);
    assert!(a.monitor_row(foreign.id).await?.is_none());
    assert!(a.monitor_gate(foreign.id).await?.is_none());
    assert!(a.monitor_generation(foreign.id).await?.is_none());
    assert!(a.list_checks(foreign.id, 50).await?.is_empty());
    let outcomes: Vec<SlotObservation> = a.recent_outcomes(foreign.id, 5).await?;
    assert!(outcomes.is_empty(), "{outcomes:?}");
    assert!(a.last_slot_at(foreign.id).await?.is_none());
    let stats = a
        .check_stats(
            foreign.id,
            OffsetDateTime::now_utc() - time::Duration::days(1),
        )
        .await?;
    assert_eq!((stats.observed, stats.missed), (0, 0));
    assert!(
        a.list_incidents(Some(foreign.id), None, 50)
            .await?
            .is_empty()
    );
    assert!(!a.open_incidents().await?.contains_key(&foreign.id));
    assert!(a.list_windows(foreign.id).await?.is_empty());
    assert!(a.list_outbox(foreign.id).await?.is_empty());
    assert!(!a.has_history(foreign.id).await?);
    assert_eq!(a.monitor_count().await?, 1);

    // And the foreign view sees exactly the other half — the rows are there, scoped.
    let bview = db.workspace(b);
    assert_eq!(bview.list_monitors().await?.len(), 1);
    assert_eq!(bview.list_checks(foreign.id, 50).await?.len(), 3);
    Ok(())
}

/// Writes: edit, delete, window-create, window-delete and insert are all scoped or refused.
#[tokio::test]
async fn a_workspace_cannot_write_another_workspaces_rows() -> anyhow::Result<()> {
    let db = store().await?;
    let (b, foreign) = second_workspace(&db).await?;
    let a = db.workspace(A);

    // A window planted under A's scope on B's monitor is "unknown monitor", never written.
    let window = a
        .create_window(
            foreign.id,
            OffsetDateTime::now_utc(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
            "not mine",
        )
        .await;
    assert!(matches!(window, Err(monitor_db::QueryError::Invalid(_))));

    let real_window = db
        .workspace(b)
        .create_window(
            foreign.id,
            OffsetDateTime::now_utc(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
            "theirs",
        )
        .await?;
    assert!(!a.delete_window(real_window.id).await?);
    assert!(db.workspace(b).delete_window(real_window.id).await?);

    // Edits and deletes through the wrong scope change nothing. A monitor that declares
    // workspace B cannot be written through A's view at all; one forged to look like A's
    // gets as far as the UPDATE, whose `AND workspace_id` matches nothing — no row moves.
    let mut foreign_edit = foreign.clone();
    foreign_edit.name = "renamed by A".to_owned();
    assert!(matches!(
        a.update_monitor(&foreign_edit, foreign.generation).await,
        Err(monitor_db::QueryError::Invalid(_))
    ));
    foreign_edit.workspace = A;
    assert_eq!(
        a.update_monitor(&foreign_edit, foreign.generation).await?,
        None
    );
    assert!(!a.delete_monitor(foreign.id).await?);
    assert_eq!(
        db.workspace(b).monitor_row(foreign.id).await?.unwrap().name,
        "b's monitor"
    );

    // An insert whose monitor disagrees with the view's scope is refused outright.
    let mut smuggled = own_monitor()?;
    smuggled.workspace = b;
    assert!(matches!(
        a.insert_monitor(&smuggled).await,
        Err(monitor_db::QueryError::Invalid(_))
    ));
    Ok(())
}

/// The API only serves the default workspace today (D-020): a monitor in workspace B is
/// as absent as one that never existed — 404 everywhere, invisible in every list.
#[tokio::test]
async fn the_api_never_reaches_another_workspaces_monitor() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(
        &format!("sqlite:{}", tmp.path().join("api.db").display()),
        1,
    )
    .await?;
    db.migrate().await?;
    let (registry, _rx) = watch::channel(Arc::new(Vec::new()));
    let app = monitor_server::router(monitor_server::AppState {
        db: db.clone(),
        statuses: Arc::new(RwLock::new(HashMap::new())),
        registry,
        vitals: Vitals::new(Arc::default(), Duration::from_secs(30)),
        admin_token: None,
        session_ttl: Duration::from_hours(12),
        session_idle: Duration::from_hours(1),
        password_work_wait: monitor_server::auth::PASSWORD_WORK_WAIT,
        token_use: std::sync::Arc::default(),
        status_dir: tmp.path().join("status"),
        test_sends: std::sync::Arc::default(),
        status_writes: std::sync::Arc::default(),
        status_slugs: std::sync::Arc::default(),
        status_slug_version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
    let (_b, foreign) = second_workspace(&db).await?;
    record(&db, &foreign, CheckOutcome::HttpFail { status: 500 }).await?;
    let id = foreign.id.0;

    assert_eq!(
        status_of(&app, "GET", &format!("/api/monitors/{id}")).await?,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(&app, "PATCH", &format!("/api/monitors/{id}")).await?,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(&app, "DELETE", &format!("/api/monitors/{id}")).await?,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(&app, "GET", &format!("/api/monitors/{id}/checks")).await?,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(&app, "GET", &format!("/api/monitors/{id}/stats")).await?,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(&app, "GET", &format!("/api/monitors/{id}/maintenance")).await?,
        StatusCode::NOT_FOUND
    );

    // Lists: B's monitor and B's incident are invisible.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/monitors")
                .body(Body::empty())
                .unwrap(),
        )
        .await?;
    let body = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body)?["monitors"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/incidents?monitor_id={id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await?;
    let body = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body)?["incidents"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    Ok(())
}

/// The engine observes across workspaces — tenancy is a boundary on requests, not on the
/// scheduler — and each workspace's coverage counts only its own slots.
#[tokio::test]
async fn the_engine_observes_every_workspace_with_independent_coverage() -> anyhow::Result<()> {
    let fixture = Router::new().route("/ok", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(axum::serve(listener, fixture).into_future());
    let url = Url::parse(&format!("http://127.0.0.1:{port}/ok"))?;

    let db = store().await?;
    let b = db.create_workspace("tenant-b", "Tenant B").await?;
    // Stored rows ride the 15 s schema floor; the scheduled structs get fast timings.
    let mut a_mon = NewMonitor::new("a", url.clone())?.into_monitor_for(A);
    let mut b_mon = NewMonitor::new("b", url)?.into_monitor_for(b);
    db.workspace(A).insert_monitor(&a_mon).await?;
    db.workspace(b).insert_monitor(&b_mon).await?;
    for scheduled in [&mut a_mon, &mut b_mon] {
        scheduled.interval = Duration::from_millis(80);
        scheduled.timeout = Duration::from_millis(250);
    }

    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (handle, _writer) = writer::spawn(
        db.clone(),
        statuses,
        HashMap::new(),
        Arc::default(),
        32,
        false,
    );
    let (_tx, registry_rx) = watch::channel(Arc::new(vec![a_mon.clone(), b_mon.clone()]));
    let shutdown = CancellationToken::new();
    let _coordinator = Coordinator::spawn(
        registry_rx,
        handle,
        ProbeClient::new(
            ProbePolicy {
                allow_local_targets: true,
                extra_roots: Vec::new(),
            },
            64 * 1024,
        )?,
        Config {
            tick: Duration::from_millis(20),
            dispatch_grace: Duration::from_millis(60),
            global_slots: 4,
            per_origin_slots: 2,
            results_bound: 64,
            shutdown_deadline: Duration::from_millis(500),
        },
        Vitals::new(Arc::default(), Duration::from_secs(30)),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    shutdown.cancel();

    // Both workspaces' monitors ran and committed — under their own workspace ids.
    let a_checks = db.workspace(A).list_checks(a_mon.id, 200).await?;
    let b_checks = db.workspace(b).list_checks(b_mon.id, 200).await?;
    assert!(
        a_checks.iter().any(|c| c.outcome == "pass"),
        "A: {a_checks:?}"
    );
    assert!(
        b_checks.iter().any(|c| c.outcome == "pass"),
        "B: {b_checks:?}"
    );

    // Coverage is per workspace: A cannot see B's committed slots, and vice versa.
    assert!(db.workspace(A).list_checks(b_mon.id, 200).await?.is_empty());
    assert!(db.workspace(b).list_checks(a_mon.id, 200).await?.is_empty());
    let a_stats = db
        .workspace(A)
        .check_stats(
            b_mon.id,
            OffsetDateTime::now_utc() - time::Duration::days(1),
        )
        .await?;
    assert_eq!((a_stats.observed, a_stats.missed), (0, 0));
    Ok(())
}
