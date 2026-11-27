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

#[expect(
    clippy::expect_used,
    reason = "a missing deploy file is a test failure, not input"
)]
fn read(name: &str) -> String {
    let path = format!("{DROPLET}/{name}");
    assert!(Path::new(&path).exists(), "missing {path}");
    std::fs::read_to_string(&path).expect("just proven to exist")
}

#[test]
fn the_migration_inspector_fails_closed() {
    // B re-review: opens the database -readonly (never writes, not even
    // journal files — the file:...?mode=ro URI is not portable across sqlite3
    // CLIs), compares the stored SHA-384 as hex(checksum) — never length() —
    // against the candidate binary's own compiled-in manifest so no source
    // tree or migrations/ directory must exist on the host, treats a failed
    // (success=0) row as divergence, bounds a busy database with .timeout,
    // and exits 1 rather than waving the switch through on any divergence or
    // query error.
    let inspector = read("inspect-migrations.sh");
    for pin in [
        "-readonly",
        "hex(checksum)",
        "--migration-manifest",
        ".timeout",
        "success",
        "set -euo pipefail",
    ] {
        assert!(
            inspector.contains(pin),
            "inspect-migrations.sh is missing '{pin}'"
        );
    }
    assert!(
        inspector.contains("exit 1"),
        "the inspection must fail closed, not warn and continue"
    );
}

#[test]
fn the_guards_run_on_staging_before_anything_moves() {
    // G2: a release name is unique — a repeated --release-dir name computes
    // the same destination, which the script must refuse while everything is
    // still a staging dir; and the schema guard + migration inspector run on
    // the STAGED binary, so a bad release is refused before anything under
    // releases/ (let alone `current`) is touched.
    let upgrade = read("upgrade.sh");
    let refuse = upgrade
        .find("\"$dest\" == \"$previous\"")
        .expect("upgrade.sh must refuse a destination that already exists");
    let publish = upgrade
        .find("mv -T \"$staging\" \"$dest\"")
        .expect("the staging dir becomes the release only after the guards");
    // The -x sanity check also mentions the staged binary — what matters is
    // that at least two references (guard + inspector) sit INSIDE the window
    // between the duplicate-name refusal and the publish move.
    let on_staging_inside: usize = upgrade
        .match_indices("$staging/bin/monitor-server")
        .map(|(pos, _)| pos)
        .filter(|pos| refuse < *pos && *pos < publish)
        .count();
    assert!(
        on_staging_inside >= 2,
        "the schema guard and the inspector must run on $staging/bin/monitor-server \
         AFTER refusing a duplicate name and BEFORE the staging dir is published"
    );
    // The staging name itself is unique per run — a repeated run must not
    // collide with its own leftover.
    assert!(
        upgrade.contains(".staging-$VERSION-"),
        "staging dirs carry a unique suffix, not the shared .staging-$VERSION"
    );
    assert!(
        upgrade.contains("already exists"),
        "the refusal must say why — a silent exit teaches nothing"
    );
}

#[test]
fn the_deploy_scripts_carry_their_promised_properties() {
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
        // B re-review: the read-only _sqlx_migrations inspection is scripted
        // and its result is BINDING — it runs before the switch under set -e
        // and aborts the upgrade, it does not warn-and-continue.
        "inspect-migrations",
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
        "9A1AC2B4BE15F9F27EEE20A734CBA4E9898F61001B3BD7C84B69B56A3E25A2B9", // CF origin-pull CA, by certificate fingerprint
        "--homedir",          // repo key verified inside a throwaway keyring…
        "--export \"$want\"", // …and ONLY the pinned key is exported to apt
        // The pinned certificate is re-encoded on its own and only that is installed:
        // nothing appended to the download can reach the trust file.
        "openssl x509 -in \"$aop_ca\" -out \"$aop_one\"",
        "install -m 0644 \"$aop_one\" /etc/caddy/cloudflare-origin-pull-ca.pem",
        // A second run must not copy over the binary the service is running from.
        "install -m 0755 \"$bin\" \"$PREFIX/releases/$name/bin/\"",
        "caddy version",      // >= 2.7 floor: {client_ip}/trusted-proxies need it
        // S3: the sources line is written inline with signed-by — a fetched list
        // could arrive carrying `trusted=yes` and undo the fingerprint pin.
        "deb [signed-by=/usr/share/keyrings/caddy-stable-archive-keyring.gpg]",
    ] {
        assert!(install.contains(pin), "install.sh is missing '{pin}'");
    }
    assert!(
        !install.contains("debian.deb.txt"),
        "the apt sources list must not be downloaded — its signed-by/trusted=yes \
         content would undo the key pin"
    );
    assert!(
        !install.contains("apt-get install -y caddy ||"),
        "the pinned repo must be registered BEFORE caddy is installed — Ubuntu's older \
         package would otherwise win and silently lack trusted-proxies support"
    );
    // N5: the sources line (and its key pin) is written *unconditionally* — a box
    // where caddy is already installed still needs the pinned repo, or its next
    // `apt-get upgrade` pulls from whatever unverifiable source it already had.
    let sources_line = install
        .find("deb [signed-by=/usr/share/keyrings/caddy-stable-archive-keyring.gpg]")
        .expect("the inline sources line");
    let conditional = install
        .find("if ! need caddy")
        .expect("the conditional install");
    assert!(
        sources_line < conditional,
        "the pinned sources line must be written BEFORE the `need caddy` check — \
         an already-installed caddy still needs the pinned repo"
    );
    assert!(
        !install.contains("set -a"),
        "the env file is sourced unexported — `set -a` would leak the admin token to \
         every child of the installer"
    );
}

