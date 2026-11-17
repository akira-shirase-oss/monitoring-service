//! Serving until something stops the process: an OS signal (a graceful stop), or an engine
//! failure — the writer or coordinator stopped (D-011). The second is not graceful: the
//! process drains like any stop, then its owner exits non-zero so the service manager
//! restarts it, and the boot records the gap (D-014). Honest "unavailable" says the monitor
//! cannot see; restarting is how it gets its sight back.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::vitals::{EngineFailure, Vitals};

/// Why serving stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// A stop signal (`SIGTERM`, `SIGINT`, …) — exit 0 after the drain.
    Signal(&'static str),
    /// An observing task stopped — exit non-zero after the drain, so it gets restarted.
    EngineFailed(String),
}

/// Serve `app` until `signal` resolves or `failure` is raised, then drain in order:
/// readiness off and the engine told to stop at once (`begin_drain`, `cancel`), open HTTP
/// requests given `http_drain` — axum alone would wait for them forever. The caller drains
/// the engine tasks after.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    signal: impl Future<Output = &'static str> + Send + 'static,
    failure: EngineFailure,
    shutdown: CancellationToken,
    vitals: Vitals,
    http_drain: Duration,
) -> std::io::Result<Stopped> {
    // The reason is handed out of the shutdown future through a slot; the guard is never
    // held across an await.
    let reason = std::sync::Arc::new(std::sync::Mutex::new(None::<Stopped>));
    let slot = std::sync::Arc::clone(&reason);
    let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel::<()>();
    let graceful = async move {
        let stopped = tokio::select! {
            name = signal => Stopped::Signal(name),
            cause = failure.wait() => Stopped::EngineFailed(cause),
        };
        match &stopped {
            Stopped::Signal(name) => tracing::info!(signal = name, "shutting down"),
            Stopped::EngineFailed(cause) => tracing::error!(
                %cause,
                "monitoring engine failed; shutting down so the service manager restarts it"
            ),
        }
        // Readiness goes false before anything stops, and stays false.
        vitals.begin_drain();
        shutdown.cancel();
        *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stopped);
        let _ = stopping_tx.send(());
    };
    // Each request carries its peer address (`ConnectInfo`): credential throttling is
    // per client (throttle.rs).
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(graceful)
    .into_future();
    let deadline = async move {
        if stopping_rx.await.is_err() {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(http_drain).await;
    };
    tokio::select! {
        result = server => result?,
        () = deadline => tracing::warn!("HTTP drain deadline hit; abandoning open requests"),
    }
    let stopped = reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or(Stopped::Signal("server stopped"));
    Ok(stopped)
}
