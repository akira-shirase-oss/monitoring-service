//! The public status snapshot — a sanitized, self-expiring artifact.
//!
//! The public status page must render while this process is dead, so the truth lives in a
//! file: every `interval` the writer rewrites `status.json` under `dir`, atomically. Its
//! `generated_at`/`expires_at` are the heartbeat — a snapshot older than its expiry means
//! "monitoring unavailable", never "all targets down" (module 07).
//!
//! Sanitization is an allowlist, not a denylist: the only fields that can leave are the
//! stable `alias` (id prefix — no names, URLs, or internal ids), `status`, and
//! `incident_open_since`. Anything not listed here does not exist for the public page.
//!
//! The heartbeat is only honest if it stops when observing stops. A live process whose
//! writer or coordinator has died would otherwise keep re-stamping its last statuses as
//! fresh. While the engine verdict is not observing (`vitals`), each beat is written
//! *born expired* (`expires_at = generated_at`), every service `unknown`, and
//! `monitoring: "unavailable"` — readers that only check expiry say "monitoring
//! unavailable" at once, without waiting out the TTL on a stale "up".

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use monitor_db::{Db, SharedStatuses};
use monitor_domain::{Monitor, MonitorId, Status};
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::vitals::Vitals;

pub struct Config {
    /// Directory the snapshot is written into (created on demand).
    pub dir: PathBuf,
    /// Write cadence — this is the heartbeat period.
    pub interval: Duration,
    /// How fresh a snapshot must be to mean "the monitor was alive when it wrote".
    pub ttl: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("status"),
            interval: Duration::from_secs(30),
            ttl: Duration::from_secs(90),
        }
    }
}

/// Stable public identity. UUIDv7-style ids created in the same millisecond share their
/// timestamp prefix, so an id-prefix alias collides for monitors registered together —
/// hash the id instead: deterministic, opaque, and uniformly distributed.
fn alias_for(id: &MonitorId) -> String {
    let digest = Sha256::digest(id.0.to_string().as_bytes());
    hex12(&digest)
}

fn hex12(digest: &[u8]) -> String {
    let mut out = String::with_capacity(12);
    for byte in &digest[..6] {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}"));
    }
    out
}

