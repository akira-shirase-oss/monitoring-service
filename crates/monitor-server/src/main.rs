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
use monitor_server::config::Config;
use monitor_server::deliver::{Config as DelivererConfig, Deliverer, Sink};
use monitor_server::probe::{ProbeClient, ProbePolicy};
use monitor_server::schedule::{Config as CoordinatorConfig, Coordinator};
use monitor_server::serve::{Limits, Stopped, serve};
use monitor_server::vitals::{Role, Supervisor, Vitals};
use monitor_server::{AppState, load_registry, rebuild_health, record_downtime};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// Writer channel depth: the coordinator parks behind a full queue instead of dropping
/// evidence — a bounded queue is the honest backpressure point.
const WRITER_BOUND: usize = 64;
/// What this process was started to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Serve,
    Init,
}

const USAGE: &str = "usage: monitor-server            serve (the store must exist)
       monitor-server --init     create and migrate the store of a new install, then exit
       monitor-server --version  print the version
       monitor-server --help     print this help
configuration: SMON_* environment variables (README, Configuration)";

/// Arguments: none to serve; `--init` to create the store of a new install (D-015);
/// `--version`; `--help`. Anything else is a usage error (exit 2) — never a server started
/// by accident with default settings.
fn parse_mode() -> Result<Mode, std::process::ExitCode> {
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => Ok(Mode::Serve),
        [flag] if flag == "--init" => Ok(Mode::Init),
        [flag] if flag == "--version" || flag == "-V" => {
            println!("monitor-server {}", env!("CARGO_PKG_VERSION"));
            Err(std::process::ExitCode::SUCCESS)
        }
        [flag] if flag == "--help" || flag == "-h" => {
            println!("{USAGE}");
            Err(std::process::ExitCode::SUCCESS)
        }
        other => {
            eprintln!("monitor-server: unknown arguments {other:?}\n{USAGE}");
            Err(std::process::ExitCode::from(2))
        }
    }
}

/// D-015: a normal start requires the store — a missing or empty one usually means the
/// data volume isn't mounted, not that history should start over. Only `--init` creates
/// one; it migrates and returns `None` (nothing to serve). The size ceiling applies to
/// every pooled connection either way (M4.2).
async fn open_store(config: &Config, mode: Mode) -> anyhow::Result<Option<monitor_db::Db>> {
    let cap = Some(config.max_db_bytes);
    let db = match mode {
        Mode::Init => monitor_db::Db::connect_opts(&config.database_url, 4, cap).await?,
        Mode::Serve => monitor_db::Db::connect_existing(&config.database_url, 4, cap).await?,
    };
    db.migrate().await?;
    if mode == Mode::Init {
        tracing::info!(database = %redact_sqlite(&config.database_url), "store ready");
        println!("store ready: {}", redact_sqlite(&config.database_url));
        db.close().await;
        return Ok(None);
    }
    // D-019: the only place `/api/backup` may write. Created owner-only at boot so a bad
    // path fails the boot, never the first backup request after an upgrade.
    prepare_backup_dir(config).await?;
    Ok(Some(db))
}

/// `SMON_BACKUP_DIR` (D-019): the confinement root for `/api/backup` snapshots —
/// created owner-only. Unset means the endpoint stays off; nothing to prepare.
async fn prepare_backup_dir(config: &Config) -> anyhow::Result<()> {
    let Some(dir) = &config.backup_dir else {
        return Ok(());
    };
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating SMON_BACKUP_DIR {}", dir.display()))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(
        dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .await
    .with_context(|| format!("making SMON_BACKUP_DIR {} owner-only", dir.display()))?;
    Ok(())
}

