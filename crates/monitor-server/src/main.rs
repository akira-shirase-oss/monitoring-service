//! `monitor-server`: the monitor's one process — management API, coordinator, probe workers
//! and the single-writer task.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use lettre::Tokio1Executor;
use lettre::message::Mailbox;
use lettre::transport::smtp::AsyncSmtpTransport;
use monitor_db::SharedStatuses;
use monitor_domain::Health;
use monitor_server::deliver::{Config as DelivererConfig, Deliverer, Sink};
use monitor_server::probe::{ProbeClient, ProbePolicy};
use monitor_server::schedule::{Config as CoordinatorConfig, Coordinator};
use monitor_server::vitals::{Role, Supervisor, Vitals};
use monitor_server::{AppState, load_registry, router};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower_http::services::{ServeDir, ServeFile};
use tracing_subscriber::EnvFilter;

/// Default management/bind port (58400; the dashboard's vite dev server is 58600).
const DEFAULT_LISTEN: &str = "127.0.0.1:58400";
const DEFAULT_DATABASE: &str = "sqlite:monitor.db";
/// Writer channel depth: the coordinator parks behind a full queue instead of dropping
/// evidence — a bounded queue is the honest backpressure point.
const WRITER_BOUND: usize = 64;
/// A coordinator that makes no loop pass for this long is stalled: its statuses are frozen,
/// so the engine reports unavailable. The loop ticks every second when healthy.
const COORDINATOR_STALL: Duration = Duration::from_secs(30);
/// How long shutdown waits for the engine tasks (coordinator drain, in-flight deliveries,
/// the writer's queued commits) after the HTTP server has stopped.
const ENGINE_DRAIN: Duration = Duration::from_secs(15);
/// The architecture's lab bound (100 monitors at 60 s); `SMON_MAX_MONITORS` overrides.
const DEFAULT_MAX_MONITORS: u64 = 100;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging()?;

    let env_bytes = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let database_url =
        std::env::var("SMON_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE.into());
    // Storage guardrail (M4.2): the database is bounded — writes past the ceiling fail
    // cleanly instead of silently eating the disk.
    let max_db_bytes = env_bytes("SMON_MAX_DB_BYTES", 2 * 1024 * 1024 * 1024);
    let listen = std::env::var("SMON_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.into());
    // Where the built dashboard lives; skip static serving when it has not been built.
    let dashboard_dir =
        std::env::var("SMON_DASHBOARD_DIR").unwrap_or_else(|_| "apps/dashboard/build".into());
    // `SMON_PROFILE=lab` permits loopback/private literal targets for local fixtures.
    let lab = std::env::var("SMON_PROFILE").is_ok_and(|profile| profile == "lab");

    // Management auth (M5.1): a token guards /api. Without one the server is dev-mode —
    // allowed on loopback only, and loudly, never quietly on a public bind. Resolved
    // before anything spawns: a refused configuration starts no work.
    let addr: SocketAddr = listen.parse().context("SMON_LISTEN must be host:port")?;
    let admin_token = resolve_admin_token(&addr)?;

    let db = monitor_db::Db::connect_opts(&database_url, 4, Some(max_db_bytes)).await?;
    db.migrate().await?;

    // Boot rebuild: streaks and incidents come back from the store, never from memory.
    let monitors = load_registry(&db)
        .await
        .context("loading the monitor registry")?;
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    let health = rebuild_health(&db, &monitors, &statuses).await?;

    // Resolve alert channels up front: email rows only enter the outbox when SMTP is
    // configured, so the writer needs the answer before it starts committing.
    let smtp = smtp_sink()?;
    let retention = retention_policy()?;
    let max_monitors: u64 = env_parse("SMON_MAX_MONITORS", DEFAULT_MAX_MONITORS)?;
    anyhow::ensure!(max_monitors > 0, "SMON_MAX_MONITORS must be at least 1");

    // Every engine task is owned by the supervisor from the moment it spawns: an exit
    // outside shutdown flips the vitals verdict, which readiness, the API statuses and the
    // public snapshot all read — a dead writer or coordinator is "monitoring unavailable",
    // never a frozen "up".
    let shutdown = CancellationToken::new();
    let write_health: monitor_db::WriteHealth = Arc::default();
    let vitals = Vitals::new(Arc::clone(&write_health), COORDINATOR_STALL);
    let supervisor = Supervisor::new(vitals.clone(), shutdown.clone());

    let (writer, writer_task) = monitor_db::writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        health,
        Arc::clone(&write_health),
        WRITER_BOUND,
        smtp.is_some(),
    );
    supervisor.watch("writer", Role::Observing, writer_task);
    let (registry_tx, registry_rx) = watch::channel(Arc::new(monitors));

    let probes = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: lab,
            extra_roots: Vec::new(),
        },
        64 * 1024,
    )
    .context("building the probe client")?;

    let coordinator_task = Coordinator::spawn(
        registry_rx,
        writer.clone(),
        probes,
        CoordinatorConfig::default(),
        vitals.clone(),
        shutdown.clone(),
    );
    supervisor.watch("coordinator", Role::Observing, coordinator_task);

    // Alerts only run when a receiver is configured; without one, outbox events stay pending
    // in the table — visible, never silently dropped.
    spawn_deliverers(&db, &writer, smtp, &shutdown, &supervisor)?;

    let status_dir = std::env::var("SMON_STATUS_DIR").unwrap_or_else(|_| "status".into());
    supervisor.watch(
        "snapshot",
        Role::Supporting,
        spawn_snapshot(&db, &statuses, &vitals, &status_dir, &shutdown),
    );
    supervisor.watch(
        "retention",
        Role::Supporting,
        spawn_retention(&db, &writer, retention, &shutdown),
    );

    let session_ttl = std::env::var("SMON_SESSION_TTL_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_hours(12), Duration::from_secs);
    let app = router(AppState {
        db,
        statuses,
        registry: registry_tx,
        vitals: vitals.clone(),
        admin_token,
        session_ttl,
        max_monitors,
        version: env!("CARGO_PKG_VERSION"),
    });

    let app = with_static_files(app, &status_dir, &dashboard_dir);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, database = %redact_sqlite(&database_url), lab, "monitor-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown, vitals))
        .await?;

    // The HTTP side is down; let the engine finish. Main's writer handle goes first — the
    // writer exits once every handle is gone, after committing what is already queued.
    drop(writer);
    if supervisor.drain(ENGINE_DRAIN).await {
        tracing::info!("engine drained");
    } else {
        tracing::warn!("engine tasks still running at the drain deadline");
    }
    Ok(())
}

