//! Writer + store integration tests on a real in-memory database. The contract under test:
//! one transaction per slot, generation invalidation, incident transitions enqueueing the
//! outbox, and `Health::rebuild` recovering streaks across a restart.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use monitor_db::{Db, Observation, WriteHealth, WriteOutcome, writer};
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
    let (handle, _task) = writer::spawn(db.clone(), statuses, HashMap::new(), Arc::default(), 8);
    Ok((db, monitor, handle))
}

fn pass(monitor: &Monitor) -> Observation {
    probe(monitor, CheckOutcome::Pass)
}

fn fail(monitor: &Monitor) -> Observation {
    probe(monitor, CheckOutcome::HttpFail { status: 500 })
}

fn probe(monitor: &Monitor, outcome: CheckOutcome) -> Observation {
    probe_at(monitor, outcome, OffsetDateTime::now_utc())
}

fn probe_at(monitor: &Monitor, outcome: CheckOutcome, at: OffsetDateTime) -> Observation {
    Observation {
        monitor_id: monitor.id,
        generation: monitor.generation,
        slot_at: at,
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

/// M6.2: the incident machine reads thresholds from the same committed gate row as the
/// generation — a strict monitor opens on its first failure while a default one waits
/// for three, and an edit takes effect on the next observation.
#[tokio::test]
async fn per_monitor_thresholds_drive_the_incident_machine() -> anyhow::Result<()> {
    let (db, _monitor, writer) = store().await?;
    let strict = NewMonitor::new("strict", url::Url::parse("https://strict.example.com")?)?
        .with_thresholds(1, 1)?
        .into_monitor();
    db.insert_monitor(&strict).await?;

    // One failure on the strict monitor opens its incident immediately.
    let outcome = writer.record(fail(&strict)).await?;
    assert_eq!(outcome, WriteOutcome::Committed(Effect::IncidentOpened));
    assert!(db.open_incident(strict.id).await?.is_some());
    // And its single fresh pass recovers it (passes_to_recover = 1).
    let outcome = writer.record(pass(&strict)).await?;
    assert_eq!(outcome, WriteOutcome::Committed(Effect::IncidentRecovered));

    // An edit to the stored thresholds takes effect on the next committed observation:
    // bump passes_to_recover to 3, re-open, then prove two passes are not enough.
    let mut edited = strict.clone();
    edited.thresholds = monitor_domain::Thresholds::new(1, 3)?;
    db.update_monitor(&edited, strict.generation).await?;
    let gate = db.monitor_gate(strict.id).await?.expect("gate");
    assert_eq!(gate.thresholds.passes_to_recover, 3);
    let at_gen = |outcome| Observation {
        monitor_id: strict.id,
        generation: gate.generation,
        slot_at: OffsetDateTime::now_utc(),
        slot: SlotObservation::Probe(outcome),
        http_status: Some(500),
        latency_ms: Some(42),
        detail: None,
    };
    writer.record(at_gen(CheckOutcome::Transport)).await?;
    for _ in 0..2 {
        assert_eq!(
            writer.record(at_gen(CheckOutcome::Pass)).await?,
            WriteOutcome::Committed(Effect::Unchanged),
            "edited recover threshold of 3 must not close on the second pass"
        );
    }
    assert_eq!(
        writer.record(at_gen(CheckOutcome::Pass)).await?,
        WriteOutcome::Committed(Effect::IncidentRecovered)
    );
    Ok(())
}

/// M6.11: an active window suppresses the incident open *and* its outbox event while
/// every check row still commits — suppression silences the page, never the evidence.
/// The streak keeps counting through the window, so the first post-expiry failure
/// opens the incident immediately rather than needing a fresh streak.
#[tokio::test]
async fn a_maintenance_window_suppresses_opens_but_not_evidence() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    let now = OffsetDateTime::now_utc();
    let window = db
        .create_window(
            monitor.id,
            now - time::Duration::seconds(10),
            now + time::Duration::seconds(600),
            "kernel rebuild",
        )
        .await?;
    assert_eq!(window.reason, "kernel rebuild");
    assert!(
        db.monitor_gate(monitor.id)
            .await?
            .expect("gate")
            .maintenance,
        "an enclosing window must show active in the gate"
    );
    let listed = db.list_windows(monitor.id).await?;
    assert_eq!(listed.len(), 1);

    // Three failures inside the window — every check commits as evidence, none opens
    // an incident and none queues an alert. The first two are still below threshold
    // (Unchanged); the crossing itself is what's suppressed.
    for _ in 0..2 {
        assert_eq!(
            writer.record(fail(&monitor)).await?,
            WriteOutcome::Committed(Effect::Unchanged)
        );
    }
    assert_eq!(
        writer.record(fail(&monitor)).await?,
        WriteOutcome::Committed(Effect::IncidentSuppressed)
    );
    assert!(db.open_incident(monitor.id).await?.is_none());
    assert_eq!(db.list_checks(monitor.id, 10).await?.len(), 3);
    assert!(db.list_outbox(monitor.id).await?.is_empty());

    // Deleting the window early stands in for expiry — it's the same gate flag. The
    // very next failure opens immediately: the streak counted through the window.
    assert!(db.delete_window(window.id).await?);
    assert!(
        !db.delete_window(window.id).await?,
        "second delete is 404-truth"
    );
    assert!(
        !db.monitor_gate(monitor.id)
            .await?
            .expect("gate")
            .maintenance,
        "a deleted window must stop suppressing at the next gate read"
    );
    assert_eq!(
        writer.record(fail(&monitor)).await?,
        WriteOutcome::Committed(Effect::IncidentOpened)
    );
    assert!(db.open_incident(monitor.id).await?.is_some());
    assert_eq!(db.list_outbox(monitor.id).await?.len(), 1);
    Ok(())
}

/// M6.3: the four incident times are tracked separately and honestly — `first_fail_at` is
/// the streak's first failing slot, `opened_at` when the threshold was crossed,
/// `last_fail_at` the latest failing slot while open, `resolved_at` on recovery.
#[tokio::test]
async fn incident_tracks_first_fail_open_last_fail_and_resolve_separately() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    // Streak begins at slot 1, threshold crossed at slot 3 — the notice is two slots
    // after the failure started, and that difference is the whole point.
    let base = OffsetDateTime::now_utc();
    let slot = |offset_s: i64| base + time::Duration::seconds(offset_s);
    for offset in 1..=5 {
        writer
            .record(probe_at(&monitor, CheckOutcome::Transport, slot(offset)))
            .await?;
    }

    let incident = db
        .open_incident(monitor.id)
        .await?
        .expect("incident open after 5 failures");
    assert_eq!(
        incident.first_fail_at.map(OffsetDateTime::unix_timestamp),
        Some(slot(1).unix_timestamp()),
        "streak began at slot 1"
    );
    assert_eq!(
        incident.opened_at.unix_timestamp(),
        slot(3).unix_timestamp(),
        "noticed when the third landed"
    );
    assert_eq!(
        incident.last_fail_at.map(OffsetDateTime::unix_timestamp),
        Some(slot(5).unix_timestamp()),
        "latest failure while open"
    );
    assert!(incident.resolved_at.is_none());

    // Recovery stamps resolved_at and freezes the other three.
    for offset in 6..=7 {
        writer
            .record(probe_at(&monitor, CheckOutcome::Pass, slot(offset)))
            .await?;
    }
    let incidents = db
        .list_incidents(Some(monitor.id), Some("resolved"), 5)
        .await?;
    let resolved = &incidents[0];
    assert_eq!(
        resolved.first_fail_at.map(OffsetDateTime::unix_timestamp),
        Some(slot(1).unix_timestamp())
    );
    assert_eq!(
        resolved.last_fail_at.map(OffsetDateTime::unix_timestamp),
        Some(slot(5).unix_timestamp())
    );
    assert_eq!(
        resolved.resolved_at.map(OffsetDateTime::unix_timestamp),
        Some(slot(7).unix_timestamp())
    );
    Ok(())
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
async fn check_stats_counts_window_rows_and_leaves_expected_to_the_caller() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    // Before the window: must not be counted.
    let mut old = pass(&monitor);
    old.slot_at = OffsetDateTime::now_utc() - time::Duration::days(2);
    writer.record(old).await?;
    // In-window: two passes, one failure, one missed.
    writer.record(pass(&monitor)).await?;
    writer.record(pass(&monitor)).await?;
    writer.record(fail(&monitor)).await?;
    writer.record(missed(&monitor)).await?;

    let stats = db
        .check_stats(
            monitor.id,
            OffsetDateTime::now_utc() - time::Duration::hours(1),
        )
        .await?;
    assert_eq!(stats.observed, 3);
    assert_eq!(stats.passed, 2);
    assert_eq!(stats.missed, 1);
    // `expected` is the caller's job (interval over elapsed wall clock); the store reports 0.
    assert_eq!(stats.expected, 0);
    assert_eq!(stats.coverage_bps(), None);
    Ok(())
}