/// A coordinator that makes no loop pass for this long is stalled: its statuses are frozen,
/// so the engine reports unavailable. The loop ticks every second when healthy.
const COORDINATOR_STALL: Duration = Duration::from_secs(30);
/// A stall this long is not going to clear by itself: exit so the service manager restarts
/// the process (D-011).
const COORDINATOR_STALL_EXIT: Duration = Duration::from_secs(120);
/// How long shutdown waits for the engine tasks (coordinator drain, in-flight deliveries,
/// the writer's queued commits) after the HTTP server has stopped.
const ENGINE_DRAIN: Duration = Duration::from_secs(15);
/// How long shutdown lets open HTTP requests finish. axum's graceful shutdown waits
/// without a deadline; the API's own request timeout is 10 s, so this is a little more.
const HTTP_DRAIN: Duration = Duration::from_secs(12);

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    let mode = match parse_mode() {
        Ok(mode) => mode,
        Err(exit) => return Ok(exit),
    };

    // Configuration first, before logging or any work: every invalid `SMON_*` value is
    // reported at once and the boot stops (defaults: `config.rs`, README).
    let config = Config::from_env().context("loading SMON_* configuration")?;
    init_logging(&config)?;

    // Management auth (M5.1): a token guards /api. Without one the server is dev-mode —
    // allowed on loopback only, and loudly, never quietly on a public bind. Resolved
    // before anything spawns: a refused configuration starts no work.
    let addr = config.listen;
    let admin_token = resolve_admin_token(&addr, config.admin_token.as_deref())?;

    let Some(db) = open_store(&config, mode).await? else {
        return Ok(std::process::ExitCode::SUCCESS); // `--init` done
    };

    // Boot rebuild: streaks and incidents come back from the store, never from memory.
    let monitors = load_registry(&db)
        .await
        .context("loading the monitor registry")?;
    let statuses: SharedStatuses = Arc::new(RwLock::new(HashMap::new()));
    let health = rebuild_health(&db, &monitors, &statuses)
        .await
        .context("rebuilding health from the store")?;

    // Resolve alert channels up front: email rows only enter the outbox when SMTP is
    // configured, so the writer needs the answer before it starts committing.
    let smtp = smtp_sink(&config)?;

    // Every engine task is owned by the supervisor from the moment it spawns: an exit
    // outside shutdown flips the vitals verdict, which readiness, the API statuses and the
    // public snapshot all read — a dead writer or coordinator is "monitoring unavailable",
    // never a frozen "up".
    let shutdown = CancellationToken::new();
    let write_health: monitor_db::WriteHealth = Arc::default();
    let vitals = Vitals::new(Arc::clone(&write_health), COORDINATOR_STALL);
    let supervisor = Supervisor::new(vitals.clone(), shutdown.clone());

    let writer = spawn_writer(
        &db,
        &statuses,
        health,
        &write_health,
        smtp.is_some(),
        &supervisor,
    )
    .await?;
    // Restart honesty: time the process was down is a gap in evidence. One `missed` row
    // per monitor that slept through a slot resets its streaks (failures either side of
    // the outage are not consecutive) and makes its status `unknown` until a fresh probe
    // lands — committed before the coordinator or the first snapshot beat runs.
    let coordinator_config = CoordinatorConfig::default();
    record_boot_gaps(&writer, &db, &monitors, coordinator_config.dispatch_grace).await;
    let (registry_tx, registry_rx) = watch::channel(Arc::new(monitors));

    spawn_coordinator(
        &config,
        registry_rx,
        &writer,
        coordinator_config,
        &vitals,
        &shutdown,
        &supervisor,
    )?;

    // Alerts only run when a receiver is configured; without one, outbox events stay pending
    // in the table — visible, never silently dropped.
    spawn_deliverers(&config, &db, &writer, smtp, &shutdown, &supervisor);

    supervisor.watch(
        "snapshot",
        Role::Supporting,
        spawn_snapshot(&config, &db, &statuses, &vitals, &shutdown),
    );
    supervisor.watch(
        "retention",
        Role::Supporting,
        spawn_retention(&config, &db, &writer, &vitals, &shutdown),
    );

    let snapshot_db = db.clone();
    let snapshot_statuses = Arc::clone(&statuses);
    let state = app_state(
        &config,
        db,
        statuses,
        registry_tx,
        vitals.clone(),
        admin_token,
    );
    let app = assemble(state, &config).await?;

    // Install the stop signals before serving: a registration failure is a boot error,
    // not a detached panic that would look like a clean shutdown (rust-mastery ch. 12).
    let stop = stop_signal().context("installing SIGTERM/SIGINT handlers")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(
        addr = %bound,
        database = %redact_sqlite(&config.database_url),
        lab = config.lab,
        "monitor-server listening"
    );

    let stopped = serve(
        listener,
        app,
        stop,
        supervisor.engine_failure(),
        shutdown,
        vitals,
        HTTP_DRAIN,
        Limits {
            max_connections: config.max_connections,
            write_stall: config.write_stall,
            ..Limits::default()
        },
    )
    .await?;
    finish(
        stopped,
        &supervisor,
        writer,
        &snapshot_db,
        &snapshot_statuses,
        &config,
    )
    .await
    .map(|()| std::process::ExitCode::SUCCESS)
}

