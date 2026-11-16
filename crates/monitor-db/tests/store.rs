//! Writer + store integration tests on a real in-memory database. The contract under test:
//! one transaction per slot, generation invalidation, incident transitions enqueueing the
//! outbox, and `Health::rebuild` recovering streaks across a restart.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use monitor_db::{Db, Observation, RetentionPolicy, WriteHealth, WriteOutcome, writer};
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
    let (handle, _task) = writer::spawn(
        db.clone(),
        statuses,
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
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
        .retain(RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()))
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
        false,
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
    let pending = db.pending_outbox("webhook", 3, 10).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event_id, event_id);
    assert_eq!(pending[0].monitor_name, "fixture");
    assert_eq!(pending[0].kind, "incident_opened");

    // A failed attempt books a retry in the future: no longer due.
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    writer
        .mark_delivery(&event_id, 1, Some(later), None)
        .await?;
    assert!(db.pending_outbox("webhook", 3, 10).await?.is_empty());

    // Past the retry time it is due again — simulate by scheduling in the past.
    let past = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    writer.mark_delivery(&event_id, 2, Some(past), None).await?;
    assert_eq!(db.pending_outbox("webhook", 3, 10).await?.len(), 1);

    // Exhausted (attempts = max, no next attempt) is terminal: never pending again, but the
    // row stays in the table.
    writer.mark_delivery(&event_id, 3, None, None).await?;
    assert!(db.pending_outbox("webhook", 3, 10).await?.is_empty());
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
    assert!(db.pending_outbox("webhook", 8, 10).await?.is_empty());
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
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("capped.db").display());
    let setup = Db::connect(&url, 1).await?;
    setup.migrate().await?;
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com/health")?)?.into_monitor();
    setup.insert_monitor(&monitor).await?;
    // SQLite clamps max_page_count up to the current size, so cap at exactly the used
    // bytes: the database may not grow, and writes fail once free space runs out. The
    // production pool size (4) — the ceiling must hold on whichever connection commits.
    let used = setup.used_bytes().await?;
    drop(setup);
    let db = Db::connect_opts(&url, 4, Some(used)).await?;
    assert_eq!(db.max_bytes().await?, used);

    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let write_health: WriteHealth = Arc::default();
    let (handle, _task) = writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        HashMap::new(),
        Arc::clone(&write_health),
        8,
        false,
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
    let (handle, _task) = writer::spawn(
        db,
        statuses,
        HashMap::new(),
        Arc::clone(&write_health),
        8,
        false,
    );
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
    let (handle, _task) = writer::spawn(
        db.clone(),
        statuses,
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
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
    let due = restored.pending_outbox("webhook", 10, 10).await?;
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

/// M7.1: with the email channel configured, an incident transition enqueues one outbox row
/// per channel — same event, independent delivery bookkeeping.
#[tokio::test]
async fn an_incident_enqueues_one_row_per_configured_channel() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    // The email channel is what production gets when SMON_SMTP_DSN is set.
    let (handle, _task) = writer::spawn(
        db.clone(),
        statuses,
        HashMap::new(),
        Arc::default(),
        8,
        true,
    );

    for _ in 0..3 {
        handle.record(fail(&monitor)).await?;
    }

    let rows = db.list_outbox(monitor.id).await?;
    assert_eq!(rows.len(), 2, "one outbox row per configured channel");
    let mut channels: Vec<&str> = rows.iter().map(|row| row.channel.as_str()).collect();
    channels.sort_unstable();
    assert_eq!(channels, ["email", "webhook"]);
    assert_eq!(rows[0].incident_id, rows[1].incident_id);
    assert_eq!(rows[0].kind, rows[1].kind);
    assert_ne!(
        rows[0].event_id, rows[1].event_id,
        "each channel tracks its own delivery"
    );
    assert_eq!(
        db.pending_outbox("email", 8, 10).await?.len(),
        1,
        "the email row is pending on its own channel"
    );
    Ok(())
}