#[tokio::test]
async fn retention_aggregates_then_deletes_and_spares_open_incidents() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    let second =
        NewMonitor::new("second", url::Url::parse("https://two.example.com")?)?.into_monitor();
    db.insert_monitor(&second).await?;

    let old = OffsetDateTime::now_utc() - time::Duration::days(9);
    let aged = |m: &Monitor, outcome: CheckOutcome| {
        let mut observation = probe(m, outcome);
        observation.slot_at = old;
        observation
    };
    // Monitor A (no incident): 2 old passes + 1 old failure + 1 old missed → expired.
    writer.record(aged(&monitor, CheckOutcome::Pass)).await?;
    writer.record(aged(&monitor, CheckOutcome::Pass)).await?;
    writer
        .record(aged(&monitor, CheckOutcome::HttpFail { status: 500 }))
        .await?;
    writer
        .record({
            let mut observation = missed(&monitor);
            observation.slot_at = old;
            observation
        })
        .await?;
    // Monitor B: 3 old failures → an open incident born 9 days ago, still open.
    for _ in 0..3 {
        writer
            .record(aged(&second, CheckOutcome::Transport))
            .await?;
    }
    assert!(db.open_incident(second.id).await?.is_some());
    // Recent rows for A — inside the window, must stay raw.
    writer.record(pass(&monitor)).await?;

    let report = writer
        .retain(OffsetDateTime::now_utc() - time::Duration::days(7))
        .await?;
    assert_eq!(report.rows_deleted, 4, "monitor A's four expired rows");
    assert_eq!(report.days_written, 1, "one monitor-day aggregated");

    // A: raw expired rows are gone, aggregates carry their counts; the recent row is raw.
    let checks = db.list_checks(monitor.id, 50).await?;
    assert_eq!(checks.len(), 1);
    let stats = db
        .check_stats(
            monitor.id,
            OffsetDateTime::now_utc() - time::Duration::days(10),
        )
        .await?;
    assert_eq!((stats.observed, stats.passed, stats.missed), (4, 3, 1));

    // B: open incident → every raw row preserved, nothing aggregated away.
    assert_eq!(db.list_checks(second.id, 50).await?.len(), 3);
    let stats_b = db
        .check_stats(
            second.id,
            OffsetDateTime::now_utc() - time::Duration::days(10),
        )
        .await?;
    assert_eq!(stats_b.observed, 3);

    // Outbox is never expired: monitor B's incident_opened event is still there.
    assert_eq!(db.list_outbox(second.id).await?.len(), 1);
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
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        HashMap::new(),
        Arc::default(),
        8,
    );

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
async fn update_monitor_bumps_generation_and_rejects_stale_writes() -> anyhow::Result<()> {
    let (db, monitor, _writer) = store().await?;

    // First write at the generation we read succeeds and bumps.
    let mut edited = monitor.clone();
    edited.name = "edited".to_owned();
    edited.paused = true;
    let generation = db.update_monitor(&edited, monitor.generation).await?;
    assert_eq!(generation, Some(monitor.generation + 1));

    // A second write carrying the pre-edit generation is a stale write: it must not apply.
    let stale = db.update_monitor(&edited, monitor.generation).await?;
    assert_eq!(stale, None);

    let stored = db
        .list_monitors()
        .await?
        .into_iter()
        .find(|row| row.id == monitor.id.0.to_string())
        .expect("monitor row");
    assert_eq!(stored.name, "edited");
    assert!(stored.paused);
    assert_eq!(stored.generation, i64::try_from(monitor.generation)? + 1);
    Ok(())
}

