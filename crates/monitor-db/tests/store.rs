//! Writer + store integration tests on a real in-memory database. The contract under test:
//! one transaction per slot, generation invalidation, incident transitions enqueueing the
//! outbox, and `Health::rebuild` recovering streaks across a restart.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use monitor_db::{Db, Observation, QueryError, RetentionPolicy, WriteHealth, WriteOutcome, writer};
use monitor_domain::{
    CheckOutcome, Effect, Health, Monitor, MonitorId, NewMonitor, Role, SlotObservation, Status,
    WorkspaceId,
};
use time::OffsetDateTime;

async fn store() -> anyhow::Result<(Db, Monitor, writer::WriterHandle)> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com/health")?)?.into_monitor();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
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
        workspace: monitor.workspace,
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
        workspace: monitor.workspace,
        generation: monitor.generation,
        slot_at: OffsetDateTime::now_utc(),
        slot: SlotObservation::Missed,
        http_status: None,
        latency_ms: None,
        detail: None,
    }
}

/// A monitor-owned coverage gap: `missed` on the wire, `address_budget` in the
/// detail column — the scheduler's commit for a start the per-address bound refused.
fn budgeted(monitor: &Monitor, at: OffsetDateTime) -> Observation {
    Observation {
        monitor_id: monitor.id,
        workspace: monitor.workspace,
        generation: monitor.generation,
        slot_at: at,
        slot: SlotObservation::Budgeted,
        http_status: None,
        latency_ms: None,
        detail: Some("address_budget".to_owned()),
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
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&strict)
        .await?;

    // One failure on the strict monitor opens its incident immediately.
    let outcome = writer.record(fail(&strict)).await?;
    assert_eq!(outcome, WriteOutcome::Committed(Effect::IncidentOpened));
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(strict.id)
            .await?
            .is_some()
    );
    // And its single fresh pass recovers it (passes_to_recover = 1).
    let outcome = writer.record(pass(&strict)).await?;
    assert_eq!(outcome, WriteOutcome::Committed(Effect::IncidentRecovered));

    // An edit to the stored thresholds takes effect on the next committed observation:
    // bump passes_to_recover to 3, re-open, then prove two passes are not enough.
    let mut edited = strict.clone();
    edited.thresholds = monitor_domain::Thresholds::new(1, 3)?;
    db.workspace(WorkspaceId::DEFAULT)
        .update_monitor(&edited, strict.generation)
        .await?;
    let gate = db
        .workspace(WorkspaceId::DEFAULT)
        .monitor_gate(strict.id)
        .await?
        .expect("gate");
    assert_eq!(gate.thresholds.passes_to_recover, 3);
    let at_gen = |outcome| Observation {
        monitor_id: strict.id,
        workspace: strict.workspace,
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
        .workspace(WorkspaceId::DEFAULT)
        .create_window(
            monitor.id,
            now - time::Duration::seconds(10),
            now + time::Duration::seconds(600),
            "kernel rebuild",
        )
        .await?;
    assert_eq!(window.reason, "kernel rebuild");
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .monitor_gate(monitor.id)
            .await?
            .expect("gate")
            .maintenance,
        "an enclosing window must show active in the gate"
    );
    let listed = db
        .workspace(WorkspaceId::DEFAULT)
        .list_windows(monitor.id)
        .await?;
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
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(monitor.id)
            .await?
            .is_none()
    );
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor.id, 10)
            .await?
            .len(),
        3
    );
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_outbox(monitor.id)
            .await?
            .is_empty()
    );

    // Deleting the window early stands in for expiry — it's the same gate flag. The
    // very next failure opens immediately: the streak counted through the window.
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .delete_window(window.id)
            .await?
    );
    assert!(
        !db.workspace(WorkspaceId::DEFAULT)
            .delete_window(window.id)
            .await?,
        "second delete is 404-truth"
    );
    assert!(
        !db.workspace(WorkspaceId::DEFAULT)
            .monitor_gate(monitor.id)
            .await?
            .expect("gate")
            .maintenance,
        "a deleted window must stop suppressing at the next gate read"
    );
    assert_eq!(
        writer.record(fail(&monitor)).await?,
        WriteOutcome::Committed(Effect::IncidentOpened)
    );
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(monitor.id)
            .await?
            .is_some()
    );
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_outbox(monitor.id)
            .await?
            .len(),
        1
    );
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
        .workspace(WorkspaceId::DEFAULT)
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
        .workspace(WorkspaceId::DEFAULT)
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

    let checks = db
        .workspace(WorkspaceId::DEFAULT)
        .list_checks(monitor.id, 10)
        .await?;
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|row| row.outcome == "http_fail"));

    let incident = db
        .workspace(WorkspaceId::DEFAULT)
        .open_incident(monitor.id)
        .await?;
    assert!(
        incident.is_some(),
        "incident must open with the third commit"
    );

    let outbox = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?;
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

    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(monitor.id)
            .await?
            .is_none()
    );
    let outbox = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?;
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

    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(monitor.id)
            .await?
            .is_none()
    );
    let outcomes = db
        .workspace(WorkspaceId::DEFAULT)
        .recent_outcomes(monitor.id, 10)
        .await?;
    assert!(outcomes.contains(&SlotObservation::Missed));
    Ok(())
}