/// M7.2: session rows are the cookie's authority — create, validate, expire, sweep.
#[tokio::test]
async fn sessions_validate_expire_and_sweep() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let now = time::OffsetDateTime::now_utc();

    db.create_session("live", now + time::Duration::hours(1))
        .await?;
    db.create_session("dead", now - time::Duration::seconds(1))
        .await?;
    assert!(db.session_valid("live").await?);
    assert!(!db.session_valid("missing").await?);
    // An expired row is denied and deleted on sight — one lookup is the whole contract.
    assert!(!db.session_valid("dead").await?);

    db.create_session("gone-soon", now - time::Duration::seconds(1))
        .await?;
    // `dead` was already deleted by the lazy guard above — the sweep takes what's left.
    assert_eq!(
        db.sweep_expired_sessions().await?,
        1,
        "the untouched expired row swept"
    );
    assert!(
        db.session_valid("live").await?,
        "the live session survives the sweep"
    );
    db.delete_session("live").await?;
    assert!(!db.session_valid("live").await?);
    Ok(())
}

fn at(monitor: &Monitor, outcome: CheckOutcome, days_ago: i64, minute: i64) -> Observation {
    probe_at(
        monitor,
        outcome,
        OffsetDateTime::now_utc() - time::Duration::days(days_ago)
            + time::Duration::minutes(minute),
    )
}

/// Retention (D-012): an open incident keeps its *birth evidence* raw — the failing streak
/// from `first_fail_at` to `opened_at`, at most `failures_to_open` rows — not every raw
/// row the monitor ever wrote. Before, one long-open incident exempted the monitor's whole
/// history from expiry, and its raw rows grew without bound.
#[tokio::test]
async fn an_open_incident_keeps_only_its_birth_streak_past_retention() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    // 20 days ago: healthy history.
    for minute in 0..5 {
        writer
            .record(at(&monitor, CheckOutcome::Pass, 20, minute))
            .await?;
    }
    // 9 days ago: the birth streak — three failures open the incident.
    for minute in 0..3 {
        writer
            .record(at(&monitor, CheckOutcome::Transport, 9, minute))
            .await?;
    }
    let incident = db
        .open_incident(monitor.id)
        .await?
        .expect("incident opened");
    // 8 days ago: still failing while open; then one recent failure.
    for minute in 0..4 {
        writer
            .record(at(&monitor, CheckOutcome::Timeout, 8, minute))
            .await?;
    }
    writer.record(fail(&monitor)).await?;

    let report = writer
        .retain(RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()))
        .await?;
    assert_eq!(
        report.rows_deleted, 9,
        "5 old passes + 4 old open-period failures"
    );

    let kept = db.list_checks(monitor.id, 50).await?;
    let first_fail = incident.first_fail_at.expect("first_fail_at");
    let birth: Vec<_> = kept
        .iter()
        .filter(|row| row.slot_at >= first_fail && row.slot_at <= incident.opened_at)
        .collect();
    assert_eq!(
        birth.len(),
        3,
        "the birth streak stays raw while the incident is open"
    );
    assert_eq!(
        kept.len(),
        4,
        "birth streak + the row inside the raw window"
    );
    // Nothing expired was lost: aggregates carry it, so coverage stays honest.
    let stats = db
        .check_stats(
            monitor.id,
            OffsetDateTime::now_utc() - time::Duration::days(30),
        )
        .await?;
    assert_eq!(stats.observed, 13);
    assert_eq!(stats.passed, 5);
    assert!(
        db.open_incident(monitor.id).await?.is_some(),
        "the incident itself is untouched"
    );
    Ok(())
}

