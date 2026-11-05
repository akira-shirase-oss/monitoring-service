//! `monitor-server`: the monitor's one process. Management API now; coordinator next.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context;
use monitor_server::{AppState, router};
use tower_http::services::{ServeDir, ServeFile};
use tracing_subscriber::EnvFilter;

/// Default management/bind port (58400; the dashboard's vite dev server is 58600).
const DEFAULT_LISTEN: &str = "127.0.0.1:58400";
const DEFAULT_DATABASE: &str = "sqlite:monitor.db";

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

    let db = monitor_db::Db::connect(&database_url, 4).await?;
    db.migrate().await?;

    let mut app = router(AppState {
        db,
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
    tracing::info!(%addr, database = %redact_sqlite(&database_url), "monitor-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for ctrl-c");
        return;
    }
    tracing::info!("shutting down");
}

/// The database URL is local-only, but keep a habit: never log a URL whole — parameters may
/// one day carry a credential.
fn redact_sqlite(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}
