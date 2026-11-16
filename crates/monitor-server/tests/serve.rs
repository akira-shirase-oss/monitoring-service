//! `serve` stops on an OS signal *or* an engine failure. A dead writer or coordinator used
//! to leave the process alive and "unavailable" forever, so systemd's `Restart=on-failure`
//! never fired: honest, but never healed (D-011).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use monitor_server::serve::{Stopped, serve};
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
    Router::new().route("/livez", get(|| async { "alive" }))
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