/// Retention (D-012) beyond raw checks: delivered outbox rows expire 30 days after delivery,
/// resolved incidents (with every outbox row they own) 365 days after resolution, daily
/// aggregates and ended maintenance windows after 365 days. Open incidents and undelivered
/// rows of kept incidents are never pruned.
#[tokio::test]
async fn retention_prunes_delivered_outbox_old_incidents_aggregates_and_windows()
-> anyhow::Result<()> {
    let (db, ancient, writer) = store().await?;
    let recent =
        NewMonitor::new("recent", url::Url::parse("https://two.example.com")?)?.into_monitor();
    let open =
        NewMonitor::new("open", url::Url::parse("https://three.example.com")?)?.into_monitor();
    db.insert_monitor(&recent).await?;
    db.insert_monitor(&open).await?;

    // An incident opened and resolved ~400 days ago: past every horizon.
    for minute in 0..3 {
        writer
            .record(at(&ancient, CheckOutcome::Transport, 400, minute))
            .await?;
    }
    for minute in 3..5 {
        writer
            .record(at(&ancient, CheckOutcome::Pass, 400, minute))
            .await?;
    }
    // One resolved ~20 days ago: kept, but its opened-event was delivered 40 days ago
    // (prunable) while the recovered-event is still pending (kept).
    for minute in 0..3 {
        writer
            .record(at(&recent, CheckOutcome::Transport, 20, minute))
            .await?;
    }
    for minute in 3..5 {
        writer
            .record(at(&recent, CheckOutcome::Pass, 20, minute))
            .await?;
    }
    let recent_events = db.list_outbox(recent.id).await?;
    assert_eq!(recent_events.len(), 2);
    writer
        .mark_delivery(
            &recent_events[0].event_id,
            1,
            None,
            Some(OffsetDateTime::now_utc() - time::Duration::days(40)),
        )
        .await?;
    // An open incident from 2 days ago, its event undelivered.
    for minute in 0..3 {
        writer
            .record(at(&open, CheckOutcome::Transport, 2, minute))
            .await?;
    }
    // Maintenance windows: one ended 400 days ago, one ended yesterday.
    let now = OffsetDateTime::now_utc();
    db.create_window(
        ancient.id,
        now - time::Duration::days(401),
        now - time::Duration::days(400),
        "old",
    )
    .await?;
    db.create_window(
        ancient.id,
        now - time::Duration::days(2),
        now - time::Duration::days(1),
        "new",
    )
    .await?;

    let report = writer
        .retain(RetentionPolicy::default().cutoffs(now))
        .await?;
    assert_eq!(report.incidents_deleted, 1);
    assert_eq!(
        report.outbox_deleted, 3,
        "ancient incident's two rows + one delivered row"
    );
    assert_eq!(
        report.aggregates_deleted, 0,
        "raw rows already past the aggregate horizon are dropped, never aggregated"
    );
    assert_eq!(report.windows_deleted, 1);

    assert!(
        db.list_incidents(Some(ancient.id), None, 10)
            .await?
            .is_empty()
    );
    assert!(db.list_outbox(ancient.id).await?.is_empty());
    let ancient_stats = db
        .check_stats(ancient.id, now - time::Duration::days(500))
        .await?;
    assert_eq!(
        ancient_stats.observed, 0,
        "aggregates past 365 days are gone"
    );
    assert_eq!(db.list_windows(ancient.id).await?.len(), 1);

    assert_eq!(db.list_incidents(Some(recent.id), None, 10).await?.len(), 1);
    let left = db.list_outbox(recent.id).await?;
    assert_eq!(left.len(), 1);
    assert!(left[0].delivered_at.is_none(), "the pending event survives");
    let recent_stats = db
        .check_stats(recent.id, now - time::Duration::days(30))
        .await?;
    assert_eq!(recent_stats.observed, 5, "young aggregates are kept");

    assert!(db.open_incident(open.id).await?.is_some());
    assert_eq!(db.list_outbox(open.id).await?.len(), 1);

    Ok(())
}