/// The public snapshot served unmodified at /status.json — outside /api, so public viewers
/// never touch the management surface; expiry is the file's own claim. Then the dashboard
/// build as the fallback, when it has been built.
fn with_static_files(app: axum::Router, status_dir: &str, dashboard_dir: &str) -> axum::Router {
    let app = app.route_service(
        "/status.json",
        ServeFile::new(Path::new(status_dir).join("status.json")),
    );
    if Path::new(dashboard_dir).is_dir() {
        let index = ServeFile::new(format!("{dashboard_dir}/200.html"));
        tracing::info!(dir = %dashboard_dir, "serving the dashboard build");
        app.fallback_service(ServeDir::new(dashboard_dir).fallback(index))
    } else {
        tracing::info!(dir = %dashboard_dir, "dashboard not built; API only");
        app
    }
}

/// Boot rebuild: replay each monitor's stored tail through `Health` under its *current*
/// thresholds, seed the status map, and hand the machines to the writer. The rebuild tail
/// is sized from the monitor's own thresholds — a streak can never exceed them.
async fn rebuild_health(
    db: &monitor_db::Db,
    monitors: &[monitor_domain::Monitor],
    statuses: &SharedStatuses,
) -> anyhow::Result<HashMap<monitor_domain::MonitorId, Health>> {
    let mut health = HashMap::with_capacity(monitors.len());
    for monitor in monitors {
        let open = db.open_incident(monitor.id).await?.is_some();
        let tail = db
            .recent_outcomes(monitor.id, monitor.thresholds.rebuild_tail())
            .await?;
        let rebuilt = Health::rebuild(open, &tail, monitor.thresholds);
        statuses
            .write()
            .map_err(|_| anyhow::anyhow!("status map poisoned at boot"))?
            .insert(monitor.id, rebuilt.status());
        health.insert(monitor.id, rebuilt);
    }
    Ok(health)
}