/// After serving stops: on an engine failure, one honest "monitoring unavailable" beat
/// first; then the engine drain; then the exit code — 0 for a signal, an error (exit 1)
/// for an engine failure so `Restart=on-failure` / launchd `KeepAlive` restart it.
async fn finish(
    stopped: Stopped,
    supervisor: &Supervisor,
    writer: monitor_db::writer::WriterHandle,
    db: &monitor_db::Db,
    statuses: &SharedStatuses,
    config: &Config,
) -> anyhow::Result<()> {
    if matches!(stopped, Stopped::EngineFailed(_)) {
        let beat = monitor_server::snapshot::Config {
            dir: config.status_dir.clone().into(),
            interval: config.status_interval,
            ttl: config.status_ttl,
        };
        if let Err(error) = monitor_server::snapshot::write_beat(db, statuses, &beat, false).await {
            tracing::error!(%error, "final unavailable snapshot not written");
        }
    }
    // The HTTP side is down; let the engine finish. Main's writer handle goes first — the
    // writer exits once every handle is gone, after committing what is already queued.
    drop(writer);
    if supervisor.drain(ENGINE_DRAIN).await {
        tracing::info!("engine drained");
    } else {
        tracing::warn!("engine tasks still running at the drain deadline");
    }
    match stopped {
        Stopped::Signal(_) => Ok(()),
        Stopped::EngineFailed(cause) => Err(anyhow::anyhow!(
            "monitoring engine failed ({cause}); exiting so the service manager restarts it"
        )),
    }
}

/// The single writer, on a connection of its own so its commits never queue behind API
/// reads for a pooled one, owned by the supervisor as an observing task.
async fn spawn_writer(
    db: &monitor_db::Db,
    statuses: &SharedStatuses,
    health: HashMap<monitor_domain::MonitorId, monitor_domain::Health>,
    write_health: &monitor_db::WriteHealth,
    queue_email: bool,
    supervisor: &Supervisor,
) -> anyhow::Result<monitor_db::writer::WriterHandle> {
    let writer_db = db
        .writer_connection()
        .await
        .context("opening the writer's connection")?;
    let (writer, task) = monitor_db::writer::spawn(
        writer_db,
        Arc::clone(statuses),
        health,
        Arc::clone(write_health),
        WRITER_BOUND,
        queue_email,
    );
    supervisor.watch("writer", Role::Observing, task);
    Ok(writer)
}

