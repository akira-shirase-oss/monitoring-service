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

use monitor_domain::{
    CheckOutcome, CoverageStats, Effect, Monitor, MonitorId, SlotObservation, Thresholds,
};

pub mod writer;
pub use writer::{
    Observation, SharedStatuses, WriteHealth, WriteOutcome, WriterError, WriterHandle,
};

#[derive(Debug, Error)]
pub enum DbError {
    #[error("invalid database URL: {0}")]
    BadUrl(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Query(#[from] QueryError),
    /// A normal start found no store (D-015).
    #[error(
        "no store at {}. If it lives on a separate volume, the volume is not mounted (yet). \
         For a new install, create it once with `monitor-server --init`.",
        path.display()
    )]
    NoStore { path: std::path::PathBuf },
    /// A normal start found a file with no tables where the store should be (D-015).
    #[error(
        "the store at {} is empty (no tables): refusing to start on a fresh store where one is \
         expected. For a new install, run `monitor-server --init`.",
        path.display()
    )]
    EmptyStore { path: std::path::PathBuf },
}

#[derive(Debug, Error)]
pub enum QueryError {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error("stored row violates the domain contract: {0}")]
    Corrupt(String),
    #[error("value out of range: {0}")]
    OutOfRange(&'static str),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("I/O error: {0}")]
    Io(String),
}

impl QueryError {
    pub(crate) fn io(error: &std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// What one `retain` pass (or batch) did — for logs and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetainReport {
    /// Daily-aggregate rows the pass wrote or merged.
    pub days_written: u64,
    /// Raw check rows the pass deleted after aggregating.
    pub rows_deleted: u64,
    /// Daily-aggregate rows past the aggregate horizon.
    pub aggregates_deleted: u64,
    /// Resolved incidents past the incident horizon (their outbox rows go with them).
    pub incidents_deleted: u64,
    /// Outbox rows deleted: delivered ones past the outbox horizon, plus every row of a
    /// pruned incident.
    pub outbox_deleted: u64,
    /// The delivered-past-horizon share of `outbox_deleted` (bounded per batch).
    pub delivered_outbox_deleted: u64,
    /// Maintenance windows that ended before the incident horizon.
    pub windows_deleted: u64,
}

impl RetainReport {
    /// Whether a batch of size `batch` found less than a full batch of every kind — the
    /// pass is complete (nothing expired is left beyond what this batch took).
    pub fn exhausted(&self, batch: u32) -> bool {
        let batch = u64::from(batch.max(1));
        [
            self.rows_deleted,
            self.aggregates_deleted,
            self.incidents_deleted,
            self.windows_deleted,
        ]
        .iter()
        .all(|&count| count < batch)
            // Delivered outbox rows are bounded per batch too; incident-owned rows ride
            // their incident, so only the delivered share is compared.
            && self.delivered_outbox_deleted < batch
    }

    /// Fold one batch into a running total.
    pub fn add(&mut self, other: &Self) {
        self.days_written += other.days_written;
        self.rows_deleted += other.rows_deleted;
        self.aggregates_deleted += other.aggregates_deleted;
        self.incidents_deleted += other.incidents_deleted;
        self.outbox_deleted += other.outbox_deleted;
        self.delivered_outbox_deleted += other.delivered_outbox_deleted;
        self.windows_deleted += other.windows_deleted;
    }
}

/// How long each kind of record lives (D-012). Every table the monitor writes has a
/// horizon except `monitors` (bounded by `SMON_MAX_MONITORS`), open incidents (they end
/// when evidence closes them) and undelivered outbox rows of kept incidents (visible
/// "no receiver" evidence until their incident ages out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Raw `checks` rows (then rolled into daily aggregates). An open incident's birth
    /// streak is the one exception — at most `failures_to_open` rows.
    pub checks: std::time::Duration,
    /// `check_stats_daily` rows.
    pub aggregates: std::time::Duration,
    /// Resolved incidents, counted from `resolved_at`; ended maintenance windows, from
    /// `ends_at` (a window only explains evidence that is itself kept).
    pub incidents: std::time::Duration,
    /// Delivered outbox rows, counted from `delivered_at`.
    pub delivered_outbox: std::time::Duration,
}

/// Pages retention may allocate past `SMON_MAX_DB_BYTES` for its daily summaries (1 MiB at
/// the default 4 KiB page size). Deleting a thin slice of expired rows scattered among
/// unexpired evidence frees no whole page, while its summaries need new ones; without a
/// reserve such a pass fails `SQLITE_FULL` at the cap every time. The allowance is absolute
/// (`reserve_ceiling`): across all passes together the file exceeds the cap by at most this
/// much, and only by what summaries actually used — once it is spent, retention at the cap
/// fails again and the remedy is a larger cap.
pub const RETENTION_RESERVE_PAGES: u64 = 256;

const DAY: std::time::Duration = std::time::Duration::from_hours(24);

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            checks: DAY * 7,
            aggregates: DAY * 365,
            incidents: DAY * 365,
            delivered_outbox: DAY * 30,
        }
    }
}

impl RetentionPolicy {
    /// The expiry instants for a pass run at `now`. A horizon too large to subtract
    /// saturates to "keep everything".
    pub fn cutoffs(&self, now: OffsetDateTime) -> RetentionCutoffs {
        let before = |keep: std::time::Duration| {
            time::Duration::try_from(keep)
                .ok()
                .and_then(|keep| now.checked_sub(keep))
                .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        };
        RetentionCutoffs {
            checks: before(self.checks),
            aggregates: before(self.aggregates),
            incidents: before(self.incidents),
            delivered_outbox: before(self.delivered_outbox),
        }
    }
}

/// Records older than these instants expire in a `retain` pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionCutoffs {
    pub checks: OffsetDateTime,
    pub aggregates: OffsetDateTime,
    pub incidents: OffsetDateTime,
    pub delivered_outbox: OffsetDateTime,
}