#[tokio::test]
async fn delete_only_succeeds_without_history() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    assert!(!db.has_history(monitor.id).await?);
    assert!(db.delete_monitor(monitor.id).await?);
    assert!(db.list_monitors().await?.is_empty());

    // With a committed check, delete is refused — the row and its evidence both survive.
    let monitor = NewMonitor::new("kept", url::Url::parse("https://example.com")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    writer.record(pass(&monitor)).await?;
    assert!(db.has_history(monitor.id).await?);
    assert!(!db.delete_monitor(monitor.id).await?);
    assert_eq!(db.list_checks(monitor.id, 10).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn outbox_lifecycle_pending_retry_exhausted_delivered() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    let outbox = db.list_outbox(monitor.id).await?;
    assert_eq!(outbox.len(), 1);
    let event_id = outbox[0].event_id.clone();

    // Fresh event: due immediately.
    let pending = db.pending_outbox(3, 10).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event_id, event_id);
    assert_eq!(pending[0].monitor_name, "fixture");
    assert_eq!(pending[0].kind, "incident_opened");

    // A failed attempt books a retry in the future: no longer due.
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    writer
        .mark_delivery(&event_id, 1, Some(later), None)
        .await?;
    assert!(db.pending_outbox(3, 10).await?.is_empty());

    // Past the retry time it is due again — simulate by scheduling in the past.
    let past = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    writer.mark_delivery(&event_id, 2, Some(past), None).await?;
    assert_eq!(db.pending_outbox(3, 10).await?.len(), 1);

    // Exhausted (attempts = max, no next attempt) is terminal: never pending again, but the
    // row stays in the table.
    writer.mark_delivery(&event_id, 3, None, None).await?;
    assert!(db.pending_outbox(3, 10).await?.is_empty());
    assert_eq!(db.list_outbox(monitor.id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_delivered_event_cannot_be_rewritten() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    let event_id = db.list_outbox(monitor.id).await?[0].event_id.clone();

    let now = OffsetDateTime::now_utc();
    writer.mark_delivery(&event_id, 1, None, Some(now)).await?;
    // The guarded update ignores any write to an already-delivered row.
    let later = now + time::Duration::minutes(5);
    writer
        .mark_delivery(&event_id, 99, Some(later), None)
        .await?;

    let row = &db.list_outbox(monitor.id).await?[0];
    assert_eq!(row.attempts, 1);
    assert!(row.delivered_at.is_some());
    assert!(row.next_attempt_at.is_none());
    assert!(db.pending_outbox(8, 10).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn mark_delivery_goes_through_the_single_writer() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    let event_id = db.list_outbox(monitor.id).await?[0].event_id.clone();

    writer
        .mark_delivery(&event_id, 1, None, Some(OffsetDateTime::now_utc()))
        .await?;
    let row = &db.list_outbox(monitor.id).await?[0];
    assert_eq!(row.attempts, 1);
    assert!(row.delivered_at.is_some());
    Ok(())
}

#[tokio::test]
async fn list_incidents_filters_by_monitor_and_state() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    let other = NewMonitor::new("other", url::Url::parse("https://example.org")?)?.into_monitor();
    db.insert_monitor(&other).await?;

    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    // `other` gets its own open incident via direct observations at its own generation.
    for _ in 0..3 {
        writer.record(fail(&other)).await?;
    }
    for _ in 0..2 {
        writer.record(pass(&monitor)).await?;
    }

    assert_eq!(db.list_incidents(None, None, 10).await?.len(), 2);
    assert_eq!(
        db.list_incidents(None, Some("open"), 10).await?.len(),
        1,
        "only `other` is still open — `monitor` recovered"
    );
    assert_eq!(
        db.list_incidents(Some(monitor.id), Some("resolved"), 10)
            .await?
            .len(),
        1
    );
    assert_eq!(
        db.list_incidents(Some(other.id), Some("open"), 10)
            .await?
            .len(),
        1
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
    let mut health = Health::rebuild(open, &tail, monitor.thresholds);
    // Two stored failures + one fresh failure must open the incident — streaks survived.
    assert_eq!(
        health.record(
            SlotObservation::Probe(CheckOutcome::Timeout),
            monitor.thresholds,
            false
        ),
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
    let mut health = Health::rebuild(open, &tail, monitor.thresholds);
    assert_eq!(health.status(), Status::Down);
    // One pass is not enough; the second fresh pass recovers.
    health.record(
        SlotObservation::Probe(CheckOutcome::Pass),
        monitor.thresholds,
        false,
    );
    assert_eq!(
        health.record(
            SlotObservation::Probe(CheckOutcome::Pass),
            monitor.thresholds,
            false
        ),
        Effect::IncidentRecovered
    );
    Ok(())
}

/// Disk guardrail (M4.2): once the page ceiling is hit, a commit fails cleanly — the
/// observation returns an error, the consecutive-failure counter grows, and the status
/// map is not advanced (memory can never be ahead of the store).
#[tokio::test]
async fn writes_past_the_page_ceiling_fail_and_flag_unhealthy() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com/health")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    // SQLite clamps max_page_count up to the current size, so cap at exactly the used
    // bytes: the database may not grow, and writes fail once free space runs out.
    let used = db.used_bytes().await?;
    db.set_max_bytes(used).await?;

    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let write_health: WriteHealth = Arc::default();
    let (handle, _task) = writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        HashMap::new(),
        Arc::clone(&write_health),
        8,
    );

    // Free pages absorb a few rows; keep writing until the ceiling bites. The counter
    // tracks *trailing consecutive* failures — a landed write resets it — so count both
    // totals and the trailing run, and compare against the run.
    let mut failures = 0;
    let mut trailing = 0;
    for _ in 0..400 {
        if handle.record(pass(&monitor)).await.is_err() {
            failures += 1;
            trailing += 1;
        } else {
            trailing = 0;
        }
    }
    assert!(
        failures > 0,
        "a capped database must eventually refuse writes"
    );
    assert_eq!(
        write_health.load(std::sync::atomic::Ordering::Relaxed),
        trailing,
        "the counter tracks consecutive failures; a landed write resets it"
    );
    Ok(())
}

#[tokio::test]
async fn a_landed_write_clears_the_failure_count() -> anyhow::Result<()> {
    let (db, monitor, _handle) = store().await?;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let write_health: WriteHealth = Arc::default();
    write_health.store(3, std::sync::atomic::Ordering::Relaxed);
    let (handle, _task) = writer::spawn(db, statuses, HashMap::new(), Arc::clone(&write_health), 8);
    handle.record(pass(&monitor)).await?;
    assert_eq!(
        write_health.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "recovery is proven by a landed write, not a timer"
    );
    Ok(())
}

/// Restore drill (M4.3): a live `VACUUM INTO` backup, restored as the database file,
/// keeps an open incident open and the pending outbox event pending — nothing about the
/// round trip replays or loses committed evidence.
#[tokio::test]
async fn backup_restore_preserves_open_incidents_and_outbox() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let live = tmp.path().join("live.db");
    let db = Db::connect(&format!("sqlite:{}", live.display()), 1).await?;
    db.migrate().await?;
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com/health")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;

    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (handle, _task) = writer::spawn(db.clone(), statuses, HashMap::new(), Arc::default(), 8);
    // Drive an incident open — check + incident + outbox land in one commit.
    for _ in 0..3 {
        handle.record(fail(&monitor)).await?;
    }
    assert!(db.open_incident(monitor.id).await?.is_some());

    // Backup while the incident is open, then "restore": open the backup as the store.
    let backup = tmp.path().join("backup.db");
    let bytes = db.backup(&backup).await?;
    assert!(bytes > 0, "a backup must not be empty");
    drop(handle);
    drop(db);

    let restored = Db::connect(&format!("sqlite:{}", backup.display()), 1).await?;
    let open = restored.open_incident(monitor.id).await?;
    assert!(open.is_some(), "restored open incident stays open");
    let due = restored.pending_outbox(10, 10).await?;
    assert_eq!(
        due.len(),
        1,
        "the pending outbox event survives the restore"
    );
    // Boot rebuild from the restored store reproduces `down` — not a fresh unknown.
    let tail = restored.recent_outcomes(monitor.id, 4).await?;
    let health = Health::rebuild(open.is_some(), &tail, monitor.thresholds);
    assert_eq!(health.status(), Status::Down);
    Ok(())
}
