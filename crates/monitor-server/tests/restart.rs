//! Restart semantics: the boot path (`rebuild_health` + `record_downtime`) on a real store
//! and a real writer, exactly as `main` runs it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use monitor_db::{Db, Observation, SharedStatuses, WriteOutcome, writer};
use monitor_domain::{CheckOutcome, Effect, NewMonitor, SlotObservation, Status};
use monitor_server::{rebuild_health, record_downtime};
use time::OffsetDateTime;

fn probe(
    monitor: &monitor_domain::Monitor,
    outcome: CheckOutcome,
    at: OffsetDateTime,
) -> Observation {
    Observation {
        monitor_id: monitor.id,
        generation: monitor.generation,
        slot_at: at,
        slot: SlotObservation::Probe(outcome),
        http_status: None,
        latency_ms: None,
        detail: None,
    }
}

/// Two failures, then the process is down for ten minutes, then one more failure. With a
/// 3-failure threshold that is *not* three consecutive failures — the outage is a gap in
/// evidence. Before, the boot rebuild carried the streak across the gap (the third failure
/// opened an incident) and served the pre-outage status as current.
#[tokio::test]
async fn downtime_is_a_gap_failures_either_side_are_not_consecutive() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("restart.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let monitor = NewMonitor::new("restarted", url::Url::parse("https://example.com/health")?)?
        .into_monitor();
    db.insert_monitor(&monitor).await?;
    let now = OffsetDateTime::now_utc();
    let minutes = |m: i64| now - time::Duration::minutes(m);

    // First life of the process: a pass, then two failures. Then it stops.
    {
        let (first_life, _task) = writer::spawn(
            db.clone(),
            Arc::default(),
            HashMap::new(),
            Arc::default(),
            8,
            false,
        );
        first_life
            .record(probe(&monitor, CheckOutcome::Pass, minutes(12)))
            .await?;
        first_life
            .record(probe(&monitor, CheckOutcome::Transport, minutes(11)))
            .await?;
        first_life
            .record(probe(&monitor, CheckOutcome::Transport, minutes(10)))
            .await?;
    }

    // Boot, ten minutes later — the same sequence main runs.
    let monitors = vec![monitor.clone()];
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    let health = rebuild_health(&db, &monitors, &statuses).await?;
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        health,
        Arc::default(),
        8,
        false,
    );
    let gaps = record_downtime(&writer, &db, &monitors, now, Duration::from_secs(5)).await?;
    assert_eq!(gaps, 1);

    let status = statuses.read().expect("statuses")[&monitor.id];
    assert_eq!(
        status,
        Status::Unknown,
        "pre-outage evidence is not current"
    );
    let newest = db.list_checks(monitor.id, 1).await?;
    assert_eq!(newest[0].outcome, "missed", "the gap is part of the record");
    assert_eq!(newest[0].slot_at, minutes(10) + time::Duration::minutes(1));

    // The first failure after the outage starts a new streak; nothing opens.
    let outcome = writer
        .record(probe(&monitor, CheckOutcome::Transport, now))
        .await?;
    assert_eq!(outcome, WriteOutcome::Committed(Effect::Unchanged));
    assert!(db.open_incident(monitor.id).await?.is_none());
    Ok(())
}

/// A quick restart — inside one interval plus grace — is not a gap: nothing is written and
/// the rebuilt status stands.
#[tokio::test]
async fn a_restart_inside_the_interval_writes_no_gap() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let monitor =
        NewMonitor::new("quick", url::Url::parse("https://example.com/health")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    let now = OffsetDateTime::now_utc();
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    writer
        .record(probe(
            &monitor,
            CheckOutcome::Pass,
            now - time::Duration::seconds(30),
        ))
        .await?;
    let gaps = record_downtime(
        &writer,
        &db,
        std::slice::from_ref(&monitor),
        now,
        Duration::from_secs(5),
    )
    .await?;
    assert_eq!(gaps, 0);
    assert_eq!(db.list_checks(monitor.id, 10).await?.len(), 1);
    assert_eq!(statuses.read().expect("statuses")[&monitor.id], Status::Up);
    Ok(())
}
