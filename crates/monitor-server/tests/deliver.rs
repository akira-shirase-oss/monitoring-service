//! Deliverer integration tests: a real outbox row, a real HTTP stub receiver, the real
//! writer — the same wiring main.rs builds, driven at lab speed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use hmac::{Hmac, Mac};
use monitor_db::{Db, Observation, OutboxRow, writer};
use monitor_domain::{CheckOutcome, Monitor, NewMonitor, SlotObservation};
use monitor_server::deliver::{Config, Deliverer};
use sha2::Sha256;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Stub {
    hits: mpsc::Sender<(HeaderMap, Vec<u8>)>,
    /// What to answer, per request: failures first, then success. `hang_for` monitor ids get
    /// a reply that never arrives inside any reasonable deadline.
    fail_first: Arc<AtomicUsize>,
    hang_for: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn receiver(State(stub): State<Stub>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = http_body_util::BodyExt::collect(body)
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let monitor_id = parsed["monitor"]["id"].as_str().unwrap_or("").to_owned();
    let hangs = stub
        .hang_for
        .lock()
        .is_ok_and(|ids| ids.contains(&monitor_id));
    let _ = stub.hits.send((parts.headers, bytes)).await;
    if hangs {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    if stub
        .fail_first
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
    {
        let mut response = Response::new(Body::from("fail"));
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        return response;
    }
    Response::new(Body::empty())
}

/// Start a stub receiver on a loopback port; returns its base URL.
async fn stub(
    fail_first: usize,
    hang_for: Vec<String>,
) -> anyhow::Result<(String, mpsc::Receiver<(HeaderMap, Vec<u8>)>)> {
    let (hits_tx, hits_rx) = mpsc::channel(64);
    let app = Router::new().route("/", post(receiver)).with_state(Stub {
        hits: hits_tx,
        fail_first: Arc::new(AtomicUsize::new(fail_first)),
        hang_for: Arc::new(std::sync::Mutex::new(hang_for)),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://127.0.0.1:{port}/"), hits_rx))
}

async fn rig() -> anyhow::Result<(Db, writer::WriterHandle)> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (handle, _task) = writer::spawn(db.clone(), statuses, HashMap::new(), 8);
    Ok((db, handle))
}

/// Insert a monitor and drive it into an incident so an `incident_opened` outbox row exists.
async fn monitor_with_event(
    db: &Db,
    writer: &writer::WriterHandle,
    name: &str,
) -> anyhow::Result<(Monitor, OutboxRow)> {
    let monitor = NewMonitor::new(name, url::Url::parse("https://example.com")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    for _ in 0..3 {
        writer
            .record(Observation {
                monitor_id: monitor.id,
                generation: monitor.generation,
                slot_at: OffsetDateTime::now_utc(),
                slot: SlotObservation::Probe(CheckOutcome::Transport),
                http_status: None,
                latency_ms: None,
                detail: None,
            })
            .await?;
    }
    let outbox = db.list_outbox(monitor.id).await?;
    assert_eq!(outbox.len(), 1);
    let row = outbox
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("one outbox row"))?;
    Ok((monitor, row))
}

fn fast_config(target: url::Url, secret: Option<&[u8]>) -> Config {
    Config {
        target: Some(target),
        secret: secret.map(<[u8]>::to_vec),
        poll_tick: Duration::from_millis(30),
        attempt_deadline: Duration::from_millis(400),
        max_attempts: 3,
        backoff_base: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(200),
        concurrency: 4,
        batch: 8,
    }
}

/// Poll the outbox row until `pred` holds or the deadline passes. The stub reports a hit
/// before the deliverer has finished marking it, so assertions on the store must wait.
async fn wait_row(
    db: &Db,
    monitor: &Monitor,
    pred: impl Fn(&OutboxRow) -> bool,
) -> anyhow::Result<OutboxRow> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let row = db
            .list_outbox(monitor.id)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("outbox row gone"))?;
        if pred(&row) || std::time::Instant::now() > deadline {
            return Ok(row);
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[tokio::test]
async fn a_pending_event_is_delivered_and_marked() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let (monitor, event) = monitor_with_event(&db, &writer, "fixture").await?;
    let (target, mut hits) = stub(0, vec![]).await?;
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(
        db.clone(),
        writer,
        fast_config(url::Url::parse(&target)?, Some(b"the-secret")),
        shutdown.clone(),
    );

    let (headers, body) = tokio::time::timeout(Duration::from_secs(5), hits.recv())
        .await?
        .expect("the event must arrive");

    assert_eq!(
        headers.get("x-smon-event").and_then(|v| v.to_str().ok()),
        Some(event.event_id.as_str())
    );
    // The signature verifies over the exact body bytes.
    let signature = headers
        .get("x-smon-signature")
        .and_then(|v| v.to_str().ok())
        .expect("signed");
    let mut mac = Hmac::<Sha256>::new_from_slice(b"the-secret")?;
    mac.update(&body);
    let expected = format!("sha256={}", hex(&mac.finalize().into_bytes()));
    assert_eq!(signature, expected);

    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(parsed["kind"], "incident_opened");
    assert_eq!(parsed["monitor"]["id"], monitor.id.0.to_string());

    // And the store records it as terminal-delivered.
    let row = wait_row(&db, &monitor, |row| row.delivered_at.is_some()).await?;
    assert!(row.delivered_at.is_some(), "event must be marked delivered");
    assert_eq!(row.attempts, 1);
    shutdown.cancel();
    task.await?;
    Ok(())
}

#[tokio::test]
async fn a_failed_attempt_is_retried_then_delivered() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let (monitor, _event) = monitor_with_event(&db, &writer, "fixture").await?;
    let (target, mut hits) = stub(1, vec![]).await?; // first attempt 500s, second succeeds
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(
        db.clone(),
        writer,
        fast_config(url::Url::parse(&target)?, None),
        shutdown.clone(),
    );

    for _ in 0..2 {
        let (_headers, _body) = tokio::time::timeout(Duration::from_secs(5), hits.recv())
            .await?
            .expect("the attempt and its retry must both arrive");
    }
    let row = wait_row(&db, &monitor, |row| row.delivered_at.is_some()).await?;
    assert_eq!(row.attempts, 2, "second attempt delivered");
    shutdown.cancel();
    task.await?;
    Ok(())
}

#[tokio::test]
async fn a_hanging_receiver_is_a_slow_retry_not_a_stalled_pool() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let (bad, _bad_event) = monitor_with_event(&db, &writer, "bad").await?;
    let (good, _good_event) = monitor_with_event(&db, &writer, "good").await?;
    let (target, mut hits) = stub(0, vec![bad.id.0.to_string()]).await?;
    let mut config = fast_config(url::Url::parse(&target)?, None);
    config.attempt_deadline = Duration::from_millis(150);
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(db.clone(), writer, config, shutdown.clone());

    // The healthy event still delivers while the hanging one occupies its retry deadline.
    let mut good_delivered = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        let Some((_headers, body)) =
            tokio::time::timeout(Duration::from_secs(4), hits.recv()).await?
        else {
            break;
        };
        let parsed: serde_json::Value = serde_json::from_slice(&body)?;
        if parsed["monitor"]["id"].as_str() == Some(good.id.0.to_string().as_str()) {
            good_delivered = true;
            break;
        }
    }
    assert!(
        good_delivered,
        "a hung delivery must not starve other events"
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await??;
    Ok(())
}

