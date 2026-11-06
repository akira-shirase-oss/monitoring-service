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

    let write_health: monitor_db::WriteHealth = Arc::default();
    let (writer, _writer_task) = monitor_db::writer::spawn(
        db.clone(),
        Arc::clone(&statuses),
        health,
        Arc::clone(&write_health),
        WRITER_BOUND,
        smtp.is_some(),
    );
    let (registry_tx, registry_rx) = watch::channel(Arc::new(monitors));

    let probes = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: lab,
            extra_roots: Vec::new(),
        },
        64 * 1024,
    )
    .context("building the probe client")?;

    let shutdown = CancellationToken::new();
    let _coordinator_task = Coordinator::spawn(
        registry_rx,
        writer.clone(),
        probes,
        CoordinatorConfig::default(),
        shutdown.clone(),
    );

    // Alerts only run when a receiver is configured; without one, outbox events stay pending
    // in the table — visible, never silently dropped.
    spawn_deliverers(&db, &writer, smtp, &shutdown)?;

    let status_dir = std::env::var("SMON_STATUS_DIR").unwrap_or_else(|_| "status".into());
    let _snapshot_task = spawn_snapshot(&db, &statuses, &status_dir, &shutdown);
    let _retention_task = spawn_retention(&db, &writer, &shutdown);

    let session_ttl = std::env::var("SMON_SESSION_TTL_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_hours(12), Duration::from_secs);
    let mut app = router(AppState {
        db,
        statuses,
        registry: registry_tx,
        write_health,
        admin_token,
        session_ttl,
        version: env!("CARGO_PKG_VERSION"),
    });

    // The public snapshot served unmodified at /status.json — outside /api, so public
    // viewers never touch the management surface; expiry is the file's own claim.
    app = app.route_service(
        "/status.json",
        ServeFile::new(Path::new(&status_dir).join("status.json")),
    );

    if Path::new(&dashboard_dir).is_dir() {
        let index = ServeFile::new(format!("{dashboard_dir}/200.html"));
        app = app.fallback_service(ServeDir::new(&dashboard_dir).fallback(index));
        tracing::info!(dir = %dashboard_dir, "serving the dashboard build");
    } else {
        tracing::info!(dir = %dashboard_dir, "dashboard not built; API only");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, database = %redact_sqlite(&database_url), lab, "monitor-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
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
        monitor_server::snapshot::Config {
            dir: dir.into(),
            interval: Duration::from_secs(env_secs("SMON_STATUS_INTERVAL_S", 30)),
            ttl: Duration::from_secs(env_secs("SMON_STATUS_TTL_S", 90)),
        },
        shutdown.clone(),
    )
}

/// Retention (M4.1): raw checks live `SMON_RETENTION_S` (default 7 days), then roll into
/// daily aggregates. Passes go through the single writer, `SMON_RETENTION_INTERVAL_S`
/// apart (default hourly) — expiry never touches open incidents or the outbox.
fn spawn_retention(
    db: &monitor_db::Db,
    writer: &monitor_db::writer::WriterHandle,
    shutdown: &CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let env_secs = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let keep = Duration::from_secs(env_secs("SMON_RETENTION_S", 7 * 86_400));
    let every = Duration::from_secs(env_secs("SMON_RETENTION_INTERVAL_S", 3_600));
    let db = db.clone();
    let writer = writer.clone();
    let shutdown = shutdown.clone();
    tracing::info!(
        keep_s = keep.as_secs(),
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
            let cutoff = time::OffsetDateTime::now_utc()
                - time::Duration::try_from(keep).unwrap_or(time::Duration::MAX);
            match writer.retain(cutoff).await {
                Ok(report) if report.rows_deleted > 0 => tracing::info!(
                    days = report.days_written,
                    rows = report.rows_deleted,
                    "retention pass"
                ),
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

async fn shutdown_signal(coordinator_stop: CancellationToken) {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for ctrl-c");
        return;
    }
    tracing::info!("shutting down");
    coordinator_stop.cancel();
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
) -> anyhow::Result<()> {
    if let Ok(target) = std::env::var("SMON_WEBHOOK_URL") {
        let target = url::Url::parse(&target).context("SMON_WEBHOOK_URL must be a URL")?;
        let secret = std::env::var("SMON_WEBHOOK_SECRET")
            .ok()
            .map(String::into_bytes);
        let _webhook_deliverer = Deliverer::spawn(
            db.clone(),
            writer.clone(),
            DelivererConfig::default(),
            Sink::Webhook { target, secret },
            shutdown.clone(),
        );
    } else {
        tracing::info!("SMON_WEBHOOK_URL unset; alerts accumulate in the outbox");
    }
    if let Some(sink) = smtp {
        let _email_deliverer = Deliverer::spawn(
            db.clone(),
            writer.clone(),
            DelivererConfig::default(),
            sink,
            shutdown.clone(),
        );
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