/// Raw rows a retention pass must keep: the birth streak (`first_fail_at` → `opened_at`)
/// of the monitor's open incident. Shared by the aggregate and the delete so the two can
/// never disagree about which rows moved.
const BIRTH_STREAK_OF_OPEN_INCIDENT: &str = "EXISTS (
    SELECT 1 FROM incidents i
     WHERE i.monitor_id = checks.monitor_id AND i.state = 'open'
       AND checks.slot_at >= COALESCE(i.first_fail_at, i.opened_at)
       AND checks.slot_at <= i.opened_at)";

/// Create a missing store file empty and owner-only (0600) before SQLite opens it: SQLite
/// would create it with the process umask (usually 0644), and its `-wal`/`-shm` take the
/// store's mode. An existing store is left as it is.
async fn create_owner_only(path: std::path::PathBuf) -> Result<(), DbError> {
    tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(QueryError::io(&error)),
        }
    })
    .await
    .map_err(|error| QueryError::Io(error.to_string()))?
    .map_err(DbError::from)
}

/// Make an existing store and its `-wal`/`-shm` owner-only (0600) when they are readable
/// or writable by anyone else — a store from before stores were created 0600, or copied in
/// by hand. Logs each file it tightens; a file that can't be changed is logged, not fatal
/// (the boot still reports what it serves from).
#[cfg(unix)]
async fn tighten_owner_only(path: std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::task::spawn_blocking(move || {
        let mut sidecars = [path.clone(), path.clone(), path];
        sidecars[1].as_mut_os_string().push("-wal");
        sidecars[2].as_mut_os_string().push("-shm");
        for file in &sidecars {
            let Ok(meta) = std::fs::metadata(file) else {
                continue; // not there: nothing to tighten
            };
            let mode = meta.permissions().mode() & 0o777;
            let group_or_other = mode & 0o077;
            if group_or_other == 0 {
                continue;
            }
            match std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => tracing::warn!(
                    file = %file.display(),
                    was = format!("{mode:o}"),
                    "store file was readable by others; made it owner-only (0600)"
                ),
                Err(error) => tracing::error!(
                    file = %file.display(),
                    %error,
                    "store file is readable by others and could not be made owner-only"
                ),
            }
        }
    })
    .await;
}

#[cfg(not(unix))]
async fn tighten_owner_only(_path: std::path::PathBuf) {}

/// A handle to the monitor's SQLite store.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
    /// The `SMON_MAX_DB_BYTES` ceiling every connection carries, if any — retention lifts
    /// it by `RETENTION_RESERVE_PAGES` for its own transaction and restores it after.
    max_db_bytes: Option<u64>,
    /// An in-memory store exists only inside this pool; no second handle can reach it.
    in_memory: bool,
}

