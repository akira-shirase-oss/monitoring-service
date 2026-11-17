//! `serve` stops on an OS signal *or* an engine failure. A dead writer or coordinator used
//! to leave the process alive and "unavailable" forever, so systemd's `Restart=on-failure`
//! never fired: honest, but never healed (D-011).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use monitor_server::serve::{Limits, Stopped, serve};
use monitor_server::vitals::{Role, Supervisor, Vitals};
use tokio_util::sync::CancellationToken;

struct Rig {
    supervisor: Supervisor,
    vitals: Vitals,
    shutdown: CancellationToken,
    listener: tokio::net::TcpListener,
}

async fn rig() -> anyhow::Result<Rig> {
    let vitals = Vitals::new(Arc::default(), Duration::from_secs(30));
    let shutdown = CancellationToken::new();
    Ok(Rig {
        supervisor: Supervisor::new(vitals.clone(), shutdown.clone()),
        vitals,
        shutdown,
        listener: tokio::net::TcpListener::bind("127.0.0.1:0").await?,
    })
}

fn app() -> Router {
    Router::new()
        .route("/livez", get(|| async { "alive" }))
        // A response far larger than the socket buffers: a client that doesn't read it
        // leaves the server's write pending.
        .route("/big", get(|| async { vec![b'x'; 4 * 1024 * 1024] }))
}

#[tokio::test]
async fn an_observing_task_stopping_ends_serving_as_an_engine_failure() -> anyhow::Result<()> {
    let rig = rig().await?;
    rig.supervisor.watch(
        "writer",
        Role::Observing,
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            panic!("writer bug");
        }),
    );
    let stopped = tokio::time::timeout(
        Duration::from_secs(10),
        serve(
            rig.listener,
            app(),
            std::future::pending(), // no OS signal ever arrives
            rig.supervisor.engine_failure(),
            rig.shutdown.clone(),
            rig.vitals,
            Duration::from_secs(2),
            Limits::default(),
        ),
    )
    .await
    .expect("a dead writer must end serving, not leave the process up forever")?;
    assert_eq!(stopped, Stopped::EngineFailed("writer panicked".to_owned()));
    assert!(rig.shutdown.is_cancelled(), "the engine is told to stop");
    Ok(())
}

#[tokio::test]
async fn a_supporting_task_stopping_keeps_serving() -> anyhow::Result<()> {
    let rig = rig().await?;
    let addr = rig.listener.local_addr()?;
    rig.supervisor
        .watch("snapshot", Role::Supporting, tokio::spawn(async {}));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(
        rig.listener,
        app(),
        async move {
            let _ = stop_rx.await;
            "SIGTERM"
        },
        rig.supervisor.engine_failure(),
        rig.shutdown.clone(),
        rig.vitals,
        Duration::from_secs(2),
        Limits::default(),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !server.is_finished(),
        "a degraded monitor keeps observing and serving"
    );
    let alive = tokio::net::TcpStream::connect(addr).await;
    assert!(alive.is_ok(), "still accepting connections");
    drop(alive);

    stop_tx.send(()).expect("signal");
    let stopped = tokio::time::timeout(Duration::from_secs(10), server).await???;
    assert_eq!(stopped, Stopped::Signal("SIGTERM"));
    Ok(())
}

/// A coordinator that is alive but stuck never "stops", so it used to stay `unavailable`
/// forever. The stall watchdog turns a stall longer than its limit into the same engine
/// failure as a dead task — exit, restart, record the gap.
#[tokio::test]
async fn a_coordinator_stalled_past_the_limit_ends_serving_as_an_engine_failure()
-> anyhow::Result<()> {
    let vitals = Vitals::new(Arc::default(), Duration::from_millis(20));
    let shutdown = CancellationToken::new();
    let supervisor = Supervisor::new(vitals.clone(), shutdown.clone());
    vitals.coordinator_beat(); // one pass, then silence
    supervisor.watch_stall(Duration::from_millis(150), Duration::from_millis(20));
    let stopped = tokio::time::timeout(
        Duration::from_secs(10),
        serve(
            tokio::net::TcpListener::bind("127.0.0.1:0").await?,
            app(),
            std::future::pending(),
            supervisor.engine_failure(),
            shutdown,
            vitals,
            Duration::from_secs(2),
            Limits::default(),
        ),
    )
    .await
    .expect("a stalled coordinator must end serving, not stay unavailable forever")?;
    let Stopped::EngineFailed(cause) = stopped else {
        panic!("expected an engine failure, got {stopped:?}");
    };
    assert!(cause.starts_with("coordinator stalled"), "{cause}");
    Ok(())
}

