//! Black-box process tests against the real `monitor-server` binary: what an operator sees
//! when the service manager starts it with bad configuration.

use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A clean environment with a scratch database, an ephemeral port and no dashboard — plus
/// the `extra` variables under test. Nothing from the developer's shell leaks in.
fn server(dir: &std::path::Path, extra: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_monitor-server"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SMON_DATABASE_URL",
            format!("sqlite:{}", dir.join("boot.db").display()),
        )
        .env("SMON_LISTEN", "127.0.0.1:0")
        .env("SMON_STATUS_DIR", dir.join("status"))
        .env("SMON_DASHBOARD_DIR", dir.join("no-dashboard"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in extra {
        command.env(name, value);
    }
    command
}

/// Wait for exit, killing the child at the deadline (so a test never leaks a server).
fn wait_exit(child: &mut Child, deadline: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stderr_of(child: &mut Child) -> String {
    use std::io::Read;
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut text);
    }
    text
}

/// Invalid `SMON_*` values used to fall back to their defaults silently: the server came up
/// "ready" with `SMON_MAX_DB_BYTES=2G` (no, or the wrong, disk cap), `SMON_PROFILE=Lab`
/// (production by accident) and `SMON_STATUS_TTL_S=90s`. Now the boot fails, names every
/// problem, and starts no work.
#[test]
fn invalid_smon_values_fail_the_boot_and_name_every_problem() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = server(
        dir.path(),
        &[
            ("SMON_MAX_DB_BYTES", "2G"),
            ("SMON_STATUS_TTL_S", "90s"),
            ("SMON_PROFILE", "Lab"),
        ],
    )
    .spawn()
    .expect("spawn monitor-server");
    let status = wait_exit(&mut child, Duration::from_secs(20))
        .expect("an invalid configuration must stop the boot, not serve");
    assert!(!status.success(), "exit status {status}");
    let stderr = stderr_of(&mut child);
    for name in ["SMON_MAX_DB_BYTES", "SMON_STATUS_TTL_S", "SMON_PROFILE"] {
        assert!(stderr.contains(name), "{name} not reported: {stderr}");
    }
    assert!(
        !dir.path().join("boot.db").exists(),
        "a refused configuration must not create the store"
    );
}

/// The webhook URL is the credential for most chat webhooks: a bad one fails the boot
/// without echoing it.
#[test]
fn a_bad_webhook_url_fails_the_boot_without_echoing_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = server(
        dir.path(),
        &[(
            "SMON_WEBHOOK_URL",
            "hooks.slack.com/services/T0SECRET/B0SECRET/XoXo",
        )],
    )
    .spawn()
    .expect("spawn monitor-server");
    let status = wait_exit(&mut child, Duration::from_secs(20)).expect("boot must stop");
    assert!(!status.success());
    let stderr = stderr_of(&mut child);
    assert!(stderr.contains("SMON_WEBHOOK_URL"), "{stderr}");
    assert!(
        !stderr.contains("T0SECRET") && !stderr.contains("XoXo"),
        "{stderr}"
    );
}

/// Start the server and stream its stdout (JSON log lines) into a channel.
fn start_logged(
    dir: &std::path::Path,
) -> anyhow::Result<(Child, std::sync::mpsc::Receiver<String>)> {
    use std::io::BufRead;
    // A normal start needs a store (D-015): create it the way an install does.
    if !dir.join("boot.db").exists() {
        let init = server(dir, &[]).arg("--init").output()?;
        anyhow::ensure!(
            init.status.success(),
            "--init: {}",
            String::from_utf8_lossy(&init.stderr)
        );
    }
    let mut child = server(dir, &[]).spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout is piped"))?;
    let (lines_tx, lines_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    Ok((child, lines_rx))
}

/// Collect log lines until one contains `needle` (or the deadline passes).
fn read_until(
    lines: &std::sync::mpsc::Receiver<String>,
    needle: &str,
    deadline: Duration,
) -> Vec<String> {
    let end = Instant::now() + deadline;
    let mut seen = Vec::new();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match lines.recv_timeout(left) {
            Ok(line) => {
                let hit = line.contains(needle);
                seen.push(line);
                if hit {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    seen
}

/// systemd, launchd and `kill` stop a service with SIGTERM. It used to take the default
/// action — immediate death, no drain: in-flight probes, queued commits and deliveries
/// abandoned, the process "killed by signal 15". SIGTERM now runs the same drain as Ctrl-C
/// (readiness off, engine cancelled, bounded HTTP + engine drains) and exits 0.
#[test]
fn sigterm_and_sigint_drain_the_engine_and_exit_cleanly() {
    for signal in ["-TERM", "-INT"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut child, lines) = start_logged(dir.path()).expect("start monitor-server");
        let boot = read_until(&lines, "monitor-server listening", Duration::from_secs(30));
        assert!(
            boot.iter().any(|l| l.contains("monitor-server listening")),
            "server never came up: {boot:?}"
        );
        let sent = Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success());

        let status = wait_exit(&mut child, Duration::from_secs(40))
            .unwrap_or_else(|| panic!("{signal}: the server did not stop within the drain bounds"));
        assert!(
            status.success(),
            "{signal}: expected a clean exit after the drain, got {status}"
        );
        let shutdown = read_until(&lines, "engine drained", Duration::from_secs(5));
        let name = &signal[1..];
        assert!(
            shutdown
                .iter()
                .any(|l| l.contains("shutting down") && l.contains(name)),
            "{signal}: no shutdown line naming the signal: {shutdown:?}"
        );
        assert!(
            shutdown.iter().any(|l| l.contains("engine drained")),
            "{signal}: the engine drain did not complete: {shutdown:?}"
        );
    }
}

/// The store lives on a volume. If the volume isn't mounted yet, the bare mountpoint is
/// an empty directory — and `create_if_missing` used to boot a fresh, empty store there,
/// serving "no monitors" as if evidence never existed. A normal start now requires the
/// store; only `--init` (a new install) may create one.
#[test]
fn a_missing_store_refuses_to_boot_and_creates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = server(dir.path(), &[])
        .spawn()
        .expect("spawn monitor-server");
    let status = wait_exit(&mut child, Duration::from_secs(20))
        .expect("a missing store must stop the boot, not serve an empty one");
    assert!(!status.success(), "exit status {status}");
    let stderr = stderr_of(&mut child);
    assert!(
        stderr.contains("boot.db") && stderr.contains("--init"),
        "{stderr}"
    );
    assert!(
        stderr.contains("mounted"),
        "the likely cause is named: {stderr}"
    );
    assert!(
        !dir.path().join("boot.db").exists(),
        "no store may be created"
    );
}

/// An empty file where the store should be (a stray `touch`, a half-restored copy) is not
/// a store either.
#[test]
fn an_empty_store_file_refuses_to_boot() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("boot.db"), b"").expect("touch");
    let mut child = server(dir.path(), &[])
        .spawn()
        .expect("spawn monitor-server");
    let status = wait_exit(&mut child, Duration::from_secs(20)).expect("must stop the boot");
    assert!(!status.success());
    assert!(stderr_of(&mut child).contains("empty"));
}