impl Db {
    /// Opens (creating if needed) a SQLite database with WAL and foreign keys on.
    ///
    /// `max_connections` exists for tests: an in-memory database must stay on one connection or
    /// every pooled checkout sees a different empty database.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, DbError> {
        Self::connect_opts(url, max_connections, None).await
    }

    /// `connect` plus an optional database size ceiling: `max_db_bytes` is enforced as
    /// `PRAGMA max_page_count`, so writes past the ceiling fail `SQLITE_FULL` — a commit
    /// that cannot land is an error the caller sees, never silent corruption.
    ///
    /// `max_page_count` is a *per-connection* setting in SQLite, so it is applied in the
    /// pool's `after_connect` hook: every connection the pool ever opens — the first, the
    /// other `max_connections - 1`, and any it reopens after an idle timeout or lifetime
    /// expiry — carries the ceiling before it can serve a query. Setting it once through
    /// the pool would cap one connection and leave the rest unbounded.
    pub async fn connect_opts(
        url: &str,
        max_connections: u32,
        max_db_bytes: Option<u64>,
    ) -> Result<Self, DbError> {
        Self::open(url, max_connections, max_db_bytes, true).await
    }

    /// `connect_opts` for a normal start (D-015): the store must already exist and hold a
    /// schema. A missing file — typically a volume that isn't mounted, leaving the bare
    /// mountpoint — or a file with no tables is an error, never a silently created fresh
    /// store that would serve "no evidence" as if none had ever existed. In-memory URLs
    /// have no file and are opened as usual.
    pub async fn connect_existing(
        url: &str,
        max_connections: u32,
        max_db_bytes: Option<u64>,
    ) -> Result<Self, DbError> {
        let options = SqliteConnectOptions::from_str(url)?;
        let path = options.get_filename().to_path_buf();
        let in_memory = path.as_os_str() == ":memory:" || url.contains("mode=memory");
        if !in_memory && !path.exists() {
            return Err(DbError::NoStore { path });
        }
        let db = Self::open(url, max_connections, max_db_bytes, false).await?;
        if !in_memory {
            let tables: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'")
                    .fetch_one(&db.pool)
                    .await
                    .map_err(QueryError::from)?;
            if tables == 0 {
                db.close().await;
                return Err(DbError::EmptyStore { path });
            }
        }
        Ok(db)
    }

    async fn open(
        url: &str,
        max_connections: u32,
        max_db_bytes: Option<u64>,
        create: bool,
    ) -> Result<Self, DbError> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(create)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        // sqlx renames an in-memory database to a shared-cache `file:` name — decide from
        // the URL the caller wrote, never from that name.
        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
        if !in_memory {
            let path = options.get_filename().to_path_buf();
            if create {
                create_owner_only(path.clone()).await?;
            }
            // Before SQLite opens it: sidecars it creates later take the store's mode.
            tighten_owner_only(path).await;
        }
        let mut pool = SqlitePoolOptions::new().max_connections(max_connections);
        if let Some(bytes) = max_db_bytes {
            pool = pool.after_connect(move |conn, _meta| {
                Box::pin(async move { apply_max_bytes(conn, bytes).await })
            });
        }
        let pool = pool.connect_with(options).await?;
        Ok(Self {
            pool,
            max_db_bytes,
            in_memory: url.contains(":memory:") || url.contains("mode=memory"),
        })
    }

    /// A second handle on the same store with one connection of its own, for the single
    /// writer: its commits never queue behind API reads for one of the pooled connections
    /// (measured: 70-96 ms a commit with 32 concurrent readers on a shared 4-connection
    /// pool, 1-3 ms on its own). Same options, the same byte cap on its connection. An
    /// in-memory store exists only in this pool, so it is shared.
    pub async fn writer_connection(&self) -> Result<Self, DbError> {
        if self.in_memory {
            return Ok(self.clone());
        }
        let options = (*self.pool.connect_options()).clone();
        let mut pool = SqlitePoolOptions::new().max_connections(1);
        if let Some(bytes) = self.max_db_bytes {
            pool = pool.after_connect(move |conn, _meta| {
                Box::pin(async move { apply_max_bytes(conn, bytes).await })
            });
        }
        Ok(Self {
            pool: pool.connect_with(options).await?,
            max_db_bytes: self.max_db_bytes,
            in_memory: false,
        })
    }

    /// Bytes the database file currently occupies (`page_count * page_size`).
    pub async fn used_bytes(&self) -> Result<u64, QueryError> {
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.pool)
            .await?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(pages).unwrap_or(0) * u64::try_from(page_size).unwrap_or(4096))
    }

    /// Current size ceiling in bytes — for tests and diagnostics.
    pub async fn max_bytes(&self) -> Result<u64, QueryError> {
        let pages: i64 = sqlx::query_scalar("PRAGMA max_page_count")
            .fetch_one(&self.pool)
            .await?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(pages).unwrap_or(u64::MAX) * u64::try_from(page_size).unwrap_or(4096))
    }

    /// Close every pooled connection; later queries fail. Shutdown and tests.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn migrate(&self) -> Result<(), DbError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    /// Online backup: `VACUUM INTO` writes one consistent snapshot file — the WAL-aware
    /// way to copy a live database without quiescing it. This reads the store; it does
    /// not write it, so the single-writer invariant is untouched. Returns the backup's
    /// size in bytes — a zero-length "backup" is no backup at all.
    ///
    /// The target must not exist: it is created empty and owner-only (0600) first —
    /// `VACUUM INTO` fills an empty file and keeps its mode — so a backup is never readable
    /// by others, not even between creation and a chmod, and never overwrites or reuses a
    /// file that was already there ([`QueryError::Invalid`]).
    pub async fn backup(&self, path: &std::path::Path) -> Result<u64, QueryError> {
        let target = path.to_path_buf();
        let created = tokio::task::spawn_blocking(move || {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            options.open(&target).map(drop)
        })
        .await
        .map_err(|error| QueryError::Io(error.to_string()))?;
        match created {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(QueryError::Invalid(
                    "the backup target already exists; choose a new path".into(),
                ));
            }
            Err(error) => return Err(QueryError::io(&error)),
        }
        // VACUUM INTO takes no bound parameters; the path is quoted as an SQL string
        // literal with '' escaping — never interpolated raw (AssertSqlSafe audited).
        let escaped = path.to_string_lossy().replace('\'', "''");
        let vacuum = sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{escaped}'")))
            .execute(&self.pool)
            .await;
        if let Err(error) = vacuum {
            // Leave no empty "backup" behind to be mistaken for one.
            let _ = std::fs::remove_file(path);
            return Err(error.into());
        }
        let bytes = std::fs::metadata(path)
            .map_err(|error| QueryError::io(&error))?
            .len();
        Ok(bytes)
    }

    /// A cheap liveness check for `/readyz`.
    pub async fn ping(&self) -> Result<(), QueryError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn insert_monitor(&self, monitor: &Monitor) -> Result<(), QueryError> {
        if self.insert_monitor_within(monitor, u64::MAX).await? {
            Ok(())
        } else {
            Err(QueryError::OutOfRange("monitor count"))
        }
    }

    /// Insert unless `max_monitors` already exist — one conditional statement, so two
    /// concurrent creates can't both slip under the cap. `false` = the cap held.
    pub async fn insert_monitor_within(
        &self,
        monitor: &Monitor,
        max_monitors: u64,
    ) -> Result<bool, QueryError> {
        let inserted = sqlx::query(
            "INSERT INTO monitors (id, name, url, interval_s, timeout_s, fail_threshold,
                                  recover_threshold, generation, paused, created_at)
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, 0, ?
             WHERE (SELECT COUNT(*) FROM monitors) < ?",
        )
        .bind(monitor.id.0.to_string())
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
        .bind(i64::from(monitor.thresholds.failures_to_open))
        .bind(i64::from(monitor.thresholds.passes_to_recover))
        .bind(i64::try_from(monitor.generation).map_err(|_| QueryError::OutOfRange("generation"))?)
        .bind(OffsetDateTime::now_utc())
        .bind(i64::try_from(max_monitors).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        Ok(inserted.rows_affected() == 1)
    }

    pub async fn list_monitors(&self) -> Result<Vec<MonitorRow>, QueryError> {
        let rows = sqlx::query_as::<_, MonitorRow>(
            "SELECT id, name, url, interval_s, timeout_s, fail_threshold, recover_threshold,
                    generation, paused, created_at
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
            "SELECT id, monitor_id, opened_at, first_fail_at, last_fail_at, resolved_at, state
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
                 fail_threshold = ?, recover_threshold = ?,
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
        .bind(i64::from(monitor.thresholds.failures_to_open))
        .bind(i64::from(monitor.thresholds.passes_to_recover))
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
            "SELECT id, monitor_id, opened_at, first_fail_at, last_fail_at, resolved_at, state FROM incidents
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

    /// What the writer needs to admit one observation: the committed generation (a claimed
    /// older generation is stale), the committed thresholds (the incident machine runs on
    /// current config, so an edit takes effect on the same observation that confirms its
    /// generation bump), and whether a maintenance window is active *now* (created or
    /// deleted windows take effect on the next slot). One row read, never cached — the
    /// gate is always fresh.
    pub async fn monitor_gate(&self, monitor: MonitorId) -> Result<Option<Gate>, QueryError> {
        let row: Option<(i64, i64, i64, bool)> = sqlx::query_as(
            "SELECT generation, fail_threshold, recover_threshold,
                    EXISTS(SELECT 1 FROM maintenance_windows w
                           WHERE w.monitor_id = monitors.id
                             AND w.starts_at <= ? AND w.ends_at > ?)
             FROM monitors WHERE id = ?",
        )
        .bind(OffsetDateTime::now_utc())
        .bind(OffsetDateTime::now_utc())
        .bind(monitor.0.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(generation, fail, recover, maintenance)| {
            let generation = u64::try_from(generation)
                .map_err(|_| QueryError::Corrupt("negative generation".into()))?;
            let thresholds = Thresholds::new(
                u8::try_from(fail).map_err(|_| QueryError::Corrupt("fail_threshold".into()))?,
                u8::try_from(recover)
                    .map_err(|_| QueryError::Corrupt("recover_threshold".into()))?,
            )
            .map_err(|_| QueryError::Corrupt("thresholds out of range".into()))?;
            Ok(Gate {
                generation,
                thresholds,
                maintenance,
            })
        })
        .transpose()
    }

    /// The monitor's current generation — the writer compares it against the generation an
    /// observation was claimed under.
    pub async fn monitor_generation(&self, monitor: MonitorId) -> Result<Option<u64>, QueryError> {
        Ok(self
            .monitor_gate(monitor)
            .await?
            .map(|gate| gate.generation))
    }

    /// Register a dashboard session (M7.2). The cookie carries only the id — the store
    /// decides validity, so a session survives a restart and dies on its own `expires_at`.
    pub async fn create_session(
        &self,
        id: &str,
        expires_at: OffsetDateTime,
    ) -> Result<(), QueryError> {
        self.create_session_at(id, OffsetDateTime::now_utc(), expires_at)
            .await
    }

    /// [`Db::create_session`] created at an explicit `now` (the server's injected clock).
    pub async fn create_session_at(
        &self,
        id: &str,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<(), QueryError> {
        sqlx::query("INSERT INTO sessions (id, created_at, expires_at) VALUES (?, ?, ?)")
            .bind(id)
            .bind(now)
            .bind(expires_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A session authorizes only while `expires_at` is in the future and it was last used
    /// less than `idle` ago (D-016). A row past either limit is deleted on sight — the
    /// sweep in retention is the sweeper, this is the guard.
    pub async fn session_valid(
        &self,
        key: &str,
        idle: std::time::Duration,
    ) -> Result<bool, QueryError> {
        self.session_valid_at(key, OffsetDateTime::now_utc(), idle)
            .await
    }

    /// [`Db::session_valid`] at an explicit `now` (tests drive the clock). A valid lookup
    /// refreshes `last_seen_at` once `min(idle / 4, 60 s)` has passed since the last
    /// refresh, so a dashboard polling every 15 s is not a write per request. A refresh
    /// that fails (a store at its byte cap) is logged and the session still authorizes:
    /// the full-disk moment is when the operator most needs the dashboard.
    pub async fn session_valid_at(
        &self,
        key: &str,
        now: OffsetDateTime,
        idle: std::time::Duration,
    ) -> Result<bool, QueryError> {
        let row: Option<(OffsetDateTime, OffsetDateTime, Option<OffsetDateTime>)> = sqlx::query_as(
            "SELECT expires_at, created_at, last_seen_at FROM sessions WHERE id = ?",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        let Some((expires, created, last_seen)) = row else {
            return Ok(false);
        };
        let idle = time::Duration::try_from(idle).unwrap_or(time::Duration::MAX);
        let unused = now - last_seen.unwrap_or(created);
        if expires > now && unused < idle {
            let refresh_every: time::Duration = (idle / 4_i32).min(time::Duration::minutes(1));
            if unused >= refresh_every {
                let touched = sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(key)
                    .execute(&self.pool)
                    .await;
                if let Err(error) = touched {
                    tracing::warn!(%error, "session last_seen refresh failed");
                }
            }
            return Ok(true);
        }
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(false)
    }

    /// Logout deletes the row — an idempotent no-op on an already-gone session.
    pub async fn delete_session(&self, id: &str) -> Result<(), QueryError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete every session past its expiry — the retention tick's housekeeping so
    /// forgotten rows can't accumulate forever.
    pub async fn sweep_expired_sessions(&self) -> Result<u64, QueryError> {
        let done = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(OffsetDateTime::now_utc())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// Register a maintenance window. `starts_at < ends_at` is enforced by the table's
    /// CHECK; a wholly-past window is accepted — it is a record explaining evidence, not
    /// dead config (the same reason resolved incidents are kept).
    pub async fn create_window(
        &self,
        monitor: MonitorId,
        starts_at: OffsetDateTime,
        ends_at: OffsetDateTime,
        reason: &str,
    ) -> Result<MaintenanceRow, QueryError> {
        let row = sqlx::query_as::<_, MaintenanceRow>(
            "INSERT INTO maintenance_windows (monitor_id, starts_at, ends_at, reason, created_at)
             VALUES (?, ?, ?, ?, ?)
             RETURNING id, monitor_id, starts_at, ends_at, reason, created_at",
        )
        .bind(monitor.0.to_string())
        .bind(starts_at)
        .bind(ends_at)
        .bind(reason)
        .bind(OffsetDateTime::now_utc())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(db) = &e {
                if db.is_check_violation() {
                    return QueryError::Invalid("window ends_at must be after starts_at".into());
                }
                if db.is_foreign_key_violation() {
                    return QueryError::Invalid("unknown monitor".into());
                }
            }
            QueryError::Sql(e)
        })?;
        Ok(row)
    }

    /// All windows for a monitor, soonest-ending first — upcoming suppression is the
    /// operationally interesting sort, history is kept after it.
    pub async fn list_windows(
        &self,
        monitor: MonitorId,
    ) -> Result<Vec<MaintenanceRow>, QueryError> {
        sqlx::query_as::<_, MaintenanceRow>(
            "SELECT id, monitor_id, starts_at, ends_at, reason, created_at
             FROM maintenance_windows WHERE monitor_id = ?
             ORDER BY ends_at ASC, id ASC",
        )
        .bind(monitor.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Cancel a window. Deleting an *active* window resumes normal alerting on the next
    /// slot; deleting a future or past one just removes the record. Returns whether a row
    /// was deleted so the API can distinguish 204 from 404.
    pub async fn delete_window(&self, window_id: i64) -> Result<bool, QueryError> {
        let deleted = sqlx::query("DELETE FROM maintenance_windows WHERE id = ?")
            .bind(window_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(deleted > 0)
    }

    /// The newest stored slot for a monitor (any outcome) — the boot's downtime check.
    pub async fn last_slot_at(
        &self,
        monitor: MonitorId,
    ) -> Result<Option<OffsetDateTime>, QueryError> {
        let last: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT MAX(slot_at) FROM checks WHERE monitor_id = ?")
                .bind(monitor.0.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(last)
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

    /// Retention pass (D-012), one transaction so a crash can't produce history that is
    /// neither raw nor aggregated, or an outbox row whose incident is gone:
    ///
    /// 1. raw checks older than `cutoffs.checks` roll into `check_stats_daily` and are
    ///    deleted — except an open incident's birth streak, its evidence while it is open;
    /// 2. daily aggregates older than `cutoffs.aggregates` are deleted (and expiring raw
    ///    rows from such days are not re-aggregated);
    /// 3. resolved incidents older than `cutoffs.incidents` are deleted with every outbox
    ///    row they own; ended maintenance windows older than it go too;
    /// 4. delivered outbox rows older than `cutoffs.delivered_outbox` are deleted.
    ///
    /// **Free before write.** At `SMON_MAX_DB_BYTES` the main file has no free page, and a
    /// pass that wrote its aggregates first failed `SQLITE_FULL` and freed nothing — a full
    /// store stayed full. So the expiring rows are first copied into a `TEMP` table (temp
    /// storage, outside the main file's page cap), every delete runs, and only then are the
    /// aggregates upserted, into the pages the deletes freed.
    ///
    /// **Bounded.** One call handles at most `batch` rows of each kind, so a first pass
    /// over months of history never holds the single writer (and the coordinator queued
    /// behind it) for seconds: the caller repeats until `RetainReport::exhausted(batch)`,
    /// one writer command per batch, and observations interleave between them.
    ///
    /// Open incidents and undelivered rows of kept incidents are never touched. Called
    /// only by the single writer.
    pub async fn retain(
        &self,
        cutoffs: &RetentionCutoffs,
        batch: u32,
    ) -> Result<RetainReport, QueryError> {
        let batch = i64::from(batch.max(1));
        let mut conn = self.pool.acquire().await?;
        let Some(cap) = self.max_db_bytes else {
            return retain_batch(&mut conn, cutoffs, batch).await;
        };
        // Lift the ceiling on this one connection for this one transaction, then put it
        // back whatever happened: SQLite clamps the restored ceiling up to the file's size,
        // so pages the summaries used stay usable and nothing more is granted. A connection
        // whose ceiling can't be restored is closed, never returned to the pool lifted.
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&mut *conn)
            .await?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&mut *conn)
            .await?;
        let cap_pages = i64::try_from(cap / u64::try_from(page_size.max(1)).unwrap_or(4096))
            .unwrap_or(i64::MAX);
        let lifted = reserve_ceiling(cap_pages, pages);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "PRAGMA max_page_count = {lifted}"
        )))
        .execute(&mut *conn)
        .await?;
        let result = retain_batch(&mut conn, cutoffs, batch).await;
        if let Err(error) = apply_max_bytes(&mut conn, cap).await {
            tracing::error!(%error, "could not restore the size ceiling; closing the connection");
            let _ = conn.close().await;
        }
        result
    }

    /// How many monitors exist (paused ones included — they keep history).
    pub async fn monitor_count(&self) -> Result<u64, QueryError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM monitors")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(count).unwrap_or(0))
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
            "SELECT id, event_id, monitor_id, incident_id, kind, channel, attempts, next_attempt_at,
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
        channel: &str,
        max_attempts: i64,
        limit: i64,
    ) -> Result<Vec<DeliveryRow>, QueryError> {
        let rows = sqlx::query_as::<_, DeliveryRow>(
            "SELECT o.event_id, o.kind, o.attempts, o.created_at, o.channel,
                    m.id AS monitor_id, m.name AS monitor_name, m.url AS monitor_url,
                    i.id AS incident_id, i.opened_at, i.first_fail_at, i.last_fail_at,
                    i.resolved_at
             FROM notify_outbox o
             JOIN monitors m ON m.id = o.monitor_id
             JOIN incidents i ON i.id = o.incident_id
             WHERE o.channel = ?
               AND o.delivered_at IS NULL AND o.attempts < ?
               AND (o.next_attempt_at IS NULL OR o.next_attempt_at <= ?)
             ORDER BY o.id LIMIT ?",
        )
        .bind(channel)
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
    /// in a single transaction — all three or none. `open_after` is the machine's post-record
    /// incident flag: a failing probe committed under an open incident also stamps
    /// `last_fail_at`, so the four incident times stay distinct (`first_fail_at` = streak
    /// start, `opened_at` = when the monitor noticed, `last_fail_at` = latest evidence,
    /// `resolved_at` = confirmed recovery).
    pub(crate) async fn commit_observation(
        &self,
        observation: &Observation,
        effect: Effect,
        open_after: bool,
        queue_email: bool,
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
            Effect::Unchanged | Effect::IncidentSuppressed => {
                // Suppressed: the check row above is the whole commit — no incident row,
                // no outbox event. The window itself is the explanation for the withheld
                // page; the machine's streak still counted it.
            }
            Effect::IncidentOpened => {
                // first_fail_at = the streak's start: the oldest consecutive failing
                // slot since the last pass/missed — derived from committed rows, so it
                // is right no matter when the machine was last in memory.
                let first_fail_at: Option<OffsetDateTime> = sqlx::query_scalar(
                    "SELECT MIN(slot_at) FROM checks
                     WHERE monitor_id = ?1
                       AND outcome IN ('http_fail', 'transport', 'timeout')
                       AND slot_at > COALESCE(
                             (SELECT MAX(slot_at) FROM checks
                              WHERE monitor_id = ?1
                                AND outcome IN ('pass', 'missed')),
                             '')",
                )
                .bind(observation.monitor_id.0.to_string())
                .fetch_one(&mut *tx)
                .await?;
                let incident_id: i64 = sqlx::query_scalar(
                    "INSERT INTO incidents (monitor_id, opened_at, first_fail_at, last_fail_at, state)
                     VALUES (?, ?, ?, ?, 'open') RETURNING id",
                )
                .bind(observation.monitor_id.0.to_string())
                .bind(observation.slot_at)
                .bind(first_fail_at.unwrap_or(observation.slot_at))
                .bind(observation.slot_at)
                .fetch_one(&mut *tx)
                .await?;
                enqueue_outbox(
                    &mut tx,
                    observation.monitor_id,
                    incident_id,
                    "incident_opened",
                    queue_email,
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
                    queue_email,
                )
                .await?;
            }
        }

        // A failing probe under an open incident is fresh evidence about *how long* the
        // target has been failing — stamp it on the same commit (no-op when closed).
        if open_after && matches!(observation.slot, SlotObservation::Probe(o) if !o.is_pass()) {
            sqlx::query(
                "UPDATE incidents SET last_fail_at = ?
                 WHERE monitor_id = ? AND state = 'open'",
            )
            .bind(observation.slot_at)
            .bind(observation.monitor_id.0.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// The ceiling (in pages) a retention batch runs under: the configured cap plus the
/// reserve — an absolute allowance, so passes that each use some of it can't ratchet the
/// effective cap upward — and never below the file's current size (SQLite would clamp up
/// anyway) nor, below the cap, under the cap.
fn reserve_ceiling(cap_pages: i64, page_count: i64) -> i64 {
    cap_pages
        .saturating_add(i64::try_from(RETENTION_RESERVE_PAGES).unwrap_or(0))
        .max(page_count)
}

/// One bounded retention batch, in one transaction on `conn` (see `Db::retain`).
async fn retain_batch(
    conn: &mut sqlx::SqliteConnection,
    cutoffs: &RetentionCutoffs,
    batch: i64,
) -> Result<RetainReport, QueryError> {
    use sqlx::Connection as _;
    let mut tx = conn.begin().await?;
    sqlx::query("DROP TABLE IF EXISTS temp.expiring")
        .execute(&mut *tx)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TEMP TABLE expiring AS
         SELECT id, monitor_id, date(slot_at) AS day, outcome FROM checks
         WHERE slot_at < ?1 AND NOT {BIRTH_STREAK_OF_OPEN_INCIDENT}
         ORDER BY id LIMIT ?2"
    )))
    .bind(cutoffs.checks)
    .bind(batch)
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query("DELETE FROM checks WHERE id IN (SELECT id FROM temp.expiring)")
        .execute(&mut *tx)
        .await?;
    let aggregate_day = cutoffs.aggregates.date().to_string();
    let pruned = prune_expired(&mut tx, cutoffs, &aggregate_day, batch).await?;
    // Space is free now; write the aggregates of what was deleted.
    let aggregated = sqlx::query(
        "INSERT INTO check_stats_daily
            (monitor_id, day, pass, http_fail, transport, timeout, missed)
         SELECT monitor_id, day,
                SUM(outcome = 'pass'), SUM(outcome = 'http_fail'),
                SUM(outcome = 'transport'), SUM(outcome = 'timeout'),
                SUM(outcome = 'missed')
         FROM temp.expiring
         WHERE day >= ?1
         GROUP BY monitor_id, day
         ON CONFLICT (monitor_id, day) DO UPDATE SET
            pass = check_stats_daily.pass + excluded.pass,
            http_fail = check_stats_daily.http_fail + excluded.http_fail,
            transport = check_stats_daily.transport + excluded.transport,
            timeout = check_stats_daily.timeout + excluded.timeout,
            missed = check_stats_daily.missed + excluded.missed",
    )
    .bind(&aggregate_day)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DROP TABLE temp.expiring")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(RetainReport {
        days_written: aggregated.rows_affected(),
        rows_deleted: deleted.rows_affected(),
        ..pruned
    })
}

/// Retention's deletes beyond raw checks — old aggregates, resolved incidents with their
/// outbox rows, ended windows, delivered outbox rows — inside the pass's transaction.
/// Returns a report with only those counts set.
async fn prune_expired(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    cutoffs: &RetentionCutoffs,
    aggregate_day: &str,
    batch: i64,
) -> Result<RetainReport, QueryError> {
    let old_days = sqlx::query(
        "DELETE FROM check_stats_daily WHERE rowid IN (
           SELECT rowid FROM check_stats_daily WHERE day < ?1 LIMIT ?2)",
    )
    .bind(aggregate_day)
    .bind(batch)
    .execute(&mut **tx)
    .await?;
    // One batch of expired incidents; their outbox rows (at most a few each) go first (FK).
    sqlx::query("DROP TABLE IF EXISTS temp.expired_incidents")
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "CREATE TEMP TABLE expired_incidents AS
         SELECT id FROM incidents WHERE state = 'resolved' AND resolved_at < ?1
         ORDER BY id LIMIT ?2",
    )
    .bind(cutoffs.incidents)
    .bind(batch)
    .execute(&mut **tx)
    .await?;
    let incident_outbox = sqlx::query(
        "DELETE FROM notify_outbox WHERE incident_id IN (SELECT id FROM temp.expired_incidents)",
    )
    .execute(&mut **tx)
    .await?;
    let incidents =
        sqlx::query("DELETE FROM incidents WHERE id IN (SELECT id FROM temp.expired_incidents)")
            .execute(&mut **tx)
            .await?;
    sqlx::query("DROP TABLE temp.expired_incidents")
        .execute(&mut **tx)
        .await?;
    let windows = sqlx::query(
        "DELETE FROM maintenance_windows WHERE id IN (
           SELECT id FROM maintenance_windows WHERE ends_at < ?1 LIMIT ?2)",
    )
    .bind(cutoffs.incidents)
    .bind(batch)
    .execute(&mut **tx)
    .await?;
    let delivered = sqlx::query(
        "DELETE FROM notify_outbox WHERE id IN (
           SELECT id FROM notify_outbox
           WHERE delivered_at IS NOT NULL AND delivered_at < ?1 LIMIT ?2)",
    )
    .bind(cutoffs.delivered_outbox)
    .bind(batch)
    .execute(&mut **tx)
    .await?;
    Ok(RetainReport {
        days_written: 0,
        rows_deleted: 0,
        aggregates_deleted: old_days.rows_affected(),
        incidents_deleted: incidents.rows_affected(),
        outbox_deleted: incident_outbox.rows_affected() + delivered.rows_affected(),
        delivered_outbox_deleted: delivered.rows_affected(),
        windows_deleted: windows.rows_affected(),
    })
}