/// N-5 re-review: a budget gap is neutral in the SQL too. `first_fail_at`
/// must span it — a `missed`+`address_budget` row is not a streak boundary —
/// and the rebuild tail must hand `Health::rebuild` the window of *real*
/// evidence with its gaps interleaved, never a tail of nothing but budget
/// rows (a starved restart would lose the streak it was holding).
#[tokio::test]
async fn a_budget_gap_is_neutral_in_first_fail_at_and_the_rebuild_tail() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    let base = OffsetDateTime::now_utc();
    let slot = |offset_s: i64| base + time::Duration::seconds(offset_s);
    // The streak starts at slot 1; a budget gap at slot 3 sits inside it.
    for offset in 1..=2 {
        writer
            .record(probe_at(&monitor, CheckOutcome::Transport, slot(offset)))
            .await?;
    }
    writer.record(budgeted(&monitor, slot(3))).await?;
    writer
        .record(probe_at(&monitor, CheckOutcome::Transport, slot(4)))
        .await?;

    let incident = db
        .workspace(WorkspaceId::DEFAULT)
        .open_incident(monitor.id)
        .await?
        .expect("three real failures around one gap open the incident");
    assert_eq!(
        incident.first_fail_at.map(OffsetDateTime::unix_timestamp),
        Some(slot(1).unix_timestamp()),
        "the gap is not a boundary — the streak began at slot 1"
    );

    // The rebuild tail: starve the tail harder than rebuild_tail() is long —
    // the real evidence must survive inside its own window, gaps included.
    for offset in 5..=8 {
        writer.record(budgeted(&monitor, slot(offset))).await?;
    }
    writer
        .record(probe_at(&monitor, CheckOutcome::Transport, slot(9)))
        .await?;
    for offset in 10..=20 {
        writer.record(budgeted(&monitor, slot(offset))).await?;
    }
    let tail = db
        .workspace(WorkspaceId::DEFAULT)
        .recent_outcomes(monitor.id, monitor.thresholds.rebuild_tail())
        .await?;
    let real: Vec<_> = tail
        .iter()
        .filter(|o| matches!(o, SlotObservation::Probe(_)))
        .collect();
    assert_eq!(
        real.len(),
        3,
        "the tail kept the last rebuild_tail real slots — gaps never crowd out evidence: {tail:?}"
    );
    assert!(
        tail.contains(&SlotObservation::Budgeted),
        "the gaps stay in the window — they are freshness truth"
    );
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
        .workspace(WorkspaceId::DEFAULT)
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
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&second)
        .await?;

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
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(second.id)
            .await?
            .is_some()
    );
    // Recent rows for A — inside the window, must stay raw.
    writer.record(pass(&monitor)).await?;

    let report = writer
        .retain(&[(
            WorkspaceId::DEFAULT,
            RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()),
        )])
        .await?;
    assert_eq!(report.rows_deleted, 4, "monitor A's four expired rows");
    assert_eq!(report.days_written, 1, "one monitor-day aggregated");

    // A: raw expired rows are gone, aggregates carry their counts; the recent row is raw.
    let checks = db
        .workspace(WorkspaceId::DEFAULT)
        .list_checks(monitor.id, 50)
        .await?;
    assert_eq!(checks.len(), 1);
    let stats = db
        .workspace(WorkspaceId::DEFAULT)
        .check_stats(
            monitor.id,
            OffsetDateTime::now_utc() - time::Duration::days(10),
        )
        .await?;
    assert_eq!((stats.observed, stats.passed, stats.missed), (4, 3, 1));

    // B: open incident → every raw row preserved, nothing aggregated away.
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(second.id, 50)
            .await?
            .len(),
        3
    );
    let stats_b = db
        .workspace(WorkspaceId::DEFAULT)
        .check_stats(
            second.id,
            OffsetDateTime::now_utc() - time::Duration::days(10),
        )
        .await?;
    assert_eq!(stats_b.observed, 3);

    // Outbox is never expired: monitor B's incident_opened event is still there.
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_outbox(second.id)
            .await?
            .len(),
        1
    );
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

    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor.id, 10)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn statuses_only_move_on_commits() -> anyhow::Result<()> {
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let monitor = NewMonitor::new("m", url::Url::parse("https://example.com")?)?.into_monitor();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
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
    let generation = db
        .workspace(WorkspaceId::DEFAULT)
        .update_monitor(&edited, monitor.generation)
        .await?;
    assert_eq!(generation, Some(monitor.generation + 1));

    // A second write carrying the pre-edit generation is a stale write: it must not apply.
    let stale = db
        .workspace(WorkspaceId::DEFAULT)
        .update_monitor(&edited, monitor.generation)
        .await?;
    assert_eq!(stale, None);

    let stored = db
        .workspace(WorkspaceId::DEFAULT)
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
    assert!(
        !db.workspace(WorkspaceId::DEFAULT)
            .has_history(monitor.id)
            .await?
    );
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .delete_monitor(monitor.id)
            .await?
    );
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_monitors()
            .await?
            .is_empty()
    );

    // With a committed check, delete is refused — the row and its evidence both survive.
    let monitor = NewMonitor::new("kept", url::Url::parse("https://example.com")?)?.into_monitor();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
    writer.record(pass(&monitor)).await?;
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .has_history(monitor.id)
            .await?
    );
    assert!(
        !db.workspace(WorkspaceId::DEFAULT)
            .delete_monitor(monitor.id)
            .await?
    );
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor.id, 10)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn outbox_lifecycle_pending_retry_exhausted_delivered() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    let outbox = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?;
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
        .mark_delivery(&event_id, 1, Some(later), None, None)
        .await?;
    assert!(db.pending_outbox("webhook", 3, 10).await?.is_empty());

    // Past the retry time it is due again — simulate by scheduling in the past.
    let past = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    writer
        .mark_delivery(&event_id, 2, Some(past), None, None)
        .await?;
    assert_eq!(db.pending_outbox("webhook", 3, 10).await?.len(), 1);

    // Exhausted (attempts = max, no next attempt) is terminal: never pending again, but the
    // row stays in the table.
    writer.mark_delivery(&event_id, 3, None, None, None).await?;
    assert!(db.pending_outbox("webhook", 3, 10).await?.is_empty());
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_outbox(monitor.id)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_delivered_event_cannot_be_rewritten() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    let event_id = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?[0]
        .event_id
        .clone();

    let now = OffsetDateTime::now_utc();
    writer
        .mark_delivery(&event_id, 1, None, Some(now), None)
        .await?;
    // The guarded update ignores any write to an already-delivered row.
    let later = now + time::Duration::minutes(5);
    writer
        .mark_delivery(&event_id, 99, Some(later), None, None)
        .await?;

    let row = &db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?[0];
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
    let event_id = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?[0]
        .event_id
        .clone();

    writer
        .mark_delivery(&event_id, 1, None, Some(OffsetDateTime::now_utc()), None)
        .await?;
    let row = &db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?[0];
    assert_eq!(row.attempts, 1);
    assert!(row.delivered_at.is_some());
    Ok(())
}

/// S1d (D-017): the workspace webhook is member work — one write opens a
/// `tenant_webhook` outbox channel carrying the row's URL and secret; the secret is
/// minted once, kept across URL edits, rotated after a clear. Unconfigured means the
/// channel is simply never planned. And `mark_delivery`'s detail lands on the row as
/// `last_error` — the refusal note a send-time SSRF refusal must leave (D-017).
#[tokio::test]
async fn a_workspace_webhook_opens_and_closes_its_outbox_channel() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    let member = db.member(WorkspaceId::DEFAULT);

    // Unconfigured: the transition plans only the operator webhook row.
    for _ in 0..3 {
        writer.record(fail(&monitor)).await?;
    }
    let channels: Vec<String> = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?
        .iter()
        .map(|row| row.channel.clone())
        .collect();
    assert_eq!(channels, ["webhook"]);

    // Configure: secret minted once — a later URL edit keeps it (receivers still verify).
    let first = member.set_webhook("https://hooks.example.com/one").await?;
    assert_eq!(first.url, "https://hooks.example.com/one");
    assert!(first.secret.starts_with("smon_whk_"));
    let second = member.set_webhook("https://hooks.example.com/two").await?;
    assert_eq!(second.secret, first.secret, "an edit keeps the secret");
    let read = member.webhook().await?.expect("webhook configured");
    assert_eq!(read.secret, first.secret);

    // Recover: the recovered transition now plans the tenant channel too, and the
    // pending row carries the workspace's target and secret for the send.
    for _ in 0..2 {
        writer.record(pass(&monitor)).await?;
    }
    let rows = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?;
    let channels: Vec<String> = rows.iter().map(|row| row.channel.clone()).collect();
    assert!(
        channels
            .iter()
            .filter(|c| c.as_str() == "tenant_webhook")
            .count()
            == 1,
        "one tenant row for the recovered transition: {channels:?}"
    );
    let pending = db.pending_outbox("tenant_webhook", 3, 10).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].webhook_url.as_deref(), Some(second.url.as_str()));
    assert_eq!(
        pending[0].webhook_secret.as_deref(),
        Some(first.secret.as_str())
    );

    // A failed send records its classified reason — visible on the row.
    let event_id = pending[0].event_id.clone();
    writer
        .mark_delivery(&event_id, 1, None, None, Some("rejected_local"))
        .await?;
    let rows = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?;
    let tenant_row = rows
        .iter()
        .find(|row| row.channel == "tenant_webhook")
        .expect("tenant row");
    assert_eq!(tenant_row.last_error.as_deref(), Some("rejected_local"));
    // And a landed delivery clears it — no stale note on a recovered row.
    writer
        .mark_delivery(&event_id, 2, None, Some(OffsetDateTime::now_utc()), None)
        .await?;
    let rows = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?;
    let tenant_row = rows
        .iter()
        .find(|row| row.channel == "tenant_webhook")
        .expect("tenant row");
    assert_eq!(tenant_row.last_error, None);

    // Cleared: secret rotated out, and the next incident plans no tenant row again.
    member.clear_webhook().await?;
    assert!(member.webhook().await?.is_none());
    let third = member
        .set_webhook("https://hooks.example.com/three")
        .await?;
    assert_ne!(third.secret, first.secret, "a clear rotates the secret");
    Ok(())
}

