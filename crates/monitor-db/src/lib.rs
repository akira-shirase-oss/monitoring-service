//! SQLite persistence for Service Monitor.
//!
//! SQLite is the authority for configuration, checks, incidents and the notification outbox.
//! The contract calls for one dedicated database worker so writes stay serialized and never
//! block probe workers; for now `Db` is a thin pool wrapper and the single-writer task arrives
//! with the coordinator (D-003). WAL mode and foreign keys are on, always.

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::str::FromStr;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use monitor_domain::{CheckOutcome, Effect, Monitor, MonitorId, SlotObservation};

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