/// `--init` is the one way to make a new store: it creates and migrates it, then exits
/// without serving; the next normal start finds it and serves.
#[test]
fn init_creates_the_store_and_a_normal_start_then_serves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut init = server(dir.path(), &[])
        .arg("--init")
        .spawn()
        .expect("spawn --init");
    let status = wait_exit(&mut init, Duration::from_secs(30)).expect("--init exits");
    assert!(status.success(), "--init failed: {}", stderr_of(&mut init));
    assert!(
        dir.path().join("boot.db").exists(),
        "--init creates the store"
    );

    let (mut child, lines) = start_logged(dir.path()).expect("start monitor-server");
    let boot = read_until(&lines, "monitor-server listening", Duration::from_secs(30));
    assert!(
        boot.iter().any(|l| l.contains("monitor-server listening")),
        "an initialized store boots: {boot:?}"
    );
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = wait_exit(&mut child, Duration::from_secs(40));
}

/// An argument the server doesn't know never starts a server: an old binary run as
/// `monitor-server --version` once booted with defaults and created a store in the cwd.
#[test]
fn an_unknown_argument_never_starts_a_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut version = server(dir.path(), &[])
        .arg("--version")
        .spawn()
        .expect("spawn");
    let status = wait_exit(&mut version, Duration::from_secs(20)).expect("--version exits");
    assert!(status.success());
    let mut out = String::new();
    if let Some(mut stdout) = version.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut out);
    }
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");

    let mut bogus = server(dir.path(), &[])
        .arg("--frobnicate")
        .spawn()
        .expect("spawn");
    let status = wait_exit(&mut bogus, Duration::from_secs(20)).expect("must exit");
    assert_eq!(status.code(), Some(2), "a usage error");
    assert!(
        !dir.path().join("boot.db").exists(),
        "and nothing is created"
    );
}

/// Without `SMON_ADMIN_TOKEN` the whole `/api` is open — honest only on loopback (dev
/// mode). A wildcard or public listen with no token must stop the boot before anything
/// runs: no listener, no snapshot, no engine. The store exists (made by `--init`), so the
/// token is the only thing that can stop it.
#[test]
fn a_tokenless_non_loopback_listen_refuses_to_boot_and_binds_nothing() {
    for listen in ["0.0.0.0:0", "[::]:0"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let init = server(dir.path(), &[])
            .arg("--init")
            .output()
            .expect("--init");
        assert!(init.status.success(), "--init: {init:?}");

        let mut child = server(dir.path(), &[("SMON_LISTEN", listen)])
            .spawn()
            .expect("spawn monitor-server");
        let status = wait_exit(&mut child, Duration::from_secs(20)).unwrap_or_else(|| {
            panic!("{listen}: a tokenless public listen must stop the boot, not serve")
        });
        assert!(!status.success(), "{listen}: exit status {status}");
        let stderr = stderr_of(&mut child);
        assert!(
            stderr.contains("SMON_ADMIN_TOKEN is required"),
            "{listen}: the cause is named: {stderr}"
        );
        let mut stdout = String::new();
        if let Some(mut out) = child.stdout.take() {
            use std::io::Read;
            let _ = out.read_to_string(&mut stdout);
        }
        assert!(
            !stdout.contains("monitor-server listening"),
            "{listen}: nothing may be bound: {stdout}"
        );
        assert!(
            !dir.path().join("status").exists(),
            "{listen}: no engine task may have run (the snapshot writer creates status/)"
        );
    }
}
