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