#[tokio::test]
async fn list_incidents_filters_by_monitor_and_state() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    let other = NewMonitor::new("other", url::Url::parse("https://example.org")?)?.into_monitor();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&other)
        .await?;

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

    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_incidents(None, None, 10)
            .await?
            .len(),
        2
    );
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_incidents(None, Some("open"), 10)
            .await?
            .len(),
        1,
        "only `other` is still open — `monitor` recovered"
    );
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_incidents(Some(monitor.id), Some("resolved"), 10)
            .await?
            .len(),
        1
    );
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_incidents(Some(other.id), Some("open"), 10)
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

    let open = db
        .workspace(WorkspaceId::DEFAULT)
        .open_incident(monitor.id)
        .await?
        .is_some();
    let tail = db
        .workspace(WorkspaceId::DEFAULT)
        .recent_outcomes(monitor.id, 4)
        .await?;
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
    let open = db
        .workspace(WorkspaceId::DEFAULT)
        .open_incident(monitor.id)
        .await?
        .is_some();
    let tail = db
        .workspace(WorkspaceId::DEFAULT)
        .recent_outcomes(monitor.id, 4)
        .await?;
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
    setup
        .workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
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
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;

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
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(monitor.id)
            .await?
            .is_some()
    );

    // Backup while the incident is open, then "restore": open the backup as the store.
    let backup = tmp.path().join("backup.db");
    let bytes = db.backup(&backup).await?;
    assert!(bytes > 0, "a backup must not be empty");
    drop(handle);
    drop(db);

    let restored = Db::connect(&format!("sqlite:{}", backup.display()), 1).await?;
    let open = restored
        .workspace(WorkspaceId::DEFAULT)
        .open_incident(monitor.id)
        .await?;
    assert!(open.is_some(), "restored open incident stays open");
    let due = restored.pending_outbox("webhook", 10, 10).await?;
    assert_eq!(
        due.len(),
        1,
        "the pending outbox event survives the restore"
    );
    // Boot rebuild from the restored store reproduces `down` — not a fresh unknown.
    let tail = restored
        .workspace(WorkspaceId::DEFAULT)
        .recent_outcomes(monitor.id, 4)
        .await?;
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
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
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

    let rows = db
        .workspace(WorkspaceId::DEFAULT)
        .list_outbox(monitor.id)
        .await?;
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

const IDLE: std::time::Duration = std::time::Duration::from_hours(1);

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
    assert!(db.session_valid("live", IDLE).await?);
    assert!(!db.session_valid("missing", IDLE).await?);
    // An expired row is denied and deleted on sight — one lookup is the whole contract.
    assert!(!db.session_valid("dead", IDLE).await?);

    db.create_session("gone-soon", now - time::Duration::seconds(1))
        .await?;
    // `dead` was already deleted by the lazy guard above — the sweep takes what's left.
    assert_eq!(
        db.sweep_expired_sessions(100).await?,
        1,
        "the untouched expired row swept"
    );
    assert!(
        db.session_valid("live", IDLE).await?,
        "the live session survives the sweep"
    );
    db.delete_session("live").await?;
    assert!(!db.session_valid("live", IDLE).await?);
    Ok(())
}