/// The management credential: configured → required on every /api call; unset → dev mode,
/// which is only honest on loopback. A tokenless public bind refuses to boot rather than
/// leave the store open.
fn resolve_admin_token(addr: &SocketAddr) -> anyhow::Result<monitor_server::auth::AdminToken> {
    let token: monitor_server::auth::AdminToken = std::env::var("SMON_ADMIN_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .map(Arc::from);
    if token.is_none() {
        if addr.ip().is_loopback() {
            tracing::warn!("SMON_ADMIN_TOKEN unset: /api is unauthenticated (loopback dev mode)");
        } else {
            anyhow::bail!("SMON_ADMIN_TOKEN is required when listening on {addr}");
        }
    }
    Ok(token)
}

/// Logging goes to stdout by default. `SMON_LOG_FILE` switches to a byte-bounded file
/// (`SMON_LOG_MAX_BYTES`, default 64 MiB per generation, two generations kept) — the
/// disk guardrail applies to our own output too, not just the store.
fn init_logging() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match std::env::var("SMON_LOG_FILE") {
        Ok(path) => {
            let max = std::env::var("SMON_LOG_MAX_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(64 * 1024 * 1024);
            let writer = monitor_server::logfile::BoundedLog::open(&path, max)
                .with_context(|| format!("opening SMON_LOG_FILE {path}"))?;
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .with_writer(writer)
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
    }
    Ok(())
}

/// The public status snapshot — the heartbeat. `SMON_STATUS_DIR` points at wherever the
/// static status page is hosted; an expired snapshot there reads "monitoring unavailable".
fn spawn_snapshot(
    db: &monitor_db::Db,
    statuses: &SharedStatuses,
    vitals: &Vitals,
    dir: &str,
    shutdown: &CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let env_secs = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    tracing::info!(dir = %dir, "writing status snapshots");
    monitor_server::snapshot::spawn(
        db.clone(),
        Arc::clone(statuses),
        vitals.clone(),
        monitor_server::snapshot::Config {
            dir: dir.into(),
            interval: Duration::from_secs(env_secs("SMON_STATUS_INTERVAL_S", 30)),
            ttl: Duration::from_secs(env_secs("SMON_STATUS_TTL_S", 90)),
        },
        shutdown.clone(),
    )
}

/// Retention (M4.1, D-012): one pass every `SMON_RETENTION_INTERVAL_S` (default hourly)
/// through the single writer — raw checks past `SMON_RETENTION_S` roll into daily
/// aggregates (an open incident keeps only its birth streak raw), then old aggregates,
/// resolved incidents, ended windows and delivered outbox rows are pruned per the policy.
fn spawn_retention(
    db: &monitor_db::Db,
    writer: &monitor_db::writer::WriterHandle,
    policy: monitor_db::RetentionPolicy,
    shutdown: &CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let env_secs = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let every = Duration::from_secs(env_secs("SMON_RETENTION_INTERVAL_S", 3_600));
    let db = db.clone();
    let writer = writer.clone();
    let shutdown = shutdown.clone();
    tracing::info!(
        checks_s = policy.checks.as_secs(),
        aggregates_s = policy.aggregates.as_secs(),
        incidents_s = policy.incidents.as_secs(),
        delivered_outbox_s = policy.delivered_outbox.as_secs(),
        every_s = every.as_secs(),
        "retention armed"
    );
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                () = shutdown.cancelled() => return,
            }
            let cutoffs = policy.cutoffs(time::OffsetDateTime::now_utc());
            match writer.retain(cutoffs).await {
                Ok(report)
                    if report.rows_deleted
                        + report.aggregates_deleted
                        + report.incidents_deleted
                        + report.outbox_deleted
                        + report.windows_deleted
                        > 0 =>
                {
                    tracing::info!(
                        days = report.days_written,
                        rows = report.rows_deleted,
                        aggregates = report.aggregates_deleted,
                        incidents = report.incidents_deleted,
                        outbox = report.outbox_deleted,
                        windows = report.windows_deleted,
                        "retention pass"
                    );
                }
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "retention pass failed"),
            }
            // Expired dashboard sessions are the same class of garbage: honest expiry
            // needs no lazy-lookup to be correct, it just keeps the table small.
            match db.sweep_expired_sessions().await {
                Ok(n) if n > 0 => tracing::debug!(sessions = n, "swept expired sessions"),
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "session sweep failed"),
            }
        }
    })
}

/// A `SMON_*` value parsed strictly: unset means `default`; set but unparsable is a boot
/// error naming the variable — never a silent fallback.
fn env_parse<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{name} is not valid UTF-8"),
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|error| anyhow::anyhow!("{name}={raw:?} is invalid: {error}")),
    }
}

