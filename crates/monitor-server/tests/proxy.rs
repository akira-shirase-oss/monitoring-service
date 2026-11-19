//! Proxy environment variables must not route probes (or alerts) anywhere.
//!
//! reqwest honours `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` by default. A proxied request is
//! resolved by the *proxy*, so the probe's DNS-layer SSRF guard never sees the name: with
//! a proxy set, `http://metadata.internal.invalid/…` "passed" with whatever the proxy
//! answered. The test re-runs itself as a child process with the variables set (edition
//! 2024 makes `set_var` unsafe, and this workspace forbids unsafe code); the parent runs a
//! recording "proxy" that must see no connection at all.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use monitor_db::{Db, Observation, writer};
use monitor_domain::{CheckOutcome, NewMonitor, SlotObservation, WorkspaceId};
use monitor_server::deliver::{Config, Deliverer, Sink};
use monitor_server::probe::{ProbeClient, ProbePolicy};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

const CHILD: &str = "SMON_TEST_PROXY_CHILD";
const TEST: &str = "proxy_environment_variables_route_nothing";

#[test]
fn proxy_environment_variables_route_nothing() {
    if std::env::var_os(CHILD).is_some() {
        child().expect("the child run");
        return;
    }
    // The "proxy": accepts, records, answers 200 to anything — a proxied probe would pass.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind recorder");
    let proxy = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                seen.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0_u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
            }
        });
    }
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1");
    for name in ["NO_PROXY", "no_proxy"] {
        command.env_remove(name);
    }
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env(name, &proxy);
    }
    let output = command.output().expect("run the child");
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "the proxy from the environment saw a request — it would have answered for the target"
    );
}

/// Runs with the proxy variables set.
fn child() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        // A probe of a name under the strict profile must resolve it itself (and refuse or
        // fail), never hand it to a proxy.
        let probes = ProbeClient::new(
            ProbePolicy {
                allow_local_targets: false,
                extra_roots: Vec::new(),
            },
            64 * 1024,
        )?;
        let url = url::Url::parse("http://metadata.internal.invalid/latest/meta-data")?;
        let result = probes.probe(&url, Duration::from_secs(3)).await;
        assert_ne!(result.outcome, CheckOutcome::Pass, "{result:?}");

        // An alert goes straight to its configured receiver.
        let received = Arc::new(AtomicUsize::new(0));
        let stub = {
            let received = Arc::clone(&received);
            axum::Router::new().route(
                "/",
                axum::routing::post(move || {
                    let received = Arc::clone(&received);
                    async move {
                        received.fetch_add(1, Ordering::SeqCst);
                        ""
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let target = format!("http://{}/", listener.local_addr()?);
        tokio::spawn(axum::serve(listener, stub).into_future());

        let db = Db::connect("sqlite::memory:", 1).await?;
        db.migrate().await?;
        let monitor =
            NewMonitor::new("proxied", url::Url::parse("https://example.com")?)?.into_monitor();
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&monitor)
            .await?;
        let (writer, _task) = writer::spawn(
            db.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            HashMap::new(),
            Arc::default(),
            8,
            false,
        );
        for _ in 0..3 {
            writer
                .record(Observation {
                    monitor_id: monitor.id,
                    workspace: monitor.workspace,
                    generation: monitor.generation,
                    slot_at: OffsetDateTime::now_utc(),
                    slot: SlotObservation::Probe(CheckOutcome::Transport),
                    http_status: None,
                    latency_ms: None,
                    detail: None,
                })
                .await?;
        }
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(
            db,
            writer,
            Config {
                poll_tick: Duration::from_millis(30),
                attempt_deadline: Duration::from_millis(500),
                max_attempts: 3,
                backoff_base: Duration::from_millis(20),
                backoff_cap: Duration::from_millis(200),
                concurrency: 2,
                batch: 8,
            },
            Sink::Webhook {
                target: url::Url::parse(&target)?,
                secret: None,
            },
            shutdown.clone(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while received.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        shutdown.cancel();
        let _ = task.await;
        assert!(
            received.load(Ordering::SeqCst) >= 1,
            "the alert must reach its receiver directly"
        );
        Ok(())
    })
}
