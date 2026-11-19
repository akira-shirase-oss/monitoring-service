//! Snapshot writer tests: the file is the heartbeat, the allowlist is the sanitizer.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use monitor_db::{Db, SharedStatuses, writer};
use monitor_domain::{CheckOutcome, Monitor, NewMonitor, SlotObservation, Status, WorkspaceId};
use monitor_server::snapshot;
use monitor_server::vitals::{Role, Vitals};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

fn monitor(name: &str) -> Monitor {
    let url = url::Url::parse("https://internal.example.com/health")
        .unwrap_or_else(|e| panic!("test url: {e}"));
    match NewMonitor::new(name, url) {
        Ok(new) => new.into_monitor(),
        Err(e) => panic!("test monitor invalid: {e}"),
    }
}

#[test]
fn the_snapshot_is_an_allowlist_not_a_leak() {
    // Names, URLs and full ids are internal — the only keys the public artifact may carry.
    let monitors = vec![monitor("a secret production name")];
    let statuses: SharedStatuses =
        Arc::new(RwLock::new(HashMap::from([(monitors[0].id, Status::Down)])));
    let mut open = HashMap::new();
    open.insert(monitors[0].id, OffsetDateTime::now_utc());

    let snap = snapshot::build(
        &monitors,
        &statuses,
        &open,
        std::time::Duration::from_secs(90),
        true,
    );
    let service = &snap["services"][0];
    let mut keys: Vec<&str> = service
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["alias", "incident_open_since", "status"]);
    let serialized = serde_json::to_string(&snap).unwrap();
    assert!(!serialized.contains("internal.example.com"));
    assert!(!serialized.contains("a secret production name"));
    assert!(!serialized.contains(&monitors[0].id.0.to_string()));
    // The alias is stable (hash of the id — not a timestamp prefix that collides for
    // monitors registered in the same millisecond) and reveals nothing internal.
    let mut expected = String::with_capacity(12);
    for byte in &Sha256::digest(monitors[0].id.0.to_string().as_bytes())[..6] {
        std::fmt::Write::write_fmt(&mut expected, format_args!("{byte:02x}")).unwrap();
    }
    assert_eq!(service["alias"], expected);
    assert_eq!(service["alias"].as_str().unwrap().len(), 12);
    assert_eq!(service["status"], "down");
    assert!(service["incident_open_since"].is_string());
}

#[test]
fn a_monitor_with_no_committed_evidence_is_unknown() {
    let monitors = vec![monitor("fresh")];
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    let snap = snapshot::build(
        &monitors,
        &statuses,
        &HashMap::new(),
        std::time::Duration::from_secs(90),
        true,
    );
    assert_eq!(snap["services"][0]["status"], "unknown");
    assert!(snap["services"][0]["incident_open_since"].is_null());
}

