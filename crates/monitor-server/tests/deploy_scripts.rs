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
fn the_unit_hardening_check_passes() {
    // The static half runs anywhere (it is what deploy-check's VM job runs in full).
    harness("check-unit-hardening.sh");
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

    // S3-9: nothing a local user can read via `ps`/proc ever carries a credential,
    // and every fetched trust root is pinned by its own fingerprint before use.
    let backup = read("smon-backup.sh");
    assert!(
        backup.contains("-H \"@") || backup.contains("--header \"@"),
        "the admin Bearer token must travel in a header *file*, never argv"
    );
    assert!(
        !backup.contains("Bearer $token\"") && !backup.contains("Bearer ${token}"),
        "the Bearer token must not be an argv fragment"
    );
    assert!(
        backup.contains("mktemp") && backup.contains("trap"),
        "the header file is temporary and cleaned up"
    );
    assert!(
        backup.contains("\" EXIT") && !backup.contains("' RETURN"),
        "the token-file trap must fire on a set -e exit — a RETURN trap does not"
    );
    for pin in [
        "65760C51EDEA2017CEA2CA15155B6D79CA56EA34", // caddy repo key, by fingerprint
        "FA7511982D758634477A48DC6375F8FC8D5FED9FDB43235BC3923887FDEA0597", // CF origin-pull CA file, sha256
        "--homedir",          // repo key verified inside a throwaway keyring…
        "--export \"$want\"", // …and ONLY the pinned key is exported to apt
        "sha256sum",          // the CA pin covers every byte of the file, not just the first cert
        "caddy version",      // >= 2.7 floor: {client_ip}/trusted-proxies need it
    ] {
        assert!(install.contains(pin), "install.sh is missing '{pin}'");
    }
    assert!(
        !install.contains("apt-get install -y caddy ||"),
        "the pinned repo must be registered BEFORE caddy is installed — Ubuntu's older \
         package would otherwise win and silently lack trusted-proxies support"
    );
    assert!(
        !install.contains("set -a"),
        "the env file is sourced unexported — `set -a` would leak the admin token to \
         every child of the installer"
    );

    // N1: the edge forwards the visitor's address, and only Cloudflare gets to say
    // what it is — the default client_ip_headers reads left-most X-Forwarded-For,
    // which a client writes itself (a fresh throttle key per request).
    let caddyfile = read("Caddyfile");
    for pin in [
        "header_up X-Forwarded-For {client_ip}",
        "trusted_proxies",
        "client_ip_headers CF-Connecting-IP",
        "trusted_proxies_strict",
    ] {
        assert!(caddyfile.contains(pin), "Caddyfile is missing '{pin}'");
    }

    // The unit check's systemd-analyze half must run against a staged --root:
    // bare `verify` judges the host's filesystem — on a CI runner without an
    // install, /opt/smon paths are absent and the check fails on host state
    // (and the host's own units leak into the verdict).
    let hardening = read("check-unit-hardening.sh");
    for pin in ["--root", "mktemp -d", "chmod +x"] {
        assert!(
            hardening.contains(pin),
            "check-unit-hardening.sh must stage a root for verify — missing '{pin}'"
        );
    }
    assert!(
        !hardening.contains("systemd-analyze verify \"$unit_file\"")
            && !hardening.contains("systemd-analyze verify \"$unit\""),
        "bare verify judges the host's filesystem, not the unit"
    );
}

#[test]
fn the_server_reports_its_schema_version() {
    // The guard's other half: the binary must report the newest migration it knows.
    // (The value itself — today's migrations — is proven by the stub harness running a
    // real built binary only in CI's release flow; here we pin the flag's existence.)
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    assert!(src.contains("--db-version"));
}
