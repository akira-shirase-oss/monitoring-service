//! `monitor-server`: the monitor's one process — management API, coordinator, probe workers
//! and the single-writer task.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use monitor_db::SharedStatuses;
use monitor_domain::Health;
use monitor_server::probe::{ProbeClient, ProbePolicy};
use monitor_server::schedule::{Config as CoordinatorConfig, Coordinator};
use monitor_server::{AppState, load_registry, router};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower_http::services::{ServeDir, ServeFile};
use tracing_subscriber::EnvFilter;

/// Default management/bind port (58400; the dashboard's vite dev server is 58600).
const DEFAULT_LISTEN: &str = "127.0.0.1:58400";
const DEFAULT_DATABASE: &str = "sqlite:monitor.db";
/// Writer channel depth: the coordinator parks behind a full queue instead of dropping
/// evidence — a bounded queue is the honest backpressure point.
const WRITER_BOUND: usize = 64;
/// Stored observations replayed per monitor on boot; streaks never exceed the thresholds.
const REBUILD_TAIL: i64 = 4;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let database_url =
        std::env::var("SMON_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE.into());
    let listen = std::env::var("SMON_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.into());
    // Where the built dashboard lives; skip static serving when it has not been built.
    let dashboard_dir =
        std::env::var("SMON_DASHBOARD_DIR").unwrap_or_else(|_| "apps/dashboard/build".into());
    // `SMON_PROFILE=lab` permits loopback/private literal targets for local fixtures.
    let lab = std::env::var("SMON_PROFILE").is_ok_and(|profile| profile == "lab");

    let db = monitor_db::Db::connect(&database_url, 4).await?;
    db.migrate().await?;

    // Boot rebuild: streaks and incidents come back from the store, never from memory.
    let monitors = load_registry(&db)
        .await
        .context("loading the monitor registry")?;
    let mut health = HashMap::with_capacity(monitors.len());
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    for monitor in &monitors {
        let open = db.open_incident(monitor.id).await?.is_some();
        let tail = db.recent_outcomes(monitor.id, REBUILD_TAIL).await?;
        let rebuilt = Health::rebuild(open, &tail);
        statuses
            .write()
            .map_err(|_| anyhow::anyhow!("status map poisoned at boot"))?
            .insert(monitor.id, rebuilt.status());
        health.insert(monitor.id, rebuilt);
    }

    let (writer, _writer_task) =
        monitor_db::writer::spawn(db.clone(), Arc::clone(&statuses), health, WRITER_BOUND);
    let (registry_tx, registry_rx) = watch::channel(Arc::new(monitors));

    let probes = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: lab,
        },
        64 * 1024,
    )
    .context("building the probe client")?;

    let shutdown = CancellationToken::new();
    let _coordinator_task = Coordinator::spawn(
        registry_rx,
        writer,
        probes,
        CoordinatorConfig::default(),
        shutdown.clone(),
    );

    let mut app = router(AppState {
        db,
        statuses,
        registry: registry_tx,
        version: env!("CARGO_PKG_VERSION"),
    });

    if Path::new(&dashboard_dir).is_dir() {
        let index = ServeFile::new(format!("{dashboard_dir}/200.html"));
        app = app.fallback_service(ServeDir::new(&dashboard_dir).fallback(index));
        tracing::info!(dir = %dashboard_dir, "serving the dashboard build");
    } else {
        tracing::info!(dir = %dashboard_dir, "dashboard not built; API only");
    }

    let addr: SocketAddr = listen.parse().context("SMON_LISTEN must be host:port")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, database = %redact_sqlite(&database_url), lab, "monitor-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
}

async fn shutdown_signal(coordinator_stop: CancellationToken) {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for ctrl-c");
        return;
    }
    tracing::info!("shutting down");
    coordinator_stop.cancel();
}

/// The database URL is local-only, but keep a habit: never log a URL whole — parameters may
/// one day carry a credential.
fn redact_sqlite(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}