/// D-016: a session unused for longer than the idle limit ends, even inside its absolute
/// lifetime; use keeps it alive (refreshing `last_seen_at`), up to that lifetime.
#[tokio::test]
async fn an_idle_session_ends_and_use_keeps_it_alive() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let t0 = OffsetDateTime::now_utc();
    let min = time::Duration::minutes;
    db.create_session("used", t0 + time::Duration::hours(12))
        .await?;
    db.create_session("idle", t0 + time::Duration::hours(12))
        .await?;

    // "used" is used every 40 minutes for 3 hours: always valid.
    for step in 1..=4 {
        assert!(
            db.session_valid_at("used", t0 + min(40 * step), IDLE)
                .await?,
            "used at +{} min",
            40 * step
        );
    }
    // "idle" was last used at creation: past the hour it is gone, and stays gone.
    assert!(db.session_valid_at("idle", t0 + min(59), IDLE).await?);
    assert!(!db.session_valid_at("idle", t0 + min(59 + 61), IDLE).await?);
    assert!(!db.session_valid_at("idle", t0 + min(59 + 62), IDLE).await?);
    // And "used" ends the same way once its use stops.
    assert!(
        !db.session_valid_at("used", t0 + min(160 + 61), IDLE)
            .await?
    );
    // The absolute lifetime still wins over use.
    db.create_session("short", t0 + min(30)).await?;
    assert!(db.session_valid_at("short", t0 + min(20), IDLE).await?);
    assert!(!db.session_valid_at("short", t0 + min(31), IDLE).await?);
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
        .workspace(WorkspaceId::DEFAULT)
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
        .retain(&[(
            WorkspaceId::DEFAULT,
            RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()),
        )])
        .await?;
    assert_eq!(
        report.rows_deleted, 9,
        "5 old passes + 4 old open-period failures"
    );

    let kept = db
        .workspace(WorkspaceId::DEFAULT)
        .list_checks(monitor.id, 50)
        .await?;
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
        .workspace(WorkspaceId::DEFAULT)
        .check_stats(
            monitor.id,
            OffsetDateTime::now_utc() - time::Duration::days(30),
        )
        .await?;
    assert_eq!(stats.observed, 13);
    assert_eq!(stats.passed, 5);
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .open_incident(monitor.id)
            .await?
            .is_some(),
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
    let w = db.workspace(WorkspaceId::DEFAULT);
    let recent =
        NewMonitor::new("recent", url::Url::parse("https://two.example.com")?)?.into_monitor();
    let open =
        NewMonitor::new("open", url::Url::parse("https://three.example.com")?)?.into_monitor();
    w.insert_monitor(&recent).await?;
    w.insert_monitor(&open).await?;

    // An incident opened and resolved ~400 days ago: past every horizon. One resolved
    // ~20 days ago: kept, but its opened-event was delivered 40 days ago (prunable) while
    // the recovered-event is still pending (kept).
    for (monitor, days) in [(&ancient, 400), (&recent, 20)] {
        for minute in 0..3 {
            writer
                .record(at(monitor, CheckOutcome::Transport, days, minute))
                .await?;
        }
        for minute in 3..5 {
            writer
                .record(at(monitor, CheckOutcome::Pass, days, minute))
                .await?;
        }
    }
    let recent_events = w.list_outbox(recent.id).await?;
    assert_eq!(recent_events.len(), 2);
    writer
        .mark_delivery(
            &recent_events[0].event_id,
            1,
            None,
            Some(OffsetDateTime::now_utc() - time::Duration::days(40)),
            None,
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
    w.create_window(
        ancient.id,
        now - time::Duration::days(401),
        now - time::Duration::days(400),
        "old",
    )
    .await?;
    w.create_window(
        ancient.id,
        now - time::Duration::days(2),
        now - time::Duration::days(1),
        "new",
    )
    .await?;

    let report = writer
        .retain(&[(
            WorkspaceId::DEFAULT,
            RetentionPolicy::default().cutoffs(now),
        )])
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
        w.list_incidents(Some(ancient.id), None, 10)
            .await?
            .is_empty()
    );
    assert!(w.list_outbox(ancient.id).await?.is_empty());
    let ancient_stats = w
        .check_stats(ancient.id, now - time::Duration::days(500))
        .await?;
    assert_eq!(
        ancient_stats.observed, 0,
        "aggregates past 365 days are gone"
    );
    assert_eq!(w.list_windows(ancient.id).await?.len(), 1);

    assert_eq!(w.list_incidents(Some(recent.id), None, 10).await?.len(), 1);
    let left = w.list_outbox(recent.id).await?;
    assert_eq!(left.len(), 1);
    assert!(left[0].delivered_at.is_none(), "the pending event survives");
    let recent_stats = w
        .check_stats(recent.id, now - time::Duration::days(30))
        .await?;
    assert_eq!(recent_stats.observed, 5, "young aggregates are kept");

    assert!(w.open_incident(open.id).await?.is_some());
    assert_eq!(w.list_outbox(open.id).await?.len(), 1);

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
    setup
        .workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
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
        .retain(&[(
            WorkspaceId::DEFAULT,
            RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()),
        )])
        .await
        .expect("retention must run at the cap");
    assert_eq!(report.rows_deleted, stored, "every expired row is freed");
    let stats = db
        .workspace(WorkspaceId::DEFAULT)
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
        .retain(&[(
            WorkspaceId::DEFAULT,
            RetentionPolicy::default().cutoffs(now),
        )])
        .await?;
    assert_eq!((first.rows_deleted, first.aggregates_deleted), (4, 0));
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .check_stats(monitor.id, now - time::Duration::days(30))
            .await?
            .observed,
        4
    );

    let short = RetentionPolicy {
        aggregates: std::time::Duration::from_hours(24 * 10),
        ..RetentionPolicy::default()
    };
    let second = writer
        .retain(&[(WorkspaceId::DEFAULT, short.cutoffs(now))])
        .await?;
    assert!(second.aggregates_deleted >= 1, "{second:?}");
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .check_stats(monitor.id, now - time::Duration::days(30))
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
            .retain(&[(
                WorkspaceId::DEFAULT,
                RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc())
            )])
            .await
            .is_err()
    );
    assert!(
        writer
            .mark_delivery("evt", 1, None, None, None)
            .await
            .is_err()
    );
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
    let report = writer
        .retain_in_batches(&[(WorkspaceId::DEFAULT, cutoffs)], 10)
        .await?;
    assert_eq!(report.rows_deleted, 25);
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor.id, 100)
            .await?
            .len(),
        0
    );
    let stats = db
        .workspace(WorkspaceId::DEFAULT)
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
        tokio::spawn(async move {
            writer
                .retain_in_batches(&[(WorkspaceId::DEFAULT, cutoffs)], 10)
                .await
        })
    };
    tokio::task::yield_now().await;
    writer.record(pass(&monitor)).await?;
    // The observation is committed; how much expired history was still waiting? With
    // batches, nearly all of it (the record waited for at most a batch or two).
    let waiting = db
        .workspace(WorkspaceId::DEFAULT)
        .list_checks(monitor.id, 5_000)
        .await?
        .len();
    assert!(
        waiting > 1_000,
        "the observation waited for the whole pass ({waiting} rows left when it landed)"
    );
    let report = retention.await??;
    assert_eq!(report.rows_deleted, 2_000);
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor.id, 10)
            .await?
            .len(),
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
        setup
            .workspace(WorkspaceId::DEFAULT)
            .insert_monitor(monitor)
            .await?;
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
        .retain(&[(
            WorkspaceId::DEFAULT,
            RetentionPolicy::default().cutoffs(OffsetDateTime::now_utc()),
        )])
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

/// The store and its backups hold session digests, monitored URLs and alert history; the
/// RUSTSEC-2026-0459 assessment in deny.toml relies on nobody but the service user being
/// able to read or plant files beside them. A new store and every backup are created
/// owner-only (0600) whatever the process umask, and the WAL inherits the store's mode.
#[cfg(unix)]
#[tokio::test]
async fn a_new_store_and_its_backups_are_owner_only() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &std::path::Path| -> anyhow::Result<u32> {
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
    };
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("private.db");
    let db = Db::connect(&format!("sqlite:{}", path.display()), 1).await?;
    db.migrate().await?;
    let monitor =
        NewMonitor::new("private", url::Url::parse("https://example.com/health")?)?.into_monitor();
    db.workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
    assert_eq!(mode(&path)?, 0o600, "the store");
    let wal = tmp.path().join("private.db-wal");
    if wal.exists() {
        assert_eq!(mode(&wal)?, 0o600, "the WAL takes the store's mode");
    }

    let backup = tmp.path().join("backup.db");
    assert!(db.backup(&backup).await? > 0);
    assert_eq!(mode(&backup)?, 0o600, "the backup");

    // An existing file is never overwritten or reused (and keeps its own contents).
    std::fs::write(tmp.path().join("taken.db"), b"someone else's file")?;
    assert!(db.backup(&tmp.path().join("taken.db")).await.is_err());
    assert_eq!(
        std::fs::read(tmp.path().join("taken.db"))?,
        b"someone else's file"
    );

    // A symlink where the snapshot would land is refused too: `create_new` fails on any
    // existing name, link or file — the target's contents are never followed or touched.
    let outside = tmp.path().join("outside.db");
    std::fs::write(&outside, b"not a backup")?;
    std::os::unix::fs::symlink(&outside, tmp.path().join("link.db"))?;
    assert!(db.backup(&tmp.path().join("link.db")).await.is_err());
    assert_eq!(std::fs::read(&outside)?, b"not a backup");
    Ok(())
}