/// `PRAGMA max_page_count` from a byte ceiling on one connection — computed against the
/// database's own page size, never an assumed 4 KiB. SQLite clamps the value up to the
/// current file size, so a store already past the ceiling simply cannot grow further.
async fn apply_max_bytes(conn: &mut sqlx::SqliteConnection, bytes: u64) -> Result<(), sqlx::Error> {
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut *conn)
        .await?;
    let page_size = u64::try_from(page_size).unwrap_or(4096).max(1);
    let pages = i64::try_from((bytes / page_size).max(1)).unwrap_or(i64::MAX);
    // PRAGMAs reject bound parameters, so the statement is built dynamically —
    // `pages` is a computed integer, never input (AssertSqlSafe audited).
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "PRAGMA max_page_count = {pages}"
    )))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn enqueue_outbox(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    monitor: MonitorId,
    incident_id: i64,
    kind: &'static str,
    queue_email: bool,
) -> Result<(), QueryError> {
    // One row per delivery channel: webhook is always planned (pending rows are the
    // visible "no receiver" signal); email rows only exist when SMTP is configured —
    // an unconfigured channel is no plan, not a pending one.
    let channels: &[&str] = if queue_email {
        &["webhook", "email"]
    } else {
        &["webhook"]
    };
    for channel in channels {
        sqlx::query(
            "INSERT INTO notify_outbox (event_id, monitor_id, incident_id, kind, channel, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(monitor.0.to_string())
        .bind(incident_id)
        .bind(kind)
        .bind(*channel)
        .bind(OffsetDateTime::now_utc())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Map a slot onto the `checks.outcome` text + status column. `pub` for the guide drift
/// test — the wire vocabulary is this function, so the guide's word list is checked
/// against what the store actually writes.
pub fn encode_slot(slot: SlotObservation, http_status: Option<u16>) -> (&'static str, Option<i64>) {
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

/// The admission gate for one observation — generation, committed thresholds, and whether
/// a maintenance window is active at commit time (suppression is a committed-read fact,
/// never a cached flag).
pub struct Gate {
    pub generation: u64,
    pub thresholds: monitor_domain::Thresholds,
    pub maintenance: bool,
}

/// One `maintenance_windows` row — schedule-scoped suppression. `active` is computed at
/// read time so the API never has to re-derive it.
#[derive(Debug, FromRow)]
pub struct MaintenanceRow {
    pub id: i64,
    pub monitor_id: String,
    pub starts_at: OffsetDateTime,
    pub ends_at: OffsetDateTime,
    pub reason: String,
    pub created_at: OffsetDateTime,
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
    pub fail_threshold: i64,
    pub recover_threshold: i64,
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
        let thresholds = Thresholds::new(
            u8::try_from(row.fail_threshold)
                .map_err(|_| QueryError::Corrupt("fail_threshold".into()))?,
            u8::try_from(row.recover_threshold)
                .map_err(|_| QueryError::Corrupt("recover_threshold".into()))?,
        )
        .map_err(|_| QueryError::Corrupt("thresholds out of range".into()))?;
        Ok(Self {
            id: MonitorId(id),
            name: row.name,
            url,
            interval: std::time::Duration::from_secs(interval),
            timeout: std::time::Duration::from_secs(timeout),
            thresholds,
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
    /// The streak's first failing slot — when the target *started* failing (the monitor
    /// noticed at `opened_at`). `NULL` on rows opened before M6.3.
    pub first_fail_at: Option<OffsetDateTime>,
    /// The latest failing slot while the incident was open — how long the failure lasted.
    pub last_fail_at: Option<OffsetDateTime>,
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
    pub channel: String,
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
    pub channel: String,
    pub attempts: i64,
    pub created_at: OffsetDateTime,
    pub monitor_id: String,
    pub monitor_name: String,
    pub monitor_url: String,
    pub incident_id: i64,
    pub opened_at: OffsetDateTime,
    pub first_fail_at: Option<OffsetDateTime>,
    pub last_fail_at: Option<OffsetDateTime>,
    pub resolved_at: Option<OffsetDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reserve is an absolute allowance above the configured cap, not a fresh 256
    /// pages on top of whatever size earlier passes reached: repeated passes that each need
    /// new pages must not ratchet the effective cap upward. Below the cap the batch's
    /// ceiling is never *lowered* under the cap.
    #[test]
    fn the_retention_reserve_is_bounded_by_the_cap_not_the_current_size() {
        let reserve = i64::try_from(RETENTION_RESERVE_PAGES).expect("small");
        let cap = 1_000;
        assert_eq!(
            reserve_ceiling(cap, 500),
            cap + reserve,
            "below the cap: never lowered"
        );
        assert_eq!(
            reserve_ceiling(cap, cap),
            cap + reserve,
            "at the cap: the reserve"
        );
        assert_eq!(
            reserve_ceiling(cap, cap + 100),
            cap + reserve,
            "part of the reserve used: only the rest is left — no ratchet"
        );
        assert_eq!(
            reserve_ceiling(cap, cap + reserve + 7),
            cap + reserve + 7,
            "never below the file's own size"
        );
    }

    const POOL: u32 = 4;

    /// Check out every pooled connection at once — the pool must open all of them, so
    /// nothing below can pass by reusing the one connection that happened to be configured.
    async fn hold_all(db: &Db) -> Vec<sqlx::pool::PoolConnection<sqlx::Sqlite>> {
        let mut held = Vec::new();
        for _ in 0..POOL {
            held.push(db.pool.acquire().await.expect("acquire"));
        }
        held
    }

    /// `PRAGMA max_page_count` is per connection in SQLite. A ceiling set once through
    /// the pool lands on one connection; the others — and every connection the pool opens
    /// later — would grow the file without limit.
    #[tokio::test]
    async fn the_size_ceiling_is_set_on_every_pooled_connection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}", tmp.path().join("cap.db").display());
        let cap: u64 = 64 * 1024 * 1024;
        let db = Db::connect_opts(&url, POOL, Some(cap))
            .await
            .expect("connect");
        db.migrate().await.expect("migrate");

        for mut conn in hold_all(&db).await {
            let pages: i64 = sqlx::query_scalar("PRAGMA max_page_count")
                .fetch_one(&mut *conn)
                .await
                .expect("max_page_count");
            let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
                .fetch_one(&mut *conn)
                .await
                .expect("page_size");
            assert_eq!(
                u64::try_from(pages * page_size).expect("positive"),
                cap,
                "every connection must carry the ceiling"
            );
        }
    }

    /// The behavior the ceiling exists for: once the file is at its cap, a large write
    /// fails `SQLITE_FULL` on *whichever* connection the pool hands out.
    #[tokio::test]
    async fn a_capped_store_refuses_growth_on_every_connection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}", tmp.path().join("full.db").display());
        let setup = Db::connect(&url, 1).await.expect("connect");
        setup.migrate().await.expect("migrate");
        let used = setup.used_bytes().await.expect("used");
        setup.pool.close().await;

        // Reopen capped at exactly the current size: the file may not grow.
        let db = Db::connect_opts(&url, POOL, Some(used))
            .await
            .expect("reconnect");
        for (n, mut conn) in hold_all(&db).await.into_iter().enumerate() {
            let grew = sqlx::query(
                "INSERT INTO sessions (id, created_at, expires_at)
                 VALUES (hex(randomblob(500000)), 'x', 'x')",
            )
            .execute(&mut *conn)
            .await;
            let Err(sqlx::Error::Database(error)) = grew else {
                panic!("connection {n} grew a capped database: {grew:?}");
            };
            assert!(
                error.message().contains("full"),
                "connection {n}: expected SQLITE_FULL, got {}",
                error.message()
            );
        }
    }

    /// The writer's own connection (D-018) after a store error and after the connection
    /// itself is lost (an I/O failure, or sqlx's idle and lifetime limits): the pool opens
    /// a replacement, and the replacement is under the same bounds — still one connection,
    /// still the `SMON_MAX_DB_BYTES` ceiling.
    #[tokio::test]
    async fn the_writers_connection_recovers_and_keeps_its_bounds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}", tmp.path().join("w.db").display());
        let setup = Db::connect(&url, 1).await.expect("connect");
        setup.migrate().await.expect("migrate");
        let used = setup.used_bytes().await.expect("used");
        setup.pool.close().await;
        let api = Db::connect_opts(&url, POOL, Some(used))
            .await
            .expect("api pool");
        let writer = api.writer_connection().await.expect("writer connection");
        let oversized = "INSERT INTO sessions (id, created_at, expires_at)
                    VALUES (hex(randomblob(500000)), 'x', 'x')";
        let small = "INSERT INTO sessions (id, created_at, expires_at) VALUES ('s', 'x', 'x')";

        // A store error (the ceiling: SQLITE_FULL) is an error, not a dead connection.
        assert!(sqlx::query(oversized).execute(&writer.pool).await.is_err());
        sqlx::query(small)
            .execute(&writer.pool)
            .await
            .expect("writes after an error");

        // Lose the connection outright; the next use opens a replacement.
        let conn = writer.pool.acquire().await.expect("acquire");
        conn.close().await.expect("close");
        sqlx::query("DELETE FROM sessions")
            .execute(&writer.pool)
            .await
            .expect("writes after the connection is lost");
        assert!(
            writer.pool.size() <= 1,
            "still one connection: {}",
            writer.pool.size()
        );
        let refused = sqlx::query(oversized).execute(&writer.pool).await;
        let Err(sqlx::Error::Database(error)) = refused else {
            panic!("the replacement connection grew a capped store: {refused:?}");
        };
        assert!(error.message().contains("full"), "{}", error.message());
    }
}
