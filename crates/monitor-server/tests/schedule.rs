//! Coordinator end-to-end tests: real SQLite, the real writer task and real probe workers
//! against an ephemeral fixture. The registry monitor carries a sub-second interval for test
//! speed — the scheduler only reads the struct; the stored row (15 s floor) exists for the
//! writer's generation check.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use monitor_db::{Db, writer};
use monitor_domain::{Monitor, NewMonitor};
use monitor_server::probe::{ProbeClient, ProbePolicy};
use monitor_server::schedule::{Config, Coordinator};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use url::Url;

fn fast_config() -> Config {
    Config {
        tick: Duration::from_millis(20),
        dispatch_grace: Duration::from_millis(60),
        global_slots: 4,
        per_origin_slots: 2,
        results_bound: 64,
        shutdown_deadline: Duration::from_millis(500),
    }
}

async fn fixture(router: Router) -> anyhow::Result<Url> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(axum::serve(listener, router).into_future());
    Ok(Url::parse(&format!("http://127.0.0.1:{port}/"))?)
}

/// Registers a monitor row (the 15 s schema floor) then returns the registry copy the
/// scheduler will actually use — same id and generation, fast timings for the test.
async fn monitor(
    db: &Db,
    url: Url,
    interval: Duration,
    timeout: Duration,
) -> anyhow::Result<Monitor> {
    let stored = NewMonitor::new("fixture", url.clone())?.into_monitor();
    db.insert_monitor(&stored).await?;
    let mut scheduled = stored.clone();
    scheduled.interval = interval;
    scheduled.timeout = timeout;
    Ok(scheduled)
}

/// Builds a memory store — one `Db` per test, because each `Db::connect("sqlite::memory:")`
/// is a *different* private database.
async fn store() -> anyhow::Result<Db> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    Ok(db)
}

fn rig(
    db: &Db,
    monitors: Vec<Monitor>,
) -> anyhow::Result<(watch::Sender<Arc<Vec<Monitor>>>, CancellationToken)> {
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (handle, _writer) = writer::spawn(db.clone(), statuses, HashMap::new(), Arc::default(), 32);
    let (registry_tx, registry_rx) = watch::channel(Arc::new(monitors));
    let probes = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: true,
        },
        64 * 1024,
    )?;
    let shutdown = CancellationToken::new();
    let _coordinator =
        Coordinator::spawn(registry_rx, handle, probes, fast_config(), shutdown.clone());
    Ok((registry_tx, shutdown))
}

async fn outcomes(db: &Db, monitor: &Monitor) -> anyhow::Result<Vec<String>> {
    let checks = db.list_checks(monitor.id, 200).await?;
    Ok(checks.into_iter().map(|row| row.outcome).collect())
}

#[tokio::test]
async fn slots_fire_and_commit_real_checks() -> anyhow::Result<()> {
    let url = fixture(Router::new().route("/ok", get(|| async { "ok" }))).await?;
    let db = store().await?;
    let monitor = monitor(
        &db,
        url.join("ok")?,
        Duration::from_millis(80),
        Duration::from_millis(250),
    )
    .await?;
    let (_tx, shutdown) = rig(&db, vec![monitor.clone()])?;

    tokio::time::sleep(Duration::from_millis(700)).await;
    shutdown.cancel();

    let outcomes = outcomes(&db, &monitor).await?;
    assert!(
        outcomes.iter().filter(|o| *o == "pass").count() >= 3,
        "expected several committed passes, got {outcomes:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_in_flight_probe_turns_later_slots_into_misses() -> anyhow::Result<()> {
    // Target takes ~400 ms to answer; slots are 80 ms apart — overlapping slots must be
    // recorded as misses, never run concurrently.
    let url = fixture(Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            "slow"
        }),
    ))
    .await?;
    let db = store().await?;
    // The probe deadline (800 ms) exceeds the handler's sleep, so each slot completes — the
    // test isolates overlap → missed, not timeout.
    let monitor = monitor(
        &db,
        url.join("slow")?,
        Duration::from_millis(80),
        Duration::from_millis(800),
    )
    .await?;
    let (_tx, shutdown) = rig(&db, vec![monitor.clone()])?;

    tokio::time::sleep(Duration::from_millis(900)).await;
    shutdown.cancel();

    let outcomes = outcomes(&db, &monitor).await?;
    let misses = outcomes.iter().filter(|o| *o == "missed").count();
    let passes = outcomes.iter().filter(|o| *o == "pass").count();
    assert!(misses >= 1, "expected misses, got {outcomes:?}");
    assert!(passes >= 1, "expected passes, got {outcomes:?}");
    Ok(())
}

#[tokio::test]
async fn registry_updates_schedule_a_new_monitor() -> anyhow::Result<()> {
    let url = fixture(Router::new().route("/ok", get(|| async { "ok" }))).await?;
    let db = store().await?;
    let (registry, shutdown) = rig(&db, Vec::new())?;

    let monitor = monitor(
        &db,
        url.join("ok")?,
        Duration::from_millis(80),
        Duration::from_millis(250),
    )
    .await?;
    if registry.send(Arc::new(vec![monitor.clone()])).is_err() {
        anyhow::bail!("coordinator not listening");
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.cancel();

    let outcomes = outcomes(&db, &monitor).await?;
    assert!(
        outcomes.iter().any(|o| o == "pass"),
        "expected the new monitor to be probed, got {outcomes:?}"
    );
    Ok(())
}
