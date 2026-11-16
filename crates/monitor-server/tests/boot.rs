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
