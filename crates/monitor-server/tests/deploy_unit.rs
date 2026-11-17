//! The shipped systemd unit is part of the security boundary: a monitor that parses
//! responses from the network and holds alert credentials must not run as root.

const UNIT: &str = include_str!("../../../deploy/monitor-server.service");

/// `key=value` settings of the `[Service]` section (later lines win, as in systemd).
fn service_setting(key: &str) -> Option<&'static str> {
    let mut in_service = false;
    let mut found = None;
    for line in UNIT.lines().map(str::trim) {
        if line.starts_with('[') {
            in_service = line == "[Service]";
            continue;
        }
        if in_service
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            found = Some(value.trim());
        }
    }
    found
}

#[test]
fn the_unit_runs_as_a_dedicated_unprivileged_user() {
    let user = service_setting("User").expect("the unit must set User=");
    assert!(!matches!(user, "" | "root" | "0"), "User={user} is root");
    let group = service_setting("Group").expect("the unit must set Group=");
    assert!(!matches!(group, "" | "root" | "0"), "Group={group} is root");
    // Nothing the monitor does needs a capability: it binds an unprivileged port.
    assert_eq!(service_setting("CapabilityBoundingSet"), Some(""));
    assert_eq!(service_setting("NoNewPrivileges"), Some("true"));
    assert_eq!(service_setting("ProtectSystem"), Some("strict"));
    assert_eq!(service_setting("ProtectHome"), Some("true"));
}

/// systemd's stop sends SIGTERM, then SIGKILL after `TimeoutStopSec`. The server drains
/// HTTP for up to 12 s and the engine for up to 15 s; the kill must not land inside that.
#[test]
fn the_stop_timeout_outlasts_the_drain() {
    let timeout: u64 = service_setting("TimeoutStopSec")
        .and_then(|value| value.trim_end_matches('s').parse().ok())
        .expect("the unit must set TimeoutStopSec in seconds");
    assert!(
        timeout > 12 + 15,
        "TimeoutStopSec={timeout} cuts the drain short"
    );
}

const DEPLOY_SH: &str = include_str!("../../../scripts/deploy.sh");

/// The `KEY=value` lines of the env template `deploy.sh` writes, as an operator gets them
/// by deleting the leading `# `.
fn env_template_settings() -> Option<Vec<(String, &'static str)>> {
    let start = DEPLOY_SH.find("<<'ENV'\n")? + "<<'ENV'\n".len();
    let end = start + DEPLOY_SH[start..].find("\nENV\n")?;
    Some(
        DEPLOY_SH[start..end]
            .lines()
            .filter_map(|line| line.strip_prefix("# SMON_"))
            .filter_map(|rest| rest.split_once('='))
            .map(|(key, value)| (format!("SMON_{key}"), value))
            .collect(),
    )
}

/// systemd's `EnvironmentFile=` has no inline comments: `KEY=43200   # note` sets the value
/// to the whole string. With strict `SMON_*` parsing, uncommenting such a template line
/// stops the boot. Every template line must be a bare `KEY=value`, and every example value
/// in it must pass the parser as written.
#[test]
fn the_env_template_has_no_inline_comments_and_its_examples_parse() {
    let settings = env_template_settings().expect("deploy.sh writes an env template");
    assert!(settings.len() >= 15, "template parsed: {settings:?}");
    for (key, value) in &settings {
        assert!(
            !value.contains('#'),
            "{key}: inline comment in the template value {value:?} — systemd keeps it"
        );
    }
    let examples: std::collections::HashMap<String, String> = settings
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.clone(), (*value).to_owned()))
        .collect();
    if let Err(error) =
        monitor_server::config::Config::from_lookup(|name| examples.get(name).cloned())
    {
        panic!("the template's example values must boot: {error}");
    }
}

const PLIST: &str = include_str!("../../../deploy/com.shirase.smon.plist");
const RUNBOOK: &str = include_str!("../../../deploy/RUNBOOK.md");

/// launchd sends SIGTERM, then SIGKILL after `ExitTimeOut` (its default is shorter than
/// the server's 12 s HTTP + 15 s engine drain), so `launchctl kickstart -k` could cut the
/// drain short.
#[test]
fn the_launchd_job_waits_out_the_drain() {
    let after_key = PLIST
        .split("<key>ExitTimeOut</key>")
        .nth(1)
        .expect("the plist must set ExitTimeOut");
    let seconds: u64 = after_key
        .split("<integer>")
        .nth(1)
        .and_then(|rest| rest.split("</integer>").next())
        .and_then(|value| value.trim().parse().ok())
        .expect("ExitTimeOut is an <integer>");
    assert!(
        seconds > 12 + 15,
        "ExitTimeOut={seconds} cuts the drain short"
    );
}

