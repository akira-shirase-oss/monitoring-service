//! Serving until something stops the process: an OS signal (a graceful stop), or an engine
//! failure — the writer or coordinator stopped (D-011). The second is not graceful: the
//! process drains like any stop, then its owner exits non-zero so the service manager
//! restarts it, and the boot records the gap (D-014). Honest "unavailable" says the monitor
//! cannot see; restarting is how it gets its sight back.
//!
//! Connections are served on hyper directly (M2), because the listener is the part an
//! anonymous client reaches before any request exists:
//! - **header-read timeout** ([`Limits::header_read_timeout`]): a request whose header has
//!   not arrived in full by then is closed. hyper arms the same deadline while a keep-alive
//!   connection waits for its next request, so it is the **idle timeout** too;
//! - **connection cap** ([`Limits::max_connections`], `SMON_MAX_CONNECTIONS`): a permit is
//!   taken *before* `accept`, so past the cap new connections wait in the kernel's backlog
//!   instead of taking file descriptors from probes, SQLite and the snapshot;
//! - accept errors (`EMFILE` and the like) back off and retry; they never end serving.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use crate::vitals::{EngineFailure, Vitals};

/// Why serving stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// A stop signal (`SIGTERM`, `SIGINT`, …) — exit 0 after the drain.
    Signal(&'static str),
    /// An observing task stopped — exit non-zero after the drain, so it gets restarted.
    EngineFailed(String),
}

/// What one client can hold on the listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// A request's header must arrive in full within this; an idle keep-alive connection is
    /// closed after it too.
    pub header_read_timeout: Duration,
    /// Connections served at once. Keep it well below the process's file-descriptor limit
    /// (the unit's `LimitNOFILE=4096`): probes, SQLite and the snapshot need theirs.
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(10),
            max_connections: 1024,
        }
    }
}

/// How long accept waits after an error (`EMFILE`, `ENFILE`, …) before trying again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Serve `app` until `signal` resolves or `failure` is raised, then drain in order:
/// readiness off and the engine told to stop at once (`begin_drain`, `cancel`), accept
/// stops, open connections finish their current request (graceful shutdown) within
/// `http_drain`, after which they are abandoned. The caller drains the engine tasks after.
#[expect(
    clippy::too_many_arguments,
    reason = "each is a distinct stop input or bound"
)]
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    signal: impl Future<Output = &'static str> + Send + 'static,
    failure: EngineFailure,
    shutdown: CancellationToken,
    vitals: Vitals,
    http_drain: Duration,
    limits: Limits,
) -> std::io::Result<Stopped> {
    let stop_accepting = CancellationToken::new();
    let connections = TaskTracker::new();
    let watcher = {
        let stop_accepting = stop_accepting.clone();
        async move {
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
            stop_accepting.cancel();
            stopped
        }
    };
    let (stopped, ()) = tokio::join!(
        watcher,
        accept_loop(&listener, &app, &stop_accepting, &connections, limits)
    );
    connections.close();
    if tokio::time::timeout(http_drain, connections.wait())
        .await
        .is_err()
    {
        tracing::warn!("HTTP drain deadline hit; abandoning open connections");
    }
    Ok(stopped)
}

/// Accept until `stop` — each connection only after a permit is free.
async fn accept_loop(
    listener: &tokio::net::TcpListener,
    app: &axum::Router,
    stop: &CancellationToken,
    connections: &TaskTracker,
    limits: Limits,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections.max(1)));
    let mut warned_full = false;
    loop {
        let permit = if let Ok(permit) = Arc::clone(&permits).try_acquire_owned() {
            permit
        } else {
            if !warned_full {
                tracing::warn!(
                    max = limits.max_connections,
                    "connection cap reached; new connections wait"
                );
                warned_full = true;
            }
            tokio::select! {
                permit = Arc::clone(&permits).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return, // the semaphore is never closed
                },
                () = stop.cancelled() => return,
            }
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "accept failed; backing off");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
            () = stop.cancelled() => return,
        };
        if permits.available_permits() > 0 {
            warned_full = false;
        }
        // Each request carries its peer address (`ConnectInfo`): credential throttling is
        // per client (throttle.rs).
        let service = TowerToHyperService::new(app.clone().map_request(
            move |mut request: axum::http::Request<hyper::body::Incoming>| {
                request.extensions_mut().insert(ConnectInfo(peer));
                request
            },
        ));
        let stop = stop.clone();
        connections.spawn(async move {
            let _permit = permit;
            let connection = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_read_timeout)
                .serve_connection(TokioIo::new(stream), service);
            let mut connection = std::pin::pin!(connection);
            tokio::select! {
                result = connection.as_mut() => {
                    if let Err(error) = result {
                        tracing::debug!(%error, %peer, "connection ended");
                    }
                }
                () = stop.cancelled() => {
                    // Finish the request in flight, then close; the caller bounds the wait.
                    connection.as_mut().graceful_shutdown();
                    let _ = connection.await;
                }
            }
        });
    }
}