#[tokio::test]
async fn exhaustion_is_terminal_and_the_row_stays_visible() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let (monitor, event) = monitor_with_event(&db, &writer, "fixture").await?;
    let (target, mut hits) = stub(10, vec![]).await?; // always 500
    let mut config = fast_config(url::Url::parse(&target)?, None);
    config.max_attempts = 2;
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(db.clone(), writer, config, shutdown.clone());

    for _ in 0..2 {
        let _ = tokio::time::timeout(Duration::from_secs(5), hits.recv())
            .await?
            .expect("each attempt arrives");
    }
    // The second failure is terminal: the row stops being pending and no more hits arrive.
    let row = wait_row(&db, &monitor, |row| {
        row.attempts >= 2 && row.next_attempt_at.is_none()
    })
    .await?;
    let extra = tokio::time::timeout(Duration::from_millis(400), hits.recv()).await;
    assert!(
        extra.is_err(),
        "exhausted events must not be delivered again"
    );

    assert_eq!(row.event_id, event.event_id);
    assert!(row.delivered_at.is_none());
    assert!(row.next_attempt_at.is_none());
    assert!(db.pending_outbox(2, 10).await?.is_empty());
    // But it stays in the table — visible, not silently dropped.
    assert_eq!(db.list_outbox(monitor.id).await?.len(), 1);
    shutdown.cancel();
    task.await?;
    Ok(())
}

#[tokio::test]
async fn no_target_means_pending_not_dropped() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let (monitor, _event) = monitor_with_event(&db, &writer, "fixture").await?;
    let shutdown = CancellationToken::new();
    let mut config = fast_config(url::Url::parse("http://127.0.0.1:1/")?, None);
    config.target = None;
    let task = Deliverer::spawn(db.clone(), writer, config, shutdown.clone());
    task.await?; // no target: the loop returns after logging
    let row = &db.list_outbox(monitor.id).await?[0];
    assert!(row.delivered_at.is_none());
    assert_eq!(row.attempts, 0, "nothing was attempted");
    Ok(())
}

#[tokio::test]
async fn pending_events_survive_a_deliverer_restart() -> anyhow::Result<()> {
    // Deliverer A targets a dead port: the attempt fails, a retry is booked, the process
    // goes away. Deliverer B (the "restart") picks the same pending row up and delivers it —
    // the row, not the process, owns the event.
    let (db, writer) = rig().await?;
    let (monitor, _event) = monitor_with_event(&db, &writer, "fixture").await?;

    let shutdown_a = CancellationToken::new();
    let mut dead = fast_config(url::Url::parse("http://127.0.0.1:1/")?, None);
    dead.attempt_deadline = Duration::from_millis(200);
    let task_a = Deliverer::spawn(db.clone(), writer.clone(), dead, shutdown_a.clone());
    let row = wait_row(&db, &monitor, |row| row.attempts >= 1).await?;
    assert_eq!(
        row.attempts, 1,
        "failed attempt recorded before the restart"
    );
    assert!(row.delivered_at.is_none());
    shutdown_a.cancel();
    task_a.await?;

    let (target, mut hits) = stub(0, vec![]).await?;
    let shutdown_b = CancellationToken::new();
    let task_b = Deliverer::spawn(
        db.clone(),
        writer,
        fast_config(url::Url::parse(&target)?, None),
        shutdown_b.clone(),
    );
    let (_headers, _body) = tokio::time::timeout(Duration::from_secs(5), hits.recv())
        .await?
        .expect("the restarted deliverer must pick the pending row up");
    let row = wait_row(&db, &monitor, |row| row.delivered_at.is_some()).await?;
    assert_eq!(row.attempts, 2, "attempt count carried across the restart");
    shutdown_b.cancel();
    task_b.await?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}
