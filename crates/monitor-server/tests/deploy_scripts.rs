//! The droplet package's shell is verified like the server: stub-based harnesses run the
//! backup script and the schema guard end to end (no server, no R2, no secrets), and the
//! deploy scripts are pinned to the properties the runbook promises — idempotent install,
//! verified downloads, an atomic `current` switch, the forward-only-migrations guard, and
//! automatic rollback on a failed upgrade.

use std::path::Path;
use std::process::Command;

const DROPLET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/droplet");

fn harness(name: &str) {
    let script = format!("{DROPLET}/{name}");
    let output = Command::new("bash")
        .arg(&script)
        .output()
        .unwrap_or_else(|e| {
            panic!("running {script}: {e}");
        });
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{name} failed (status {}):\n{stdout}\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("0 failed"),
        "{name} reported failures:\n{stdout}"
    );
}

#[test]
fn the_backup_script_passes_its_stub_harness() {
    harness("test-backup.sh");
}

#[test]
fn the_schema_guard_passes_its_stub_harness() {
    harness("test-schema-guard.sh");
}

#[test]
fn the_deploy_scripts_carry_their_promised_properties() {
    let read = |name: &str| {
        let path = format!("{DROPLET}/{name}");
        assert!(Path::new(&path).exists(), "missing {path}");
        std::fs::read_to_string(&path).unwrap()
    };
    let install = read("install.sh");
    assert!(
        install.contains("umask 077"),
        "install must run under umask 077"
    );
    assert!(
        install.contains("read -rs"),
        "secrets are read with echo off"
    );
    assert!(
        install.contains("useradd") && install.contains(" --init"),
        "install must create the user and initialise once"
    );

    let upgrade = read("upgrade.sh");
    for pin in [
        "sha256sum -c",      // downloads are verified before staging
        "mv -T",             // the `current` switch is an atomic rename
        "smon-schema-guard", // the forward-only-migrations guard runs first
        "tail -n +4",        // keeps the newest 3 releases
        "systemctl restart monitor-server",
        "rolling back", // automatic rollback on a failed upgrade
    ] {
        assert!(upgrade.contains(pin), "upgrade.sh is missing '{pin}'");
    }

    let rollback = read("rollback.sh");
    assert!(
        rollback.contains("smon-schema-guard") && rollback.contains("mv -T"),
        "rollback must run the schema guard and switch atomically"
    );

    let guard = read("schema-guard.sh");
    for pin in ["--db-version", "_sqlx_migrations", "success = 1", "CLOSED"] {
        assert!(guard.contains(pin), "schema-guard.sh is missing '{pin}'");
    }
}

#[test]
fn the_server_reports_its_schema_version() {
    // The guard's other half: the binary must report the newest migration it knows.
    // (The value itself — today's migrations — is proven by the stub harness running a
    // real built binary only in CI's release flow; here we pin the flag's existence.)
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    assert!(src.contains("--db-version"));
}