/// Re-copying the new unit onto an install that ran as root fails (217/USER without the
/// account; SQLite can't write root-owned data/). The upgrade section must carry the steps.
#[test]
fn the_runbook_upgrade_covers_moving_a_root_install_to_smon() {
    let upgrade = RUNBOOK
        .split("## Upgrade")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("RUNBOOK has an Upgrade section");
    for step in [
        "useradd --system",
        "chown -R smon:smon /opt/smon/data /opt/smon/status",
        "daemon-reload",
    ] {
        assert!(
            upgrade.contains(step),
            "Upgrade section is missing `{step}`"
        );
    }
}

/// `[Unit]` settings (where systemd's start-limit keys live).
fn unit_setting(key: &str) -> Option<&'static str> {
    let mut in_unit = false;
    let mut found = None;
    for line in UNIT.lines().map(str::trim) {
        if line.starts_with('[') {
            in_unit = line == "[Unit]";
            continue;
        }
        if in_unit
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            found = Some(value.trim());
        }
    }
    found
}

/// A deterministic crash (a writer panic on every boot) must not restart forever every 3 s:
/// past the start limit the unit is left `failed` — a state an operator (and the expiring
/// public heartbeat) can see — instead of a silent loop.
#[test]
fn the_unit_stops_restarting_a_crash_loop() {
    let interval: u64 = unit_setting("StartLimitIntervalSec")
        .and_then(|value| value.trim_end_matches('s').parse().ok())
        .expect("[Unit] must set StartLimitIntervalSec in seconds");
    let burst: u64 = unit_setting("StartLimitBurst")
        .and_then(|value| value.parse().ok())
        .expect("[Unit] must set StartLimitBurst");
    assert!(
        burst >= 2 && interval >= burst * 3,
        "{burst} starts in {interval}s"
    );
}

fn seconds(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(minutes) = value.strip_suffix("min") {
        return minutes.trim().parse::<u64>().ok().map(|m| m * 60);
    }
    value.trim_end_matches('s').parse().ok()
}

/// The store lives on a volume that may attach after boot. Without a mount dependency the
/// unit starts against the bare mountpoint (226/NAMESPACE from `ReadWritePaths`, or an empty
/// directory) and fails.
#[test]
fn the_unit_waits_for_the_data_and_status_mounts() {
    let mounts = unit_setting("RequiresMountsFor").expect("[Unit] must set RequiresMountsFor");
    for path in ["/opt/smon/data", "/opt/smon/status"] {
        assert!(
            mounts.split_whitespace().any(|m| m == path),
            "RequiresMountsFor lacks {path}"
        );
    }
}

/// Restarts back off, and the start limit is out of reach of a backed-off loop: a slow
/// dependency keeps being retried (no permanent `failed` after ~20 s, which is what 5
/// starts in 600 s at `RestartSec=3s` produced), while a crash loop is spaced out to
/// `RestartMaxDelaySec` instead of hammering every few seconds. On systemd < 254, which
/// ignores `RestartSteps`/`RestartMaxDelaySec`, the fixed `RestartSec` still takes minutes
/// to reach the limit.
#[test]
fn restarts_back_off_and_a_slow_dependency_is_retried() {
    let restart = service_setting("RestartSec")
        .and_then(seconds)
        .expect("RestartSec");
    let steps: u64 = service_setting("RestartSteps")
        .and_then(|value| value.parse().ok())
        .expect("RestartSteps");
    let max_delay = service_setting("RestartMaxDelaySec")
        .and_then(seconds)
        .expect("RestartMaxDelaySec");
    let interval = unit_setting("StartLimitIntervalSec")
        .and_then(seconds)
        .expect("interval");
    let burst: u64 = unit_setting("StartLimitBurst")
        .and_then(|value| value.parse().ok())
        .expect("burst");
    assert!(restart >= 10, "RestartSec={restart}s retries too fast");
    assert!(
        steps >= 3 && max_delay >= 300,
        "backoff {steps} steps to {max_delay}s"
    );
    assert!(
        max_delay * burst > interval,
        "at the backed-off delay a crash loop must never reach the start limit"
    );
    assert!(
        restart * burst >= 120,
        "without backoff support the limit must still take minutes, not {}s",
        restart * burst
    );
}
