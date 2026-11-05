//! Writer + store integration tests on a real in-memory database. The contract under test:
//! one transaction per slot, generation invalidation, incident transitions enqueueing the
//! outbox, and `Health::rebuild` recovering streaks across a restart.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use monitor_db::{Db, Observation, WriteOutcome, writer};
use monitor_domain::{
    CheckOutcome, Effect, Health, Monitor, MonitorId, NewMonitor, SlotObservation, Status,
};
use time::OffsetDateTime;

async fn store() -> anyhow::Result<(Db, Monitor, writer::WriterHandle)> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com/health")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (handle, _task) = writer::spawn(db.clone(), statuses, HashMap::new(), 8);
    Ok((db, monitor, handle))
}

fn pass(monitor: &Monitor) -> Observation {
    probe(monitor, CheckOutcome::Pass)
}

fn fail(monitor: &Monitor) -> Observation {
    probe(monitor, CheckOutcome::HttpFail { status: 500 })
}

fn probe(monitor: &Monitor, outcome: CheckOutcome) -> Observation {
    Observation {
        monitor_id: monitor.id,
        generation: monitor.generation,
        slot_at: OffsetDateTime::now_utc(),
        slot: SlotObservation::Probe(outcome),
        http_status: Some(500),
        latency_ms: Some(42),
        detail: None,
    }
}

fn missed(monitor: &Monitor) -> Observation {
    Observation {
        monitor_id: monitor.id,
        generation: monitor.generation,
        slot_at: OffsetDateTime::now_utc(),
        slot: SlotObservation::Missed,
        http_status: None,
        latency_ms: None,
        detail: None,
    }
}

#[tokio::test]
async fn three_failures_commit_check_incident_and_outbox_together() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;

    for expected in [Effect::Unchanged, Effect::Unchanged, Effect::IncidentOpened] {
        assert_eq!(
            writer.record(fail(&monitor)).await?,
            WriteOutcome::Committed(expected)
        );
    }

    let checks = db.list_checks(monitor.id, 10).await?;
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|row| row.outcome == "http_fail"));

    let incident = db.open_incident(monitor.id).await?;
    assert!(
        incident.is_some(),
        "incident must open with the third commit"
    );

    let outbox = db.list_outbox(monitor.id).await?;
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].kind, "incident_opened");
    Ok(())
}

#[tokio::test]
async fn two_fresh_passes_resolve_the_incident_in_one_commit() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }

    for expected in [Effect::Unchanged, Effect::IncidentRecovered] {
        assert_eq!(
            writer.record(pass(&monitor)).await?,
            WriteOutcome::Committed(expected)
        );
    }

    assert!(db.open_incident(monitor.id).await?.is_none());
    let outbox = db.list_outbox(monitor.id).await?;
    assert_eq!(
        outbox
            .iter()
            .map(|row| row.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["incident_opened", "incident_recovered"]
    );
    Ok(())
}

#[tokio::test]
async fn a_missed_slot_is_stored_but_resets_the_streak() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    writer.record(fail(&monitor)).await?;
    writer.record(fail(&monitor)).await?;
    writer.record(missed(&monitor)).await?;
    // Two post-miss failures must not open an incident: the gap reset the streak.
    writer.record(fail(&monitor)).await?;
    writer.record(fail(&monitor)).await?;

    assert!(db.open_incident(monitor.id).await?.is_none());
    let outcomes = db.recent_outcomes(monitor.id, 10).await?;
    assert!(outcomes.contains(&SlotObservation::Missed));
    Ok(())
}

#[tokio::test]
async fn stale_and_unknown_observations_are_dropped() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;

    let mut stale = fail(&monitor);
    stale.generation = monitor.generation + 1;
    assert_eq!(
        writer.record(stale).await?,
        WriteOutcome::DroppedStaleGeneration
    );

    let mut unknown = fail(&monitor);
    unknown.monitor_id = MonitorId(uuid::Uuid::now_v7());
    assert_eq!(
        writer.record(unknown).await?,
        WriteOutcome::DroppedUnknownMonitor
    );

    assert!(db.list_checks(monitor.id, 10).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn statuses_only_move_on_commits() -> anyhow::Result<()> {
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let monitor = NewMonitor::new("m", url::Url::parse("https://example.com")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    let (writer, _task) = writer::spawn(db.clone(), Arc::clone(&statuses), HashMap::new(), 8);

    assert_eq!(
        statuses
            .read()
            .ok()
            .and_then(|m| m.get(&monitor.id).copied()),
        None
    );
    writer.record(pass(&monitor)).await?;
    assert_eq!(
        statuses
            .read()
            .ok()
            .and_then(|m| m.get(&monitor.id).copied()),
        Some(Status::Up)
    );
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    assert_eq!(
        statuses
            .read()
            .ok()
            .and_then(|m| m.get(&monitor.id).copied()),
        Some(Status::Down)
    );
    Ok(())
}

#[tokio::test]
async fn restart_rebuilds_streaks_and_open_incidents_from_the_store() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    writer.record(fail(&monitor)).await?;
    writer.record(fail(&monitor)).await?;
    drop(writer); // "restart": only the store survives.

    let open = db.open_incident(monitor.id).await?.is_some();
    let tail = db.recent_outcomes(monitor.id, 4).await?;
    let mut health = Health::rebuild(open, &tail);
    // Two stored failures + one fresh failure must open the incident — streaks survived.
    assert_eq!(
        health.record(SlotObservation::Probe(CheckOutcome::Timeout)),
        Effect::IncidentOpened
    );

    // And an open incident rebuilt across a restart stays open.
    let (db, monitor, writer) = store().await?;
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    drop(writer);
    let open = db.open_incident(monitor.id).await?.is_some();
    let tail = db.recent_outcomes(monitor.id, 4).await?;
    let mut health = Health::rebuild(open, &tail);
    assert_eq!(health.status(), Status::Down);
    // One pass is not enough; the second fresh pass recovers.
    health.record(SlotObservation::Probe(CheckOutcome::Pass));
    assert_eq!(
        health.record(SlotObservation::Probe(CheckOutcome::Pass)),
        Effect::IncidentRecovered
    );
    Ok(())
}
