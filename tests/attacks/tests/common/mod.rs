//! One running instance per test binary: the real `monitor-server` binary on a
//! loopback port with a temp store, strict profile — the production refusal gates,
//! not lab allowances. Cookie mode stays `auto`: the `__Host-` cookie parses and
//! replays over plain HTTP for a test client (`off` is a lab-only escape since
//! the N2 review, and this suite must not run lab).
//!
//! Read-only with respect to repo state: everything the instance writes lands in
//! a `TempDir`; the port is whatever 127.0.0.1:0 gave us; the child is killed on drop.

#![allow(
    dead_code,
    reason = "shared spawner: helpers may be unused per test binary"
)]

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};

/// How long the suite waits for `/livez` before declaring the boot failed.
const BOOT_DEADLINE: Duration = Duration::from_secs(15);
/// A client that follows no redirects and never uses a proxy — attack requests go
/// exactly where they're aimed.
pub(crate) fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()?)
}

/// The running server. Dropping it SIGKILLs the child (it gets no graceful shutdown —
/// that's fine; sqlite is crash-safe by design) and deletes the temp store.
pub(crate) struct Instance {
    /// `http://127.0.0.1:{port}` — the attack surface.
    pub(crate) base: String,
    /// The operator Bearer the instance was booted with.
    pub(crate) token: &'static str,
    child: std::process::Child,
    tmp: tempfile::TempDir,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where the built binary is: `SMON_ATTACK_BIN` overrides (CI builds release);
/// otherwise the workspace's debug target dir.
fn server_binary() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("SMON_ATTACK_BIN") {
        return path.into();
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    dir.join(if cfg!(windows) {
        "monitor-server.exe"
    } else {
        "monitor-server"
    })
}

/// Boot a fresh instance. Fails the test loudly if the binary is missing or the
/// port never answers — an unrunnable suite is a failure, not a skip.
pub(crate) async fn spawn() -> anyhow::Result<Instance> {
    let bin = server_binary();
    if !bin.exists() {
        bail!(
            "monitor-server binary not found at {} — run `cargo build -p monitor-server` \
             or set SMON_ATTACK_BIN",
            bin.display()
        );
    }
    let tmp = tempfile::TempDir::new()?;
    let db = tmp.path().join("attack.db");
    let status_dir = tmp.path().join("status");
    std::fs::create_dir(&status_dir)?;
    let dashboard_dir = tmp.path().join("dash");
    std::fs::create_dir(&dashboard_dir)?;

    // A free port: bind :0, read the port, drop. The tiny rebind race is retried
    // below — a boot that lost it fails the probe loop, and one retry covers it.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let envs = [
        ("SMON_DATABASE_URL", format!("sqlite:{}", db.display())),
        ("SMON_LISTEN", format!("127.0.0.1:{port}")),
        (
            "SMON_ADMIN_TOKEN",
            // ≥32 bytes per config validation; a fixed value, not a secret —
            // the suite owns the instance it attacks.
            "f00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafe".to_owned(),
        ),
        ("SMON_STATUS_DIR", status_dir.display().to_string()),
        ("SMON_DASHBOARD_DIR", dashboard_dir.display().to_string()),
        // Cookie mode left at `auto`: the `Secure`-marked `__Host-` cookie parses
        // and replays fine over http:// for a test client (TLS is Caddy's job in
        // the real deployment) — and `off` is refused outside lab since the
        // N2 review anyway, while the suite must keep the strict profile.
        (
            "SMON_LOG_FILE",
            tmp.path().join("server.log").display().to_string(),
        ),
    ];
    // stderr is inherited — a failed --init explains itself on the test log.
    let status = std::process::Command::new(&bin)
        .arg("--init")
        .envs(envs.iter().map(|(k, v)| (*k, v)))
        .stdout(Stdio::null())
        .status()
        .context("--init failed to run")?;
    if !status.success() {
        bail!("monitor-server --init exited {status}");
    }
    let child = std::process::Command::new(&bin)
        .envs(envs.iter().map(|(k, v)| (*k, v)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning monitor-server")?;

    let instance = Instance {
        base: format!("http://127.0.0.1:{port}"),
        token: "f00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafe",
        child,
        tmp,
    };
    let http = client()?;
    let deadline = std::time::Instant::now() + BOOT_DEADLINE;
    loop {
        match http.get(format!("{}/livez", instance.base)).send().await {
            Ok(response) if response.status() == reqwest::StatusCode::OK => break,
            _ if std::time::Instant::now() > deadline => {
                // Surface the boot log's tail — an unreadable failure is worse.
                let log = std::fs::read_to_string(instance.tmp.path().join("server.log"))
                    .unwrap_or_else(|_| "<no log>".to_owned());
                bail!("monitor-server never answered /livez; log tail:\n{log}");
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    Ok(instance)
}

/// Bearer-authed request helper.
#[expect(
    clippy::unwrap_used,
    reason = "test helper: a client that can't build is a failed test"
)]
pub(crate) fn authed(
    instance: &Instance,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    client()
        .unwrap()
        .request(method, format!("{}{}", instance.base, path))
        .bearer_auth(instance.token)
}