/// `VACUUM INTO` takes no bound parameters, so the backup path is an SQL string literal
/// with `'` doubled. A path containing quotes (and the SQL they could smuggle) must write
/// exactly that file and nothing else.
#[tokio::test]
async fn a_backup_path_with_quotes_writes_exactly_that_file() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = Db::connect(&format!("sqlite:{}", tmp.path().join("q.db").display()), 1).await?;
    db.migrate().await?;
    let name = "it's a backup'; DROP TABLE monitors; --.db";
    let path = tmp.path().join(name);
    assert!(db.backup(&path).await? > 0);
    assert!(path.is_file(), "the file is named exactly {name:?}");
    let mut names: Vec<String> = std::fs::read_dir(tmp.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|file| !file.starts_with("q.db"))
        .collect();
    names.sort();
    assert_eq!(names, [name], "and nothing else was written");
    assert!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_monitors()
            .await
            .is_ok(),
        "the store is intact"
    );
    Ok(())
}

/// L6: a store created before stores were owner-only (or copied in with `cp`) kept its old
/// mode — `-rw-r--r--` for the store, its `-wal` and `-shm` after the upgrade — and the
/// RUSTSEC-2026-0459 assessment relies on owner-only files. Opening a store now tightens
/// it and its sidecars to 0600.
#[cfg(unix)]
#[tokio::test]
async fn opening_an_existing_store_makes_it_and_its_sidecars_owner_only() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("old.db");
    let url = format!("sqlite:{}", path.display());
    {
        let db = Db::connect(&url, 1).await?;
        db.migrate().await?;
        let monitor =
            NewMonitor::new("old", url::Url::parse("https://example.com/")?)?.into_monitor();
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&monitor)
            .await?;
        db.close().await;
    }
    let files: Vec<_> = ["old.db", "old.db-wal", "old.db-shm"]
        .iter()
        .map(|name| tmp.path().join(name))
        .collect();
    for file in &files {
        if !file.exists() {
            std::fs::write(file, b"")?; // a sidecar left beside it
        }
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644))?;
    }
    let db = Db::connect_existing(&url, 1, None).await?;
    for file in &files {
        let mode = std::fs::metadata(file)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_monitors()
            .await?
            .len(),
        1,
        "the store is untouched"
    );
    Ok(())
}

/// L5: what the RUNBOOK's rollback rests on. Migrations are forward-only and recorded in
/// sqlx's `_sqlx_migrations` table (not SQLite's `user_version`): a binary that doesn't
/// know a migration the store has applied refuses to start on it. Rolling back across a
/// migration therefore means restoring the backup taken before the upgrade.
#[tokio::test]
async fn an_older_binary_refuses_a_store_a_newer_one_migrated() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("newer.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    db.close().await;
    // A newer binary's migration, as it would have been recorded.
    let raw = sqlx::SqlitePool::connect(&url).await?;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from a newer release', TRUE, X'00', 0)",
    )
    .execute(&raw)
    .await?;
    let user_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&raw)
        .await?;
    raw.close().await;
    assert_eq!(user_version, 0, "user_version is not what guards this");

    let db = Db::connect_existing(&url, 1, None).await?;
    let refused = db
        .migrate()
        .await
        .expect_err("an unknown applied migration must refuse");
    assert!(
        refused.to_string().contains("9999"),
        "the error names the migration: {refused}"
    );
    Ok(())
}

/// Commit latency of the writer over `writer_db` while 32 tasks read through `api`, as
/// the dashboard and API clients do (median, microseconds).
async fn commit_p50_under_reads(api: &Db, writer_db: Db) -> anyhow::Result<u128> {
    let mut monitors = Vec::new();
    for i in 0..50 {
        let monitor = NewMonitor::new(
            format!("r{i}"),
            url::Url::parse(&format!("https://r{i}.example/"))?,
        )?
        .into_monitor();
        api.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&monitor)
            .await?;
        monitors.push(monitor);
    }
    let (handle, _task) = writer::spawn(
        writer_db,
        Arc::new(RwLock::new(HashMap::new())),
        HashMap::new(),
        Arc::default(),
        64,
        false,
    );
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..32 {
        let api = api.clone();
        let stop = Arc::clone(&stop);
        readers.push(tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = api.workspace(WorkspaceId::DEFAULT).list_monitors().await;
            }
        }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut latencies = Vec::new();
    for n in 0..100 {
        let started = std::time::Instant::now();
        handle.record(pass(&monitors[n % monitors.len()])).await?;
        latencies.push(started.elapsed().as_micros());
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for reader in readers {
        let _ = reader.await;
    }
    latencies.sort_unstable();
    Ok(latencies[latencies.len() / 2])
}

/// The load run's commit-lag rise under a million API reads: the writer shared the API's
/// four pooled connections, so each commit queued behind reads for a connection (measured
/// 70-96 ms a commit with 32 readers, against 1-3 ms on a connection of its own). The
/// writer gets its own connection; compared in one run, so machine speed cancels out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_writers_commits_never_queue_behind_api_reads() -> anyhow::Result<()> {
    // Best of two runs each: a loaded machine only adds noise, while the queueing a
    // shared pool causes is structural (about 50x here), so the minimum shows it.
    let (mut shared, mut own) = (u128::MAX, u128::MAX);
    for _ in 0..2 {
        let tmp = tempfile::tempdir()?;
        let api = Db::connect_opts(
            &format!("sqlite:{}", tmp.path().join("s.db").display()),
            4,
            None,
        )
        .await?;
        api.migrate().await?;
        shared = shared.min(commit_p50_under_reads(&api, api.clone()).await?);

        let tmp = tempfile::tempdir()?;
        let api = Db::connect_opts(
            &format!("sqlite:{}", tmp.path().join("o.db").display()),
            4,
            None,
        )
        .await?;
        api.migrate().await?;
        let writer_db = api.writer_connection().await?;
        own = own.min(commit_p50_under_reads(&api, writer_db).await?);
    }
    assert!(
        own * 3 < shared,
        "the writer's connection still queues behind reads: median {own} µs vs {shared} µs shared"
    );
    Ok(())
}

