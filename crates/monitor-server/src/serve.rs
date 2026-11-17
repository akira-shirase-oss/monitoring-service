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
    /// A response write (or flush) that makes no progress for this long closes the
    /// connection: a client that never reads can't keep its slot (`SMON_WRITE_STALL_MS`).
    pub write_stall: Duration,
    /// Requests served on one connection; then it is closed after the current response,
    /// so pipelining can't keep one connection busy for ever.
    pub max_requests_per_connection: usize,
    /// How long one connection may live, however busy; then it is closed after the
    /// current response.
    pub max_connection_age: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(10),
            max_connections: 1024,
            write_stall: Duration::from_secs(10),
            max_requests_per_connection: 1000,
            max_connection_age: Duration::from_secs(300),
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
        connections.spawn(serve_connection(
            stream,
            peer,
            app.clone(),
            permit,
            stop.clone(),
            limits,
        ));
    }
}

/// One connection, under every per-connection bound: the header deadline (hyper), the
/// write-stall deadline ([`StallGuard`]), the request cap and the maximum age. The last
/// two close it *gracefully* — after the response in flight — and if even that response
/// can't be written, the stall deadline ends it.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    app: axum::Router,
    permit: tokio::sync::OwnedSemaphorePermit,
    stop: CancellationToken,
    limits: Limits,
) {
    let _permit = permit;
    let opened = tokio::time::Instant::now();
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Each request carries its peer address (`ConnectInfo`, for per-client throttling).
    // The response that reaches the request cap — or is sent past the maximum age — says
    // `Connection: close`, and hyper closes after writing it: exact even when the client
    // pipelines (hyper may serve many buffered requests in one poll).
    let service = {
        let served = Arc::clone(&served);
        let cap = limits.max_requests_per_connection.max(1);
        let max_age = limits.max_connection_age;
        TowerToHyperService::new(tower::service_fn(
            move |mut request: axum::http::Request<hyper::body::Incoming>| {
                let app = app.clone();
                let last = served.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 >= cap
                    || opened.elapsed() >= max_age;
                async move {
                    request.extensions_mut().insert(ConnectInfo(peer));
                    let mut response = app.oneshot(request).await?;
                    if last {
                        response.headers_mut().insert(
                            axum::http::header::CONNECTION,
                            axum::http::HeaderValue::from_static("close"),
                        );
                    }
                    Ok::<_, std::convert::Infallible>(response)
                }
            },
        ))
    };
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout)
        .serve_connection(
            TokioIo::new(StallGuard::new(stream, limits.write_stall)),
            service,
        );
    let mut connection = std::pin::pin!(connection);
    let reason = tokio::select! {
        result = connection.as_mut() => {
            if let Err(error) = result {
                tracing::debug!(%error, %peer, "connection ended");
            }
            return;
        }
        () = stop.cancelled() => "shutting down",
        // A connection idle at its age is closed here; a busy one by its next response.
        () = tokio::time::sleep(limits.max_connection_age) => "maximum age",
    };
    tracing::debug!(%peer, reason, "closing the connection after its current response");
    // Finish the response in flight, then close. A write that stalls ends it at the stall
    // deadline; the caller bounds the shutdown case as a whole.
    connection.as_mut().graceful_shutdown();
    if let Err(error) = connection.await {
        tracing::debug!(%error, %peer, "connection ended");
    }
}

/// The connection's socket, with a write deadline: a `poll_write`/`poll_flush`/
/// `poll_shutdown` that makes no progress for `stall` fails with `TimedOut`, which ends
/// the connection. Progress (any byte accepted) resets it. Reads are untouched — the
/// header deadline covers those.
struct StallGuard<T> {
    inner: T,
    stall: Duration,
    deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<T> StallGuard<T> {
    fn new(inner: T, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            deadline: None,
        }
    }

    /// The write side is pending: arm the deadline (once) and fail when it passes.
    fn stalled<R>(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<R>> {
        let stall = self.stall;
        let deadline = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(stall)));
        match deadline.as_mut().poll(cx) {
            std::task::Poll::Ready(()) => std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "response write made no progress",
            ))),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    /// The write side moved: disarm.
    fn progressed<R>(&mut self, ready: std::io::Result<R>) -> std::task::Poll<std::io::Result<R>> {
        self.deadline = None;
        std::task::Poll::Ready(ready)
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for StallGuard<T> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for StallGuard<T> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_write(cx, buf) {
            std::task::Poll::Ready(result) => this.progressed(result),
            std::task::Poll::Pending => this.stalled(cx),
        }
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
            std::task::Poll::Ready(result) => this.progressed(result),
            std::task::Poll::Pending => this.stalled(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_flush(cx) {
            std::task::Poll::Ready(result) => this.progressed(result),
            std::task::Poll::Pending => this.stalled(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_shutdown(cx) {
            std::task::Poll::Ready(result) => this.progressed(result),
            std::task::Poll::Pending => this.stalled(cx),
        }
    }
}