/// Retention at the disk cap (D-012 + M4.2): once the store reaches `SMON_MAX_DB_BYTES`,
/// the retention pass is the only thing that can make room — so it must be able to run
/// *at* the cap. It used to start with an aggregate INSERT, which needs a fresh page,
/// failed `SQLITE_FULL`, rolled back, and freed nothing: a full store stayed full (and
/// "store writes failing") forever.
#[tokio::test]
async fn retention_frees_space_in_a_store_filled_to_its_cap() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("full.db").display());
    let setup = Db::connect(&url, 1).await?;
    setup.migrate().await?;
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com/health")?)?.into_monitor();
    setup.insert_monitor(&monitor).await?;
    let used = setup.used_bytes().await?;
    drop(setup);

    let db = Db::connect_opts(&url, 4, Some(used + 64 * 1024)).await?;
    let write_health: WriteHealth = Arc::default();
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::new(RwLock::new(HashMap::new())),
        HashMap::new(),
        Arc::clone(&write_health),
        8,
        false,
    );
    // Fill to the cap with evidence that has already expired, spread over ~290 days so
    // its daily aggregates need fresh pages (as months of real history do).
    let old = OffsetDateTime::now_utc() - time::Duration::days(8);
    let mut stored = 0;
    let mut full = false;
    for n in 0..20_000_i64 {
        let at = old - time::Duration::days(n % 290) + time::Duration::seconds(n / 290);
        let observation = probe_at(&monitor, CheckOutcome::Pass, at);
        if writer.record(observation).await.is_err() {
            full = true;
            break;
        }
        stored += 1;
    }
    assert!(
        full && stored > 100,
        "the store must reach its cap ({stored} rows stored)"
    );
    assert!(write_health.load(std::sync::atomic::Ordering::Relaxed) > 0);

    let report = writer
        .retain(RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()))
        .await
        .expect("retention must run at the cap");
    assert_eq!(report.rows_deleted, stored, "every expired row is freed");
    let stats = db
        .check_stats(
            monitor.id,
            OffsetDateTime::now_utc() - time::Duration::days(365),
        )
        .await?;
    assert_eq!(stats.passed, stored, "and nothing is lost: aggregated");

    writer
        .record(pass(&monitor))
        .await
        .expect("writes land again after retention");
    assert_eq!(write_health.load(std::sync::atomic::Ordering::Relaxed), 0);
    Ok(())
}

/// Aggregates age out on their own horizon (D-012), independent of raw checks.
#[tokio::test]
async fn aggregates_age_out_on_their_own_horizon() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for minute in 0..4 {
        writer
            .record(at(&monitor, CheckOutcome::Pass, 20, minute))
            .await?;
    }
    let now = OffsetDateTime::now_utc();
    let first = writer
        .retain(RetentionPolicy::default().cutoffs(now))
        .await?;
    assert_eq!((first.rows_deleted, first.aggregates_deleted), (4, 0));
    assert_eq!(
        db.check_stats(monitor.id, now - time::Duration::days(30))
            .await?
            .observed,
        4
    );

    let short = RetentionPolicy {
        aggregates: std::time::Duration::from_hours(24 * 10),
        ..RetentionPolicy::default()
    };
    let second = writer.retain(short.cutoffs(now)).await?;
    assert!(second.aggregates_deleted >= 1, "{second:?}");
    assert_eq!(
        db.check_stats(monitor.id, now - time::Duration::days(30))
            .await?
            .observed,
        0
    );
    Ok(())
}

/// `WriteHealth` means "observations are not landing" — the verdict that makes the whole
/// monitor *unavailable* (statuses `unknown`, born-expired beat). A failed retention pass
/// or delivery mark is a supporting duty failing; it used to bump the same counter and
/// flip the monitor to "not observing" until the next observation commit.
#[tokio::test]
async fn only_failed_observation_commits_count_as_write_failures() -> anyhow::Result<()> {
    let (db, monitor, _) = store().await?;
    let write_health: WriteHealth = Arc::default();
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::new(RwLock::new(HashMap::new())),
        HashMap::new(),
        Arc::clone(&write_health),
        8,
        false,
    );
    db.close().await; // every command now fails
    let failures = || write_health.load(std::sync::atomic::Ordering::Relaxed);

    assert!(
        writer
            .retain(RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()))
            .await
            .is_err()
    );
    assert!(writer.mark_delivery("evt", 1, None, None).await.is_err());
    assert_eq!(
        failures(),
        0,
        "supporting-duty failures are not observation failures"
    );

    assert!(writer.record(pass(&monitor)).await.is_err());
    assert_eq!(failures(), 1, "a failed observation commit is");
    Ok(())
}

/// Batched retention (D-012): one writer command handles at most `batch` rows of each kind,
/// and a pass is a series of commands — the folded result equals a single unbounded pass.
#[tokio::test]
async fn retention_runs_in_bounded_batches_and_converges() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for minute in 0..25 {
        writer
            .record(at(&monitor, CheckOutcome::Pass, 9, minute))
            .await?;
    }
    let cutoffs = RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc());
    let report = writer.retain_in_batches(cutoffs, 10).await?;
    assert_eq!(report.rows_deleted, 25);
    assert_eq!(db.list_checks(monitor.id, 100).await?.len(), 0);
    let stats = db
        .check_stats(
            monitor.id,
            OffsetDateTime::now_utc() - time::Duration::days(30),
        )
        .await?;
    assert_eq!(stats.passed, 25, "aggregates merge across batches");
    Ok(())
}