/// The management API's state: the shared engine handles plus the settings it reads.
fn app_state(
    config: &Config,
    db: monitor_db::Db,
    statuses: SharedStatuses,
    registry: watch::Sender<Arc<Vec<monitor_domain::Monitor>>>,
    vitals: Vitals,
    admin_token: monitor_server::auth::AdminToken,
) -> AppState {
    AppState {
        db,
        statuses,
        registry,
        vitals,
        admin_token,
        session_ttl: config.session_ttl,
        session_idle: config.session_idle,
        clock: monitor_server::system_clock(),
        trusted_proxy: config.trusted_proxy,
        backup_dir: config.backup_dir.clone(),
        cookie_secure: config.cookie_secure,
        max_monitors: config.max_monitors,
        allow_local_targets: config.lab,
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// The whole HTTP surface (`monitor_server::app`: the API, the public snapshot, the
/// dashboard build, the browser hardening headers). The CSP allows the dashboard build's
/// inline bootstrap by hash, read from the build once, on a blocking thread. No route is
/// added here — the auth inventory reads lib.rs (`tests/auth_inventory.rs`).
async fn assemble(state: AppState, config: &Config) -> anyhow::Result<axum::Router> {
    let dir = std::path::PathBuf::from(&config.dashboard_dir);
    let csp = tokio::task::spawn_blocking(move || monitor_server::headers::dashboard_csp(&dir))
        .await
        .context("reading the dashboard build for its CSP")?;
    let dashboard = Path::new(&config.dashboard_dir);
    if dashboard.is_dir() {
        tracing::info!(dir = %config.dashboard_dir, "serving the dashboard build");
    } else {
        tracing::info!(dir = %config.dashboard_dir, "dashboard not built; API only");
    }
    Ok(monitor_server::app(
        state,
        Path::new(&config.status_dir),
        dashboard,
        &csp,
    ))
}

/// The coordinator and its probe client (the SSRF policy follows `SMON_PROFILE`), owned
/// by the supervisor as an observing task.
fn spawn_coordinator(
    config: &Config,
    registry: watch::Receiver<Arc<Vec<monitor_domain::Monitor>>>,
    writer: &monitor_db::writer::WriterHandle,
    coordinator_config: CoordinatorConfig,
    vitals: &Vitals,
    shutdown: &CancellationToken,
    supervisor: &Supervisor,
) -> anyhow::Result<()> {
    let probes = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: config.lab,
            extra_roots: Vec::new(),
        },
        64 * 1024,
    )
    .context("building the probe client")?;
    let coordinator_task = Coordinator::spawn(
        registry,
        writer.clone(),
        probes,
        coordinator_config,
        vitals.clone(),
        shutdown.clone(),
    );
    supervisor.watch("coordinator", Role::Observing, coordinator_task);
    supervisor.watch_stall(COORDINATOR_STALL_EXIT, Duration::from_secs(5));
    Ok(())
}

/// D-014: commit the downtime gap for every monitor that slept through a slot. A failure
/// here is logged and visible through the write-failure counter (readiness), not fatal —
/// the monitor still boots and observes.
async fn record_boot_gaps(
    writer: &monitor_db::writer::WriterHandle,
    db: &monitor_db::Db,
    monitors: &[monitor_domain::Monitor],
    dispatch_grace: Duration,
) {
    let now = time::OffsetDateTime::now_utc();
    match record_downtime(writer, db, monitors, now, dispatch_grace).await {
        Ok(0) => {}
        Ok(gaps) => tracing::info!(monitors = gaps, "recorded downtime gaps from before boot"),
        Err(error) => tracing::error!(%error, "could not record downtime gaps"),
    }
}