/// The session-churn run caught it: a session insert meeting a held write lock used to
/// return `SQLITE_BUSY` instantly — SQLite's default `busy_timeout` is 0. Pooled connections
/// now wait out the lock and then succeed; the churn gate wants zero login failures.
#[tokio::test]
async fn a_pooled_write_waits_out_a_held_write_lock() -> anyhow::Result<()> {
    // A lock holder outside our pool: an immediate transaction keeps the write lock.
    use sqlx::Connection as _;
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("store.db").display());
    let db = Db::connect(&url, 2).await?;
    db.migrate().await?;

    let mut holder = sqlx::SqliteConnection::connect(&url).await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut holder).await?;

    let writer = {
        let db = db.clone();
        tokio::spawn(async move { db.create_session("s1", OffsetDateTime::now_utc()).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        !writer.is_finished(),
        "the write finished while the lock was held — it failed instantly"
    );
    sqlx::query("COMMIT").execute(&mut holder).await?;
    writer.await??;
    Ok(())
}

/// S1b store contract: `signup` consumes its invite atomically — the workspace, the
/// account and the spent marker land together or not at all, and one code mints one
/// account even when raced. `used_by` naming the new account is what forced the consume
/// to run after the inserts (SQLite FKs are per-statement).
#[tokio::test]
async fn a_signup_consumes_its_invite_atomically() -> anyhow::Result<()> {
    let (db, _, _) = store().await?;
    let now = OffsetDateTime::now_utc();
    let invite = db
        .create_invite("digest-x", now, now + time::Duration::days(1))
        .await?;
    let account = monitor_db::NewAccount {
        id: monitor_domain::AccountId::new(),
        workspace: WorkspaceId::new(),
        workspace_slug: "acme-123".into(),
        workspace_name: "acme".into(),
        email: "a@b.c".into(),
        password_hash: "phc".into(),
    };
    let row = db.signup(&invite.id, &account, now).await?;
    assert_eq!(row.email, "a@b.c");
    assert_eq!(db.list_workspaces().await?.len(), 2, "default + account's");
    let spent = db.invite_by_digest("digest-x").await?.expect("invite row");
    assert!(spent.used_at.is_some() && spent.used_by.as_deref() == Some(row.id.as_str()));

    // Spent: a second signup carrying the same code (different account ids, different
    // email) is refused — and no second workspace appears.
    let mut retry_account = account.clone();
    retry_account.id = monitor_domain::AccountId::new();
    retry_account.workspace = WorkspaceId::new();
    retry_account.workspace_slug = "retry-1".into();
    retry_account.email = "retry@b.c".into();
    let retry = db.signup(&invite.id, &retry_account, now).await;
    assert!(matches!(retry, Err(QueryError::Invalid(_))));
    assert_eq!(db.list_workspaces().await?.len(), 2);

    // Expired and forged ids are the same refusal; and a fresh invite raced by two
    // signups mints exactly one account (the store's WHERE used_at IS NULL decides).
    let dead = db
        .create_invite(
            "digest-dead",
            now - time::Duration::days(2),
            now - time::Duration::days(1),
        )
        .await?;
    let mut second = account.clone();
    second.id = monitor_domain::AccountId::new();
    second.workspace = WorkspaceId::new();
    second.workspace_slug = "other-1".into();
    second.email = "b@b.c".into();
    assert!(matches!(
        db.signup(&dead.id, &second, now).await,
        Err(QueryError::Invalid(_))
    ));
    let fresh = db
        .create_invite("digest-y", now, now + time::Duration::days(1))
        .await?;
    let (won, lost) = (
        db.signup(&fresh.id, &second, now).await,
        db.signup(&fresh.id, &second, now).await,
    );
    assert!(won.is_ok());
    assert!(matches!(lost, Err(QueryError::Invalid(_))));
    Ok(())
}

/// S1c: a signup seats its account as the workspace's owner; the roster admin enforces
/// the last-owner rule inside the transaction.
#[tokio::test]
async fn membership_roles() -> anyhow::Result<()> {
    let (db, _, _) = store().await?;
    let now = OffsetDateTime::now_utc();
    let invite = db
        .create_invite("digest-m", now, now + time::Duration::days(1))
        .await?;
    let account = monitor_db::NewAccount {
        id: monitor_domain::AccountId::new(),
        workspace: WorkspaceId::new(),
        workspace_slug: "ws-m".into(),
        workspace_name: "ws".into(),
        email: "m@b.c".into(),
        password_hash: "phc".into(),
    };
    let row = db.signup(&invite.id, &account, now).await?;
    let ws = account.workspace;
    let owner = account.id;

    // The signup itself seated the owner row.
    let scope = db.owner(ws);
    let members = scope.list_members().await?;
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].account_id, row.id);
    assert_eq!(members[0].role, "owner");
    assert_eq!(db.member_role(ws, owner).await?, Some(Role::Owner));

    // The last owner cannot be demoted or removed — either path would orphan the
    // workspace.
    assert!(matches!(
        scope.set_member_role(owner, Role::Member).await,
        Err(QueryError::Invalid(_))
    ));
    assert!(matches!(
        scope.remove_member(owner).await,
        Err(QueryError::Invalid(_))
    ));

    // Seat a second owner and the first may step down; removing a stranger is a
    // plain false, not an error. The member must be a real account — the FK says so.
    let invite_b = db
        .create_invite("digest-n", now, now + time::Duration::days(1))
        .await?;
    let second = db
        .signup(
            &invite_b.id,
            &monitor_db::NewAccount {
                id: monitor_domain::AccountId::new(),
                workspace: WorkspaceId::new(),
                workspace_slug: "ws-n".into(),
                workspace_name: "ws-n".into(),
                email: "n@b.c".into(),
                password_hash: "phc".into(),
            },
            now,
        )
        .await?
        .id;
    let second = monitor_domain::AccountId(uuid::Uuid::parse_str(&second)?);
    scope.add_member(second, Role::Owner, now).await?;
    assert!(matches!(
        scope.add_member(second, Role::Viewer, now).await,
        Err(QueryError::Invalid(_))
    ));
    assert!(scope.set_member_role(owner, Role::Viewer).await?);
    assert_eq!(db.member_role(ws, owner).await?, Some(Role::Viewer));
    assert!(scope.remove_member(owner).await?);
    assert_eq!(db.member_role(ws, owner).await?, None);
    assert!(
        !scope
            .remove_member(monitor_domain::AccountId::new())
            .await?
    );
    Ok(())
}

/// S1c tokens: only the digest persists; owner is refused outright; lookup is by
/// digest; last-used lands through the batch touch, never per request.
#[tokio::test]
async fn api_tokens_digest_and_touch() -> anyhow::Result<()> {
    let (db, _, _) = store().await?;
    let now = OffsetDateTime::now_utc();
    let scope = db.owner(WorkspaceId::DEFAULT);
    assert!(matches!(
        scope
            .create_api_token("boss", Role::Owner, "d-owner", None, now)
            .await,
        Err(QueryError::Invalid(_))
    ));
    let minted = scope
        .create_api_token("ci", Role::Member, "d-ci", None, now)
        .await?;
    assert_eq!(minted.role, "member");
    assert!(minted.last_used_at.is_none());
    assert!(
        db.api_token_by_digest("d-ci")
            .await?
            .is_some_and(|row| row.id == minted.id)
    );
    assert!(db.api_token_by_digest("d-wrong").await?.is_none());
    let later = now + time::Duration::minutes(5);
    assert_eq!(
        db.touch_api_tokens(&[(
            monitor_domain::ApiTokenId(uuid::Uuid::parse_str(&minted.id)?),
            later
        )])
        .await?,
        1
    );
    assert_eq!(
        db.api_token_by_digest("d-ci").await?.unwrap().last_used_at,
        Some(later)
    );
    // Revocation stamps the row and refuses a second stamp — the row stays.
    assert!(
        scope
            .revoke_api_token(
                monitor_domain::ApiTokenId(uuid::Uuid::parse_str(&minted.id)?),
                later
            )
            .await?
    );
    assert!(
        !scope
            .revoke_api_token(
                monitor_domain::ApiTokenId(uuid::Uuid::parse_str(&minted.id)?),
                later
            )
            .await?
    );
    assert!(
        db.api_token_by_digest("d-ci")
            .await?
            .is_some_and(|row| row.revoked_at == Some(later))
    );
    Ok(())
}

