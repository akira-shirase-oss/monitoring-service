//! Scripts that start their own monitor-server must never touch someone else's: the backup
//! drill used to bind :58400 (the default an installed monitor listens on) and `kill -9`
//! whatever held it — on a machine with a live instance, that instance.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DRILL: &str = include_str!("../../../scripts/backup-drill.sh");
const DEV: &str = include_str!("../../../scripts/dev.sh");

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts")
        .join(name)
}

/// Lines that run (comments stripped).
fn code(script: &str) -> Vec<&str> {
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

#[test]
fn the_drill_and_dev_scripts_never_target_or_kill_the_default_port() {
    for (name, text) in [("backup-drill.sh", DRILL), ("dev.sh", DEV)] {
        for line in code(text) {
            assert!(!line.contains("58400"), "{name} targets :58400: {line}");
            let kills_by_port =
                (line.contains("lsof") && line.contains("kill")) || line.contains("xargs kill");
            assert!(
                !kills_by_port,
                "{name} kills by port, not by its own PID: {line}"
            );
        }
    }
    // Each picks its own port, overridable, and guards it before starting anything.
    assert!(DRILL.contains("SMON_DRILL_PORT") && DRILL.contains("require_free_port"));
    assert!(DEV.contains("SMON_DEV_PORT") && DEV.contains("require_free_port"));
    assert!(
        DEV.contains("SMON_LISTEN"),
        "dev.sh must listen on its own port"
    );
}

/// Hold a port, point a script at it: the script must refuse at once and leave the holder
/// alone (the listener still accepts afterwards).
#[test]
fn a_script_refuses_a_port_that_is_already_in_use_and_kills_nothing() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = held.local_addr().expect("addr").port().to_string();
    let spare = std::net::TcpListener::bind("127.0.0.1:0").expect("bind spare");
    let spare_port = spare.local_addr().expect("addr").port().to_string();
    drop(spare);

    for (name, envs) in [
        (
            "backup-drill.sh",
            vec![
                ("SMON_DRILL_PORT", port.as_str()),
                ("SMON_DRILL_FIXTURE_PORT", spare_port.as_str()),
            ],
        ),
        ("dev.sh", vec![("SMON_DEV_PORT", port.as_str())]),
    ] {
        let started = Instant::now();
        let mut command = Command::new("bash");
        command.arg(script(name));
        for (key, value) in &envs {
            command.env(key, value);
        }
        let output = command.output().expect("run script");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{name} must refuse an occupied port"
        );
        assert!(stderr.contains("in use"), "{name}: {stderr}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{name} must refuse before starting anything"
        );
        assert!(
            std::net::TcpStream::connect(held.local_addr().expect("addr")).is_ok(),
            "{name} must not touch the process holding the port"
        );
    }
}
