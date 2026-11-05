//! SQLite persistence for Service Monitor.
//!
//! SQLite is the authority for configuration, checks, incidents and the notification outbox.
//! The contract calls for one dedicated database worker so writes stay serialized and never
//! block probe workers; for now `Db` is a thin pool wrapper and the single-writer task arrives
//! with the coordinator (D-003). WAL mode and foreign keys are on, always.

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;
use std::str::FromStr;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use monitor_domain::{CheckOutcome, CoverageStats, Effect, Monitor, MonitorId, SlotObservation};

pub mod writer;
pub use writer::{Observation, SharedStatuses, WriteOutcome, WriterError, WriterHandle};

#[derive(Debug, Error)]
pub enum DbError {
    #[error("invalid database URL: {0}")]
    BadUrl(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

#[derive(Debug, Error)]
pub enum QueryError {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error("stored row violates the domain contract: {0}")]
    Corrupt(String),
    #[error("value out of range: {0}")]
    OutOfRange(&'static str),
}

/// What one `retain` pass did — for logs and tests.
#[derive(Debug, Clone, Copy)]
pub struct RetainReport {
    /// Daily-aggregate rows the pass wrote or merged.
    pub days_written: u64,
    /// Raw check rows the pass deleted after aggregating.
    pub rows_deleted: u64,
}

/// A handle to the monitor's SQLite store.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Opens (creating if needed) a SQLite database with WAL and foreign keys on.
    ///
    /// `max_connections` exists for tests: an in-memory database must stay on one connection or
    /// every pooled checkout sees a different empty database.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, DbError> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<(), DbError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    /// A cheap liveness check for `/readyz`.
    pub async fn ping(&self) -> Result<(), QueryError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn insert_monitor(&self, monitor: &Monitor) -> Result<(), QueryError> {
        sqlx::query(
            "INSERT INTO monitors (id, name, url, interval_s, timeout_s, generation, paused, created_at)
             VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
        )
        .bind(monitor.id.0.to_string())
        .bind(&monitor.name)
        .bind(monitor.url.as_str())
        .bind(i64::try_from(monitor.interval.as_secs()).map_err(|_| QueryError::OutOfRange("interval"))?)
        .bind(i64::try_from(monitor.timeout.as_secs()).map_err(|_| QueryError::OutOfRange("timeout"))?)
        .bind(i64::try_from(monitor.generation).map_err(|_| QueryError::OutOfRange("generation"))?)
        .bind(OffsetDateTime::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_monitors(&self) -> Result<Vec<MonitorRow>, QueryError> {
        let rows = sqlx::query_as::<_, MonitorRow>(
            "SELECT id, name, url, interval_s, timeout_s, generation, paused, created_at
             FROM monitors ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The single open incident for a monitor, if one exists — what a restart rebuilds
    /// `Health::with_open_incident` from.
    pub async fn open_incident(
        &self,
        monitor: MonitorId,
    ) -> Result<Option<IncidentRow>, QueryError> {
        let row = sqlx::query_as::<_, IncidentRow>(
            "SELECT id, monitor_id, opened_at, resolved_at, state
             FROM incidents WHERE monitor_id = ? AND state = 'open'",
        )
        .bind(monitor.0.to_string())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Persist an edited monitor, bumping `generation` in the same statement. The write is
    /// conditioned on the generation the caller read — two racing edits can't silently
    /// overwrite each other. Returns `None` when the generation no longer matches.
    pub async fn update_monitor(
        &self,
        monitor: &Monitor,
        expected_generation: u64,
    ) -> Result<Option<u64>, QueryError> {
        let result = sqlx::query(
            "UPDATE monitors
             SET name = ?, url = ?, interval_s = ?, timeout_s = ?, paused = ?,
                 generation = generation + 1
             WHERE id = ? AND generation = ?",
        )
        .bind(&monitor.name)
        .bind(monitor.url.as_str())
        .bind(
            i64::try_from(monitor.interval.as_secs())
                .map_err(|_| QueryError::OutOfRange("interval"))?,
        )
        .bind(
            i64::try_from(monitor.timeout.as_secs())
                .map_err(|_| QueryError::OutOfRange("timeout"))?,
        )
        .bind(monitor.paused)
        .bind(monitor.id.0.to_string())
        .bind(i64::try_from(expected_generation).map_err(|_| QueryError::OutOfRange("generation"))?)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.monitor_generation(monitor.id).await
    }

    /// Delete a monitor that has never recorded evidence, in a single conditional statement:
    /// atomic, so a probe committing a check between "does it have history?" and "delete"
    /// can't lose its rows. `false` means the row was absent *or* has history — the caller
    /// distinguishes with `has_history` for the response code.
    pub async fn delete_monitor(&self, monitor: MonitorId) -> Result<bool, QueryError> {
        let result = sqlx::query(
            "DELETE FROM monitors WHERE id = ?
             AND NOT EXISTS (SELECT 1 FROM checks WHERE monitor_id = ?)
             AND NOT EXISTS (SELECT 1 FROM incidents WHERE monitor_id = ?)",
        )
        .bind(monitor.0.to_string())
        .bind(monitor.0.to_string())
        .bind(monitor.0.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether the monitor has any recorded evidence (checks or incidents). The FKs would
    /// refuse the delete anyway; asking first turns a constraint violation into a clean
    /// answer the API can translate to a `conflict` response.
    pub async fn has_history(&self, monitor: MonitorId) -> Result<bool, QueryError> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM checks WHERE monitor_id = ?
                           UNION ALL
                           SELECT 1 FROM incidents WHERE monitor_id = ? LIMIT 1)",
        )
        .bind(monitor.0.to_string())
        .bind(monitor.0.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    /// Every currently-open incident keyed by monitor — the snapshot writer's view of
    /// "which services are in a recorded outage".
    pub async fn open_incidents(&self) -> Result<HashMap<MonitorId, OffsetDateTime>, QueryError> {
        let rows = sqlx::query_as::<_, (String, OffsetDateTime)>(
            "SELECT monitor_id, opened_at FROM incidents WHERE state = 'open'",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, opened_at)| {
                Uuid::parse_str(&id)
                    .map(|id| (MonitorId(id), opened_at))
                    .map_err(|e| QueryError::Corrupt(format!("monitor id: {e}")))
            })
            .collect()
    }

    /// Incidents, newest first; both filters optional (`None` = unfiltered).
    pub async fn list_incidents(
        &self,
        monitor: Option<MonitorId>,
        state: Option<&str>,
        limit: i64,
    ) -> Result<Vec<IncidentRow>, QueryError> {
        let monitor_id = monitor.map(|id| id.0.to_string());
        let state = state.map(str::to_owned);
        let rows = sqlx::query_as::<_, IncidentRow>(
            "SELECT id, monitor_id, opened_at, resolved_at, state FROM incidents
             WHERE (?1 IS NULL OR monitor_id = ?1) AND (?2 IS NULL OR state = ?2)
             ORDER BY opened_at DESC, id DESC LIMIT ?3",
        )
        .bind(&monitor_id)
        .bind(&state)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The monitor's current generation — the writer compares it against the generation an
    /// observation was claimed under.
    pub async fn monitor_generation(&self, monitor: MonitorId) -> Result<Option<u64>, QueryError> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT generation FROM monitors WHERE id = ?")
            .bind(monitor.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|(generation,)| {
            u64::try_from(generation).map_err(|_| QueryError::Corrupt("negative generation".into()))
        })
        .transpose()
    }

    /// The `limit` most recent slot observations for a monitor, oldest→newest — the tail the
    /// restart rebuild replays through `Health`.
    pub async fn recent_outcomes(
        &self,
        monitor: MonitorId,
        limit: i64,
    ) -> Result<Vec<SlotObservation>, QueryError> {
        let rows = sqlx::query_as::<_, (String, Option<i64>)>(
            "SELECT outcome, http_status FROM checks
             WHERE monitor_id = ? ORDER BY slot_at DESC, id DESC LIMIT ?",
        )
        .bind(monitor.0.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .rev()
            .map(|(outcome, http_status)| decode_outcome(outcome, *http_status))
            .collect()
    }

    /// Outcome counts for a monitor since `since` — the row half of `CoverageStats`;
    /// the API computes `expected` from the monitor's interval over the elapsed window.
    /// Unions live rows with the daily aggregates retention writes, so coverage over
    /// long windows stays honest after raw rows expire.
    pub async fn check_stats(
        &self,
        monitor: MonitorId,
        since: OffsetDateTime,
    ) -> Result<CoverageStats, QueryError> {
        let day = since.date().to_string();
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT outcome, SUM(c) FROM (
               SELECT outcome, COUNT(*) AS c FROM checks
                WHERE monitor_id = ?1 AND slot_at >= ?2 GROUP BY outcome
               UNION ALL SELECT 'pass', pass FROM check_stats_daily
                WHERE monitor_id = ?1 AND day >= ?3
               UNION ALL SELECT 'http_fail', http_fail FROM check_stats_daily
                WHERE monitor_id = ?1 AND day >= ?3
               UNION ALL SELECT 'transport', transport FROM check_stats_daily
                WHERE monitor_id = ?1 AND day >= ?3
               UNION ALL SELECT 'timeout', timeout FROM check_stats_daily
                WHERE monitor_id = ?1 AND day >= ?3
               UNION ALL SELECT 'missed', missed FROM check_stats_daily
                WHERE monitor_id = ?1 AND day >= ?3
             ) GROUP BY outcome",
        )
        .bind(monitor.0.to_string())
        .bind(since)
        .bind(day)
        .fetch_all(&self.pool)
        .await?;
        let (mut observed, mut missed, mut passed) = (0_u64, 0_u64, 0_u64);
        for (outcome, count) in rows {
            let count = u64::try_from(count).map_err(|_| QueryError::Corrupt("count".into()))?;
            match outcome.as_str() {
                "missed" => missed += count,
                "pass" => {
                    observed += count;
                    passed += count;
                }
                "http_fail" | "transport" | "timeout" => observed += count,
                other => return Err(QueryError::Corrupt(format!("outcome {other}"))),
            }
        }
        Ok(CoverageStats {
            expected: 0,
            observed,
            missed,
            passed,
        })
    }

    /// Retention pass: roll checks older than `cutoff` into `check_stats_daily`, then
    /// delete the raw rows — one transaction, so a crash can't produce history that is
    /// neither raw nor aggregated. Monitors with an open incident keep their raw rows:
    /// an incident's birth evidence never expires while the incident is open.
    /// Called only by the single writer.
    pub async fn retain(&self, cutoff: OffsetDateTime) -> Result<RetainReport, QueryError> {
        let mut tx = self.pool.begin().await?;
        let aggregated = sqlx::query(
            "INSERT INTO check_stats_daily
                (monitor_id, day, pass, http_fail, transport, timeout, missed)
             SELECT monitor_id, date(slot_at),
                    SUM(outcome = 'pass'), SUM(outcome = 'http_fail'),
                    SUM(outcome = 'transport'), SUM(outcome = 'timeout'),
                    SUM(outcome = 'missed')
             FROM checks
             WHERE slot_at < ?1
               AND monitor_id NOT IN (SELECT monitor_id FROM incidents WHERE state = 'open')
             GROUP BY monitor_id, date(slot_at)
             ON CONFLICT (monitor_id, day) DO UPDATE SET
                pass = check_stats_daily.pass + excluded.pass,
                http_fail = check_stats_daily.http_fail + excluded.http_fail,
                transport = check_stats_daily.transport + excluded.transport,
                timeout = check_stats_daily.timeout + excluded.timeout,
                missed = check_stats_daily.missed + excluded.missed",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query(
            "DELETE FROM checks WHERE slot_at < ?1
             AND monitor_id NOT IN (SELECT monitor_id FROM incidents WHERE state = 'open')",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RetainReport {
            days_written: aggregated.rows_affected(),
            rows_deleted: deleted.rows_affected(),
        })
    }

    /// Check history for a monitor, newest first.
    pub async fn list_checks(
        &self,
        monitor: MonitorId,
        limit: i64,
    ) -> Result<Vec<CheckRow>, QueryError> {
        let rows = sqlx::query_as::<_, CheckRow>(
            "SELECT id, monitor_id, generation, slot_at, outcome, http_status, latency_ms,
                    detail, recorded_at
             FROM checks WHERE monitor_id = ? ORDER BY slot_at DESC, id DESC LIMIT ?",
        )
        .bind(monitor.0.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Outbox rows for a monitor, oldest first — delivery and tests both read this.
    pub async fn list_outbox(&self, monitor: MonitorId) -> Result<Vec<OutboxRow>, QueryError> {
        let rows = sqlx::query_as::<_, OutboxRow>(
            "SELECT id, event_id, monitor_id, incident_id, kind, attempts, next_attempt_at,
                    delivered_at, created_at
             FROM notify_outbox WHERE monitor_id = ? ORDER BY id",
        )
        .bind(monitor.0.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Outbox rows due for delivery: not yet delivered, not exhausted (`attempts < max`), and
    /// either never attempted or past their scheduled retry. `max` belongs to the deliverer's
    /// config — the store just records what the deliverer asked it to.
    pub async fn pending_outbox(
        &self,
        max_attempts: i64,
        limit: i64,
    ) -> Result<Vec<DeliveryRow>, QueryError> {
        let rows = sqlx::query_as::<_, DeliveryRow>(
            "SELECT o.event_id, o.kind, o.attempts, o.created_at,
                    m.id AS monitor_id, m.name AS monitor_name, m.url AS monitor_url,
                    i.id AS incident_id, i.opened_at, i.resolved_at
             FROM notify_outbox o
             JOIN monitors m ON m.id = o.monitor_id
             JOIN incidents i ON i.id = o.incident_id
             WHERE o.delivered_at IS NULL AND o.attempts < ?
               AND (o.next_attempt_at IS NULL OR o.next_attempt_at <= ?)
             ORDER BY o.id LIMIT ?",
        )
        .bind(max_attempts)
        .bind(OffsetDateTime::now_utc())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Record a delivery attempt's bookkeeping. `delivered_at` marks the event terminal;
    /// `next_attempt_at = None` with `delivered_at = None` is the exhausted terminal state —
    /// the row stays in the table, visible, never silently dropped.
    pub(crate) async fn update_delivery(
        &self,
        event_id: &str,
        attempts: i64,
        next_attempt_at: Option<OffsetDateTime>,
        delivered_at: Option<OffsetDateTime>,
    ) -> Result<(), QueryError> {
        sqlx::query(
            "UPDATE notify_outbox SET attempts = ?, next_attempt_at = ?, delivered_at = ?
             WHERE event_id = ? AND delivered_at IS NULL",
        )
        .bind(attempts)
        .bind(next_attempt_at)
        .bind(delivered_at)
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Commit one slot's observation, its incident transition and the notification outbox row
    /// in a single transaction — all three or none.
    pub(crate) async fn commit_observation(
        &self,
        observation: &Observation,
        effect: Effect,
    ) -> Result<(), QueryError> {
        let mut tx = self.pool.begin().await?;
        let (outcome, http_status) = encode_slot(observation.slot, observation.http_status);
        sqlx::query(
            "INSERT INTO checks
                (monitor_id, generation, slot_at, outcome, http_status, latency_ms, detail, recorded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(observation.monitor_id.0.to_string())
        .bind(i64::try_from(observation.generation).map_err(|_| QueryError::OutOfRange("generation"))?)
        .bind(observation.slot_at)
        .bind(outcome)
        .bind(http_status)
        .bind(
            observation
                .latency_ms
                .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX)),
        )
        .bind(&observation.detail)
        .bind(OffsetDateTime::now_utc())
        .execute(&mut *tx)
        .await?;

        match effect {
            Effect::Unchanged => {}
            Effect::IncidentOpened => {
                let incident_id: i64 = sqlx::query_scalar(
                    "INSERT INTO incidents (monitor_id, opened_at, state)
                     VALUES (?, ?, 'open') RETURNING id",
                )
                .bind(observation.monitor_id.0.to_string())
                .bind(observation.slot_at)
                .fetch_one(&mut *tx)
                .await?;
                enqueue_outbox(
                    &mut tx,
                    observation.monitor_id,
                    incident_id,
                    "incident_opened",
                )
                .await?;
            }
            Effect::IncidentRecovered => {
                let incident_id: Option<i64> = sqlx::query_scalar(
                    "UPDATE incidents SET state = 'resolved', resolved_at = ?
                     WHERE monitor_id = ? AND state = 'open' RETURNING id",
                )
                .bind(observation.slot_at)
                .bind(observation.monitor_id.0.to_string())
                .fetch_optional(&mut *tx)
                .await?;
                let Some(incident_id) = incident_id else {
                    return Err(QueryError::Corrupt(
                        "recovery without an open incident".into(),
                    ));
                };
                enqueue_outbox(
                    &mut tx,
                    observation.monitor_id,
                    incident_id,
                    "incident_recovered",
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn enqueue_outbox(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    monitor: MonitorId,
    incident_id: i64,
    kind: &'static str,
) -> Result<(), QueryError> {
    sqlx::query(
        "INSERT INTO notify_outbox (event_id, monitor_id, incident_id, kind, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(monitor.0.to_string())
    .bind(incident_id)
    .bind(kind)
    .bind(OffsetDateTime::now_utc())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Map a slot onto the `checks.outcome` text + status column.
fn encode_slot(slot: SlotObservation, http_status: Option<u16>) -> (&'static str, Option<i64>) {
    let status = http_status.map(i64::from);
    match slot {
        SlotObservation::Missed => ("missed", None),
        SlotObservation::Probe(CheckOutcome::Pass) => ("pass", status),
        SlotObservation::Probe(CheckOutcome::HttpFail { .. }) => ("http_fail", status),
        SlotObservation::Probe(CheckOutcome::Transport) => ("transport", None),
        SlotObservation::Probe(CheckOutcome::Timeout) => ("timeout", None),
    }
}

/// Rebuild a `SlotObservation` from a stored row; unknown text is corruption, not a default.
fn decode_outcome(outcome: &str, http_status: Option<i64>) -> Result<SlotObservation, QueryError> {
    let status = http_status
        .map(|value| {
            u16::try_from(value).map_err(|_| QueryError::Corrupt("http_status out of range".into()))
        })
        .transpose()?;
    Ok(match outcome {
        "missed" => SlotObservation::Missed,
        "pass" => SlotObservation::Probe(CheckOutcome::Pass),
        "http_fail" => SlotObservation::Probe(CheckOutcome::HttpFail {
            status: status.ok_or_else(|| QueryError::Corrupt("http_fail without status".into()))?,
        }),
        "transport" => SlotObservation::Probe(CheckOutcome::Transport),
        "timeout" => SlotObservation::Probe(CheckOutcome::Timeout),
        other => return Err(QueryError::Corrupt(format!("unknown outcome {other:?}"))),
    })
}

/// One `monitors` row, as stored. Converting to `Monitor` validates the stored strings again —
/// the database is local but the domain contract still holds ("parse, don't validate" works at
/// every boundary, including our own store).
#[derive(Debug, FromRow)]
pub struct MonitorRow {
    pub id: String,
    pub name: String,
    pub url: String,
    pub interval_s: i64,
    pub timeout_s: i64,
    pub generation: i64,
    pub paused: bool,
    pub created_at: OffsetDateTime,
}

impl TryFrom<MonitorRow> for Monitor {
    type Error = QueryError;

    fn try_from(row: MonitorRow) -> Result<Self, Self::Error> {
        let id = Uuid::parse_str(&row.id)
            .map_err(|e| QueryError::Corrupt(format!("monitor id: {e}")))?;
        let url = url::Url::parse(&row.url)
            .map_err(|e| QueryError::Corrupt(format!("monitor url: {e}")))?;
        let interval = u64::try_from(row.interval_s)
            .map_err(|_| QueryError::Corrupt("negative interval".into()))?;
        let timeout = u64::try_from(row.timeout_s)
            .map_err(|_| QueryError::Corrupt("negative timeout".into()))?;
        let generation = u64::try_from(row.generation)
            .map_err(|_| QueryError::Corrupt("negative generation".into()))?;
        Ok(Self {
            id: MonitorId(id),
            name: row.name,
            url,
            interval: std::time::Duration::from_secs(interval),
            timeout: std::time::Duration::from_secs(timeout),
            generation,
            paused: row.paused,
        })
    }
}

#[derive(Debug, FromRow)]
pub struct IncidentRow {
    pub id: i64,
    pub monitor_id: String,
    pub opened_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    pub state: String,
}

#[derive(Debug, FromRow)]
pub struct CheckRow {
    pub id: i64,
    pub monitor_id: String,
    pub generation: i64,
    pub slot_at: OffsetDateTime,
    pub outcome: String,
    pub http_status: Option<i64>,
    pub latency_ms: Option<i64>,
    pub detail: Option<String>,
    pub recorded_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
pub struct OutboxRow {
    pub id: i64,
    pub event_id: String,
    pub monitor_id: String,
    pub incident_id: i64,
    pub kind: String,
    pub attempts: i64,
    pub next_attempt_at: Option<OffsetDateTime>,
    pub delivered_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

/// One pending event joined with the monitor and incident its payload needs.
#[derive(Debug, FromRow)]
pub struct DeliveryRow {
    pub event_id: String,
    pub kind: String,
    pub attempts: i64,
    pub created_at: OffsetDateTime,
    pub monitor_id: String,
    pub monitor_name: String,
    pub monitor_url: String,
    pub incident_id: i64,
    pub opened_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
}