/// S3 (D-022): a retention pass reads each workspace's own horizon. A `free`
/// workspace's evidence expires on the free plan's shorter clocks while an
/// `operator` workspace's same-age rows are still kept — one pass, one batch
/// bound, different truths per tier.
#[tokio::test]
async fn retention_ages_each_workspace_on_its_own_plan() -> anyhow::Result<()> {
    let (db, monitor, writer) = store().await?;
    // The seeded default workspace is the operator's own (migration 0015); a
    // workspace a signup mints is `free`.
    let seeded = db
        .workspace_row(WorkspaceId::DEFAULT)
        .await?
        .expect("seeded default");
    assert_eq!(seeded.plan, "operator");
    let free = db.create_workspace("free-co", "Free Co").await?;
    assert_eq!(
        db.workspace_row(free).await?.expect("free workspace").plan,
        "free"
    );
    let free_monitor =
        NewMonitor::new("free", url::Url::parse("https://free.example/")?)?.into_monitor_for(free);
    db.workspace(free).insert_monitor(&free_monitor).await?;

    // Checks 3 days old: past the free plan's 2-day horizon, inside the
    // configured 7-day one.
    let age = OffsetDateTime::now_utc() - time::Duration::days(3);
    for m in [&monitor, &free_monitor] {
        let mut observation = probe(m, CheckOutcome::Pass);
        observation.slot_at = age;
        writer.record(observation).await?;
    }

    let configured = RetentionPolicy::default();
    let now = OffsetDateTime::now_utc();
    let cutoffs: Vec<(WorkspaceId, _)> = db
        .list_workspaces()
        .await?
        .into_iter()
        .map(|row| {
            let workspace = WorkspaceId(uuid::Uuid::parse_str(&row.id)?);
            let plan = monitor_domain::Plan::parse(&row.plan)
                .ok_or_else(|| anyhow::anyhow!("unreadable plan"))?;
            Ok((
                workspace,
                RetentionPolicy::for_plan(plan, &configured).cutoffs(now),
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    let report = writer.retain(&cutoffs).await?;
    assert_eq!(
        report.rows_deleted, 1,
        "only the free workspace's row expired"
    );
    assert!(
        db.workspace(free)
            .list_checks(free_monitor.id, 10)
            .await?
            .is_empty(),
        "free evidence expired on the free horizon"
    );
    assert_eq!(
        db.workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor.id, 10)
            .await?
            .len(),
        1,
        "operator evidence keeps the configured horizon"
    );
    Ok(())
}

/// The session sweep deletes in bounded batches like every other retention
/// delete (D-012): at most `limit` rows per call, converging by repeating —
/// so housekeeping never holds the store long.
#[tokio::test]
async fn the_session_sweep_deletes_in_bounded_batches() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let past = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    for n in 0..3 {
        db.create_session(&format!("dead-{n}"), past).await?;
    }
    db.create_session("live", OffsetDateTime::now_utc() + time::Duration::hours(1))
        .await?;

    assert_eq!(db.sweep_expired_sessions(2).await?, 2, "one bounded batch");
    assert_eq!(db.sweep_expired_sessions(2).await?, 1, "the remainder");
    assert_eq!(db.sweep_expired_sessions(2).await?, 0, "converged");
    assert!(
        db.session_valid("live", IDLE).await?,
        "the live session was never swept"
    );
    Ok(())
}

/// S3-8: a plan's horizons are a *ceiling*, not an override — `for_plan` takes the
/// field-wise minimum of the plan's retention and the operator's configured
/// ceilings, so a configured value shorter than the free plan's still wins, and
/// the operator tier keeps the configured policy whole.
#[test]
fn plan_retention_never_exceeds_the_operator_ceiling() {
    let configured = RetentionPolicy {
        checks: std::time::Duration::from_secs(3_600), // under free's 2 d
        aggregates: std::time::Duration::from_hours(9_600), // over free's 90 d
        incidents: std::time::Duration::from_hours(9_600), // over free's 90 d
        delivered_outbox: std::time::Duration::from_secs(3_600), // under free's 7 d
    };
    let free = RetentionPolicy::for_plan(monitor_domain::Plan::Free, &configured);
    let free_fixed = monitor_domain::Plan::Free
        .limits()
        .retention
        .expect("the free tier fixes horizons");
    // Shorter configured values win; longer ones clamp to the plan's own horizon.
    assert_eq!(free.checks, configured.checks);
    assert_eq!(free.delivered_outbox, configured.delivered_outbox);
    assert_eq!(free.aggregates, free_fixed.aggregates);
    assert_eq!(free.incidents, free_fixed.incidents);
    // The operator tier defers entirely: configured is the whole policy.
    let operator = RetentionPolicy::for_plan(monitor_domain::Plan::Operator, &configured);
    assert_eq!(operator, configured);
}

/// S3: expired invitations are honest-expired on read already — the sweep's job is
/// keeping the table small, in bounded batches like every other retention delete
/// (D-012). A live invitation survives the pass.
#[tokio::test]
async fn expired_invitations_are_swept_and_live_ones_kept() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let owner = db.owner(WorkspaceId::DEFAULT);
    owner
        .invite_member("a@x.example", Role::Member, "test")
        .await?;
    owner
        .invite_member("b@x.example", Role::Viewer, "test")
        .await?;
    let now = OffsetDateTime::now_utc();
    assert_eq!(owner.list_invitations(now).await?.len(), 2);
    // Nothing is old yet.
    assert_eq!(db.sweep_expired_invitations(now, 500).await?, 0);
    // Past the 72 h TTL both rows are gone — and a one-row batch still converges.
    let future = now + time::Duration::hours(73);
    assert_eq!(db.sweep_expired_invitations(future, 1).await?, 1);
    assert_eq!(db.sweep_expired_invitations(future, 500).await?, 1);
    assert_eq!(db.sweep_expired_invitations(future, 500).await?, 0);
    assert_eq!(owner.list_invitations(future).await?.len(), 0);
    Ok(())
}

/// S3: a released slug stays held against other tenants — but only the newest 64
/// per workspace, so a rename loop can't grow the tombstone table without limit.
/// The proof is cross-tenant: once `ws`'s oldest tombstone falls out of the
/// window, another workspace may take the name again.
#[tokio::test]
async fn released_slugs_are_held_but_capped_per_workspace() -> anyhow::Result<()> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let owner = db.owner(WorkspaceId::DEFAULT);
    owner
        .set_status_page(Some("ws-first"), Some("Page"), &[])
        .await?;
    // 70 renames: the newest tombstones cap at 64, so the earliest names come free.
    for i in 0..70 {
        owner
            .set_status_page(Some(&format!("s{i}")), Some("Page"), &[])
            .await?;
    }
    // A second workspace to ask the question with (the held check is cross-tenant).
    let now = OffsetDateTime::now_utc();
    let invite = db
        .create_invite("digest", now, now + time::Duration::hours(1))
        .await?;
    let account = db
        .signup(
            &invite.id,
            &monitor_db::NewAccount {
                id: monitor_domain::AccountId(uuid::Uuid::now_v7()),
                workspace: WorkspaceId(uuid::Uuid::now_v7()),
                workspace_slug: "ws-other".to_owned(),
                workspace_name: "Other".to_owned(),
                email: "other@x.example".to_owned(),
                password_hash: "x".to_owned(),
            },
            now,
        )
        .await?;
    let other = db.owner(WorkspaceId(uuid::Uuid::parse_str(&account.workspace_id)?));
    // The oldest tombstone fell out of the 64-name window → free to take.
    other
        .set_status_page(Some("ws-first"), Some("Theirs"), &[])
        .await?;
    // The most recently released name is still held → conflict.
    let held = other
        .set_status_page(Some("s68"), Some("Theirs"), &[])
        .await;
    assert!(matches!(held, Err(QueryError::Conflict(_))), "{held:?}");
    Ok(())
}