#[test]
fn the_snapshot_declares_its_own_expiry() {
    let monitors = vec![];
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    let snap = snapshot::build(
        &monitors,
        &statuses,
        &HashMap::new(),
        std::time::Duration::from_secs(90),
        true,
    );
    let generated = OffsetDateTime::parse(
        snap["generated_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let expires = OffsetDateTime::parse(
        snap["expires_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    // expires_at - generated_at ≈ the TTL (within a second of slop for the test run).
    let ttl = (expires - generated).whole_seconds();
    assert!((89..=91).contains(&ttl), "ttl was {ttl}s");
    // And it is in the future — a fresh snapshot is a live heartbeat.
    assert!(expires > OffsetDateTime::now_utc());
}

#[tokio::test]
async fn the_writer_beats_a_real_file_with_committed_evidence() {
    let dir = std::env::temp_dir().join(format!("smon-snap-{}", uuid::Uuid::now_v7()));
    let db = Db::connect("sqlite::memory:", 1).await.unwrap();
    db.migrate().await.unwrap();
    let monitor = monitor("snap-target");
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await
        .unwrap();
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    // One committed pass → the snapshot's status comes from the committed map.
    writer
        .record(monitor_db::Observation {
            monitor_id: monitor.id,
            workspace: monitor.workspace,
            generation: monitor.generation,
            slot_at: OffsetDateTime::now_utc(),
            slot: SlotObservation::Probe(CheckOutcome::Pass),
            http_status: Some(200),
            latency_ms: Some(5),
            detail: None,
        })
        .await
        .unwrap();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let _handle = snapshot::spawn(
        db,
        Arc::clone(&statuses),
        Vitals::new(Arc::default(), std::time::Duration::from_secs(30)),
        snapshot::Config {
            dir: dir.clone(),
            interval: std::time::Duration::from_millis(50),
            ttl: std::time::Duration::from_secs(90),
        },
        shutdown.clone(),
    );

    // Poll for the file — the first tick is immediate-ish, don't sleep a fixed second.
    let mut snap = None;
    for _ in 0..100 {
        if let Ok(bytes) = std::fs::read(dir.join("status.json")) {
            snap = Some(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let snap = snap.expect("snapshot file appeared");
    let services = snap["services"].as_array().unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0]["status"], "up");
    assert!(services[0]["incident_open_since"].is_null());
    shutdown.cancel();
}

/// Honest unavailability, public side: a live process whose writer has died would keep
/// re-stamping its last statuses as a fresh heartbeat. While the engine is not observing,
/// every beat is born expired, every service `unknown`, and says `monitoring: unavailable`.
#[tokio::test]
async fn a_dead_engine_stops_the_heartbeat_reading_fresh() {
    let dir = std::env::temp_dir().join(format!("smon-snap-dead-{}", uuid::Uuid::now_v7()));
    let db = Db::connect("sqlite::memory:", 1).await.unwrap();
    db.migrate().await.unwrap();
    let monitor = monitor("snap-target");
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await
        .unwrap();
    // The committed map still says up — exactly the stale value a dead writer leaves.
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::from([(monitor.id, Status::Up)])));
    let vitals = Vitals::new(Arc::default(), std::time::Duration::from_secs(30));
    vitals.task_stopped("writer", Role::Observing, "panicked");

    let shutdown = tokio_util::sync::CancellationToken::new();
    let _handle = snapshot::spawn(
        db,
        Arc::clone(&statuses),
        vitals,
        snapshot::Config {
            dir: dir.clone(),
            interval: std::time::Duration::from_millis(50),
            ttl: std::time::Duration::from_secs(90),
        },
        shutdown.clone(),
    );
    let mut snap = None;
    for _ in 0..100 {
        if let Ok(bytes) = std::fs::read(dir.join("status.json")) {
            snap = Some(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    shutdown.cancel();
    let snap = snap.expect("snapshot file appeared");
    assert_eq!(snap["monitoring"], "unavailable");
    assert_eq!(snap["services"][0]["status"], "unknown");
    let parse = |key: &str| {
        OffsetDateTime::parse(
            snap[key].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
    };
    assert!(
        parse("expires_at") <= parse("generated_at"),
        "an unavailable beat must already be expired for expiry-only readers"
    );
}

/// The final beat an engine failure writes before the process exits: unavailable at once,
/// not after the TTL runs out on a frozen "up".
#[tokio::test]
async fn a_final_beat_reads_unavailable_immediately() {
    let dir = std::env::temp_dir().join(format!("smon-snap-final-{}", uuid::Uuid::now_v7()));
    let db = Db::connect("sqlite::memory:", 1).await.unwrap();
    db.migrate().await.unwrap();
    let monitor = monitor("final");
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await
        .unwrap();
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::from([(monitor.id, Status::Up)])));
    let config = snapshot::Config {
        dir: dir.clone(),
        interval: std::time::Duration::from_secs(30),
        ttl: std::time::Duration::from_secs(90),
    };
    snapshot::write_beat(&db, &statuses, &config, false)
        .await
        .expect("beat written");
    let snap: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("status.json")).unwrap()).unwrap();
    assert_eq!(snap["monitoring"], "unavailable");
    assert_eq!(snap["services"][0]["status"], "unknown");
    assert_eq!(snap["expires_at"], snap["generated_at"]);
}