/// The point of batching: an observation committed while a large retention pass runs lands
/// between batches, not after the whole pass. Before, the pass was one transaction inside
/// the single writer, and the coordinator (which awaits its commits) stalled behind it.
#[tokio::test]
async fn an_observation_lands_while_a_large_retention_pass_is_still_running() -> anyhow::Result<()>
{
    let (db, monitor, writer) = store().await?;
    for second in 0..2_000 {
        writer
            .record(probe_at(
                &monitor,
                CheckOutcome::Pass,
                OffsetDateTime::now_utc() - time::Duration::days(9)
                    + time::Duration::seconds(second),
            ))
            .await?;
    }
    let cutoffs = RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc());
    let retention = {
        let writer = writer.clone();
        tokio::spawn(async move { writer.retain_in_batches(cutoffs, 10).await })
    };
    tokio::task::yield_now().await;
    writer.record(pass(&monitor)).await?;
    // The observation is committed; how much expired history was still waiting? With
    // batches, nearly all of it (the record waited for at most a batch or two).
    let waiting = db.list_checks(monitor.id, 5_000).await?.len();
    assert!(
        waiting > 1_000,
        "the observation waited for the whole pass ({waiting} rows left when it landed)"
    );
    let report = retention.await??;
    assert_eq!(report.rows_deleted, 2_000);
    assert_eq!(
        db.list_checks(monitor.id, 10).await?.len(),
        1,
        "the fresh row stays"
    );
    Ok(())
}

/// Retention at the cap, harder case: the expired rows are a thin slice scattered among
/// unexpired evidence, so deleting them frees no whole page, while their daily summaries
/// (one row per monitor-day) need new pages. Free-before-write alone fails `SQLITE_FULL`
/// here and the next pass fails the same way; the pass needs a small page reserve.
#[tokio::test]
async fn retention_at_the_cap_has_room_for_summaries_of_a_thin_expired_slice() -> anyhow::Result<()>
{
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("thin.db").display());
    let setup = Db::connect(&url, 1).await?;
    setup.migrate().await?;
    let monitors: Vec<Monitor> = (0..50)
        .map(|n| {
            NewMonitor::new(
                format!("m{n}"),
                url::Url::parse(&format!("https://m{n}.example.com"))?,
            )
            .map(NewMonitor::into_monitor)
            .map_err(anyhow::Error::from)
        })
        .collect::<anyhow::Result<_>>()?;
    for monitor in &monitors {
        setup.insert_monitor(monitor).await?;
    }
    let (writer, _task) = writer::spawn(
        setup.clone(),
        Arc::new(RwLock::new(HashMap::new())),
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    // Interleaved by insertion order: an expired row per monitor, then unexpired rows.
    for days_ago in [8, 9, 10] {
        for monitor in &monitors {
            writer
                .record(at(monitor, CheckOutcome::Pass, days_ago, 0))
                .await?;
        }
        for minute in 0..20 {
            for monitor in &monitors {
                writer
                    .record(at(monitor, CheckOutcome::Pass, 2, minute))
                    .await?;
            }
        }
    }
    drop(writer);
    let used = setup.used_bytes().await?;
    drop(setup);

    let db = Db::connect_opts(&url, 4, Some(used)).await?;
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::new(RwLock::new(HashMap::new())),
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    let report = writer
        .retain(RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()))
        .await
        .expect("retention must have room for the summaries at the cap");
    assert_eq!(report.rows_deleted, 150);
    assert_eq!(report.days_written, 150);
    // The reserve is bounded: the file grew past the cap by at most the reserve.
    let page_size = 4096;
    assert!(
        db.used_bytes().await? <= used + monitor_db::RETENTION_RESERVE_PAGES * page_size,
        "the reserve is a bound, not a lift of the cap"
    );
    // No pooled connection is left lifted: every ceiling is at most the file's size.
    let now_used = db.used_bytes().await?;
    for _ in 0..8 {
        assert!(
            db.max_bytes().await? <= now_used.max(used),
            "the cap is restored"
        );
    }
    Ok(())
}