/// A healthy coordinator keeps beating; the watchdog stays quiet.
#[tokio::test]
async fn a_beating_coordinator_is_never_called_stalled() -> anyhow::Result<()> {
    let vitals = Vitals::new(Arc::default(), Duration::from_millis(20));
    let shutdown = CancellationToken::new();
    let supervisor = Supervisor::new(vitals.clone(), shutdown.clone());
    supervisor.watch_stall(Duration::from_millis(150), Duration::from_millis(20));
    let failure = supervisor.engine_failure();
    for _ in 0..30 {
        vitals.coordinator_beat();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let raised = tokio::time::timeout(Duration::from_millis(1), failure.wait()).await;
    assert!(
        raised.is_err(),
        "no failure while the coordinator keeps beating"
    );
    shutdown.cancel();
    assert!(supervisor.drain(Duration::from_secs(2)).await);
    Ok(())
}

/// A frozen *process* (SIGSTOP, a VM pause, a cgroup freezer) is not a stalled
/// coordinator. On resume the watchdog used to fire before the healthy coordinator could
/// beat (SIGSTOP for 130 s → exit 1 within 1 s). Here the whole runtime is blocked — every
/// task frozen at once, like SIGSTOP — past the stall limit, then resumes; the coordinator
/// keeps beating and no failure may be raised.
#[tokio::test(flavor = "current_thread")]
async fn a_process_freeze_is_not_mistaken_for_a_stalled_coordinator() {
    let vitals = Vitals::new(Arc::default(), Duration::from_millis(50));
    let shutdown = CancellationToken::new();
    let supervisor = Supervisor::new(vitals.clone(), shutdown.clone());
    let failure = supervisor.engine_failure();
    // The coordinator: a pass every 100 ms — its next deadline after the freeze falls
    // *after* the watchdog's, so the watchdog judges first on resume.
    vitals.coordinator_beat();
    let beating = {
        let vitals = vitals.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = ticks.tick() => vitals.coordinator_beat(),
                    () = shutdown.cancelled() => return,
                }
            }
        })
    };
    supervisor.watch_stall(Duration::from_millis(150), Duration::from_millis(20));
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Freeze everything for 400 ms (> the 150 ms limit).
    // Blocking the runtime thread is the point here: it freezes every task at once.
    std::thread::sleep(Duration::from_millis(400));

    // Resume and give both a few rounds.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let raised = tokio::time::timeout(Duration::from_millis(1), failure.wait()).await;
    shutdown.cancel();
    let _ = beating.await;
    assert!(
        raised.is_err(),
        "a freeze of the whole process must not read as a stall"
    );
}

/// Serve `app()` with `limits` on an ephemeral port; returns the address and a stop handle.
async fn serving(
    limits: Limits,
) -> anyhow::Result<(std::net::SocketAddr, tokio::sync::oneshot::Sender<()>)> {
    let rig = rig().await?;
    let addr = rig.listener.local_addr()?;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(
        rig.listener,
        app(),
        async move {
            let _ = stop_rx.await;
            "SIGTERM"
        },
        rig.supervisor.engine_failure(),
        rig.shutdown.clone(),
        rig.vitals,
        Duration::from_secs(2),
        limits,
    ));
    Ok((addr, stop_tx))
}