/// D-012's horizons, each overridable in seconds.
fn retention_policy() -> anyhow::Result<monitor_db::RetentionPolicy> {
    let defaults = monitor_db::RetentionPolicy::default();
    let secs = |name: &str, default: Duration| -> anyhow::Result<Duration> {
        let value = env_parse(name, default.as_secs())?;
        anyhow::ensure!(value > 0, "{name} must be at least 1 second");
        Ok(Duration::from_secs(value))
    };
    Ok(monitor_db::RetentionPolicy {
        checks: secs("SMON_RETENTION_S", defaults.checks)?,
        aggregates: secs("SMON_AGGREGATE_RETENTION_S", defaults.aggregates)?,
        incidents: secs("SMON_INCIDENT_RETENTION_S", defaults.incidents)?,
        delivered_outbox: secs("SMON_OUTBOX_RETENTION_S", defaults.delivered_outbox)?,
    })
}

async fn shutdown_signal(shutdown: CancellationToken, vitals: Vitals) {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for ctrl-c");
        return;
    }
    tracing::info!("shutting down");
    // Readiness goes false before anything stops, and stays false.
    vitals.begin_drain();
    shutdown.cancel();
}

/// The database URL is local-only, but keep a habit: never log a URL whole — parameters may
/// one day carry a credential.
fn redact_sqlite(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Spawn one deliverer per configured channel. Webhook rows are always enqueued — pending
/// rows are the visible "no receiver" signal — while email rows only exist when SMTP is
/// configured, so an unconfigured channel is never a dead plan.
fn spawn_deliverers(
    db: &monitor_db::Db,
    writer: &monitor_db::writer::WriterHandle,
    smtp: Option<Sink>,
    shutdown: &CancellationToken,
    supervisor: &Supervisor,
) -> anyhow::Result<()> {
    if let Ok(target) = std::env::var("SMON_WEBHOOK_URL") {
        let target = url::Url::parse(&target).context("SMON_WEBHOOK_URL must be a URL")?;
        let secret = std::env::var("SMON_WEBHOOK_SECRET")
            .ok()
            .map(String::into_bytes);
        let webhook_deliverer = Deliverer::spawn(
            db.clone(),
            writer.clone(),
            DelivererConfig::default(),
            Sink::Webhook { target, secret },
            shutdown.clone(),
        );
        supervisor.watch("webhook deliverer", Role::Supporting, webhook_deliverer);
    } else {
        tracing::info!("SMON_WEBHOOK_URL unset; alerts accumulate in the outbox");
    }
    if let Some(sink) = smtp {
        let email_deliverer = Deliverer::spawn(
            db.clone(),
            writer.clone(),
            DelivererConfig::default(),
            sink,
            shutdown.clone(),
        );
        supervisor.watch("email deliverer", Role::Supporting, email_deliverer);
    }
    Ok(())
}

/// The email alert channel (M7.1): `SMON_SMTP_DSN` selects the relay
/// (`smtps://user:pass@host[:465]`, `smtp://user:pass@host[:587]?tls=required` for mandatory
/// STARTTLS, or a bare `smtp://` for an explicitly plaintext local relay), `SMON_SMTP_FROM`
/// is the sender mailbox, `SMON_SMTP_TO` a comma-separated recipient list. All three are
/// required together; a partial configuration is a boot error, not a half-live channel.
fn smtp_sink() -> anyhow::Result<Option<Sink>> {
    let Ok(dsn) = std::env::var("SMON_SMTP_DSN") else {
        return Ok(None);
    };
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::from_url(&dsn)
        .context("SMON_SMTP_DSN must be smtp:// or smtps://")?
        .build();
    let parse_mailbox = |raw: &str, var: &str| -> anyhow::Result<Mailbox> {
        raw.trim()
            .parse::<Mailbox>()
            .with_context(|| format!("{var} contains an invalid mailbox: {raw:?}"))
    };
    let from = std::env::var("SMON_SMTP_FROM")
        .context("SMON_SMTP_FROM is required when SMON_SMTP_DSN is set")
        .and_then(|raw| parse_mailbox(&raw, "SMON_SMTP_FROM"))?;
    let recipients = std::env::var("SMON_SMTP_TO")
        .context("SMON_SMTP_TO is required when SMON_SMTP_DSN is set")?;
    let to: Vec<Mailbox> = recipients
        .split(',')
        .map(|raw| parse_mailbox(raw, "SMON_SMTP_TO"))
        .collect::<anyhow::Result<_>>()?;
    anyhow::ensure!(
        !to.is_empty(),
        "SMON_SMTP_TO must name at least one recipient"
    );
    Ok(Some(Sink::Email { mailer, from, to }))
}