/// S3 re-review + N3 ruling: 0018 is restored byte-for-byte to its applied
/// content, so the hold lands in 0019 — where it can only name what survives
/// the rename: `default`, the seeded workspace's known name. Names other
/// workspaces lost to the backfill were already rewritten before any hold
/// existed — on an applied database they are unrecoverable, and the test marks
/// that residual honestly rather than pretend coverage. Fresh migrate proves
/// `default` held; the real files are then replayed against pre-0018 fixtures.
#[tokio::test]
async fn the_slug_backfill_holds_every_name_it_replaces() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("store.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let raw = sqlx::SqlitePool::connect(&url).await?;

    // A fresh DB: the seeded `default` is renamed, but the name stays held by the
    // seeded workspace — another tenant can never claim /s/default.
    let held_default: Option<String> =
        sqlx::query_scalar("SELECT workspace_id FROM status_held_slugs WHERE slug = 'default'")
            .fetch_optional(&raw)
            .await?;
    assert_eq!(
        held_default.as_deref(),
        Some("00000000-0000-0000-0000-000000000001"),
        "`default` was renamed without holding it — any tenant can now claim it"
    );

    // Fixture rows standing in for a pre-0018 store: an unpublished email-derived
    // slug (renamed, held), a published page (untouched, unheld), an already
    // random slug (untouched), and an outsider that will try to take the name.
    let (old, published, untouched, outsider) = (
        uuid::Uuid::now_v7().to_string(),
        uuid::Uuid::now_v7().to_string(),
        uuid::Uuid::now_v7().to_string(),
        uuid::Uuid::now_v7().to_string(),
    );
    for (id, slug, title) in [
        (old.as_str(), "alice.example.com", None),
        (published.as_str(), "bob.example.com", Some("Live")),
        (untouched.as_str(), "ws-already-random", None),
        (outsider.as_str(), "ws-outsider", None),
    ] {
        sqlx::query(
            "INSERT INTO workspaces (id, slug, name, created_at, status_title)
             VALUES (?, ?, 'ws', '2026-01-01T00:00:00.000Z', ?)",
        )
        .bind(id)
        .bind(slug)
        .bind(title)
        .execute(&raw)
        .await?;
    }
    // The hold lives in its own migration (N3 ruling: 0018 is restored to its
    // applied bytes; the hold arrives as 0019). Replay the pair in order against
    // the pre-0018 fixtures.
    sqlx::raw_sql(include_str!("../migrations/0018_review_followups.sql"))
        .execute(&raw)
        .await?;
    sqlx::raw_sql(include_str!("../migrations/0019_hold_backfilled_slugs.sql"))
        .execute(&raw)
        .await?;

    let new_slug: String = sqlx::query_scalar("SELECT slug FROM workspaces WHERE id = ?")
        .bind(&old)
        .fetch_one(&raw)
        .await?;
    assert!(new_slug.starts_with("ws-"), "renamed: {new_slug}");
    let held_old: Option<String> = sqlx::query_scalar(
        "SELECT workspace_id FROM status_held_slugs WHERE slug = 'alice.example.com'",
    )
    .fetch_optional(&raw)
    .await?;
    // The name was already rewritten by 0018 before 0019 could see it — on an
    // applied database this residual is unrecoverable; `default` is the only
    // replaced name the migration can still hold (N3 ruling).
    assert_eq!(
        held_old.as_deref(),
        None,
        "unrecoverable post-rename: only `default` can still be held"
    );
    let bob_slug: String = sqlx::query_scalar("SELECT slug FROM workspaces WHERE id = ?")
        .bind(&published)
        .fetch_one(&raw)
        .await?;
    assert_eq!(
        bob_slug, "bob.example.com",
        "a published page keeps its URL"
    );
    let held_bob: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM status_held_slugs WHERE slug = 'bob.example.com'")
            .fetch_one(&raw)
            .await?;
    assert_eq!(held_bob, 0, "a name still in use is not tombstoned");

    // The contract end to end on the recoverable name: `default` is held under
    // the seeded workspace, so the outsider's claim is a conflict — while the
    // backfilled name that was already rewritten is claimable again (the
    // accepted residual: its old value no longer exists anywhere to hold).
    let outsider_view = db.owner(WorkspaceId(uuid::Uuid::parse_str(&outsider)?));
    let claimed = outsider_view
        .set_status_page(Some("default"), Some("Theirs"), &[])
        .await;
    assert!(
        matches!(claimed, Err(QueryError::Conflict(_))),
        "another tenant took the seeded name: {claimed:?}"
    );
    let backfilled = outsider_view
        .set_status_page(Some("alice.example.com"), Some("Theirs"), &[])
        .await;
    assert!(
        backfilled.is_ok(),
        "the post-rename name is unrecoverable — claimable, not held: {backfilled:?}"
    );
    raw.close().await;
    Ok(())
}

/// N4: the `default` hold must not lapse. The bounded held-set (newest 64 per
/// workspace) exists for churned tenant names, but `default` is the seeded
/// page's name — readers who knew it must never see it serve a new tenant,
/// however many renames the operator does afterwards.
#[tokio::test]
async fn the_default_slug_never_lapses_out_of_its_hold() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("store.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let seed = WorkspaceId::DEFAULT;
    // 65 renames: one more than the per-workspace held cap. Each rename holds the
    // name it leaves behind and prunes the oldest holds.
    for i in 0..65 {
        db.owner(seed)
            .set_status_page(Some(&format!("renamed-{i}")), None, &[])
            .await?;
    }
    let raw = sqlx::SqlitePool::connect(&url).await?;
    let held: Option<String> =
        sqlx::query_scalar("SELECT workspace_id FROM status_held_slugs WHERE slug = 'default'")
            .fetch_optional(&raw)
            .await?;
    assert_eq!(
        held.as_deref(),
        Some(seed.0.to_string().as_str()),
        "`default` fell out of the held set after the cap — any tenant can claim it"
    );
    raw.close().await;
    Ok(())
}