/// Write `bytes`, then wait (up to `limit`) for the server to close the connection.
/// Returns what it sent back and whether it closed.
async fn closed_within(
    addr: std::net::SocketAddr,
    bytes: &[u8],
    limit: Duration,
) -> anyhow::Result<(String, bool)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream.write_all(bytes).await?;
    let mut received = Vec::new();
    let closed = tokio::time::timeout(limit, async {
        let mut buf = [0_u8; 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await
    .is_ok();
    Ok((String::from_utf8_lossy(&received).into_owned(), closed))
}

/// M2: an anonymous client could hold sockets for ever — `axum::serve` set no header-read
/// timeout, no idle timeout and no cap (a half-sent header and an idle keep-alive both
/// stayed open 45 s+). Now a header that doesn't finish in time is closed, an idle
/// keep-alive is closed after the same deadline, and past the cap new connections wait.
#[tokio::test]
async fn a_slow_header_and_an_idle_keep_alive_are_closed_and_the_cap_holds() -> anyhow::Result<()> {
    let limits = Limits {
        header_read_timeout: Duration::from_millis(500),
        max_connections: 2,
        ..Limits::default()
    };
    let (addr, _stop) = serving(limits).await?;

    let (received, closed) = closed_within(
        addr,
        b"GET /livez HTTP/1.1\r\nHost: x\r\nX-Slow: ",
        Duration::from_secs(3),
    )
    .await?;
    assert!(closed, "a header that never finishes must be closed");
    assert!(!received.contains("alive"), "and never served: {received}");

    let (received, closed) = closed_within(
        addr,
        b"GET /livez HTTP/1.1\r\nHost: x\r\n\r\n",
        Duration::from_secs(3),
    )
    .await?;
    assert!(received.starts_with("HTTP/1.1 200"), "{received}");
    assert!(
        closed,
        "an idle keep-alive must be closed after the deadline"
    );

    // Two silent connections fill the cap: a third is not served until one is closed.
    let holders = [
        tokio::net::TcpStream::connect(addr).await?,
        tokio::net::TcpStream::connect(addr).await?,
    ];
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (received, _) = closed_within(
        addr,
        b"GET /livez HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        Duration::from_millis(300),
    )
    .await?;
    assert!(received.is_empty(), "served past the cap: {received}");
    // The holders' header deadline frees their slots; then the next one is served.
    let (received, _) = closed_within(
        addr,
        b"GET /livez HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    )
    .await?;
    assert!(received.starts_with("HTTP/1.1 200"), "{received}");
    drop(holders);
    Ok(())
}

/// W1: a response write had no deadline. Clients that pipeline requests and never read
/// kept their connections — and every slot under the cap — for as long as they liked, so
/// nobody else was served. A write that makes no progress for `write_stall` now closes the
/// connection.
#[tokio::test]
async fn a_client_that_never_reads_loses_its_slot_after_the_write_stall() -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let limits = Limits {
        header_read_timeout: Duration::from_secs(5),
        max_connections: 2,
        write_stall: Duration::from_millis(300),
        ..Limits::default()
    };
    let (addr, _stop) = serving(limits).await?;
    let mut stallers = Vec::new();
    for _ in 0..2 {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream
            .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\n\r\nGET /big HTTP/1.1\r\nHost: x\r\n\r\n")
            .await?;
        stallers.push(stream); // never read
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (received, _) = closed_within(
        addr,
        b"GET /livez HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    )
    .await?;
    assert!(
        received.starts_with("HTTP/1.1 200"),
        "two non-reading clients still hold every slot: {received:?}"
    );
    drop(stallers);
    Ok(())
}

/// W1: pipelining kept one connection busy without stalling a write — so the stall
/// deadline alone isn't enough. A connection is closed after `max_requests_per_connection`.
#[tokio::test]
async fn a_connection_is_closed_after_its_request_cap() -> anyhow::Result<()> {
    let limits = Limits {
        max_requests_per_connection: 5,
        ..Limits::default()
    };
    let (addr, _stop) = serving(limits).await?;
    let burst = "GET /livez HTTP/1.1\r\nHost: x\r\n\r\n".repeat(20);
    let (received, closed) = closed_within(addr, burst.as_bytes(), Duration::from_secs(3)).await?;
    assert!(closed, "the connection stays open past its request cap");
    assert_eq!(received.matches("HTTP/1.1 200").count(), 5, "{received}");
    Ok(())
}

/// W1: a connection that keeps sending requests one by one never trips the header or the
/// write deadline; `max_connection_age` closes it anyway, after the current response.
#[tokio::test]
async fn a_busy_connection_is_closed_at_its_maximum_age() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let limits = Limits {
        max_connection_age: Duration::from_millis(800),
        ..Limits::default()
    };
    let (addr, _stop) = serving(limits).await?;
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let started = std::time::Instant::now();
    let mut served = 0;
    let closed = loop {
        if started.elapsed() > Duration::from_secs(4) {
            break false;
        }
        if stream
            .write_all(b"GET /livez HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .is_err()
        {
            break true;
        }
        let mut buf = [0_u8; 512];
        match tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => break true,
            Ok(Ok(_)) => served += 1,
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(closed, "a busy connection lived past its maximum age");
    assert!(served >= 3, "it was served until then ({served})");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "closed at about its age, not later: {:?}",
        started.elapsed()
    );
    Ok(())
}
