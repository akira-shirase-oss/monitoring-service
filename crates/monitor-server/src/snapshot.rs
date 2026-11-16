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

/// Periodic snapshot writer. Its failures are honest: a stopped writer means an expiring
/// snapshot, which the reader reads as "monitoring unavailable" — the correct verdict.
pub fn spawn(
    db: Db,
    statuses: SharedStatuses,
    vitals: Vitals,
    config: Config,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = std::fs::create_dir_all(&config.dir) {
            tracing::error!(%error, dir = %config.dir.display(), "status dir unusable; no snapshots");
            return;
        }
        let mut ticks = tokio::time::interval(config.interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                () = shutdown.cancelled() => return,
            }
            let monitors = match super::load_registry(&db).await {
                Ok(monitors) => monitors,
                Err(error) => {
                    tracing::error!(%error, "snapshot: registry read failed; skipping beat");
                    continue;
                }
            };
            let open = match db.open_incidents().await {
                Ok(open) => open,
                Err(error) => {
                    tracing::error!(%error, "snapshot: incidents read failed; skipping beat");
                    continue;
                }
            };
            let observing = vitals.verdict().observing();
            let json = build(&monitors, &statuses, &open, config.ttl, observing);
            let target = config.dir.join("status.json");
            // Write-then-rename: a reader mid-beat sees the old file or the new one, never a
            // torn half-snapshot.
            let tmp = config.dir.join(".status.json.tmp");
            match serde_json::to_vec_pretty(&json) {
                Ok(bytes) => {
                    if let Err(error) = std::fs::write(&tmp, bytes) {
                        tracing::error!(%error, "snapshot write failed; skipping beat");
                        continue;
                    }
                    if let Err(error) = std::fs::rename(&tmp, &target) {
                        tracing::error!(%error, "snapshot rename failed; skipping beat");
                    }
                }
                Err(error) => tracing::error!(%error, "snapshot serialize failed"),
            }
        }
    })
}