#[test]
fn the_migration_inspection_is_hex_and_version_18_is_scoped() {
    // N-4 re-review: the DEPLOY.md instruction must be runnable as written —
    // hex(checksum) is what `shasum -a 384` produces; length(checksum) is not.
    // And the version-18 tell is scoped to a database that last ran a
    // pre-merge build — on the first deploy of this release line version 18
    // is ordinary, so the doc must not send an operator to "repair" a
    // healthy database.
    let deploy = read("DEPLOY.md");
    assert!(
        deploy.contains("hex(checksum)"),
        "the migration inspection must select hex(checksum) for the shasum comparison"
    );
    assert!(
        !deploy.contains("length(checksum)"),
        "length(checksum) can never be compared to shasum -a 384"
    );
    assert!(
        deploy.contains("pre-merge"),
        "the version-18 tell must be scoped to a pre-merge database"
    );
}

#[test]
fn the_edge_and_unit_files_carry_their_promised_properties() {
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

    // hyper's header-read deadline doubles as the keep-alive idle bound, and on it
    // the server closes the connection without a response. An upstream keepalive
    // longer than the bound reuses a connection the server is closing — the reuse
    // races the close and a non-idempotent request comes back a 502. The proxy's
    // transport must expire its idle upstreams strictly below the server's bound.
    let proxy = caddyfile
        .find("reverse_proxy")
        .expect("Caddyfile must proxy to the server");
    let transport = caddyfile[proxy..]
        .find("transport http")
        .expect("reverse_proxy must set an http transport with a bounded keepalive");
    let transport_end = caddyfile[proxy + transport..]
        .find('}')
        .expect("the transport block must close");
    let block = &caddyfile[proxy + transport..proxy + transport + transport_end];
    let keepalive = block
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("keepalive "))
        .expect("the http transport must set `keepalive` below the server's bound")
        .trim()
        .trim_end_matches(';');
    let (num, unit) = keepalive.split_at(
        keepalive
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(keepalive.len()),
    );
    let seconds = num.trim().parse::<f64>().expect("a keepalive duration")
        * match unit.trim() {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            other => panic!("unhandled keepalive unit {other:?}"),
        };
    assert!(
        seconds
            < monitor_server::serve::Limits::default()
                .header_read_timeout
                .as_secs_f64(),
        "the upstream keepalive ({seconds}s) must sit below the server's idle bound \
         — a reused connection would race the close"
    );

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
fn the_install_job_uses_a_runner_path_and_runs_install_twice() {
    // CI 44754877109: the job staged under `$RUNNER_WORK`, which GitHub never sets —
    // the empty expansion made `work` "/install-check" and the runner user could not
    // mkdir at the filesystem root. The job never passed once. Pin the real variable
    // (RUNNER_TEMP) and the promised shape: install runs, then runs again.
    let workflow = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/deploy-check.yml"
    ))
    .unwrap();
    assert!(
        !workflow.contains("RUNNER_WORK"),
        "RUNNER_WORK is not a GitHub variable — it expands to '/'"
    );
    assert!(
        workflow.contains("$RUNNER_TEMP"),
        "the scratch dir must live under the runner's temp"
    );
    assert_eq!(
        workflow.matches("deploy/droplet/install.sh").count(),
        2,
        "the job must run install.sh, then re-run it for idempotence"
    );
    // The filesystem-remote stub only proves the pipeline when the credentials
    // the pipeline reads are installed — both invocations stage them (the second
    // hits install's "already installed" branch, which is what keeps it honest).
    assert_eq!(
        workflow.matches("--r2-secret").count(),
        2,
        "every install.sh invocation must stage the fake R2 credentials"
    );
    assert_eq!(
        workflow.matches("--r2-env").count(),
        2,
        "every install.sh invocation must stage the fake r2.env"
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