#[derive(Debug, Serialize)]
struct ServiceView {
    /// Hashed public identity — correlates across snapshots, reveals nothing internal.
    alias: String,
    status: Status,
    /// When the currently-open incident began, if the store says one is open — evidence
    /// time, not "now".
    #[serde(with = "time::serde::rfc3339::option")]
    incident_open_since: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
struct Snapshot {
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    /// `available` | `unavailable` — whether the engine was observing when it wrote this.
    monitoring: &'static str,
    services: Vec<ServiceView>,
}

/// Build the snapshot from committed state. Statuses come from the post-commit map; open
/// incidents come from the store. A monitor with no committed evidence is `unknown` —
/// and a paused monitor reports `unknown` here too, because the public question is
/// "is the service up", not "is the monitor running". `observing = false` (the engine
/// verdict) writes the born-expired, all-`unknown` form described in the module docs.
pub fn build<S: std::hash::BuildHasher>(
    monitors: &[Monitor],
    statuses: &SharedStatuses,
    open: &HashMap<MonitorId, OffsetDateTime, S>,
    ttl: Duration,
    observing: bool,
) -> serde_json::Value {
    let now = OffsetDateTime::now_utc();
    let committed = if observing {
        statuses.read().map(|map| map.clone()).unwrap_or_default()
    } else {
        HashMap::new()
    };
    let services = monitors
        .iter()
        .map(|monitor| ServiceView {
            alias: alias_for(&monitor.id),
            status: committed
                .get(&monitor.id)
                .copied()
                .unwrap_or(Status::Unknown),
            incident_open_since: open.get(&monitor.id).copied(),
        })
        .collect();
    let expires_at = if observing {
        now + time::Duration::try_from(ttl).unwrap_or(time::Duration::MAX)
    } else {
        now
    };
    serde_json::to_value(Snapshot {
        generated_at: now,
        expires_at,
        monitoring: if observing {
            "available"
        } else {
            "unavailable"
        },
        services,
    })
    .unwrap_or_else(|_| serde_json::json!({"services": []}))
}

/// Why one beat was not written.
#[derive(Debug, thiserror::Error)]
pub enum BeatError {
    #[error("reading the store: {0}")]
    Store(#[from] monitor_db::QueryError),
    #[error("serializing the snapshot: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("writing the snapshot file: {0}")]
    Io(#[from] std::io::Error),
}

/// A service on a workspace's public page (S2, D-021): the allowlist is the monitor's
/// *name* — the owner chose to publish it — plus the committed status, the open
/// incident's evidence time, and the coverage/success pair the detail page reports.
/// No ids, no URLs.
#[derive(Debug, Serialize)]
struct PageServiceView {
    name: String,
    status: Status,
    #[serde(with = "time::serde::rfc3339::option")]
    incident_open_since: Option<OffsetDateTime>,
    /// `null` when the window owes no slots or nothing was observed — "can't say",
    /// never a fabricated 0%.
    coverage_bps: Option<u32>,
    success_bps: Option<u32>,
}

#[derive(Debug, Serialize)]
struct PageSnapshot {
    title: String,
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    monitoring: &'static str,
    services: Vec<PageServiceView>,
}

/// The per-workspace page beat (S2): same heartbeat, same born-expired rule, scoped to
/// the owner's allowlist. A monitor nobody published simply does not exist here.
fn build_page(
    page: &monitor_db::StatusPage,
    statuses: &SharedStatuses,
    open: &HashMap<MonitorId, OffsetDateTime>,
    coverage: &HashMap<MonitorId, (Option<u32>, Option<u32>)>,
    ttl: Duration,
    observing: bool,
) -> serde_json::Value {
    let now = OffsetDateTime::now_utc();
    let committed = if observing {
        statuses.read().map(|map| map.clone()).unwrap_or_default()
    } else {
        HashMap::new()
    };
    let services = page
        .monitors
        .iter()
        .filter_map(|row| {
            let id = MonitorId(uuid::Uuid::parse_str(&row.monitor_id).ok()?);
            let (coverage_bps, success_bps) = coverage.get(&id).copied().unwrap_or((None, None));
            Some(PageServiceView {
                name: row.name.clone(),
                status: committed.get(&id).copied().unwrap_or(Status::Unknown),
                incident_open_since: open.get(&id).copied(),
                coverage_bps,
                success_bps,
            })
        })
        .collect();
    let expires_at = if observing {
        now + time::Duration::try_from(ttl).unwrap_or(time::Duration::MAX)
    } else {
        now
    };
    serde_json::to_value(PageSnapshot {
        title: page.title.clone(),
        generated_at: now,
        expires_at,
        monitoring: if observing {
            "available"
        } else {
            "unavailable"
        },
        services,
    })
    .unwrap_or_else(|_| serde_json::json!({"services": []}))
}

/// Write `bytes` to `dir/name` atomically and world-readable — the shared discipline
/// behind `status.json` and every `s/<slug>.json` page.
fn write_snapshot(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Result<(), BeatError> {
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    std::fs::set_permissions(
        &tmp,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
    )?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

/// Coverage/success window the public pages report — the dashboard's own default.
const PAGE_STATS_WINDOW: time::Duration = time::Duration::days(1);

/// Write one beat now. `observing = false` writes the born-expired, all-`unknown` form —
/// the process's last word before it exits on an engine failure.
pub async fn write_beat(
    db: &Db,
    statuses: &SharedStatuses,
    config: &Config,
    observing: bool,
) -> Result<(), BeatError> {
    // The global page is the operator's view — the default workspace (D-021).
    let scoped = db.workspace(monitor_domain::WorkspaceId::DEFAULT);
    let monitors = scoped
        .list_monitors()
        .await?
        .into_iter()
        .map(Monitor::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let open = scoped.open_incidents().await?;
    let json = build(&monitors, statuses, &open, config.ttl, observing);
    let bytes = serde_json::to_vec_pretty(&json)?;
    std::fs::create_dir_all(&config.dir)?;
    std::fs::create_dir_all(config.dir.join("s"))?;
    // Write-then-rename: a reader mid-beat sees the old file or the new one, never a torn
    // half-snapshot.
    write_snapshot(&config.dir, "status.json", &bytes)?;

    // Per-workspace pages (S2): one `s/<slug>.json` per enabled page, on the same
    // heartbeat. A page that turned off since the last beat has its file removed —
    // a disabled page serves 404, never last-known data.
    let pages = db.status_pages().await?;
    let mut live = std::collections::HashSet::with_capacity(pages.len());
    let since = OffsetDateTime::now_utc() - PAGE_STATS_WINDOW;
    for page in &pages {
        // Files are named by workspace, not slug: a released or renamed slug resolves
        // to this workspace's file, never a previous owner's bytes (S2 review).
        live.insert(page.workspace.0.to_string());
        let scoped = db.workspace(page.workspace);
        let open = scoped.open_incidents().await?;
        let mut coverage = HashMap::with_capacity(page.monitors.len());
        for row in &page.monitors {
            let Ok(id) = uuid::Uuid::parse_str(&row.monitor_id) else {
                continue;
            };
            let stats = scoped.check_stats(MonitorId(id), since).await?;
            coverage.insert(MonitorId(id), (stats.coverage_bps(), stats.success_bps()));
        }
        let json = build_page(page, statuses, &open, &coverage, config.ttl, observing);
        write_snapshot(
            &config.dir.join("s"),
            &format!("{}.json", page.workspace.0),
            &serde_json::to_vec_pretty(&json)?,
        )?;
    }
    if let Ok(entries) = std::fs::read_dir(config.dir.join("s")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            if !live.contains(id) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// Asks the writer for a beat now instead of at its next tick. A status-page save
/// signals it, so a page someone just published answers with real data at once
/// rather than reading "monitoring unavailable" until the heartbeat comes round.
pub type Wake = std::sync::Arc<tokio::sync::Notify>;

/// The least time between two woken beats. A beat reads every enabled page, so saves
/// arriving in a burst share one beat instead of each buying a full pass.
const WAKE_FLOOR: Duration = Duration::from_secs(1);

/// Periodic snapshot writer. Its failures are honest: a stopped writer means an expiring
/// snapshot, which the reader reads as "monitoring unavailable" — the correct verdict.
pub fn spawn(
    db: Db,
    statuses: SharedStatuses,
    vitals: Vitals,
    config: Config,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_woken(db, statuses, vitals, config, Wake::default(), shutdown)
}

/// [`spawn`], plus a [`Wake`] that brings the next beat forward.
pub fn spawn_woken(
    db: Db,
    statuses: SharedStatuses,
    vitals: Vitals,
    config: Config,
    wake: Wake,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = std::fs::create_dir_all(&config.dir) {
            tracing::error!(%error, dir = %config.dir.display(), "status dir unusable; no snapshots");
            return;
        }
        let mut ticks = tokio::time::interval(config.interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut wake_from = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                // A wake that arrives inside the floor is kept (`Notify` holds one
                // permit) and answered when the floor has passed.
                () = async {
                    tokio::time::sleep_until(wake_from).await;
                    wake.notified().await;
                } => {
                    wake_from = tokio::time::Instant::now() + WAKE_FLOOR;
                }
                () = shutdown.cancelled() => return,
            }
            let observing = vitals.verdict().observing();
            if let Err(error) = write_beat(&db, &statuses, &config, observing).await {
                tracing::error!(%error, "snapshot beat skipped");
            }
        }
    })
}