/// The management credential: configured → required on every /api call; unset → dev mode,
/// which is only honest on loopback. A tokenless public bind refuses to boot rather than
/// leave the store open. (A blank `SMON_ADMIN_TOKEN` is already a config error.)
fn resolve_admin_token(
    addr: &SocketAddr,
    token: Option<&str>,
) -> anyhow::Result<monitor_server::auth::AdminToken> {
    let token: monitor_server::auth::AdminToken = token.map(Arc::from);
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
fn init_logging(config: &Config) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match &config.log_file {
        Some(path) => {
            let writer = monitor_server::logfile::BoundedLog::open(path, config.log_max_bytes)
                .with_context(|| format!("opening SMON_LOG_FILE {path}"))?;
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .with_writer(writer)
                .init();
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
    }
    // Typos in variable names can't be validated, only noticed.
    let unknown = Config::unknown_keys(
        std::env::vars_os().map(|(key, _)| key.to_string_lossy().into_owned()),
    );
    if !unknown.is_empty() {
        tracing::warn!(?unknown, "SMON_* variables nothing reads (typo?)");
    }
    Ok(())
}

/// The public status snapshot — the heartbeat. `SMON_STATUS_DIR` points at wherever the
/// static status page is hosted; an expired snapshot there reads "monitoring unavailable".
fn spawn_snapshot(
    config: &Config,
    db: &monitor_db::Db,
    statuses: &SharedStatuses,
    vitals: &Vitals,
    shutdown: &CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tracing::info!(dir = %config.status_dir, "writing status snapshots");
    monitor_server::snapshot::spawn(
        db.clone(),
        Arc::clone(statuses),
        vitals.clone(),
        monitor_server::snapshot::Config {
            dir: config.status_dir.clone().into(),
            interval: config.status_interval,
            ttl: config.status_ttl,
        },
        shutdown.clone(),
    )
}

/// Retention (M4.1, D-012): one pass every `SMON_RETENTION_INTERVAL_S` (default hourly)
/// through the single writer — raw checks past `SMON_RETENTION_S` roll into daily
/// aggregates (an open incident keeps only its birth streak raw), then old aggregates,
/// resolved incidents, ended windows and delivered outbox rows are pruned per the policy.
fn spawn_retention(
    config: &Config,
    db: &monitor_db::Db,
    writer: &monitor_db::writer::WriterHandle,
    vitals: &Vitals,
    shutdown: &CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let policy = config.retention;
    let every = config.retention_interval;
    let vitals = vitals.clone();
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
            let pass = writer.retain(cutoffs).await;
            // A failing pass degrades the monitor (D-011) until a pass succeeds again; the
            // error is logged there, and `faults` gets a fixed cause (it is public).
            vitals.retention_outcome(pass.as_ref().map(|_| ()));
            match pass {
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
                Ok(_) | Err(_) => {}
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

/// SIGTERM (systemd, launchd, `kill`) and SIGINT (Ctrl-C) mean the same thing: stop. Both
/// are registered here, before the server starts; the returned future resolves with the
/// signal's name.
#[cfg(unix)]
fn stop_signal() -> std::io::Result<impl std::future::Future<Output = &'static str>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        }
    })
}

#[cfg(not(unix))]
fn stop_signal() -> std::io::Result<impl std::future::Future<Output = &'static str>> {
    Ok(async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => "ctrl-c",
            Err(error) => {
                tracing::error!(%error, "ctrl-c handler failed; stopping");
                "ctrl-c handler error"
            }
        }
    })
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
    config: &Config,
    db: &monitor_db::Db,
    writer: &monitor_db::writer::WriterHandle,
    smtp: Option<Sink>,
    shutdown: &CancellationToken,
    supervisor: &Supervisor,
) {
    if let Some(target) = config.webhook_url.clone() {
        let secret = config.webhook_secret.clone().map(String::into_bytes);
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
}

/// The email alert channel (M7.1): `SMON_SMTP_DSN` selects the relay — prefer
/// `smtps://user:pass@host[:465]` (implicit TLS, no cleartext preamble);
/// `smtp://user:pass@host[:587]?tls=required` is mandatory STARTTLS where implicit TLS
/// isn't offered, and a bare `smtp://` an explicitly plaintext local relay. `SMON_SMTP_FROM`
/// is the sender mailbox, `SMON_SMTP_TO` a comma-separated recipient list. All three are
/// required together; a partial configuration is a boot error, not a half-live channel.
fn smtp_sink(config: &Config) -> anyhow::Result<Option<Sink>> {
    let Some(smtp) = &config.smtp else {
        return Ok(None);
    };
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::from_url(&smtp.dsn)
        .context("SMON_SMTP_DSN must be smtp:// or smtps://")?
        .build();
    let parse_mailbox = |raw: &str, var: &str| -> anyhow::Result<Mailbox> {
        raw.trim()
            .parse::<Mailbox>()
            .with_context(|| format!("{var} contains an invalid mailbox: {raw:?}"))
    };
    let from = parse_mailbox(&smtp.from, "SMON_SMTP_FROM")?;
    let to: Vec<Mailbox> = smtp
        .to
        .split(',')
        .map(|raw| parse_mailbox(raw, "SMON_SMTP_TO"))
        .collect::<anyhow::Result<_>>()?;
    anyhow::ensure!(
        !to.is_empty(),
        "SMON_SMTP_TO must name at least one recipient"
    );
    Ok(Some(Sink::Email { mailer, from, to }))
}
